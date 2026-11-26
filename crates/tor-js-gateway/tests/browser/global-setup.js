// Spawns the gateway and a static page server, mirroring the kps repo's
// interop pattern (tests/interop there). The page imports the *published*
// @kpstreams packages from this package's node_modules via an import map.
// This test is self-contained (page/index.html) and independent of the
// top-level website, which has its own build and hosting.
import { spawn } from 'node:child_process'
import dgram from 'node:dgram'
import { createHash, randomBytes } from 'node:crypto'
import { createServer } from 'node:http'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import net from 'node:net'
import { tmpdir } from 'node:os'
import { dirname, extname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { zstdCompressSync } from 'node:zlib'

const here = dirname(fileURLToPath(import.meta.url))
// The built binary lands in the Cargo workspace-wide target/ (repo root, four
// levels up), not under the crate.
const workspaceRoot = resolve(here, '../../../..')
const gatewayBin =
  process.env.GATEWAY_BIN ?? join(workspaceRoot, 'target/debug/tor-js-gateway')
const stateFilePath = join(here, '.run-state.json')

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.map': 'application/json',
}

// Route the bare package specifiers to the installed dist trees (see
// page/index.html's import map); everything else serves from page/.
const STATIC_ROUTES = [
  ['/kps/core/', join(here, 'node_modules/@kpstreams/core/dist')],
  ['/kps/webrtc-client/', join(here, 'node_modules/@kpstreams/webrtc-client/dist')],
]

function startStaticServer() {
  const server = createServer(async (req, res) => {
    try {
      let p = decodeURIComponent(new URL(req.url, 'http://x').pathname)
      if (p.includes('..')) { res.writeHead(400); return res.end('bad') }
      let filePath
      const route = STATIC_ROUTES.find(([prefix]) => p.startsWith(prefix))
      if (route) {
        filePath = join(route[1], p.slice(route[0].length))
      } else {
        if (p === '/' || p === '') p = '/index.html'
        filePath = join(here, 'page', p)
      }
      const data = await readFile(filePath)
      res.writeHead(200, {
        'content-type': MIME[extname(filePath).toLowerCase()] ?? 'application/octet-stream',
      })
      res.end(data)
    } catch {
      res.writeHead(404)
      res.end('not found')
    }
  })
  return new Promise((res, rej) => {
    server.once('error', rej)
    server.listen(0, '127.0.0.1', () => res(server))
  })
}

/// A local TCP echo server for the CONNECT tunnel tests.
function startEcho() {
  const server = net.createServer(s => {
    s.on('error', () => {}) // aborted tunnels RST us by design (§4)
    s.pipe(s)
  })
  return new Promise(res =>
    server.listen(0, '127.0.0.1', () =>
      res({
        port: server.address().port,
        close: () => new Promise(r => server.close(r)),
      })
    )
  )
}

/// A UDP relay in front of the gateway that drops about one packet in `oneIn`
/// each way, so the browser suite can run under loss (GATEWAY_LOSS=<oneIn>).
/// Loopback never loses packets, which hides everything about stream resets
/// that only matters when a FIN, a reset or a retransmission is lost. Each
/// client gets its own upstream socket, so the gateway sees distinct peers.
/// The drop sequence is a fixed xorshift, so runs are similar, not identical.
async function startLossyRelay(gatewayPort, oneIn) {
  let rng = 0x9e3779b97f4a7c15n
  const drop = () => {
    rng ^= (rng << 13n) & 0xffffffffffffffffn
    rng ^= rng >> 7n
    rng ^= (rng << 17n) & 0xffffffffffffffffn
    return rng % BigInt(oneIn) === 0n
  }
  const front = dgram.createSocket('udp4')
  const upstream = new Map() // "ip:port" of a client -> its socket to the gateway
  let dropped = 0
  let forwarded = 0
  front.on('message', (msg, rinfo) => {
    const key = `${rinfo.address}:${rinfo.port}`
    let back = upstream.get(key)
    if (!back) {
      back = dgram.createSocket('udp4')
      back.on('message', reply => {
        if (drop()) { dropped++; return }
        forwarded++
        front.send(reply, rinfo.port, rinfo.address)
      })
      back.bind(0, '127.0.0.1')
      upstream.set(key, back)
    }
    if (drop()) { dropped++; return }
    forwarded++
    back.send(msg, gatewayPort, '127.0.0.1')
  })
  await new Promise(res => front.bind(0, '127.0.0.1', res))
  return {
    port: front.address().port,
    stats: () => ({ dropped, forwarded }),
    close: () => {
      for (const b of upstream.values()) b.close()
      front.close()
    },
  }
}

/// Data-dir fixtures so the browser can exercise the real data path: a
/// bootstrap archive to download, and a cached consensus whose relay allowlist
/// contains the echo server (CONNECT only permits advertised relays).
async function writeFixtures(dataDir, echoPort) {
  await mkdir(dataDir, { recursive: true })

  // Deliberately larger than a single WebRTC data-channel message, so a
  // chunking or reassembly bug shows up as a hash mismatch.
  const bootstrapZip = randomBytes(256 * 1024)
  await writeFile(join(dataDir, 'bootstrap.zip'), bootstrapZip)
  const compressed = zstdCompressSync(bootstrapZip)
  await writeFile(join(dataDir, 'bootstrap.zip.zst'), compressed)

  await writeFile(
    join(dataDir, 'consensus-microdesc.txt'),
    `r test AAAAAAAAAAAAAAAAAAAAAAAAAAA 2026-01-01 00:00:00 127.0.0.1 ${echoPort} 0\n`
  )

  return {
    bootstrapLen: compressed.length,
    bootstrapSha256: createHash('sha256').update(compressed).digest('hex'),
    uncompressedLen: bootstrapZip.length,
  }
}

