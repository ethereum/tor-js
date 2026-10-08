# Changelog

## Unreleased

One addition. The WASM changes, so the anon-rpc worker's keccak256 pin changes
too.

### Added

- **Per-key circuit isolation: `fetch(url, { isolationKey })`.** Every stream a
  client opened could share circuits, so requests made seconds apart left the
  same exit and whoever answered them could link them. Requests with different
  `isolationKey`s now never share a circuit (each key is its own arti isolation
  group); requests with the same key may. Use one key per identity a caller
  needs kept apart, such as a wallet account, or a fresh random key per request.
  Omitting it keeps the old behaviour. An empty or over-256-byte key is
  rejected with `INVALID_OPTIONS`.
- **anon-rpc worker: `x-anon-rpc-isolation` request header.** The anon-rpc
  spec's request init has no isolation field, and headers already pass through
  every harness unchanged, so a host sets this header and the worker turns it
  into `isolationKey`. The header is consumed, never forwarded to the
  destination.

## 0.4.3

Two fixes, no API changes. The WASM changes, so the anon-rpc worker's
keccak256 pin changes too.

### Fixes

- **The client could abort in its first 15 seconds.** arti's guard manager
  computed "now minus the 15-second guard connect timeout" and panicked if that
  went below zero. On wasm the clock starts at page load, worker creation or
  process start, so a client that got that far within 15 seconds (easy with a
  warm cache) could take the whole WASM instance down on its first circuit,
  with `Can't subtract connect timeout from now.` The arti fork is now pinned
  to `voltrevo/arti@e623ace`, which carries the fix. The same fix has been
  proposed upstream.
- **Fast bootstrap no longer needs `crypto.subtle`.** It hashed each
  microdescriptor with `crypto.subtle.digest`, which only exists in secure
  contexts and which some hosts withhold on purpose (the anon-rpc node harness
  removes it). There, fast bootstrap failed with `Cannot read properties of
  undefined (reading 'digest')` and fell back to a slow bootstrap from the Tor
  network; the same happened on an `http://` page. SHA-256 is now computed in
  Rust, with no measurable slowdown.

### Other

- The demo gateway address used by the website, tests and CI is now
  `152.236.3.22:12298:uEiCH0VdbhryzJcMtCfB0vLA4sOvHdQ4hq5w7CopcwU59bQ`.
- `npm run test:browser` and `npm run test:worker` no longer pass
  `--with-deps` to `playwright install`, so they fail on missing system
  libraries instead of asking for a sudo password partway through.

## 0.4.2

No API breaks. One behaviour change to be aware of: setting `logLevel` without a
`log` now prints to the console (see **Changed**). The WASM grows about 10% gzip
for `.onion` support.

### Added

- **`.onion` connections.** `fetch()` now reaches v3 onion services, via
  arti's `onion-service-client` feature. There is nothing to configure: a
  `.onion` URL just works. The cost is roughly 190 KB gzip on the WASM, paid
  whether or not a caller ever dials one: `tor-js/wasm-base64` goes from 2.3 to
  2.5 MB and `tor-js/wasm-file` from 1.7 to 1.9 MB. The default `tor-js` entry
  point's JS is unchanged; its growth is in the WASM it fetches. This is
  client-side connections only: onion-service client authorization and hosting
  an onion service are not enabled.
- **Demo-gateway warning.** A gateway can declare itself a demonstration
  instance (`"demo": true` in `/metadata.json`, [PROTOCOL.md §5](PROTOCOL.md)).
  tor-js checks in the background and warns once per gateway per process,
  pointing at how to run your own. The check never delays or fails a request.
  Because the warning is meant for a person, it is the one message that is not
  silenced by `TorClient`'s quiet default: it goes to your `log` if you set
  one, and to the console otherwise, unless `logLevel` excludes `warn`. This
  is the new `Log.warnOnce(key, ...)`. `ArtiSocketProvider` and `KpsGateway`
  gained an optional `log` option; `TorClient` passes its own, and a provider
  you construct yourself without one skips the check.
- **anon-rpc worker logs reach the host.** `dist/anon-rpc-worker.js` now sends
  its own, tor-js's and arti's logs to the host's log capability (anon-rpc
  SPEC §13); previously only the worker's own lifecycle lines did. The level is
  set with `logLevel` in the object form of the worker config
  (`{ gateways: [...], logLevel: "debug" }`) and defaults to `info`, because
  the host's log buffer is bounded and a debug firehose would evict the lines
  that explain a failure. An unrecognised level fails readiness. The worker's
  bytes changed, so **its keccak256 pin changes** with this release.

