// Printed at the end of `npm test`.
//
// `npm test` is deliberately the hermetic subset: no network, no browser
// download, no WASM toolchain — nothing beyond the Rust and Node you already
// need to build this repo. That is a good default, but it means a green run
// does NOT mean "everything passed", and a green run that quietly skipped half
// the suites is worse than a slow one. So the command says what it left out.

const skipped = [
  [
    "test:integration",
    "gateway over real KPS/QUIC (37 tests)",
    "builds the gateway and binds UDP ports",
  ],
  [
    "test:browser",
    "gateway over real WebRTC, in Chromium (6 tests)",
    "downloads a browser; fails if Chromium's system libraries are missing",
  ],
  [
    "test:worker",
    "anon-rpc worker harness, offline",
    "builds the WASM artifact (wasm-pack + pinned binaryen) and downloads a browser",
  ],
  [
    "test:live",
    "real fetches over the real Tor network",
    "needs the network; non-deterministic, which is why CI runs it nightly and non-gating",
  ],
];

const width = Math.max(...skipped.map(([name]) => name.length));

console.log("\n✓ hermetic suites passed (unit + both Rust crates).");
console.log("\n  Not run by `npm test` — each needs more than a checkout:\n");
for (const [name, what, why] of skipped) {
  console.log(`    npm run ${name.padEnd(width)}  ${what}`);
  console.log(`    ${" ".repeat(width + 8)}  ↳ ${why}`);
}
console.log("\n  `npm run test:all` runs everything.\n");