async function startGateway(stateDir, echoPort) {
  const dataDir = join(stateDir, 'data')
  const fixtures = await writeFixtures(dataDir, echoPort)
  const cfg = {
    data_dir: dataDir,
    kps_port: 0,
    kps_key_file: join(stateDir, 'kps.key'),
    // No mirror: the browser suite covers the data plane, not worker bundles.
    keccak_repo: '',
    keccak_branch: '',
    keccak_poll_interval: 86400,
    keccak_manual_sync_min_interval: 1800,
    demo: false,
    advertised_addresses: ['127.0.0.1'],
    tunnel_max: 8192,
    tunnel_per_ip: 16,
    tunnel_idle_timeout: 300,
    tunnel_max_lifetime: 3600,
  }
  const cfgPath = join(stateDir, 'config.json5')
  await writeFile(cfgPath, JSON.stringify(cfg))
  const child = spawn(gatewayBin, ['--config', cfgPath, 'run', '--no-sync'], {
    // The echo server is on loopback, which is_local refuses by default.
    env: { ...process.env, TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS: '1' },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  // kps prints its drain-backstop warning straight to stderr (not through
  // tracing). Keep it so teardown can fail the run on it.
  const stderrLog = { text: '' }
  child.stderr.on('data', c => {
    stderrLog.text += c.toString()
    process.stderr.write(`[gateway] ${c}`)
  })
  return new Promise((res, rej) => {
    let buf = ''
    const timer = setTimeout(() => {
      child.kill()
      rej(new Error(`gateway: timed out waiting for address\n${buf}`))
    }, 20_000)
    const onData = chunk => {
      buf += chunk.toString()
      process.stdout.write(`[gateway] ${chunk}`)
      const m = buf.match(/127\.0\.0\.1:\d+:u[A-Za-z0-9_-]+/)
      if (m) {
        clearTimeout(timer)
        res({ address: m[0], child, fixtures, stderrLog })
      }
    }
    child.stdout.on('data', onData)
    child.on('exit', code => {
      clearTimeout(timer)
      rej(new Error(`gateway exited (${code}) before printing address\n${buf}`))
    })
  })
}

export default async function globalSetup() {
  const stateDir = await mkdtemp(join(tmpdir(), 'tjg-browser-'))
  const echo = await startEcho()
  const gateway = await startGateway(stateDir, echo.port)
  const httpServer = await startStaticServer()
  const baseUrl = `http://127.0.0.1:${httpServer.address().port}`

  // Under GATEWAY_LOSS, the browser dials the relay: same certhash, the
  // relay's port.
  const loss = parseInt(process.env.GATEWAY_LOSS ?? '0', 10)
  let relay = null
  let gatewayAddress = gateway.address
  if (loss > 0) {
    const [ip, port, certhash] = gateway.address.split(':')
    relay = await startLossyRelay(parseInt(port, 10), loss)
    gatewayAddress = `${ip}:${relay.port}:${certhash}`
    console.log(`[setup] dialing through a relay dropping ~1 in ${loss} packets each way`)
  }
  await writeFile(
    stateFilePath,
    JSON.stringify(
      {
        gatewayAddress,
        advertisedAddress: gateway.address,
        loss,
        baseUrl,
        echoTarget: `127.0.0.1:${echo.port}`,
        ...gateway.fixtures,
      },
      null,
      2
    )
  )
  console.log(`[setup] gateway: ${gatewayAddress}`)
  console.log(`[setup] page:    ${baseUrl}`)
  console.log(`[setup] echo:    127.0.0.1:${echo.port}`)

  return async () => {
    if (relay) {
      const { dropped, forwarded } = relay.stats()
      console.log(`[teardown] relay dropped ${dropped} of ${dropped + forwarded} packets`)
      relay.close()
    }
    await echo.close()
    await new Promise(res => httpServer.close(() => res()))
    if (gateway.child.exitCode === null) {
      gateway.child.kill('SIGTERM')
      await new Promise(res => {
        const t = setTimeout(() => {
          try { gateway.child.kill('SIGKILL') } catch {}
          res()
        }, 3000)
        gateway.child.on('exit', () => { clearTimeout(t); res() })
      })
    }
    try { await rm(stateDir, { recursive: true, force: true }) } catch {}
    try { await rm(stateFilePath, { force: true }) } catch {}

    // The kps driver closes a channel whose sent data never drained after
    // 30 s, and warns. The tests can't see it: the slot comes back and every
    // echo still completes. Any such line is an accounting bug, so fail.
    // The backstop only runs while the channel's connection is open, so a
    // channel stuck within 30 s of its connection closing goes unreported;
    // under loss the churn test holds its connection open 31 s to cover its
    // own tail.
    const backstop = gateway.stderrLog.text
      .split('\n')
      .filter(l => l.includes('[kps-rtc] warning'))
    if (backstop.length > 0) {
      throw new Error(`gateway hit the kps drain backstop ${backstop.length} time(s):\n${backstop.join('\n')}`)
    }
  }
}
