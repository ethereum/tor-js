// Unit tests for gateway selection in ArtiSocketProvider: unordered (shuffled)
// preference, least-outstanding balancing, failure cooldown with re-admission,
// bounded attempts, and bootstrap failover.
//
// No network: a fake `dial` supplies fake KPS connections/streams, so this runs
// in CI. The source is esbuild-bundled first (it's TypeScript), matching how
// test/anon-rpc-worker builds the worker under test.
//
//   npm run test:unit

import { test, before, describe } from 'node:test'
import assert from 'node:assert/strict'
import { build } from 'esbuild'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))
const root = resolve(here, '../..')
const bundle = resolve(here, '.tmp-socketProvider.mjs')

let ArtiSocketProvider

before(async () => {
  await build({
    entryPoints: [resolve(root, 'src/socketProvider.ts')],
    bundle: true,
    format: 'esm',
    platform: 'node',
    outfile: bundle,
    external: ['@kpstreams/*', 'node:*'],
    logLevel: 'silent',
  })
  ;({ ArtiSocketProvider } = await import(bundle))
})

// --- fakes -----------------------------------------------------------------

const enc = new TextEncoder()
const OK_HEAD = 'HTTP/1.1 200 OK\r\n\r\n'

/**
 * A fake KPS stream that replays `head` (+ optional body) to the reader. With
 * `echo`, tunnel bytes written after the request line come back.
 */
function makeStream({ head = OK_HEAD, body = null, endAfterHead = false, echo = false } = {}) {
  let settle
  let ctl
  let writes = 0
  const closed = new Promise((r) => { settle = r })
  return {
    readable: new ReadableStream({
      start(c) {
        ctl = c
        c.enqueue(enc.encode(head))
        if (body) c.enqueue(body)
        if (endAfterHead) c.close()
      },
    }),
    writable: new WritableStream({ write(chunk) { if (echo && writes++ > 0) ctl.enqueue(chunk) } }),
    closed,
    closeWrite: async () => {},
    close: async () => settle({ ok: true }),
  }
}

/**
 * Build a dial fn over a mutable behaviour map keyed by address. Each entry
 * records call counts, and `mode` can be flipped mid-test:
 *   'ok' | 'reject' (dial fails) | 'hang' (dial never settles)
 */
function makeDial(behaviours) {
  return async (address) => {
    const b = behaviours[address]
    if (!b) throw new Error(`no behaviour for ${address}`)
    b.dials = (b.dials ?? 0) + 1
    if (b.mode === 'reject') throw new Error('dial refused')
    if (b.mode === 'hang') return new Promise(() => {})
    let settle
    const closed = new Promise((r) => { settle = r })
    return {
      openStream: async () => {
        b.opens = (b.opens ?? 0) + 1
        if (b.openMode === 'reject') throw new Error('openStream refused')
        if (b.openMode === 'hang') return new Promise(() => {})
        return makeStream(b.stream)
      },
      closed,
      close: () => settle(),
    }
  }
}

const ADDRS = [
  '10.0.0.1:12298:hashA',
  '10.0.0.2:12298:hashB',
  '10.0.0.3:12298:hashC',
  '10.0.0.4:12298:hashD',
]

/** Provider over `addrs`, with a behaviour map and kps-only strategy. */
function setup(addrs, { timing, mode = 'ok' } = {}) {
  const behaviours = Object.fromEntries(addrs.map((a) => [a, { mode }]))
  const provider = new ArtiSocketProvider({
    gateway: addrs.length === 1 ? addrs[0] : addrs,
    dial: makeDial(behaviours),
    strategies: ['kps'], // never try real TCP
    timing,
  })
  return { provider, behaviours }
}

/** Which gateway the shuffle made primary. Lets tests stay order-agnostic. */
const primaryOf = (provider) => provider.gateway.address
const otherThan = (addrs, addr) => addrs.filter((a) => a !== addr)

// --- tests -----------------------------------------------------------------

