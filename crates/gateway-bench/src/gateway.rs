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
    child: std::sync::Mutex<Child>,
    pid: u32,
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
        // A fresh, unpredictable dir, created here (fails if it exists), since
        // it holds the gateway's key file.
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
        let dir = std::env::temp_dir().join(format!("gateway-bench-{}-{nonce:x}", std::process::id()));
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let data = dir.join("data");
        std::fs::create_dir_all(data.join("keccak"))?;
        std::fs::write(data.join("bootstrap.zip"), [0u8; 1024])?;
        std::fs::write(data.join("bootstrap.zip.zst"), [0u8; 16])?;
        farm.write_consensus(&data)?;

        // A port that was free a moment ago. Something else could take it
        // before the gateway binds; the gateway then fails to start, and the
        // error below shows its output.
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
            // Every bench client comes from 127.0.0.1, and churn can run more
            // tunnels per connection than a tor-js client would, so neither
            // the per-IP nor the per-connection cap should be what's measured.
            "tunnel_max": 100000,
            "tunnel_per_ip": 100000,
            "tunnel_per_conn": 100000,
            "conn_per_ip": 100000,
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
                if let Some(a) = find_address(&line) {
                    return Some(a);
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
        let pid = child.id().context("gateway exited at startup")?;
        Ok(Self { address, child: std::sync::Mutex::new(child), pid, dir, backstop })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Fails if the gateway process has exited (a crash mid-run would
    /// otherwise show only as failed dials and zeroed memory samples).
    pub fn check_alive(&self) -> anyhow::Result<()> {
        if let Some(status) = self.child.lock().unwrap().try_wait()? {
            bail!("the gateway exited during the run: {status}");
        }
        Ok(())
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

/// The first `127.0.0.1:<port>:u<base64url>` in `line`, as the gateway prints
/// its KPS address. Every occurrence of "127.0.0.1:" is tried, and the
/// certhash takes base64url characters only, so trailing punctuation isn't
/// swallowed.
fn find_address(line: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = line[from..].find("127.0.0.1:") {
        let start = from + i;
        let rest = &line[start + "127.0.0.1:".len()..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && rest[digits..].starts_with(":u") {
            let hash = &rest[digits + 2..];
            let n = hash.len()
                - hash.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_').len();
            if n > 0 {
                return Some(line[start..start + "127.0.0.1:".len() + digits + 2 + n].to_owned());
            }
        }
        from = start + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::find_address;

    #[test]
    fn finds_the_address_among_other_loopback_mentions() {
        assert_eq!(
            find_address("listening on 127.0.0.1:4000, address 127.0.0.1:4000:uEiAb-_9."),
            Some("127.0.0.1:4000:uEiAb-_9".to_owned())
        );
        assert_eq!(find_address("│    127.0.0.1:1234:uEiX"), Some("127.0.0.1:1234:uEiX".to_owned()));
        assert_eq!(find_address("127.0.0.1:12: no hash"), None);
        assert_eq!(find_address("127.0.0.1:x:uEi"), None);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    /// False once the process has exited (gone, or a zombie).
    pub alive: bool,
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
        let state = status.lines().find_map(|l| l.strip_prefix("State:")).map(str::trim).unwrap_or("");
        let alive = !state.is_empty() && !state.starts_with('Z') && !state.starts_with('X');
        Self { alive, rss_kb: field("VmRSS:"), fds, threads: field("Threads:") }
    }
}
