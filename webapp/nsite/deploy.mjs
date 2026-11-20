// Publish the web app as an nsite (NIP-5A): files on Blossom servers, a site manifest on Nostr.
//
//   node nsite/deploy.mjs --key PATH [--new-key] [--dry-run]
//
// PATH holds the publishing key (hex). Use a separate key: its npub becomes part of the web
// address. With --new-key a key is created at PATH if missing. Keep PATH outside the repo
// and outside cloud-synced folders.

import { createHash } from 'node:crypto'
import { readFileSync, writeFileSync, existsSync, mkdirSync } from 'node:fs'
import { dirname } from 'node:path'
import { fileURLToPath } from 'node:url'
import { finalizeEvent, generateSecretKey, getPublicKey } from 'nostr-tools/pure'
import * as nip19 from 'nostr-tools/nip19'

const here = dirname(fileURLToPath(import.meta.url))
const root = `${here}/..`

/** Served path -> local file and content type. */
const FILES = {
  '/index.html': ['index.html', 'text/html'],
  '/favicon.svg': ['favicon.svg', 'image/svg+xml'],
  '/favicon-32.png': ['favicon-32.png', 'image/png'],
  '/og-preview.png': ['og-preview.png', 'image/png'],
  '/home.js': ['home.js', 'application/javascript'],
  '/app.html': ['app.html', 'text/html'],
  '/app.js': ['app.js', 'application/javascript'],
  '/sw.js': ['sw.js', 'application/javascript'],
  '/manifest.webmanifest': ['manifest.webmanifest', 'application/json'],
  '/icon-180.png': ['icon-180.png', 'image/png'],
  '/icon-192.png': ['icon-192.png', 'image/png'],
  '/icon-512.png': ['icon-512.png', 'image/png'],
}

/** Blossom servers to try; the ones that accept the upload are listed in the manifest. */
const BLOSSOM = [
  'https://blossom.primal.net',
  'https://cdn.nostrcheck.me',
  'https://nostr.download',
  'https://blossom.band',
  'https://cdn.satellite.earth',
]

/** Relays to publish the manifest and lists to (gateways look on popular relays). */
const RELAYS = [
  'wss://relay.nostr.net',
  'wss://nos.lol',
  'wss://relay.primal.net',
  'wss://nostr.oxtr.dev',
  'wss://relay.snort.social',
  'wss://offchain.pub',
  'wss://purplepag.es',
  'wss://relay.nostr.band',
  'wss://relay.damus.io',
]

const args = process.argv.slice(2)
const opt = (n) => (args.includes(n) ? args[args.indexOf(n) + 1] : undefined)
const keyPath = opt('--key')
if (!keyPath) throw new Error('--key PATH is required')
const dryRun = args.includes('--dry-run')

const hex = (b) => Buffer.from(b).toString('hex')
const sha256 = (buf) => createHash('sha256').update(buf).digest('hex')
const now = () => Math.floor(Date.now() / 1000)

function loadKey() {
  if (!existsSync(keyPath)) {
    if (!args.includes('--new-key')) throw new Error(`no key at ${keyPath} (use --new-key)`)
    mkdirSync(dirname(keyPath), { recursive: true })
    writeFileSync(keyPath, hex(generateSecretKey()))
    console.log('created publishing key')
  }
  return Uint8Array.from(Buffer.from(readFileSync(keyPath, 'utf8').trim(), 'hex'))
}

async function upload(server, sk, body, type, hash) {
  const auth = finalizeEvent(
    {
      kind: 24242,
      created_at: now() - 5,
      content: 'Upload Blob',
      tags: [
        ['t', 'upload'],
        ['x', hash],
        ['expiration', String(now() + 300)],
      ],
    },
    sk,
  )
  const token = Buffer.from(JSON.stringify(auth)).toString('base64url')
  const res = await fetch(`${server}/upload`, {
    method: 'PUT',
    headers: { Authorization: `Nostr ${token}`, 'Content-Type': type, 'X-SHA-256': hash },
    body,
    signal: AbortSignal.timeout(20000),
  })
  if (res.status !== 200 && res.status !== 201) {
    throw new Error(`${res.status} ${res.headers.get('x-reason') || ''}`.trim())
  }
  const desc = await res.json()
  if (desc.sha256 && desc.sha256 !== hash) throw new Error('server stored different bytes')
}

function publish(url, event) {
  return new Promise((resolve) => {
    let ws
    const timer = setTimeout(() => finish('timeout'), 10000)
    function finish(r) {
      clearTimeout(timer)
      try {
        ws.close()
      } catch {}
      resolve(r)
    }
    try {
      ws = new WebSocket(url)
    } catch (e) {
      return finish('error')
    }
    ws.onopen = () => ws.send(JSON.stringify(['EVENT', event]))
    ws.onerror = () => finish('error')
    ws.onmessage = (m) => {
      const msg = JSON.parse(m.data)
      if (msg[0] === 'OK' && msg[1] === event.id) finish(msg[2] ? 'ok' : `refused: ${msg[3]}`)
    }
  })
}

async function publishAll(label, event) {
  const results = await Promise.all(RELAYS.map((r) => publish(r, event).then((x) => [r, x])))
  const ok = results.filter(([, x]) => x === 'ok').map(([r]) => r)
  console.log(`${label}: accepted by ${ok.length}/${RELAYS.length} relays`)
  if (!ok.length) throw new Error(`${label} not accepted by any relay`)
}

const sk = loadKey()
const npub = nip19.npubEncode(getPublicKey(sk))

const files = Object.entries(FILES).map(([path, [file, type]]) => {
  const body = readFileSync(`${root}/${file}`)
  return { path, body, type, hash: sha256(body) }
})

// Upload every file to every server; keep the servers that took all of them.
const servers = []
for (const server of BLOSSOM) {
  try {
    if (!dryRun) for (const f of files) await upload(server, sk, f.body, f.type, f.hash)
    servers.push(server)
    console.log(`blossom ${server}: ok`)
  } catch (e) {
    console.log(`blossom ${server}: ${e.message}`)
  }
}
if (!servers.length) throw new Error('no Blossom server accepted the files')

const pathTags = files.map((f) => ['path', f.path, f.hash])
const aggregate = sha256(
  files
    .map((f) => `${f.hash} ${f.path}\n`)
    .sort()
    .join(''),
)
const manifest = finalizeEvent(
  {
    kind: 15128,
    created_at: now(),
    content: '',
    tags: [...pathTags, ['x', aggregate, 'aggregate'], ...servers.map((s) => ['server', s]), ['title', 'Sync']],
  },
  sk,
)
const serverList = finalizeEvent(
  { kind: 10063, created_at: now(), content: '', tags: servers.map((s) => ['server', s]) },
  sk,
)
const relayList = finalizeEvent({ kind: 10002, created_at: now(), content: '', tags: RELAYS.map((r) => ['r', r]) }, sk)

if (dryRun) {
  console.log(JSON.stringify(manifest, null, 2))
} else {
  await publishAll('relay list (10002)', relayList)
  await publishAll('blossom servers (10063)', serverList)
  await publishAll('site manifest (15128)', manifest)
}
console.log(`\nsite: https://${npub}.nsite.lol/`)
console.log(`      https://${npub}.nsite.cloud/`)
process.exitCode = 0
