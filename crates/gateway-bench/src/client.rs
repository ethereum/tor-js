//! A KPS-HTTP/1 `CONNECT` client (PROTOCOL.md §4) over WebRTC KPS streams.

use anyhow::{Context, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Opens a stream on `conn` and tunnels it to `target`. Returns the stream,
/// ready for tunnel bytes, plus any tunnel bytes that arrived with the
/// response head.
pub async fn connect(
    conn: &dyn kps::Conn,
    target: std::net::SocketAddr,
) -> anyhow::Result<(Box<dyn kps::Stream>, Vec<u8>)> {
    let mut s = conn.open_stream().await.context("open_stream")?;
    s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .context("write CONNECT")?;
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    let end = loop {
        let n = s.read(&mut buf).await.context("read response")?;
        if n == 0 {
            bail!("stream ended before the response head");
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if head.len() > 16 * 1024 {
            bail!("response head too long");
        }
    };
    let status = String::from_utf8_lossy(&head[..end]);
    if !status.starts_with("HTTP/1.1 200") {
        bail!("CONNECT refused: {}", status.lines().next().unwrap_or(""));
    }
    Ok((s, head[end..].to_vec()))
}
