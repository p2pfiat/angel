// Entry for the Scriptable (iOS) script: exposes the sending code to a hidden WebView.
// The script itself (scriptable/template.js) handles Keychain, location and the trigger.

import { buildWraps, deliver, duressPayload, messagePayload, newSecretKey, npubOf, parsePubkey } from './core.js'

const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')
const bytes = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16))

globalThis.warn = {
  newKeyHex: () => hex(newSecretKey()),
  npub: (skHex) => npubOf(bytes(skHex)),
  /** Send a duress (ctx = {device, trigger, location?}) to every watcher. */
  duress: (skHex, watchers, relays, ctx, patienceMs) =>
    deliver(buildWraps(bytes(skHex), watchers.map(parsePubkey), duressPayload(ctx)), relays, patienceMs),
  /** A session_ok for a random session: reaches the watchers but never alarms. */
  silentTest: (skHex, watchers, relays, patienceMs) =>
    deliver(
      buildWraps(
        bytes(skHex),
        watchers.map(parsePubkey),
        messagePayload({ type: 'session_ok', session: 'test-' + hex(newSecretKey()).slice(0, 10) }),
      ),
      relays,
      patienceMs,
    ),
}
