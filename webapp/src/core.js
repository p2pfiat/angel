// Duress message building and delivery, shared by the web app and its tests.
// Same wire format as the Rust agent (proto/src/lib.rs): a kind-4747 rumor with a JSON
// payload, sealed with the device key and gift-wrapped (NIP-59) to every watcher.

import { generateSecretKey, getPublicKey, finalizeEvent, verifyEvent } from 'nostr-tools/pure'
import * as nip19 from 'nostr-tools/nip19'
import * as nip44 from 'nostr-tools/nip44'
import { createRumor, createSeal } from 'nostr-tools/nip59'
import { sha256 } from '@noble/hashes/sha2.js'
import { bytesToHex, utf8ToBytes } from '@noble/hashes/utils.js'

export const RUMOR_KIND = 4747
export const VERSION = 1
const WRAP_EXPIRATION = 3 * 24 * 3600
const RELAY_OK_TIMEOUT_MS = 15000

export const DEFAULT_RELAYS = [
  'wss://relay.nostr.net',
  'wss://nostr.oxtr.dev',
  'wss://relay.snort.social',
  'wss://offchain.pub',
  'wss://nos.lol',
  'wss://relay.primal.net',
]

export function newSecretKey() {
  return generateSecretKey()
}

export function npubOf(sk) {
  return nip19.npubEncode(getPublicKey(sk))
}

/** Accepts npub or 64-char hex; returns hex or throws. */
export function parsePubkey(s) {
  s = s.trim()
  if (/^[0-9a-f]{64}$/i.test(s)) return s.toLowerCase()
  const d = nip19.decode(s)
  if (d.type !== 'npub') throw new Error('not an npub: ' + s)
  return d.data
}

/** The JSON payload, byte-compatible with the Rust Envelope for `Message::Duress`. */
export function duressPayload({ device, trigger, location, note }) {
  const context = { device, trigger }
  if (location) context.location = location
  if (note) context.note = note
  return JSON.stringify({ v: VERSION, type: 'duress', context })
}

/** NIP-59 wrap with an expiration tag, like the Rust side. */
function wrapSeal(seal, recipient) {
  const ephemeral = generateSecretKey()
  const now = Math.floor(Date.now() / 1000)
  return finalizeEvent(
    {
      kind: 1059,
      content: nip44.encrypt(JSON.stringify(seal), nip44.getConversationKey(ephemeral, recipient)),
      created_at: now - Math.floor(Math.random() * 2 * 24 * 3600),
      tags: [
        ['p', recipient],
        ['expiration', String(now + WRAP_EXPIRATION)],
      ],
    },
    ephemeral,
  )
}

/**
 * One gift wrap per recipient, all carrying the same rumor (same id). `payload` is the JSON
 * text of an Envelope, e.g. from `duressPayload` or `messagePayload`.
 */
export function buildWraps(sk, recipients, payload) {
  if (!recipients.length) throw new Error('no watchers configured')
  const rumor = createRumor(
    { kind: RUMOR_KIND, content: payload, created_at: Math.floor(Date.now() / 1000), tags: [] },
    sk,
  )
  return recipients.map((r) => wrapSeal(createSeal(rumor, sk, r), r))
}

export function buildDuress(sk, recipients, ctx) {
  return buildWraps(sk, recipients, duressPayload(ctx))
}

/** Any Envelope message, e.g. { type: 'session_ok', session: '…' } (used for a silent test). */
export function messagePayload(message) {
  return JSON.stringify({ v: VERSION, ...message })
}

/**
 * Open a gift wrap addressed to `sk`. Returns { sender, created_at, payload } only when the
 * seal is validly signed and the rumor is by the same key (as the Rust side checks), else null.
 */
export function unwrap(sk, wrap) {
  try {
    if (wrap.kind !== 1059) return null
    const seal = JSON.parse(nip44.decrypt(wrap.content, nip44.getConversationKey(sk, wrap.pubkey)))
    if (seal.kind !== 13 || !verifyEvent(seal)) return null
    const rumor = JSON.parse(nip44.decrypt(seal.content, nip44.getConversationKey(sk, seal.pubkey)))
    if (rumor.pubkey !== seal.pubkey || rumor.kind !== RUMOR_KIND) return null
    const payload = JSON.parse(rumor.content)
    if (payload.v !== VERSION) return null
    return { sender: seal.pubkey, created_at: rumor.created_at, payload }
  } catch {
    return null
  }
}

