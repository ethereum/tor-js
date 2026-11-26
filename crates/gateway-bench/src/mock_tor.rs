//! A mock Tor network for exercising the gateway: TCP "relays" on loopback
//! with fixed behaviours, and the cached consensus that makes the gateway
//! allowlist them.
//!
//! The gateway never parses relay traffic: past `CONNECT` a tunnel is a byte
//! pipe (PROTOCOL.md §4). So a relay only has to behave like a TCP peer with
//! the traffic shape or failure under test, not speak Tor's link protocol.
//! Pair it with `run --no-sync` and `TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS=1`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What a relay does with each tunnel that reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behaviour {
    /// Echoes everything back, FIN after the client's FIN.
    Echo,
    /// Sends this many bytes, then FIN. Ignores what the client sends.
    Source(u64),
    /// Reads to the client's FIN, then replies with the byte count as an
    /// 8-byte big-endian integer and FINs.
    Sink,
    /// Tor-like circuit traffic: for every 514-byte cell received, sends
    /// `n` cells back (a download-heavy circuit), FIN after the client's FIN.
    Cells(u32),
    /// Accepts and never reads or writes: tests the gateway's idle timeout.
    Blackhole,
    /// Echoes this many bytes, then resets the connection (TCP RST).
    ResetAfter(u64),
}

impl std::str::FromStr for Behaviour {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (name, arg) = s.split_once(':').unwrap_or((s, ""));
        let num = || -> anyhow::Result<u64> { Ok(arg.parse()?) };
        Ok(match name {
            "echo" => Self::Echo,
            "source" => Self::Source(num()?),
            "sink" => Self::Sink,
            "cells" => Self::Cells(num()? as u32),
            "blackhole" => Self::Blackhole,
            "reset" => Self::ResetAfter(num()?),
            _ => anyhow::bail!("unknown relay behaviour {s:?}"),
        })
    }
}

pub const CELL: usize = 514;

/// Counters shared by all relays of a farm.
#[derive(Default, Debug)]
pub struct Stats {
    pub accepted: AtomicU64,
    pub open: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
}

pub struct Relay {
    pub addr: SocketAddr,
    pub behaviour: Behaviour,
}

/// A set of running relays.
pub struct Farm {
    pub relays: Vec<Relay>,
    pub stats: Arc<Stats>,
}

impl Farm {
    /// Starts one relay per behaviour on 127.0.0.1, each on its own port.
    pub async fn start(behaviours: &[Behaviour]) -> anyhow::Result<Self> {
        let stats = Arc::new(Stats::default());
        let mut relays = Vec::new();
        for &behaviour in behaviours {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let stats = stats.clone();
            tokio::spawn(async move {
                while let Ok((sock, _)) = listener.accept().await {
                    let stats = stats.clone();
                    stats.accepted.fetch_add(1, Ordering::Relaxed);
                    stats.open.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(async move {
                        let _ = serve(sock, behaviour, &stats).await;
                        stats.open.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            });
            relays.push(Relay { addr, behaviour });
        }
        Ok(Self { relays, stats })
    }

    /// The first relay with this behaviour.
    pub fn find(&self, b: Behaviour) -> SocketAddr {
        self.relays.iter().find(|r| r.behaviour == b).expect("no such relay").addr
    }

    /// Writes `consensus-microdesc.txt` into the gateway's data dir: one `r`
    /// line per relay, which the gateway turns into its CONNECT allowlist at
    /// startup (`preload_allowlist`). Not a valid consensus otherwise, which
    /// is fine under `--no-sync`.
    pub fn write_consensus(&self, data_dir: &Path) -> anyhow::Result<()> {
        let mut text = String::new();
        for (i, r) in self.relays.iter().enumerate() {
            text += &format!(
                "r mock{i} AAAAAAAAAAAAAAAAAAAAAAAAAAA 2026-01-01 00:00:00 {} {} 0\n",
                r.addr.ip(),
                r.addr.port()
            );
        }
        std::fs::write(data_dir.join("consensus-microdesc.txt"), text)?;
        Ok(())
    }
}

async fn serve(mut sock: TcpStream, b: Behaviour, st: &Stats) -> std::io::Result<()> {
    sock.set_nodelay(true)?;
    let mut buf = vec![0u8; 64 * 1024];
    match b {
        Behaviour::Echo => loop {
            let n = sock.read(&mut buf).await?;
            if n == 0 {
                return sock.shutdown().await;
            }
            st.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
            sock.write_all(&buf[..n]).await?;
            st.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
        },
        Behaviour::Source(total) => {
            let chunk = vec![0x5a; 64 * 1024];
            let mut left = total;
            while left > 0 {
                let n = left.min(chunk.len() as u64) as usize;
                sock.write_all(&chunk[..n]).await?;
                st.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
                left -= n as u64;
            }
            sock.shutdown().await
        }
        Behaviour::Sink => {
            let mut total = 0u64;
            loop {
                let n = sock.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
            st.bytes_in.fetch_add(total, Ordering::Relaxed);
            sock.write_all(&total.to_be_bytes()).await?;
            sock.shutdown().await
        }
        Behaviour::Cells(ratio) => {
            let reply = vec![0x33; CELL * ratio as usize];
            let mut pending = 0usize;
            loop {
                let n = sock.read(&mut buf).await?;
                if n == 0 {
                    return sock.shutdown().await;
                }
                st.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
                pending += n;
                while pending >= CELL {
                    pending -= CELL;
                    sock.write_all(&reply).await?;
                    st.bytes_out.fetch_add(reply.len() as u64, Ordering::Relaxed);
                }
            }
        }
        Behaviour::Blackhole => {
            std::future::pending::<()>().await;
            Ok(())
        }
        Behaviour::ResetAfter(limit) => {
            let mut seen = 0u64;
            while seen < limit {
                let n = sock.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let n = (n as u64).min(limit - seen) as usize;
                sock.write_all(&buf[..n]).await?;
                seen += n as u64;
            }
            // SO_LINGER 0 makes the close an RST. (Deprecated because a nonzero
            // linger blocks on drop; zero doesn't.)
            #[allow(deprecated)]
            sock.set_linger(Some(std::time::Duration::ZERO))?;
            drop(sock);
            Ok(())
        }
    }
}
