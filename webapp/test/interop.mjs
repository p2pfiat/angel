// Interop helpers between the web app and the Rust watcher.
//
//   node test/interop.mjs fixture            writes proto/tests/fixtures/js_duress.json
//   node test/interop.mjs keygen             prints a throwaway "<hex secret> <npub>"
//   node test/interop.mjs send SKHEX NPUB    sends a test duress to that watcher via relays
//   node test/interop.mjs watching SKHEX NPUB whether that watcher confirmed it watches SKHEX
//   node test/interop.mjs ping SKHEX NPUB     asks that watcher now and waits for the answer

import { writeFileSync, mkdirSync } from 'node:fs'
import { getPublicKey } from 'nostr-tools/pure'
import { DEFAULT_RELAYS, buildDuress, deliver, fetchWatching, inviteEvent, newSecretKey, pingWatchers, npubOf, parsePubkey, watchState } from '../src/core.js'

const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')
const bytes = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16))

const [cmd, a, b] = process.argv.slice(2)

if (cmd === 'fixture') {
  // Test-only keys, generated fresh: they protect nothing.
  const device = newSecretKey()
  const watcher = newSecretKey()
  const ctx = { device: 'phone', trigger: 'duress_app', location: 'https://www.openstreetmap.org/?mlat=1&mlon=2' }
  const [wrap] = buildDuress(device, [getPublicKey(watcher)], ctx)
  const out = new URL('../../proto/tests/fixtures/', import.meta.url)
  mkdirSync(out, { recursive: true })
  writeFileSync(
    new URL('js_duress.json', out),
    JSON.stringify({ watcher_secret_hex: hex(watcher), device_npub: npubOf(device), context: ctx, wrap }, null, 2) + '\n',
  )
  // An invite as the app publishes it (read by watcher/src/vips.rs).
  const secret = 'AAAAAAAAAAAAAAAAAAAAAA'
  const event = inviteEvent(secret, { by: 'Zoë', watchers: [getPublicKey(watcher)], relays: ['wss://nos.lol'] }, 4000000000)
  writeFileSync(new URL('js_invite.json', out), JSON.stringify({ secret, event }, null, 2) + '\n')
  console.log('fixture written')
} else if (cmd === 'keygen') {
  const sk = newSecretKey()
  console.log(hex(sk), npubOf(sk))
} else if (cmd === 'send') {
  const wraps = buildDuress(bytes(a), [parsePubkey(b)], { device: 'phone', trigger: 'interop_test' })
  const r = await deliver(wraps, DEFAULT_RELAYS, 30000)
  // Node exits by itself once the other relays are done; exiting now trips a libuv assertion
  // on Windows while sockets are closing.
  console.log(`sent via ${r.relay} in ${r.ms} ms`)
} else if (cmd === 'watching') {
  const w = parsePubkey(b)
  const r = await fetchWatching(bytes(a), [w], DEFAULT_RELAYS)
  console.log(watchState(r[w]), JSON.stringify(r[w] || null))
} else if (cmd === 'ping') {
  const w = parsePubkey(b)
  const r = await pingWatchers(bytes(a), [w], DEFAULT_RELAYS, () => {}, 20000)
  console.log(w in r ? `answered in ${r[w]} ms` : 'no answer')
} else {
  console.error('usage: interop.mjs fixture | keygen | send SKHEX NPUB | watching SKHEX NPUB | ping SKHEX NPUB')
  process.exit(2)
}