/** Latest gift wraps for this key from one relay; resolves at end of stored events or timeout. */
function fetchFromRelay(url, pubkey, since, timeoutMs) {
  return new Promise((resolve) => {
    const events = []
    let ws
    const done = () => {
      clearTimeout(timer)
      try {
        ws.close()
      } catch {}
      resolve(events)
    }
    const timer = setTimeout(done, timeoutMs)
    try {
      ws = new WebSocket(url)
    } catch {
      return done()
    }
    ws.onopen = () => ws.send(JSON.stringify(['REQ', 'w', { kinds: [1059], '#p': [pubkey], since }]))
    ws.onerror = done
    ws.onmessage = (m) => {
      let msg
      try {
        msg = JSON.parse(m.data)
      } catch {
        return
      }
      if (msg[0] === 'EVENT' && msg[1] === 'w') events.push(msg[2])
      if (msg[0] === 'EOSE') done()
    }
  })
}

/**
 * Which watchers confirmed recently that they watch this device (`watching` messages).
 * Returns { watcherHex: { at, interval, payload } } with the newest confirmation per watcher;
 * only keys in `watchers` count. The payload may carry the join link for new angels (`invite`)
 * and the angel list (`angels`).
 */
export async function fetchWatching(sk, watchers, relays, timeoutMs = 8000, onOther = () => {}) {
  // Wraps are backdated up to 2 days and expire after 3.
  const since = Math.floor(Date.now() / 1000) - WRAP_EXPIRATION - 3600
  const lists = await Promise.all(relays.map((r) => fetchFromRelay(r, getPublicKey(sk), since, timeoutMs)))
  const seen = new Set()
  const out = {}
  for (const wrap of lists.flat()) {
    if (seen.has(wrap.id)) continue
    seen.add(wrap.id)
    const m = unwrap(sk, wrap)
    if (!m || !watchers.includes(m.sender)) continue
    if (m.payload.type !== 'watching') {
      onOther(m)
      continue
    }
    const prev = out[m.sender]
    if (!prev || prev.at < m.created_at) {
      out[m.sender] = { at: m.created_at, interval: m.payload.interval_secs, payload: m.payload }
    }
  }
  return out
}

/** The hash watchers keep of an invite's secret. */
export function secretHash(secret) {
  return bytesToHex(sha256(utf8ToBytes(secret)))
}

