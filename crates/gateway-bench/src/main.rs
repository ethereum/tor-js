//! Load and memory tests for tor-js-gateway's CONNECT tunnels over WebRTC KPS,
//! against a mock Tor relay farm (`mock_tor`). The gateway under test is a
//! real binary, run with `--no-sync --no-mirror`; nothing touches the network.
//!
//! Scenarios:
//! - `throughput`: one connection, N concurrent tunnels, bulk download or upload.
//! - `churn`: many connections, each running many short echo tunnels; memory,
//!   fds and threads sampled throughout and after the clients leave.
//! - `long`: one connection, many sequential tunnels; memory per tunnel.
//! - `relays`: just run a mock farm and write its consensus, for manual tests.

mod client;
mod gateway;
mod mock_tor;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use gateway::{Gateway, Sample};
use mock_tor::{Behaviour, Farm};

#[derive(Parser)]
struct Cli {
    /// The tor-js-gateway binary to test.
    #[arg(long, global = true, default_value = "target/release/tor-js-gateway")]
    gateway: PathBuf,
    /// RUST_LOG for the gateway. The default keeps the startup lines (the
    /// address is read from them) but not the per-tunnel and per-connection
    /// `info` lines, which production logs and which add per-tunnel cost.
    #[arg(
        long,
        global = true,
        default_value = "warn,tor_js_gateway=info,tor_js_gateway::tunnel=warn,tor_js_gateway::kps_server=warn"
    )]
    gateway_log: String,
    /// Seconds to keep each connection open after its last tunnel, so kps's
    /// 30 s drain backstop can fire on any channel left stuck (it only runs
    /// while the connection is open). Applies to `throughput` and `long`; for
    /// `churn` see --conn-hold.
    #[arg(long, global = true, default_value_t = 31)]
    hold: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Dir {
    /// Relay to client.
    Down,
    /// Client to relay.
    Up,
}