describe('single gateway', () => {
  test('connects through the only gateway', async () => {
    const { provider, behaviours } = setup([ADDRS[0]])
    const sock = await provider.connect('198.51.100.1:9001')
    assert.ok(sock.readable && sock.writable)
    assert.equal(behaviours[ADDRS[0]].dials, 1)
    provider.close()
  })

  test('surfaces the failure when it is down', async () => {
    const { provider } = setup([ADDRS[0]], { mode: 'reject' })
    await assert.rejects(() => provider.connect('198.51.100.1:9001'), /all strategies failed/)
    provider.close()
  })
})

describe('unordered list', () => {
  test('preference is not config order', async () => {
    // Shuffling means the primary varies across clients; over many
    // constructions we must see more than one distinct address chosen.
    const seen = new Set()
    for (let i = 0; i < 60; i++) {
      const { provider } = setup(ADDRS)
      seen.add(primaryOf(provider))
      provider.close()
    }
    assert.ok(seen.size > 1, `expected varied primaries, always got ${[...seen]}`)
  })

  test('non-overlapping work stays on one gateway', async () => {
    // Ties in the preferred set keep the construction-time order, so a client
    // whose tunnels do not overlap never fans out. (Tunnels held open *do*
    // spread across the preferred pair — see the balancing tests.)
    const { provider, behaviours } = setup(ADDRS)
    const primary = primaryOf(provider)
    for (let i = 0; i < 3; i++) {
      const sock = await provider.connect('198.51.100.1:9001')
      sock.close()
      await sock.closed
      await new Promise((r) => setTimeout(r, 5)) // let the release callback run
    }
    assert.equal(behaviours[primary].opens, 3)
    for (const a of otherThan(ADDRS, primary)) {
      assert.equal(behaviours[a].dials, undefined, `${a} should not have been dialed`)
    }
    provider.close()
  })
})

describe('failover', () => {
  test('falls over to a healthy gateway and keeps the same target', async () => {
    const { provider, behaviours } = setup(ADDRS)
    const primary = primaryOf(provider)
    behaviours[primary].mode = 'reject'

    const sock = await provider.connect('198.51.100.1:9001')
    assert.ok(sock.readable)
    // The failed gateway was attempted, and some other one carried it.
    assert.equal(behaviours[primary].dials, 1)
    const used = otherThan(ADDRS, primary).filter((a) => behaviours[a].opens > 0)
    assert.equal(used.length, 1, 'exactly one other gateway should have been used')
    provider.close()
  })

  test('a failed gateway is skipped while cooling down', async () => {
    const { provider, behaviours } = setup(ADDRS, {
      timing: { cooldownBaseMs: 10_000, cooldownMaxMs: 10_000 },
    })
    const primary = primaryOf(provider)
    behaviours[primary].mode = 'reject'

    await provider.connect('198.51.100.1:9001')
    assert.equal(behaviours[primary].dials, 1)
    // Even though it would now succeed, it stays cooled off and is not retried.
    behaviours[primary].mode = 'ok'
    await provider.connect('198.51.100.2:9001')
    assert.equal(behaviours[primary].dials, 1, 'cooling gateway must not be re-dialed')
    provider.close()
  })

  test('a recovered gateway is re-admitted after its cooldown', async () => {
    const { provider, behaviours } = setup(ADDRS, {
      timing: { cooldownBaseMs: 20, cooldownMaxMs: 20 },
    })
    const primary = primaryOf(provider)
    behaviours[primary].mode = 'reject'
    await provider.connect('198.51.100.1:9001')
    assert.equal(behaviours[primary].dials, 1)

    behaviours[primary].mode = 'ok'
    await new Promise((r) => setTimeout(r, 60)) // outlive the (jittered) cooldown
    await provider.connect('198.51.100.2:9001')
    assert.equal(behaviours[primary].dials, 2, 'recovered gateway should be tried again')
    provider.close()
  })

  test('rejects with every gateway attempted when all are down', async () => {
    const { provider, behaviours } = setup([ADDRS[0], ADDRS[1]], { mode: 'reject' })
    await assert.rejects(() => provider.connect('198.51.100.1:9001'), /all gateways failed/)
    assert.equal(behaviours[ADDRS[0]].dials, 1)
    assert.equal(behaviours[ADDRS[1]].dials, 1)
    provider.close()
  })
})