### Changed

- **`logLevel` without `log` now logs to the console.** Previously
  `new TorClient({ logLevel: 'debug' })` raised arti's log volume and then
  discarded every line, which read as logging being broken. The level applies
  to tor-js's own lines as well as arti's. With neither option set, the client
  is silent apart from one-time warnings (above).
- The arti fork is pinned to a newer commit (`voltrevo/arti@cc09d69`), which
  moves the WASM client to arti 0.45.
- Repository, homepage and issue links now point at
  [ethereum/tor-js](https://github.com/ethereum/tor-js).

### Gateway (`tor-js-gateway` 0.3.0)

The gateway is a separate crate, not part of the npm package; recorded here
because it ships from the same repo.

- **Worker bundles are mirrored from a GitHub branch.** `keccak_dir` is
  replaced by `keccak_repo` + `keccak_branch` (both required together; unset
  disables the capability), with `keccak_poll_interval` and
  `keccak_manual_sync_min_interval`. Publishing a bundle is a `git push`.
  A config still carrying `keccak_dir` gets a migration message. `tor-js-gateway
  sync` triggers a sync from the command line.
- **`demo` config flag**, advertised in `/metadata.json`. It is a required
  field, like the rest of the config: an existing config needs
  `"demo": false` (or `true`) added before it loads.

## 0.4.1

No API or wire changes from 0.4.0. One behaviour change to be aware of: a
truncated response body is now an error instead of a silently short read (see
**Fixes**).

### Added

- **Streaming request bodies.** `fetch()`'s `body` now accepts a `ReadableStream<Uint8Array>` alongside `string`, `Uint8Array` and `ArrayBuffer`. A stream is sent as it's produced, with `Transfer-Encoding: chunked` framing, instead of being buffered to learn its length — so uploads no longer need to fit in memory. Bodies of known size still use `Content-Length`. (Response bodies already streamed.)
- **Multiple gateways.** `gateway` accepts an array of KPS addresses to fail over and spread load between. The list is an **unordered set** — position implies no priority, and each client shuffles it, so independent clients don't all pile onto the same gateway. Within a preferred set of two, the gateway carrying the fewest connections is chosen, which also serves as the latency signal: a slow or stalled gateway stops being picked. Failures cool a gateway off with exponential backoff and it is re-admitted automatically once it recovers.
- **Fast bootstrap falls over.** Previously bootstrap was pinned to one gateway for the client's lifetime and silently degraded to slow bootstrap if that gateway was down. It now picks per attempt and tries the others. The happy path still contacts exactly one gateway.
- **Gateway attempts are bounded.** Dialing a gateway had no deadline, so one unreachable gateway could stall a relay connection indefinitely. Setup (dial, stream open, response head) now shares a 15s budget. Bootstrap body download is deliberately excluded, so a large snapshot over a slow link isn't mistaken for a dead gateway.
- **Injectable KPS transport.** `ArtiSocketProvider` accepts a `dial` function, and `TorClientOptions` accepts a `socketProvider`. Supplying a dialer means the built-in `@kpstreams` client is never loaded, so an embedder that already holds a KPS transport can bundle tor-js without it. The KPS address parser is vendored to keep that path free of runtime `@kpstreams` deps.
- Relay-connect failover keeps the target fixed across gateways, so a gateway cannot influence relay (guard) selection by refusing `CONNECT`s.
- `dist/anon-rpc-worker.js` is now built into the package: a hash-pinned [anon-rpc](https://github.com/ethereum/anon-rpc) worker that offers anonymized `fetch` from a sandboxed worker, reaching the network only through a host-granted KPS capability. Experimental and not yet a documented API; it is a hosted artifact rather than an import.

### Fixes

- **A truncated response body is now an error.** With `Content-Length` framing, a
  connection that closed with bytes still owed was reported as a clean end, so
  callers received a short body and no indication of it. Chunked framing had the
  same hole twice over: end-of-stream while awaiting the CRLF after a chunk, and
  end-of-stream before the terminating `0`-chunk, both read as a complete body.
  All three now error. Malformed chunk framing is also caught rather than
  silently dropping two bytes and corrupting the next chunk-size line.
  **This is the one behaviour change in the release**: a peer that closes early
  where it previously appeared to succeed now surfaces an error.
- **A `1xx` interim response could hang the request.** After skipping a `100
  Continue`, the reader waited for more bytes before checking what it already
  held — so an interim and final response arriving in the same packet left it
  blocked on data the server had already finished sending.
- **A malformed bootstrap archive could panic the client.** The Stored-zip parser
  computed member offsets in `usize`, which is 32 bits on wasm32: a crafted
  `compressed_size` wrapped past the bounds check and then panicked on a
  backwards slice — reachable by whoever serves the archive. Offsets are now
  accumulated in `u64`, so the check behaves identically on every target.
- **A failed bootstrap no longer crashes a Node host.** `TorClient.ready()`
  cleared its cached promise through a derived promise that nothing awaited, and
  the constructor started bootstrap with no handler attached at all. Either could
  raise an unhandled rejection — fatal in Node — on top of the error the caller
  already received.
- **Gateway: `is_local()` missed several non-routable IPv6 ranges** —
  `fe80::/10`, `fc00::/7`, multicast, and the v4-mapped and (deprecated)
  v4-compatible embeddings of broadcast/unspecified addresses. `CONNECT` targets
  must also appear in the consensus relay allowlist, so this was defence in depth
  rather than a reachable SSRF, but 11 addresses that previously passed the check
  are now refused.

### Build

- **The WASM artifact is reproducible across machines.** `dist/tor_js_bg.wasm`
  and `dist/anon-rpc-worker.js` now hash identically for any clean-tree build of
  a given commit, verified between `x86_64` and `aarch64` Linux hosts — which
  matters because the worker's keccak256 is a pinned, externally verifiable
  identity. This needed the toolchain pinned (`rust-toolchain.toml`, plus
  wasm-pack and binaryen asserted by `scripts/build.sh`), dependency and
  standard-library paths remapped out of panic locations, and a `RUSTC_WRAPPER`
  that makes cargo's `-C metadata` host-independent
  ([rust-lang/cargo#8140](https://github.com/rust-lang/cargo/issues/8140)).
  A dirty working tree deliberately does *not* reproduce: it embeds a timestamp,
  marking the build as unverifiable.
- The test suite grew from 19 tests to 408 across the Rust crates, the TypeScript
  layer, the gateway integration suite and the browser data path. The fixes above
  were all found by writing them.

## 0.4.0

**Breaking: tor-js now connects through gateways over [KPS](https://github.com/ethereum/kps) instead of WebSocket/WebRTC.** A 0.4.0 client requires a KPS gateway and cannot talk to a pre-0.4.0 gateway (or vice versa).

- **`gateway` is now a KPS address, not a URL.** Pass `ip:port:certhash` (printed by the gateway at startup), e.g. `198.51.100.7:12298:uEiAxk…9Qw`, instead of `https://…`. The client dials it over WebRTC in browsers and QUIC in Node.js (via the optional `@kpstreams/quic-client` peer dependency); relay connections are HTTP `CONNECT` tunnels on KPS streams.
- Built on KPS 0.2.1 (`@kpstreams/*` client) with the gateway on the `kps` crate 0.2.2. 0.2.1 specifies read-termination semantics (KPS SPEC §9.2: a read errors on RESET/local-close/connection-loss, EOF only on peer FIN) and stops surfacing a bare `null` on teardown ([kps#3](https://github.com/ethereum/kps/issues/3)). Paired with a tor-js fix: the WASM runtime now drives `reader.cancel()` to completion on stream drop instead of dropping its promise, which under §9.2 rejects (the connection is closed first) and would otherwise crash Node with an unhandled rejection. The gateway's 0.2.2 bump fixes a WebRTC data-channel bug where a browser's reused SCTP stream id was routed to a stale closed channel, hanging reads after ~2 streams per connection ([kps#4](https://github.com/ethereum/kps/issues/4)).
- **Fast bootstrap moved to zstd** (`/bootstrap.zip.zst`), decompressed inside the WASM.
- **`ArtiSocket` is now stream-shaped** (`readable`/`writable`) rather than event-based, preserving backpressure end to end. This affects custom `socketProvider` implementations.
- Updated Arti to 2.5.0 (tor-* 0.44).
- The repository is now a monorepo: the KPS gateway lives in `crates/tor-js-gateway/`, and the website in `website/`.

### Migrating from 0.3.x

Replace your gateway URL with the gateway's KPS address, and run a KPS gateway (see [`crates/tor-js-gateway/`](crates/tor-js-gateway/)):

```js
// 0.3.x
new TorClient({ gateway: 'https://tor-js-gateway.example.com' });
// 0.4.0
new TorClient({ gateway: '198.51.100.7:12298:uEiAxk…9Qw' });
```