#[derive(Subcommand)]
enum Cmd {
    Throughput {
        #[arg(long, default_value_t = 8)]
        tunnels: usize,
        /// Bytes per tunnel.
        #[arg(long, default_value_t = 32 << 20)]
        bytes: u64,
        #[arg(long, value_enum, default_value_t = Dir::Down)]
        dir: Dir,
    },
    Churn {
        /// Total connections.
        #[arg(long, default_value_t = 200)]
        conns: usize,
        /// Connections open at once.
        #[arg(long, default_value_t = 20)]
        parallel: usize,
        /// Tunnels per connection.
        #[arg(long, default_value_t = 50)]
        streams: usize,
        /// Tunnels open at once per connection.
        #[arg(long, default_value_t = 4)]
        concurrent: usize,
        /// Echo payload per tunnel, in bytes.
        #[arg(long, default_value_t = 16 * 1024)]
        size: usize,
        /// Seconds to keep sampling after the last client leaves.
        #[arg(long, default_value_t = 20)]
        settle: u64,
        /// Repeat the whole load this many times on the same gateway. Memory
        /// that keeps growing round after round is a leak; growth only in the
        /// first round is the allocator keeping its high-water mark.
        #[arg(long, default_value_t = 1)]
        rounds: usize,
        /// Seconds each connection stays open after its tunnels. 0 (the
        /// default) means the drain backstop can't fire in this scenario:
        /// each connection closes seconds after opening. 31 makes it able to,
        /// at the cost of ~31 s per wave of `parallel` connections.
        #[arg(long, default_value_t = 0)]
        conn_hold: u64,
    },
    Long {
        #[arg(long, default_value_t = 10_000)]
        streams: usize,
        #[arg(long, default_value_t = 4096)]
        size: usize,
        #[arg(long, default_value_t = 1000)]
        every: usize,
    },
    Relays {
        /// Relay behaviours, e.g. `echo source:1048576 sink blackhole`.
        #[arg(default_values_t = ["echo".to_owned(), "sink".to_owned(), "cells:8".to_owned(), "blackhole".to_owned()])]
        behaviours: Vec<String>,
        /// Gateway data dir to write consensus-microdesc.txt into.
        #[arg(long)]
        data_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let gw = run(cli).await?;
    // The backstop closes a channel whose data never drained after 30 s, so
    // its slot comes back and the scenario still completes; only this count
    // shows the leak. It only runs while the channel's connection is open, so
    // a channel stuck within 30 s of its connection closing goes uncounted:
    // hence --hold (throughput, long) and --conn-hold (churn).
    if let Some(gw) = gw {
        gw.check_alive()?;
        let n = gw.backstop.load(Ordering::Relaxed);
        if n > 0 {
            bail!("the gateway hit kps's drain backstop {n} time(s)");
        }
    }
    Ok(())
}

async fn run(cli: Cli) -> anyhow::Result<Option<Gateway>> {
    match cli.cmd {
        Cmd::Relays { behaviours, data_dir } => {
            let bs: Vec<Behaviour> = behaviours.iter().map(|b| b.parse()).collect::<Result<_, _>>()?;
            let farm = Farm::start(&bs).await?;
            farm.write_consensus(&data_dir)?;
            for r in &farm.relays {
                println!("{} {:?}", r.addr, r.behaviour);
            }
            println!("wrote {}; Ctrl-C to stop", data_dir.join("consensus-microdesc.txt").display());
            tokio::signal::ctrl_c().await?;
            Ok(None)
        }
        Cmd::Throughput { tunnels, bytes, dir } => {
            let farm = Farm::start(&[Behaviour::Source(bytes), Behaviour::Sink]).await?;
            let gw = Gateway::start(&cli.gateway, &farm, &cli.gateway_log).await?;
            let r = throughput(&gw, &farm, tunnels, bytes, dir, Duration::from_secs(cli.hold)).await;
            gw.check_alive()?;
            r?;
            Ok(Some(gw))
        }
        Cmd::Churn { conns, parallel, streams, concurrent, size, settle, rounds, conn_hold } => {
            let farm = Farm::start(&[Behaviour::Echo]).await?;
            let gw = Gateway::start(&cli.gateway, &farm, &cli.gateway_log).await?;
            let first = gw.sample();
            let mut after = Vec::new();
            for r in 0..rounds {
                println!("--- round {} of {rounds}", r + 1);
                let r = churn(&gw, &farm, conns, parallel, streams, concurrent, size, settle, Duration::from_secs(conn_hold)).await;
                gw.check_alive()?;
                r?;
                after.push(gw.sample().rss_kb);
            }
            let list: Vec<String> = after.iter().map(|k| format!("{:.1}", *k as f64 / 1024.0)).collect();
            println!("rounds: rss at start {:.1} MiB, after each round [{}] MiB", first.rss_kb as f64 / 1024.0, list.join(", "));
            if after.len() >= 2 {
                let per_round = (after[after.len() - 1] as f64 - after[0] as f64) / (after.len() - 1) as f64;
                let per_tunnel = per_round * 1024.0 / (conns * streams) as f64;
                println!("rounds: after round 1, rss grows {:.2} MiB per round = {per_tunnel:.0} B per tunnel", per_round / 1024.0);
            }
            Ok(Some(gw))
        }
        Cmd::Long { streams, size, every } => {
            let farm = Farm::start(&[Behaviour::Echo]).await?;
            let gw = Gateway::start(&cli.gateway, &farm, &cli.gateway_log).await?;
            let r = long(&gw, &farm, streams, size, every, Duration::from_secs(cli.hold)).await;
            gw.check_alive()?;
            r?;
            Ok(Some(gw))
        }
    }
}

async fn dial(gw: &Gateway) -> anyhow::Result<Arc<dyn kps::Conn>> {
    let c = tokio::time::timeout(Duration::from_secs(15), kps::dial_webrtc(&gw.address))
        .await
        .context("dial timed out")??;
    Ok(Arc::from(c))
}

/// One echo tunnel: send `payload`, FIN, read everything back until the
/// gateway's FIN, and check it.
async fn echo_tunnel(conn: &dyn kps::Conn, target: std::net::SocketAddr, payload: &[u8]) -> anyhow::Result<()> {
    let (mut s, mut got) = client::connect(conn, target).await?;
    let (mut rd, mut wr) = tokio::io::split(&mut s);
    let send = async {
        wr.write_all(payload).await?;
        wr.shutdown().await
    };
    let recv = async { rd.read_to_end(&mut got).await };
    let (a, b) = tokio::join!(send, recv);
    a.context("send")?;
    b.context("receive")?;
    if got != payload {
        bail!("echo differs: {} of {} bytes", got.len(), payload.len());
    }
    Ok(())
}

/// Per-tunnel limit in churn and long: well under kps's 30 s drain backstop,
/// so a stall fails here instead of racing the backstop.
const ECHO_TIMEOUT: Duration = Duration::from_secs(15);

fn payload(size: usize, seed: usize) -> Vec<u8> {
    (0..size).map(|i| ((i * 7 + seed) % 251) as u8).collect()
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

fn print_sample(label: &str, t: Duration, s: Sample, extra: &str) {
    println!(
        "{label:>10} t={:>6.1}s rss={:>7.1} MiB fds={:>5} threads={:>3} {extra}",
        t.as_secs_f64(),
        s.rss_kb as f64 / 1024.0,
        s.fds,
        s.threads
    );
}

async fn throughput(gw: &Gateway, farm: &Farm, tunnels: usize, bytes: u64, dir: Dir, hold: Duration) -> anyhow::Result<()> {
    let conn = dial(gw).await?;
    let before = gw.sample();
    let start = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..tunnels {
        let conn = conn.clone();
        let down = dir == Dir::Down;
        let target = if down { farm.find(Behaviour::Source(bytes)) } else { farm.find(Behaviour::Sink) };
        tasks.push(tokio::spawn(async move {
            let (mut s, extra) = client::connect(conn.as_ref(), target).await?;
            if down {
                let mut n = extra.len() as u64;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let k = s.read(&mut buf).await?;
                    if k == 0 {
                        break;
                    }
                    n += k as u64;
                }
                if n != bytes {
                    bail!("tunnel {i}: got {n} of {bytes} bytes");
                }
            } else {
                let chunk = vec![0xa5u8; 64 * 1024];
                let mut left = bytes;
                while left > 0 {
                    let k = left.min(chunk.len() as u64) as usize;
                    s.write_all(&chunk[..k]).await?;
                    left -= k as u64;
                }
                s.shutdown().await?;
                let mut reply = extra;
                s.read_to_end(&mut reply).await?;
                let n = u64::from_be_bytes(reply.get(..8).context("no count")?.try_into()?);
                if n != bytes {
                    bail!("tunnel {i}: relay got {n} of {bytes} bytes");
                }
            }
            anyhow::Ok(())
        }));
    }
    for t in tasks {
        t.await??;
    }
    let secs = start.elapsed().as_secs_f64();
    let total = bytes * tunnels as u64;
    let after = gw.sample();
    println!(
        "throughput {}: {tunnels} tunnels x {:.0} MiB in {secs:.2}s = {:.1} MiB/s (rss {:.1} -> {:.1} MiB)",
        if dir == Dir::Down { "down" } else { "up" },
        mb(bytes),
        mb(total) / secs,
        before.rss_kb as f64 / 1024.0,
        after.rss_kb as f64 / 1024.0
    );
    tokio::time::sleep(hold).await;
    let _ = conn.close().await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn churn(
    gw: &Gateway,
    farm: &Farm,
    conns: usize,
    parallel: usize,
    streams: usize,
    concurrent: usize,
    size: usize,
    settle: u64,
    conn_hold: Duration,
) -> anyhow::Result<()> {
    let target = farm.find(Behaviour::Echo);
    let start = Instant::now();
    let base = gw.sample();
    print_sample("start", Duration::ZERO, base, "");

    let done_conns = Arc::new(AtomicU64::new(0));
    let done_tunnels = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    let pid = gw.pid();
    let sampler = {
        let (dc, dt, f) = (done_conns.clone(), done_tunnels.clone(), failures.clone());
        tokio::spawn(async move {
            let mut peak = Sample::default();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let s = Sample::of(pid);
                if !s.alive {
                    eprintln!("the gateway process is gone");
                    return peak;
                }
                peak.rss_kb = peak.rss_kb.max(s.rss_kb);
                peak.fds = peak.fds.max(s.fds);
                print_sample(
                    "load",
                    start.elapsed(),
                    s,
                    &format!(
                        "conns={} tunnels={} failures={}",
                        dc.load(Ordering::Relaxed),
                        dt.load(Ordering::Relaxed),
                        f.load(Ordering::Relaxed)
                    ),
                );
                if dc.load(Ordering::Relaxed) as usize >= conns {
                    return peak;
                }
            }
        })
    };

    let sem = Arc::new(tokio::sync::Semaphore::new(parallel));
    let mut tasks = Vec::new();
    for c in 0..conns {
        let permit = sem.clone().acquire_owned().await?;
        let (dc, dt, f) = (done_conns.clone(), done_tunnels.clone(), failures.clone());
        let addr = gw.address.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let conn: Arc<dyn kps::Conn> =
                match tokio::time::timeout(Duration::from_secs(15), kps::dial_webrtc(&addr)).await {
                    Ok(Ok(c)) => Arc::from(c),
                    other => {
                        eprintln!("conn {c}: dial failed: {:?}", other.map(|r| r.err()));
                        f.fetch_add(1, Ordering::Relaxed);
                        dc.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                };
            let next = Arc::new(AtomicU64::new(0));
            let mut workers = Vec::new();
            for _ in 0..concurrent {
                let (conn, next, dt, f) = (conn.clone(), next.clone(), dt.clone(), f.clone());
                workers.push(tokio::spawn(async move {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                        if i >= streams {
                            return;
                        }
                        let p = payload(size, c * 1000 + i);
                        let r = tokio::time::timeout(ECHO_TIMEOUT, echo_tunnel(conn.as_ref(), target, &p)).await;
                        match r {
                            Ok(Ok(())) => {
                                dt.fetch_add(1, Ordering::Relaxed);
                            }
                            other => {
                                eprintln!("conn {c} tunnel {i}: {:?}", other.map(|r| r.err()));
                                f.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }));
            }
            for w in workers {
                let _ = w.await;
            }
            tokio::time::sleep(conn_hold).await;
            let _ = conn.close().await;
            dc.fetch_add(1, Ordering::Relaxed);
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    let load_secs = start.elapsed().as_secs_f64();
    let peak = sampler.await?;
    let tunnels = done_tunnels.load(Ordering::Relaxed);
    let end_of_load = gw.sample();
    print_sample("clients done", start.elapsed(), end_of_load, "");

    for _ in 0..settle / 5 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        print_sample("settle", start.elapsed(), gw.sample(), &format!("relay tcp open={}", farm.stats.open.load(Ordering::Relaxed)));
    }
    let end = gw.sample();
    println!(
        "churn: {conns} conns x {streams} tunnels ({parallel} conns, {concurrent} tunnels each at a time), {size} B echo: \
         {tunnels} ok, {} failed in {load_secs:.1}s ({:.0} tunnels/s)",
        failures.load(Ordering::Relaxed),
        tunnels as f64 / load_secs
    );
    println!(
        "  rss: start {:.1} MiB, peak {:.1}, after settle {:.1} (+{:.1} MiB, {:.0} B per conn, {:.0} B per tunnel); fds start {} peak {} end {}; relay tcp still open {}",
        base.rss_kb as f64 / 1024.0,
        peak.rss_kb as f64 / 1024.0,
        end.rss_kb as f64 / 1024.0,
        (end.rss_kb as f64 - base.rss_kb as f64) / 1024.0,
        (end.rss_kb as f64 - base.rss_kb as f64) * 1024.0 / conns as f64,
        (end.rss_kb as f64 - base.rss_kb as f64) * 1024.0 / tunnels.max(1) as f64,
        base.fds,
        peak.fds,
        end.fds,
        farm.stats.open.load(Ordering::Relaxed)
    );
    let failed = failures.load(Ordering::Relaxed);
    if failed > 0 {
        bail!("{failed} dials or tunnels failed");
    }
    Ok(())
}

async fn long(gw: &Gateway, farm: &Farm, streams: usize, size: usize, every: usize, hold: Duration) -> anyhow::Result<()> {
    let target = farm.find(Behaviour::Echo);
    let conn = dial(gw).await?;
    let start = Instant::now();
    let mut first: Option<(usize, Sample)> = None;
    let mut last = gw.sample();
    print_sample("start", Duration::ZERO, last, "");
    for i in 0..streams {
        let p = payload(size, i);
        tokio::time::timeout(ECHO_TIMEOUT, echo_tunnel(conn.as_ref(), target, &p))
            .await
            .with_context(|| format!("tunnel {i} timed out"))?
            .with_context(|| format!("tunnel {i}"))?;
        if (i + 1) % every == 0 {
            // Let the last tunnel's teardown finish before sampling.
            tokio::time::sleep(Duration::from_millis(500)).await;
            last = gw.sample();
            print_sample("tunnels", start.elapsed(), last, &format!("n={}", i + 1));
            if first.is_none() {
                first = Some((i + 1, last));
            }
        }
    }
    if let Some((n0, s0)) = first {
        let per = (last.rss_kb as f64 - s0.rss_kb as f64) * 1024.0 / (streams - n0).max(1) as f64;
        println!(
            "long: {streams} sequential tunnels on one connection, {size} B echo, {:.0} tunnels/s; rss grows {per:.0} B per tunnel after the first {n0}",
            streams as f64 / start.elapsed().as_secs_f64()
        );
    }
    tokio::time::sleep(hold).await;
    let _ = conn.close().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    print_sample("closed", start.elapsed(), gw.sample(), "");
    Ok(())
}