/** A new secret for an invite link: 128 bits, base64url (22 characters). */
export function newInviteSecret() {
  const b = crypto.getRandomValues(new Uint8Array(16))
  return btoa(String.fromCharCode(...b)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

const INVITE_KIND = 30078

/** The key an invite's event is signed with, derived from the link's secret. */
function inviteKey(secret) {
  return sha256(utf8ToBytes(`warn-invite:${secret}`))
}

/** The event that holds an invite's details, encrypted to the link's secret (expires: NIP-40). */
export function inviteEvent(secret, { by, watchers, relays, t }, expires) {
  const sk = inviteKey(secret)
  const ck = nip44.v2.utils.getConversationKey(sk, getPublicKey(sk))
  const content = nip44.v2.encrypt(JSON.stringify({ by, w: watchers, r: relays, t: t || 'vip' }), ck)
  return finalizeEvent(
    { kind: INVITE_KIND, created_at: Math.floor(Date.now() / 1000), tags: [['d', 'warn-invite'], ['expiration', String(expires)]], content },
    sk,
  )
}

/** Read an invite event with the link's secret: { by, watchers, relays } or null. */
export function readInviteEvent(secret, event) {
  try {
    const sk = inviteKey(secret)
    const pk = getPublicKey(sk)
    if (event.pubkey !== pk || event.kind !== INVITE_KIND || !verifyEvent(event)) return null
    const exp = (event.tags.find((t) => t[0] === 'expiration') || [])[1]
    if (exp && Number(exp) < Date.now() / 1000) return null
    const d = JSON.parse(nip44.v2.decrypt(event.content, nip44.v2.utils.getConversationKey(sk, pk)))
    const watchers = (Array.isArray(d.w) ? d.w : []).filter((w) => /^[0-9a-f]{64}$/.test(w))
    const relays = (Array.isArray(d.r) ? d.r : []).filter((r) => typeof r === 'string' && /^wss:\/\/\S+$/.test(r))
    if (!watchers.length || !relays.length) return null
    return { by: typeof d.by === 'string' ? d.by.slice(0, 40) : '', watchers, relays, t: ['angel', 'device', 'offer'].includes(d.t) ? d.t : 'vip' }
  } catch {
    return null
  }
}

/** Find an invite's details on the relays. Resolves with the first readable one, or null. */
export function fetchInvite(secret, relays, timeoutMs = 10000) {
  const pk = getPublicKey(inviteKey(secret))
  return new Promise((resolve) => {
    let left = relays.length
    let done = false
    const socks = []
    const finish = (v) => {
      if (done) return
      done = true
      clearTimeout(timer)
      socks.forEach((ws) => {
        try {
          ws.close()
        } catch {}
      })
      resolve(v)
    }
    const timer = setTimeout(() => finish(null), timeoutMs)
    const gone = () => --left <= 0 && finish(null)
    for (const url of relays) {
      let ws
      try {
        ws = new WebSocket(url)
      } catch {
        gone()
        continue
      }
      socks.push(ws)
      ws.onopen = () => ws.send(JSON.stringify(['REQ', 'i', { kinds: [INVITE_KIND], authors: [pk], limit: 1 }]))
      ws.onmessage = (m) => {
        try {
          const msg = JSON.parse(m.data)
          if (msg[0] === 'EVENT') {
            const info = readInviteEvent(secret, msg[2])
            if (info) finish({ secret, ...info })
          } else if (msg[0] === 'EOSE') {
            gone()
          }
        } catch {}
      }
      ws.onerror = gone
    }
  })
}

/**
 * An invite to become a VIP, from a link (whole URL or just the part after `#`). The short
 * form gives { secret, short: true } (details still to fetch); the long form
 * { secret, by, watchers: [hex], relays }. Null for anything else.
 */
export function parseJoin(text) {
  if (typeof text !== 'string') return null
  const frag = text.includes('#') ? text.slice(text.indexOf('#') + 1) : text
  const p = new URLSearchParams(frag.trim())
  const j = p.get('j')
  if (j && /^[A-Za-z0-9_-]{22}$/.test(j)) return { secret: j, short: true }
  const a = p.get('a')
  if (a && /^[A-Za-z0-9_-]{22}$/.test(a)) return { secret: a, short: true, angel: true }
  const d = p.get('d')
  if (d && /^[A-Za-z0-9_-]{22}$/.test(d)) return { secret: d, short: true, device: true }
  const w = p.get('w')
  if (w && /^[A-Za-z0-9_-]{22}$/.test(w)) return { secret: w, short: true, offer: true }
  if (p.get('join') !== '1') return null
  const secret = p.get('k') || ''
  if (!/^[0-9a-f]{32}$/.test(secret)) return null
  const watchers = []
  for (const w of (p.get('w') || '').split(',')) {
    try {
      const k = parsePubkey(w)
      if (!watchers.includes(k)) watchers.push(k)
    } catch {}
  }
  const relays = (p.get('r') || '').split(',').filter((r) => /^wss:\/\/[^\s]+$/.test(r))
  const by = (p.get('by') || '').trim().slice(0, 40)
  if (!watchers.length || !relays.length) return null
  return { secret, by, watchers, relays }
}

/** The short link for an invite (the same form the watchers make). */
export function joinUrl(base, secret) {
  return `${base}#j=${secret}`
}

const b64url = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
const unb64url = (s) => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0))

/** An angel's own push key (VAPID, P-256): { pub, d } in base64url. */
export async function newVapidKeys() {
  const pair = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, ['sign', 'verify'])
  const pub = new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey))
  const jwk = await crypto.subtle.exportKey('jwk', pair.privateKey)
  return { pub: b64url(pub), d: jwk.d }
}

/**
 * Subscribe this app to Web Push with the angel's key. Returns the `push` part of
 * angel_join, or throws (no permission, no support: on an iPhone only from the Home Screen).
 */
