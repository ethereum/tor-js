// Unit tests for scripts/reproducible-rustc.sh, the RUSTC_WRAPPER that derives
// `-C metadata` from a unit's identity. The derived key must not depend on
// where the repo is checked out, including roots that also occur inside
// dependency paths (e.g. /src, which is part of every registry path).
//
// No toolchain needed: a fake rustc answers the sysroot probe and echoes the
// rewritten metadata argument.
//
//   npm run test:unit

import { test, before, after, describe } from 'node:test'
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { root } from './bundle.mjs'

const wrapper = path.join(root, 'scripts', 'reproducible-rustc.sh')
const registrySource =
  '/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/getrandom-0.2.16/src/lib.rs'

let tmpDir, fakeRustc

before(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'tor-js-rustc-wrapper-'))
  fakeRustc = path.join(tmpDir, 'fake-rustc')
  fs.writeFileSync(
    fakeRustc,
    '#!/bin/sh\n' +
      'if [ "$1" = "--print" ]; then echo /usr/local/rustup/toolchains/1.97.1; exit 0; fi\n' +
      'for a in "$@"; do case "$a" in metadata=*) echo "$a";; esac; done\n',
  )
  fs.chmodSync(fakeRustc, 0o755)
})

after(() => {
  fs.rmSync(tmpDir, { recursive: true, force: true })
})

function metadataFor(workspaceRoot, source) {
  const out = execFileSync(
    'bash',
    [
      wrapper,
      fakeRustc,
      '--crate-name', 'getrandom',
      '--edition=2018',
      '--crate-type', 'lib',
      '--target', 'wasm32-unknown-unknown',
      source,
      '-C', 'metadata=0123456789abcdef',
    ],
    {
      env: {
        ...process.env,
        CARGO_HOME: '/usr/local/cargo',
        TOR_JS_WORKSPACE_ROOT: workspaceRoot,
        TOR_JS_RUSTC_KEYLOG: '',
      },
      encoding: 'utf8',
    },
  )
  return out.trim()
}

const roots = ['/src', '/work/second-build/tor-js', '/usr/local', '/registry', '/tmp/build-1/src']

describe('reproducible-rustc.sh', () => {
  test('dependency keys do not depend on the checkout path', () => {
    const keys = roots.map((r) => metadataFor(r, registrySource))
    assert.equal(new Set(keys).size, 1)
    assert.match(keys[0], /^metadata=[0-9a-f]{32}$/)
  })

  test('workspace keys do not depend on the checkout path', () => {
    // cargo passes workspace members' sources relative to the workspace root.
    const keys = roots.map((r) => metadataFor(r, 'crates/tor-js-wasm/src/lib.rs'))
    assert.equal(new Set(keys).size, 1)
  })

  test('distinct crate versions keep distinct keys', () => {
    const other = registrySource.replace('getrandom-0.2.16', 'getrandom-0.2.15')
    assert.notEqual(metadataFor('/src', registrySource), metadataFor('/src', other))
  })
})
