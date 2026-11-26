//! Runs a real `tor-js-gateway` binary against a mock farm, and samples the
//! process's memory, file descriptors and threads from /proc (Linux).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::mock_tor::Farm;

pub struct Gateway {
    pub address: String,
    pub child: Child,
    pub dir: PathBuf,
    /// Lines the gateway printed containing kps's drain-backstop warning
    /// (`[kps-rtc] warning`): channels whose sent data never drained, closed
    /// after 30 s. Every scenario fails if this is nonzero.
    pub backstop: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Gateway {
    /// Writes a config, a fake bootstrap archive and the farm's consensus into
    /// a fresh dir, then starts `bin run --no-sync --no-mirror` and waits for
    /// it to print its KPS address.
    pub async fn start(bin: &Path, farm: &Farm, log_level: &str) -> anyhow::Result<Self> {
        let dir = std::env::temp_dir().join(format!("gateway-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = dir.join("data");
        std::fs::create_dir_all(data.join("keccak"))?;
        std::fs::write(data.join("bootstrap.zip"), [0u8; 1024])?;
        std::fs::write(data.join("bootstrap.zip.zst"), [0u8; 16])?;
        farm.write_consensus(&data)?;

        let port = std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port();
        let cfg = serde_json::json!({
            "data_dir": data,
            "kps_port": port,
            "kps_key_file": dir.join("kps.key"),
            "keccak_repo": "",
            "keccak_branch": "",
            "keccak_poll_interval": 86400,
            "keccak_manual_sync_min_interval": 1800,
            "demo": false,
            "advertised_addresses": ["127.0.0.1"],
            // Every bench client comes from 127.0.0.1, so the per-IP cap
            // (also the per-connection cap) must not be what's measured.
            "tunnel_max": 100000,
            "tunnel_per_ip": 100000,
            "tunnel_idle_timeout": 300,
            "tunnel_max_lifetime": 3600,
        });
        let cfg_path = dir.join("config.json5");
        std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg)?)?;

        let mut child = Command::new(bin)
            .arg("--config")
            .arg(&cfg_path)
            .args(["run", "--no-sync", "--no-mirror"])
            .env("TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS", "1")
            .env("TOR_JS_GATEWAY_GITHUB_API", "http://127.0.0.1:1")
            .env("TOR_JS_GATEWAY_GITHUB_RAW", "http://127.0.0.1:1")
            .env("GITHUB_TOKEN", "")
            .env("RUST_LOG", log_level)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;

        // The address line can be on either stream; keep draining both after.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        for r in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let _ = tx.send(l);
                }
            });
        }
        let mut seen = String::new();
        let address = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(line) = rx.recv().await {
                seen += &line;
                seen.push('\n');
                if let Some(i) = line.find("127.0.0.1:") {
                    let tail = &line[i..];
                    let end = tail
                        .find(|c: char| !(c.is_ascii_alphanumeric() || "-_:.".contains(c)))
                        .unwrap_or(tail.len());
                    let cand = &tail[..end];
                    if cand.matches(':').count() == 2 && cand.contains(":u") {
                        return Some(cand.to_owned());
                    }
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        let Some(address) = address else { bail!("gateway did not print its address; output:\n{seen}") };
        // Keep draining so the pipes never fill and block the gateway.
        let backstop = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let count = backstop.clone();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if line.contains("[kps-rtc] warning") {
                    eprintln!("gateway: {line}");
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
        Ok(Self { address, child, dir, backstop })
    }

    pub fn pid(&self) -> u32 {
        self.child.id().expect("gateway exited")
    }

    pub fn sample(&self) -> Sample {
        Sample::of(self.pid())
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub rss_kb: u64,
    pub fds: u64,
    pub threads: u64,
}

impl Sample {
    pub fn of(pid: u32) -> Self {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
                .unwrap_or(0)
        };
        let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).map(|d| d.count() as u64).unwrap_or(0);
        Self { rss_kb: field("VmRSS:"), fds, threads: field("Threads:") }
    }
}