export async function subscribePush(vapid) {
  if (!('serviceWorker' in navigator) || !('PushManager' in window)) throw new Error('this browser cannot receive pushes')
  const reg = await navigator.serviceWorker.ready
  const key = unb64url(vapid.pub)
  let sub = await reg.pushManager.getSubscription()
  if (sub) {
    const old = sub.options && sub.options.applicationServerKey && new Uint8Array(sub.options.applicationServerKey)
    if (!old || old.length !== key.length || old.some((b, i) => b !== key[i])) {
      await sub.unsubscribe()
      sub = null
    }
  }
  if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key })
  const j = sub.toJSON()
  return { endpoint: j.endpoint, p256dh: j.keys.p256dh, auth: j.keys.auth, vapid_private: vapid.d, vapid_public: vapid.pub }
}

/** Alarms and notices sent to an angel's key (`angel_message`), newest first. */
export async function fetchAngelMessages(sk, relays, timeoutMs = 8000) {
  const since = Math.floor(Date.now() / 1000) - WRAP_EXPIRATION - 3600
  const lists = await Promise.all(relays.map((r) => fetchFromRelay(r, getPublicKey(sk), since, timeoutMs)))
  const seen = new Set()
  const out = []
  for (const wrap of lists.flat()) {
    if (seen.has(wrap.id)) continue
    seen.add(wrap.id)
    const m = unwrap(sk, wrap)
    if (!m || m.payload.type !== 'angel_message' || typeof m.payload.id !== 'string') continue
    out.push({ ...m.payload, from: m.sender, at: m.created_at })
  }
  out.sort((a, b) => b.at - a.at)
  return out
}

/** A Telegram join link as watchers send it, or null for anything else. */
export function inviteLink(s) {
  return typeof s === 'string' && /^https:\/\/t\.me\/[A-Za-z0-9_]{5,64}\?start=[A-Za-z0-9_-]{1,64}$/.test(s) ? s : null
}

/** The angel list from a `watching` payload, cleaned up, or null. */
export function angelList(payload) {
  if (!payload || typeof payload.person !== 'string' || !Array.isArray(payload.angels)) return null
  const list = payload.angels
    .filter((a) => a && Number.isSafeInteger(a.chat) && typeof a.name === 'string')
    .filter((a) => a.key === undefined || /^[0-9a-f]{64}$/.test(a.key))
    .map((a) => ({ chat: a.chat, key: a.key || null, name: a.name, active: !!a.active, missed: !!a.missed, late_ok: !!a.late_ok }))
  return { person: payload.person, list }
}

/** The VIP's devices from a `watching` payload, cleaned up, or null. */
export function phoneList(payload) {
  if (!payload || typeof payload.person !== 'string' || !Array.isArray(payload.phones)) return null
  const list = payload.phones
    .filter((p) => p && /^[0-9a-f]{64}$/.test(p.key) && typeof p.name === 'string')
    .map((p) => ({
      key: p.key,
      name: p.name.slice(0, 40),
      at: Number.isSafeInteger(p.at) ? p.at : 0,
      seen: Number.isSafeInteger(p.seen) ? p.seen : 0,
    }))
  return { person: payload.person, list }
}

/** Listen on one relay for wraps to `pubkey` (stored ones first, then live). Returns close(). */
function listen(url, pubkey, since, onEvent) {
  let ws
  try {
    ws = new WebSocket(url)
  } catch {
    return () => {}
  }
  ws.onopen = () => ws.send(JSON.stringify(['REQ', 'p', { kinds: [1059], '#p': [pubkey], since }]))
  ws.onmessage = (m) => {
    try {
      const msg = JSON.parse(m.data)
      if (msg[0] === 'EVENT' && msg[1] === 'p') onEvent(msg[2])
    } catch {}
  }
  return () => {
    try {
      ws.close()
    } catch {}
  }
}

/** Random bytes as hex. */
export function randomHex(bytes) {
  return bytesToHex(crypto.getRandomValues(new Uint8Array(bytes)))
}

/**
 * Ask every watcher "do you watch me, now?" and wait for the answers. Goes the same way as an
 * alarm (this device -> relays -> watcher -> relays -> here), so an answer proves that path.
 * `onAnswer(watcherHex, ms, payload)` is called for each answer as it comes; resolves with
 * { watcherHex: ms } when all answered or after `timeoutMs`.
 */