describe('least-outstanding balancing', () => {
  test('a second concurrent connect goes to a different gateway', async () => {
    const { provider, behaviours } = setup([ADDRS[0], ADDRS[1]])
    const primary = primaryOf(provider)
    const [secondary] = otherThan([ADDRS[0], ADDRS[1]], primary)

    // Hold the first tunnel open so the primary's in-flight count stays at 1.
    await provider.connect('198.51.100.1:9001')
    await provider.connect('198.51.100.2:9001')

    assert.equal(behaviours[primary].opens, 1)
    assert.equal(behaviours[secondary].opens, 1, 'load should move to the idle gateway')
    provider.close()
  })

  test('closing a tunnel returns capacity to the gateway', async () => {
    const { provider, behaviours } = setup([ADDRS[0], ADDRS[1]])
    const primary = primaryOf(provider)

    const sock = await provider.connect('198.51.100.1:9001')
    sock.close()
    await sock.closed
    await new Promise((r) => setTimeout(r, 10)) // let the release callback run

    await provider.connect('198.51.100.2:9001')
    assert.equal(behaviours[primary].opens, 2, 'freed gateway should be chosen again')
    provider.close()
  })

  test('the preferred set is capped, not fanned across every gateway', async () => {
    const { provider, behaviours } = setup(ADDRS)
    // Four concurrent tunnels over four gateways: only two should be touched.
    await Promise.all([0, 1, 2, 3].map((i) => provider.connect(`198.51.100.${i + 1}:9001`)))
    const touched = ADDRS.filter((a) => (behaviours[a].opens ?? 0) > 0)
    assert.equal(touched.length, 2, `expected 2 gateways in use, got ${touched.length}`)
    provider.close()
  })
})

describe('bounded attempts', () => {
  test('a hanging dial is abandoned and failover proceeds', async () => {
    const { provider, behaviours } = setup([ADDRS[0], ADDRS[1]], {
      timing: { attemptTimeoutMs: 50 },
    })
    const primary = primaryOf(provider)
    behaviours[primary].mode = 'hang'

    // A hanging dial leaves the attempt deadline as the only pending work, and
    // AbortSignal.timeout's timer is unref'd — so hold the event loop open or
    // it drains and node:test aborts the run.
    const keepAlive = setTimeout(() => {}, 30_000)
    try {
      const started = Date.now()
      const sock = await provider.connect('198.51.100.1:9001')
      const elapsed = Date.now() - started

      assert.ok(sock.readable)
      assert.ok(elapsed < 2_000, `failover took ${elapsed}ms; deadline was not enforced`)
    } finally {
      clearTimeout(keepAlive)
      provider.close()
    }
  })
})

describe('gatewayFetch (fast bootstrap)', () => {
  const body = () => ({ stream: { head: 'HTTP/1.1 200 OK\r\n\r\n', body: enc.encode('snapshot'), endAfterHead: true } })

  test('the happy path contacts exactly one gateway', async () => {
    const { provider, behaviours } = setup(ADDRS)
    for (const a of ADDRS) Object.assign(behaviours[a], body())

    const res = await provider.gatewayFetch('/bootstrap.zip.zst')
    assert.equal(res.status, 200)
    assert.equal(new TextDecoder().decode(res.body), 'snapshot')
    const touched = ADDRS.filter((a) => (behaviours[a].dials ?? 0) > 0)
    assert.deepEqual(touched, [primaryOf(provider)])
    provider.close()
  })

  test('falls over when the primary is down', async () => {
    const { provider, behaviours } = setup(ADDRS)
    for (const a of ADDRS) Object.assign(behaviours[a], body())
    const primary = primaryOf(provider)
    behaviours[primary].mode = 'reject'

    const res = await provider.gatewayFetch('/bootstrap.zip.zst')
    assert.equal(res.status, 200)
    assert.equal(behaviours[primary].dials, 1)
    provider.close()
  })

  test('rejects when no gateway is configured', async () => {
    const provider = new ArtiSocketProvider({ strategies: ['kps'] })
    assert.equal(provider.gateway, null)
    await assert.rejects(() => provider.gatewayFetch('/bootstrap.zip.zst'), /no gateway configured/)
  })
})

