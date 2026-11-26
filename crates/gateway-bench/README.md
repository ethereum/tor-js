# gateway-bench

Load and memory tests for `tor-js-gateway`'s CONNECT tunnels over WebRTC KPS.
It runs a real gateway binary against a mock Tor network on loopback. Nothing
touches the internet.

## The mock Tor network (`src/mock_tor.rs`)

Past `CONNECT`, a gateway tunnel is a byte pipe (PROTOCOL.md §4). So a mock
relay only needs to behave like a TCP peer with a given traffic shape or
failure; it doesn't speak Tor's link protocol. Each relay has one behaviour:

| Behaviour | What it does |
|---|---|
| `echo` | echoes, FIN after the client's FIN |
| `source:N` | sends N bytes, then FIN |
| `sink` | reads to FIN, replies with the byte count (8 bytes, big-endian) |
| `cells:N` | for every 514-byte cell received, sends N cells back (a download-heavy circuit) |
| `blackhole` | accepts and never reads or writes (exercises the idle timeout) |
| `reset:N` | echoes N bytes, then sends a TCP RST |

The farm writes `consensus-microdesc.txt` with one `r` line per relay. The
gateway, run with `--no-sync --no-mirror` and
`TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS=1`, allowlists exactly those relays.

`gateway-bench relays --data-dir <dir> echo sink blackhole` runs just a farm,
for manual or browser testing.

## Scenarios

```bash
cargo build --release -p tor-js-gateway -p gateway-bench
```

```bash
target/release/gateway-bench throughput --tunnels 8 --bytes 33554432 --dir down
```

```bash
target/release/gateway-bench churn --conns 500 --parallel 20 --streams 40 --concurrent 4 --rounds 3
```

```bash
target/release/gateway-bench long --streams 20000
```

- **throughput:** one connection, N concurrent tunnels, bulk download or upload.
- **churn:** many connections, each running many short echo tunnels. Samples the
  gateway's RSS, fds and threads during the load and after the clients leave.
  With `--rounds`, memory that keeps growing round after round is a leak;
  growth only in the first round is the allocator keeping its high-water mark.
- **long:** one connection, many sequential tunnels, with memory per tunnel.

Every scenario fails if the gateway prints kps's drain-backstop warning
(`[kps-rtc] warning: channel … closing anyway`), a channel whose sent data
never drained. The backstop frees the channel after 30 s, so the scenario
itself still completes and only the warning shows the leak. A channel stuck
within 30 s of the end of a scenario isn't caught.

Use `--gateway <bin>` to test another build. The gateway's per-tunnel `info`
logs are off by default (`--gateway-log`).

Linux only: process stats come from `/proc`.