export async function pingWatchers(sk, watchers, relays, onAnswer = () => {}, timeoutMs = 20000) {
  const nonce = randomHex(8)
  const all = await ask(sk, watchers, relays, { type: 'ping', nonce }, nonce, onAnswer, timeoutMs)
  const answers = {}
  for (const [k, a] of Object.entries(all)) if (a.payload.type === 'watching') answers[k] = a.ms
  return answers
}

/**
 * Send `message` to every watcher and collect the answers that refer to it (`reply_to`, or
 * `hash` for invite_ok, equal to `ref`): { watcherHex: { ms, payload } }.
 */
export async function ask(sk, watchers, relays, message, ref, onAnswer = () => {}, timeoutMs = 20000) {
  const started = Date.now()
  const since = Math.floor(started / 1000) - WRAP_EXPIRATION - 3600
  const answers = {}
  const seen = new Set()
  let finish
  const finished = new Promise((r) => (finish = r))
  const onEvent = (wrap) => {
    if (seen.has(wrap.id)) return
    seen.add(wrap.id)
    const m = unwrap(sk, wrap)
    if (!m || (m.payload.reply_to !== ref && !(m.payload.type === 'invite_ok' && m.payload.hash === ref))) return
    // An answer to an earlier try with the same ref (relays keep them for days); allow some clock skew.
    if (m.created_at < started / 1000 - 60) return
    if (!watchers.includes(m.sender) || m.sender in answers) return
    answers[m.sender] = { ms: Date.now() - started, payload: m.payload }
    onAnswer(m.sender, answers[m.sender].ms, m.payload)
    if (Object.keys(answers).length === watchers.length) finish()
  }
  // Listen first, so a fast answer is not missed.
  const closers = relays.map((r) => listen(r, getPublicKey(sk), since, onEvent))
  const timer = setTimeout(finish, timeoutMs)
  try {
    await deliver(buildWraps(sk, watchers, messagePayload(message)), relays, timeoutMs)
    await finished
  } catch {
    // Not sent at all: no answers.
  } finally {
    clearTimeout(timer)
    closers.forEach((c) => c())
  }
  return answers
}

/** 'watching' (heard within two intervals plus slack), 'offline' (heard before), or 'unconfirmed'. */
export function watchState(record, nowSecs = Math.floor(Date.now() / 1000)) {
  if (!record) return 'unconfirmed'
  return nowSecs - record.at <= 2 * record.interval + 600 ? 'watching' : 'offline'
}

/** Send every wrap to one relay; resolves true once the relay accepted all of them. */
function sendToRelay(url, wraps, timeoutMs) {
  return new Promise((resolve) => {
    let ws
    const pending = new Set(wraps.map((w) => w.id))
    const done = (ok) => {
      clearTimeout(timer)
      try {
        ws.close()
      } catch {}
      resolve(ok)
    }
    const timer = setTimeout(() => done(false), timeoutMs)
    try {
      ws = new WebSocket(url)
    } catch {
      return done(false)
    }
    ws.onopen = () => wraps.forEach((w) => ws.send(JSON.stringify(['EVENT', w])))
    ws.onerror = () => done(false)
    ws.onmessage = (m) => {
      let msg
      try {
        msg = JSON.parse(m.data)
      } catch {
        return
      }
      if (msg[0] === 'OK' && pending.has(msg[1]) && msg[2] === true) {
        pending.delete(msg[1])
        if (pending.size === 0) done(true)
      }
    }
  })
}

/**
 * Deliver to all relays in parallel; resolves with the first relay that accepted everything.
 * Keeps retrying for `patienceMs` (e.g. while the phone reconnects). The other relays keep
 * going in the background.
 */
export async function deliver(wraps, relays, patienceMs = 120000) {
  const started = Date.now()
  return new Promise((resolve, reject) => {
    let settled = false
    let running = relays.length
    const attempt = async (url) => {
      while (Date.now() - started < patienceMs) {
        if (await sendToRelay(url, wraps, RELAY_OK_TIMEOUT_MS)) {
          if (!settled) {
            settled = true
            resolve({ relay: url, ms: Date.now() - started })
          }
          return
        }
        await new Promise((r) => setTimeout(r, 1000))
      }
      if (--running === 0 && !settled) reject(new Error('no relay accepted the message'))
    }
    relays.forEach(attempt)
  })
}