describe('liveness check', () => {
  const quick = { livenessCheckMs: 10, suspectAfterMs: 30, pingTimeoutMs: 40, pingMisses: 2, attemptTimeoutMs: 200 }
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
  const send = async (sock) => {
    const w = sock.writable.getWriter()
    await w.write(enc.encode('cell'))
    w.releaseLock()
  }

  test('tunnels through a gateway that stops answering are closed, and new ones go elsewhere', async () => {
    const { provider, behaviours } = setup([ADDRS[0], ADDRS[1]], { timing: quick })
    const keepAlive = setTimeout(() => {}, 30_000)
    try {
      const dead = primaryOf(provider) // asked before connecting: least-outstanding reorders after
      const sock = await provider.connect('198.51.100.1:9001')
      behaviours[dead].openMode = 'hang' // the gateway vanished: nothing answers, nothing closes
      const started = Date.now()
      await send(sock) // owed an answer that never comes
      await sock.closed
      const elapsed = Date.now() - started
      assert.ok(elapsed < 1_000, `tunnel stayed open ${elapsed}ms after the gateway died`)
      await provider.connect('198.51.100.1:9001')
      assert.ok(behaviours[otherThan([ADDRS[0], ADDRS[1]], dead)[0]].opens >= 1, 'the new tunnel went to the live gateway')
    } finally {
      clearTimeout(keepAlive)
      provider.close()
    }
  })

  test('a gateway whose tunnels carry traffic both ways is never pinged', async () => {
    const { provider, behaviours } = setup([ADDRS[0]], { timing: quick })
    behaviours[ADDRS[0]].stream = { echo: true }
    try {
      const sock = await provider.connect('198.51.100.1:9001')
      const reader = sock.readable.getReader()
      for (let i = 0; i < 15; i++) {
        await send(sock)
        await reader.read()
        await sleep(20)
      }
      assert.equal(behaviours[ADDRS[0]].opens, 1, 'only the tunnel itself')
    } finally {
      provider.close()
    }
  })

  test('a quiet gateway that answers its ping keeps its tunnels', async () => {
    const { provider, behaviours } = setup([ADDRS[0]], { timing: quick })
    const keepAlive = setTimeout(() => {}, 30_000)
    try {
      const sock = await provider.connect('198.51.100.1:9001')
      let closed = false
      sock.closed.then(() => { closed = true })
      for (let i = 0; i < 3; i++) {
        await send(sock) // the target is slow: nothing comes back
        await sleep(100)
      }
      assert.equal(closed, false)
      assert.ok(behaviours[ADDRS[0]].opens > 1, 'pinged once it went quiet')
      assert.ok(behaviours[ADDRS[0]].opens <= 4, 'one ping per silence, not one per tick')
      assert.equal(behaviours[ADDRS[0]].dials, 1, 'no redial')
    } finally {
      clearTimeout(keepAlive)
      provider.close()
    }
  })

  test('an idle gateway, or one without tunnels, is not pinged', async () => {
    const { provider, behaviours } = setup([ADDRS[0]], { timing: quick })
    const sock = await provider.connect('198.51.100.1:9001')
    await sleep(150)
    assert.equal(behaviours[ADDRS[0]].opens, 1, 'tunnel open but nothing owed')
    await send(sock)
    sock.close()
    await sleep(150)
    assert.equal(behaviours[ADDRS[0]].opens, 1, 'no tunnels left')
    provider.close()
  })
})
