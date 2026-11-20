// Phone duress app. Holds its own device key in this browser's storage and sends a duress
// message to the configured watchers. Deliberately plain-looking.

import {
  DEFAULT_RELAYS,
  ask,
  fetchInvite,
  inviteEvent,
  joinUrl,
  newInviteSecret,
  parseJoin,
  secretHash,
  buildDuress,
  buildWraps,
  deliver,
  angelList,
  phoneList,
  fetchWatching,
  inviteLink,
  messagePayload,
  pingWatchers,
  newSecretKey,
  npubOf,
  parsePubkey,
  watchState,
} from './core.js'
import { npubEncode } from 'nostr-tools/nip19'
import { getPublicKey } from 'nostr-tools/pure'
import { addInvitedVipToAngel, hasAngel, loadAngel, prepareAngel, showAngel, startAngelJoin } from './angel.js'
import qrcode from 'qrcode-generator'

const bytesToHex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')
const hexToBytes = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16))

const KEY = 'w.state'
/** Shown in Settings, so you can tell whether the phone has the newest version. */
const APP_VERSION = '29'
/** Phase 1 starter courtesy watcher (Docker watcher open for self-enrolment). */
const COURTESY_WATCHER = {
  npub: 'npub15pn4an7stpzgk2rj300errwvf6559gc3astxw7mlmy0kmd7rd03s8xpd2n',
  name: 'Angel courtesy server',
  relays: DEFAULT_RELAYS,
}
const COURTESY_HEX = parsePubkey(COURTESY_WATCHER.npub)
/** How long a link to add a phone is valid (the watchers allow at most 30). */
const PAIR_MINUTES = 30
/** How long an angel link is valid; a new one is made when less than a month is left. */
const ANGEL_LINK_DAYS = 180
/** How long a "Protect someone" link is valid. */
const PROTECT_DAYS = 7
/** Minutes added by the "+15 min" button. */
const EXTEND_MIN = 15
const ntfyServer = 'https://ntfy.sh'
/** Ready-made iPhone Shortcuts (iCloud links). Each asks for the topic URL when added. */
const SHORTCUT_LINKS = {
  duress: 'https://www.icloud.com/shortcuts/04e34caeca4e47539b7dfd25ef6ad572',
  ok: 'https://www.icloud.com/shortcuts/c86ae7daa4ff422e9ea190e10c46c071',
  delay: 'https://www.icloud.com/shortcuts/c5ca73d06d354603aa2495fc6b5af232',
}
const PUBLIC_NTFY_RE = /^https:\/\/ntfy\.sh\/[A-Za-z0-9_-]{20,64}$/
/** A topic name shorter than this is too easy to guess; the watchers refuse it too. */
const MIN_TOPIC_LEN = 20
/** Do not auto-send again if the app is reopened within this time (iOS reloads apps). */
const AUTO_SEND_QUIET_MS = 60000

const $ = (id) => document.getElementById(id)

function load() {
  try {
    return JSON.parse(localStorage.getItem(KEY)) || null
  } catch {
    return null
  }
}
function save(s) {
  localStorage.setItem(KEY, JSON.stringify(s))
}

/** A long random topic name: whoever knows it can send to it, so it is a secret. */
function newTopic() {
  const a = new Uint8Array(18)
  crypto.getRandomValues(a)
  return 's' + bytesToHex(a)
}

/** The full topic URL. Older versions stored only the name, on ntfy.sh. */
function topicUrl(s) {
  return s.topic.startsWith('http') ? s.topic : `${ntfyServer}/${s.topic}`
}

function parseTopic(text) {
  const t = text.trim().replace(/\/+$/, '')
  const url = t.startsWith('http') ? t : `${ntfyServer}/${t}`
  const name = url.split('/').pop()
  if (!/^https:\/\/[^/]+\/[A-Za-z0-9_-]+$/.test(url) || name.length < MIN_TOPIC_LEN) {
    throw new Error(`topic: a https address ending in at least ${MIN_TOPIC_LEN} letters or digits`)
  }
  return url
}

function isPublicNtfyUrl(url) {
  return PUBLIC_NTFY_RE.test((url || '').trim().replace(/\/+$/, ''))
}

function isTopicSynced(s) {
  const url = topicUrl(s)
  return !!(
    s.topicSynced &&
    s.topicSynced.url === url &&
    s.watchers.length > 0 &&
    s.watchers.every((k) => (s.topicSynced.watchers || []).includes(k))
  )
}

/** Copy the topic, so it can be pasted when a shortcut asks for it. */
async function copyTopic(s) {
  try {
    await navigator.clipboard.writeText(topicUrl(s))
    $('topicHint').textContent = 'Topic copied.'
    if ($('shortcutHint')) $('shortcutHint').textContent = 'Topic copied — paste it when Apple Shortcuts asks for the URL.'
    return true
  } catch {
    $('topicHint').textContent = 'Could not copy: select the topic above and copy it by hand.'
    if ($('shortcutHint')) $('shortcutHint').textContent = 'Could not copy automatically: open Advanced → Copy topic first.'
    return false
  }
}

function newSession() {
  const a = new Uint8Array(12)
  crypto.getRandomValues(a)
  return bytesToHex(a)
}

function freshState() {
  return {
    sk: bytesToHex(newSecretKey()),
    topic: newTopic(),
    timer: null,
    device: 'phone',
    watchers: [],
    relays: DEFAULT_RELAYS,
    autoSend: false,
    useLocation: true,
    lastAuto: 0,
  }
}

function show(view) {
  for (const v of ['join', 'setup', 'main', 'angelJoin', 'angel']) $(v).hidden = v !== view
}

/** The line under the button on the start screen. */
function status(text) {
  $('status').textContent = text
}

/** A short message at the bottom ("Saved", "Removed … Undo"). With `undo`, an Undo button. */
let toastTimer = null
function say(text, { undo = null, ms = 2500 } = {}) {
  clearTimeout(toastTimer)
  $('toastText').textContent = text
  $('toastUndo').hidden = !undo
  $('toastUndo').onclick = () => {
    $('toast').hidden = true
    if (undo) undo()
  }
  $('toast').hidden = false
  toastTimer = setTimeout(() => ($('toast').hidden = true), ms)
}

/** Whether the user is typing in this part of the page (do not redraw it then). */
function typingIn(id) {
  const el = document.activeElement
  return !!el && el.tagName === 'INPUT' && $(id).contains(el)
}

/**
 * Removals wait a few seconds, so Undo can take them back; they are sent then, or at once when
 * Settings is left or the app goes to the background.
 */
const pending = new Map()
function later(key, send, ms = 5000) {
  pending.set(key, { send, timer: setTimeout(() => flushOne(key), ms) })
}
function cancelLater(key) {
  const p = pending.get(key)
  if (p) clearTimeout(p.timer)
  pending.delete(key)
}
function flushOne(key) {
  const p = pending.get(key)
  if (!p) return
  pending.delete(key)
  clearTimeout(p.timer)
  p.send()
}
function flushAll() {
  for (const key of [...pending.keys()]) flushOne(key)
}

  /** Remember the newest join link, watcher name, person name, and angel/device lists a watcher sent. Returns whether anything changed. */
  function noteWatcher(s, payload, at, watcherKey = null) {
    let changed = false
    if (payload && typeof payload.person === 'string' && payload.person.trim()) {
      const person = payload.person.trim()
      if (s.person !== person) {
        s.person = person
        changed = true
      }
      if (!s.angels && payload.type === 'watching') {
        s.angels = { person, list: [], at: 0 }
        changed = true
      }
    }
    if (watcherKey && payload && typeof payload.watcher === 'string') {
      const label = payload.watcher.trim().slice(0, 40)
      if (label && (!s.serverLabels || s.serverLabels[watcherKey] !== label)) {
        s.serverLabels = { ...(s.serverLabels || {}), [watcherKey]: label }
        changed = true
      }
    }
    const link = inviteLink(payload && payload.invite)
    if (link && !(s.invite && s.invite.at > at) && (!s.invite || s.invite.link !== link)) {
      s.invite = { link, at }
      changed = true
    }
    const phones = phoneList(payload)
    if (phones && !(s.phones && s.phones.at > at)) {
      // Each server knows only what reached it: keep the newest "last seen" from any of them.
      const before = new Map(((s.phones && s.phones.list) || []).map((p) => [p.key, p.seen || 0]))
      for (const p of phones.list) p.seen = Math.max(p.seen, before.get(p.key) || 0)
      if (!s.phones || JSON.stringify(s.phones.list) !== JSON.stringify(phones.list)) changed = true
      s.phones = { ...phones, at }
    }
    const angels = angelList(payload)
    if (angels && !(s.angels && s.angels.at > at)) {
      if (!s.angels || JSON.stringify(s.angels.list) !== JSON.stringify(angels.list)) changed = true
      s.angels = { ...angels, at }
    }
    return changed
  }

  /** Settings: "My angels". Every change is sent to the servers at once. */
  function showAngels(s) {
    if (typingIn('angelList')) return
    const box = $('angelList')
    box.textContent = ''
    const list = ((s.angels && s.angels.list) || []).filter((a) => !pending.has(`angel:${a.key || a.chat}`))
    const activeCount = list.filter((a) => a.active).length
    $('angelsNone').hidden = list.length > 0
    $('angelsHint').hidden = list.length === 0
    $('angelInvite').classList.toggle('primary', activeCount === 0)
    $('angelTest').hidden = activeCount === 0
    showGuide(s)
    updateSetupFocus(s)
    updateDot(s)
    for (const a of list) {
      const row = document.createElement('div')
      row.className = 'angel'
      const top = document.createElement('div')
      top.className = 'angelTop'
      const name = document.createElement('input')
      name.type = 'text'
      name.value = a.name
      name.setAttribute('aria-label', `Name of angel ${a.name}`)
      const tag = document.createElement('span')
      tag.className = 'tag'
      tag.textContent = a.key ? 'app' : 'Telegram'
      const remove = document.createElement('button')
      remove.type = 'button'
      remove.className = 'danger'
      remove.textContent = 'Remove'
      remove.setAttribute('aria-label', `Remove ${a.name}`)
      top.append(name, tag, remove)
      const switches = document.createElement('div')
      switches.className = 'switches'
      const sw = {}
      for (const [key, label] of [['active', 'On'], ['missed', 'Gets "did not check in"'], ['late_ok', 'Gets "checked in late"']]) {
        const l = document.createElement('label')
        l.className = 'sw'
        const c = document.createElement('input')
        c.type = 'checkbox'
        c.checked = a[key]
        l.append(label, c)
        switches.append(l)
        sw[key] = c
        c.onchange = () => changeAngel(s, a, { [key]: c.checked })
      }
      name.onchange = () => {
        const n = name.value.trim()
        if (n && n !== a.name) changeAngel(s, a, { name: n })
        else name.value = a.name
      }
      remove.onclick = () => {
        const id = `angel:${a.key || a.chat}`
        later(id, () => changeAngel(s, a, { removed: true }, true))
        showAngels(s)
        say(`${a.name} removed`, { undo: () => (cancelLater(id), showAngels(s)), ms: 5000 })
      }
      row.append(top, switches)
      box.append(row)
    }
  }

  /** Send one angel's change to every server, and keep it here. */
  async function changeAngel(s, a, change, quiet = false) {
    const now = { chat: a.chat, ...(a.key ? { key: a.key } : {}), name: a.name, active: a.active, missed: a.missed, late_ok: a.late_ok, ...change }
    try {
      await sendMessage(s, { type: 'contact_changed', person: s.angels.person, removed: false, ...now })
      const same = (b) => (b.key || b.chat) === (a.key || a.chat)
      s.angels = {
        ...s.angels,
        at: Math.floor(Date.now() / 1000),
        list: s.angels.list.flatMap((b) => (same(b) ? (now.removed ? [] : [{ ...b, ...change }]) : [b])),
      }
      Object.assign(a, change)
      save(s)
      if (!quiet) say('Saved')
    } catch (e) {
      say('Not saved: ' + e.message, { ms: 5000 })
    }
    if (now.removed) showAngels(s)
  }

  /** Settings: "My devices". Renames and removals are sent at once. */
  function showPhones(s) {
    if (typingIn('phoneList')) return
    const box = $('phoneList')
    box.textContent = ''
    const all = (s.phones && s.phones.list) || []
    const list = all.filter((p) => !pending.has(`phone:${p.key}`))
    const me = getPublicKey(hexToBytes(s.sk))
    for (const p of list) {
      const row = document.createElement('div')
      row.className = 'angel'
      const top = document.createElement('div')
      top.className = 'angelTop'
      const name = document.createElement('input')
      name.type = 'text'
      name.value = p.name
      name.setAttribute('aria-label', `Name of device ${p.name}`)
      name.onchange = () => {
        const n = name.value.trim()
        if (n && n !== p.name) changePhone(s, p, { name: n })
        else name.value = p.name
      }
      top.append(name)
      if (p.key === me) {
        const tag = document.createElement('span')
        tag.className = 'tag'
        tag.textContent = 'this phone'
        top.append(tag)
      } else if (list.length > 1) {
        const remove = document.createElement('button')
        remove.type = 'button'
        remove.className = 'danger'
        remove.textContent = 'Remove'
        remove.setAttribute('aria-label', `Remove ${p.name}`)
        remove.onclick = () => {
          const id = `phone:${p.key}`
          later(id, () => changePhone(s, p, { removed: true }, true))
          showPhones(s)
          say(`${p.name} removed: it can no longer send alarms`, { undo: () => (cancelLater(id), showPhones(s)), ms: 5000 })
        }
        top.append(remove)
      }
      const when = document.createElement('div')
      when.className = 'sub'
      const added = p.at ? `added ${new Date(p.at * 1000).toLocaleDateString()}` : 'put in the server settings by hand'
      const seen = p.key === me ? 'this phone' : p.seen ? `last seen ${ago(p.seen)}` : 'not seen yet'
      when.textContent = `${seen[0].toUpperCase()}${seen.slice(1)} · ${added}`
      row.append(top, when)
      box.append(row)
    }
  }

  /** Send one device's change to every server, and keep it here. */
  async function changePhone(s, p, change, quiet = false) {
    try {
      await sendMessage(s, { type: 'phone_changed', key: p.key, name: change.name || p.name, removed: !!change.removed })
      s.phones = {
        ...s.phones,
        at: Math.floor(Date.now() / 1000),
        list: s.phones.list.flatMap((q) => (q.key === p.key ? (change.removed ? [] : [{ ...q, ...change }]) : [q])),
      }
      Object.assign(p, change)
      save(s)
      if (!quiet) say('Saved')
    } catch (e) {
      say('Not saved: ' + e.message, { ms: 5000 })
    }
    if (change.removed) showPhones(s)
  }

  const PAIR_TEXT = 'Open this on the new phone to add it to Sync:'

  /** Add a phone: register a short-lived link with my servers, then show it. */
  async function addPhone(s) {
    $('phoneGo').disabled = true
    $('phoneShareBox').hidden = true
    const msg = $('phoneMsg')
    msg.textContent = 'Asking your servers…'
    const secret = newInviteSecret()
    const hash = secretHash(secret)
    const expires = Math.floor(Date.now() / 1000) + PAIR_MINUTES * 60
    const answers = await ask(hexToBytes(s.sk), s.watchers, s.relays, { type: 'invite_created', hash, expires, device: true }, hash)
    $('phoneGo').disabled = false
    const ok = s.watchers.filter((k) => answers[k] && answers[k].payload.type === 'invite_ok')
    if (!ok.length) {
      msg.textContent = 'No server answered, so there is no link. Try again in a moment.'
      return
    }
    const by = (s.phones && s.phones.person) || (s.angels && s.angels.person) || ''
    try {
      await deliver([inviteEvent(secret, { by, watchers: ok, relays: s.relays, t: 'device' }, expires)], allRelays(s))
    } catch (e) {
      msg.textContent = `Could not store the link: ${e.message}`
      return
    }
    msg.textContent = ''
    const missing = s.watchers.length - ok.length
    sharePanel($('phoneShareBox'), {
      link: `${location.href.split('#')[0]}#d=${secret}`,
      text: PAIR_TEXT,
      plainCopy: true,
      note:
        `Scan this with the new phone, or send it the link. It works once, within ${PAIR_MINUTES} minutes.` +
        (missing ? ` ${missing} server${missing > 1 ? 's' : ''} did not answer and will not know the new phone.` : ''),
    })
  }

  /** Draw a QR code for `text` on the canvas, black on white (also in dark mode, for scanners). */
  function drawQr(canvas, text) {
    const qr = qrcode(0, 'M')
    qr.addData(text)
    qr.make()
    const n = qr.getModuleCount()
    const quiet = 4
    const scale = Math.floor(240 / (n + 2 * quiet))
    const size = (n + 2 * quiet) * scale
    canvas.width = canvas.height = size
    const g = canvas.getContext('2d')
    g.fillStyle = '#fff'
    g.fillRect(0, 0, size, size)
    g.fillStyle = '#000'
    for (let r = 0; r < n; r++) for (let c = 0; c < n; c++) if (qr.isDark(r, c)) g.fillRect((c + quiet) * scale, (r + quiet) * scale, scale, scale)
  }

  /**
   * The one way links are shown: a QR code, Share, Copy, what it is for, and the link itself.
   * `plainCopy`: copy only the link (not the message text).
   */
  function sharePanel(box, { link, text, note, plainCopy = false }) {
    box.textContent = ''
    box.hidden = false
    const canvas = document.createElement('canvas')
    canvas.setAttribute('aria-label', 'QR code of the link')
    drawQr(canvas, link)
    const row = document.createElement('div')
    row.className = 'row3'
    const share = document.createElement('button')
    share.type = 'button'
    share.className = 'primary'
    share.textContent = 'Share link'
    share.hidden = !navigator.share
    share.onclick = async () => {
      try {
        await navigator.share({ text, url: link })
      } catch { }
    }
    const copy = document.createElement('button')
    copy.type = 'button'
    copy.textContent = 'Copy link'
    copy.onclick = async () => {
      try {
        await navigator.clipboard.writeText(plainCopy ? link : `${text}\n${link}`)
        say('Copied: paste it in a message')
      } catch {
        say('Could not copy: select the link below and copy it by hand', { ms: 4000 })
      }
    }
    row.append(share, copy)
    const p = document.createElement('p')
    p.className = 'hint'
    p.style.marginTop = '10px'
    p.textContent = note || ''
    const mono = document.createElement('div')
    mono.className = 'mono'
    mono.textContent = link
    box.append(canvas, row, p, mono)
  }

  const PROTECT_TEXT =
    "I'd like to protect you: with this app one tap alerts the people you choose, and my servers watch over you. Open the link on your phone."

  /** The app's standard relays plus the configured ones (invites are looked up on these). */
  function allRelays(s) {
    return [...new Set([...DEFAULT_RELAYS, ...((s && s.relays) || [])])]
  }

  function isStandalone() {
    return !!(navigator.standalone || matchMedia('(display-mode: standalone)').matches)
  }
  function isIOS() {
    return /iPhone|iPad|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1)
  }
  function isAndroid() {
    return /Android/i.test(navigator.userAgent)
  }
  function isMobile() {
    return isIOS() || isAndroid()
  }
  /** iPhone Safari (not the Home Screen app): storage there is not the app's. */
  function iosBrowser() {
    return isIOS() && !isStandalone()
  }

  function shortKey(k) {
    const npub = npubEncode(k)
    return `${npub.slice(0, 12)}…${npub.slice(-6)}`
  }

  /** Settings: people who accepted my invite, with a button to become their angel. */
  function showJoined(s) {
    const box = $('joinedList')
    box.textContent = ''
    const open = Object.entries(s.joined || {}).filter(([, j]) => !j.done)
    for (const [person, j] of open) {
      const p = document.createElement('p')
      p.textContent = `${person} accepted your invite. You are now ${person}'s angel.`
      const row = document.createElement('div')
      row.className = 'row3'
      if (j.invite) {
        const b = document.createElement('button')
        b.type = 'button'
        b.textContent = 'Also on Telegram'
        b.onclick = () => {
          j.done = true
          save(s)
          location.href = j.invite
        }
        row.append(b)
      }
      const d = document.createElement('button')
      d.type = 'button'
      d.textContent = 'Dismiss'
      d.onclick = () => {
        j.done = true
        save(s)
        showJoined(s)
      }
      row.append(d)
      p.append(row)
      box.append(p)
    }
    // People who were invited (or only on the starter courtesy server) cannot pass servers on.
    const onlyCourtesy = s.watchers.length === 1 && s.watchers[0] === COURTESY_HEX
    $('protectCard').hidden = !!s.invitedBy || (!!s.courtesy && onlyCourtesy) || !s.watchers.length
    updateDot(s)
  }

  /** Note `vip_joined` messages from the watchers. */
  function noteJoined(s, m) {
    if (m.payload.type !== 'vip_joined' || typeof m.payload.person !== 'string') return
    const j = ((s.joined = s.joined || {})[m.payload.person] ||= { at: m.created_at, done: false })
    j.invite = j.invite || inviteLink(m.payload.invite)
    const hadSecret = !!j.angelSecret
    if (typeof m.payload.angel_secret === 'string' && !j.angelSecret) j.angelSecret = m.payload.angel_secret
    if (!j.angelJoined || (!hadSecret && j.angelSecret)) {
      j.angelJoined = true
      const name = (s.angels && s.angels.person) || (s.phones && s.phones.person) || s.person || ''
      addInvitedVipToAngel(m.payload.person, s.watchers, s.relays, j.angelSecret || null, name)
        .then(() => showInvite(s))
        .catch(() => { })
    }
  }

  /**
   * Make sure there is an app angel link that is valid for at least another month: register its
   * secret with my watchers and put the details on the relays.
   */
  let angelLinkInFlight = null
  async function ensureAngelLink(s) {
    const t = Math.floor(Date.now() / 1000)
    if (s.angelLink && s.angelLink.expires - t > 30 * 86400) return
    if (angelLinkInFlight) return angelLinkInFlight
    const person = (s.angels && s.angels.person) || (s.phones && s.phones.person) || s.person
    if (!person || !s.watchers.length) return
    angelLinkInFlight = (async () => {
      try {
        const secret = newInviteSecret()
        const hash = secretHash(secret)
        const expires = t + ANGEL_LINK_DAYS * 86400
        const answers = await ask(hexToBytes(s.sk), s.watchers, s.relays, { type: 'invite_created', hash, expires, angel: true }, hash)
        const ok = s.watchers.filter((k) => answers[k] && answers[k].payload.type === 'invite_ok')
        if (!ok.length) return
        await deliver([inviteEvent(secret, { by: person, watchers: ok, relays: s.relays, t: 'angel' }, expires)], allRelays(s))
        s.angelLink = { link: `${location.href.split('#')[0]}#a=${secret}`, expires }
        save(s)
        showInvite(s)
      } finally {
        angelLinkInFlight = null
      }
    })()
    return angelLinkInFlight
  }

  const ANGEL_APP_TEXT =
    "Will you be one of my angels? If I'm ever in trouble, you get an alarm on your phone. Open the link, tap Join and allow notifications."

  const ANGEL_TEXT =
    "Will you be one of my angels? If I'm ever in trouble, you get a message on Telegram. Tap the link and press Start."

  /** Settings: the "Become my angel" card. */
  /** The angel link shown now: in the app (default) or on Telegram. */
  function angelLinkNow(s) {
    const app = s.angelLink && s.angelLink.link
    const tg = s.invite && s.invite.link
    const mode = s.angelMode === 'tg' ? (tg ? 'tg' : 'app') : app ? 'app' : 'tg'
    return { mode, link: mode === 'app' ? app : tg, app, tg }
  }

  function showInvite(s) {
    const { mode, link, app, tg } = angelLinkNow(s)
    $('angelUseApp').classList.toggle('on', mode === 'app')
    $('angelUseTg').classList.toggle('on', mode === 'tg')
    $('angelUseApp').setAttribute('aria-selected', String(mode === 'app'))
    $('angelUseTg').setAttribute('aria-selected', String(mode === 'tg'))
    $('angelUseApp').disabled = !app
    $('angelUseTg').disabled = !tg
    $('angelReady').hidden = !link
    $('angelNone').hidden = !!link
    const a = loadAngel()
    $('gAngelFor').hidden = !hasAngel()
    if (a && a.vips) $('beAngelText').textContent = `You get alarms for ${Object.keys(a.vips).join(', ')}.`
    if (!link) return
    const how =
      mode === 'app'
        ? 'They open it on their phone, tap Join and allow notifications. They then show up in your list.'
        : 'They tap Start in Telegram and get your alarms from then on. They then show up in your list.'
    $('angelIntro').textContent =
      s.invitedBy && !(s.angels && s.angels.list.length)
        ? `First send this link to ${s.invitedBy}: they become your first angel. ${how}`
        : `Let someone scan this, or send them the link. ${how}`
    sharePanel($('angelShareBox'), {
      link,
      text: mode === 'app' ? ANGEL_APP_TEXT : ANGEL_TEXT,
      note: 'The same link works for everyone you ask.',
    })
  }

  /** Keep the newest confirmation per watcher (also from earlier checks, for offline use). */
  function mergeWatching(s, fresh) {
    const all = { ...(s.watching || {}) }
    for (const [k, v] of Object.entries(fresh)) if (!all[k] || all[k].at < v.at) all[k] = v
    // Forget watchers that were removed from the settings.
    for (const k of Object.keys(all)) if (!s.watchers.includes(k)) delete all[k]
    if (s.serverLabels) for (const k of Object.keys(s.serverLabels)) if (!s.watchers.includes(k)) delete s.serverLabels[k]
    return all
  }

  function ago(secs) {
    const d = Math.floor(Date.now() / 1000) - secs
    if (d < 3600) return `${Math.max(1, Math.round(d / 60))} min ago`
    if (d < 86400) return `${Math.round(d / 3600)} h ago`
    return `${Math.round(d / 86400)} days ago`
  }

  /** Settings, Status: how many servers answered, and the dot on the Settings button. */
  function showWatchingSummary(s) {
    if (!s || !s.watchers) return
    const w = s.watching || {}
    const ok = s.watchers.filter((k) => watchState(w[k]) === 'watching').length
    const el = $('serverSummary')
    if (!s.checkedAt) el.textContent = 'Checking your servers…'
    else if (ok === s.watchers.length) el.textContent = ok === 1 ? 'Your server is watching over you.' : `All ${ok} servers are watching over you.`
    else if (ok > 0) el.textContent = `${ok} of ${s.watchers.length} servers are watching over you.`
    else el.textContent = 'No server confirms this phone: alarms may reach nobody.'
    el.style.color = s.checkedAt && ok === 0 ? 'var(--bad)' : ''
    $('watcherTip').hidden = !(s.watchers.length === 1 && s.watchers[0] === COURTESY_HEX)
    showGuide(s)
    updateSetupFocus(s)
    updateDot(s)
  }

  /** A dot on Settings when something needs a look: news, 0 active angels, or no server confirms this phone. */
  function updateDot(s) {
    const news = Object.values(s.joined || {}).some((j) => !j.done)
    const ok = (s.watchers || []).some((k) => watchState((s.watching || {})[k]) === 'watching')
    const noAngels = !!(s.watchers && s.watchers.length && s.angels && !s.angels.list.some((a) => a.active))
    $('notice').hidden = !(news || (s.checkedAt && s.watchers.length && !ok) || noAngels)
  }

  /** A server's name: user override, advertised watcher name, or the start of its key. */
  function serverName(s, k) {
    return (s.serverNames && s.serverNames[k]) || (s.serverLabels && s.serverLabels[k]) || shortKey(k)
  }

  /** Guided 3-step onboarding checklist in Settings for new users and starter servers. */
  function showGuide(s) {
    const ready = !!(s && s.watchers && s.watchers.length)
    const activeAngels = ((s && s.angels && s.angels.list) || []).filter((a) => a.active).length
    const onlyCourtesy = ready && s.watchers.length === 1 && s.watchers[0] === COURTESY_HEX
    const visible = ready && (activeAngels === 0 || (!s.guideDismissed && (s.courtesy || onlyCourtesy || s.watchers.length === 1)))
    $('setupGuide').hidden = !visible
    if (!visible) return

    const w = s.watching || {}
    const ok = s.watchers.filter((k) => watchState(w[k]) === 'watching').length
    $('guideServersDot').className = 'state ' + (ok > 0 ? 'ok' : s.checkedAt ? 'warn' : '')
    $('guideServers').textContent = onlyCourtesy
      ? `1. Connected to ${serverName(s, COURTESY_HEX)}`
      : `1. Connected to ${s.watchers.length} server${s.watchers.length > 1 ? 's' : ''}`
    $('guideServersSub').textContent = onlyCourtesy
      ? 'Starter server — it sees your alarms until you run your own.'
      : ok > 0
        ? `${ok} watching you now.`
        : 'Checking connection…'

    $('guideAngelsDot').className = 'state ' + (activeAngels > 0 ? 'ok' : s.checkedAt ? 'bad' : 'warn')
    $('guideAngelsText').textContent =
      activeAngels > 0
        ? `2. ${activeAngels} active angel${activeAngels > 1 ? 's' : ''} ready`
        : '2. Invite your first angel — without an angel, alarms reach nobody.'
    $('guideInviteBtn').textContent = activeAngels > 0 ? 'Invite another' : 'Invite an angel'
    $('guideInviteBtn').classList.toggle('primary', activeAngels === 0)
    $('guideTestBtn').hidden = activeAngels === 0

    $('guideWatcher').hidden = !(onlyCourtesy || s.watchers.length === 1)
    $('guideWatcherDot').className = 'state ' + (onlyCourtesy ? 'warn' : 'ok')
    $('guideDismiss').hidden = activeAngels === 0
  }

  function removeWatcher(s, k) {
    const left = s.watchers.filter((w) => w !== k)
    const label = serverName(s, k)
    const msg = left.length
      ? `Remove ${label}? Alarms will go only to the ${left.length} server${left.length > 1 ? 's' : ''} left.`
      : `Remove ${label}? This is your last server: Sync stops sending alarms until you add another.`
    if (!confirm(msg)) return
    s.watchers = left
    s.watching = mergeWatching(s, {})
    if (s.serverNames) delete s.serverNames[k]
    if (s.serverLabels) delete s.serverLabels[k]
    s.angelLink = null
    if (k === COURTESY_HEX) s.courtesy = false
    if (!left.length) {
      delete s.invitedBy
      delete s.person
      delete s.angels
      delete s.phones
      delete s.invite
      delete s.protectLink
      delete s.timer
      delete s.topicSynced
      delete s.showAllSettings
      delete s.checkedAt
    }
    save(s)
    fillSetup(s)
    say('Server removed')
    if (left.length) liveCheck(s)
  }

  /** Settings, Status: ask every server now and show each answer as it comes. */
  async function liveCheck(s) {
    if (!s || !s.watchers || !s.watchers.length) return
    const list = $('watchList')
    list.textContent = ''
    const lines = {}
    for (const k of s.watchers) {
      const item = document.createElement('div')
      item.className = 'item'
      const dot = document.createElement('span')
      dot.className = 'state'
      const grow = document.createElement('div')
      grow.className = 'grow'
      const top = document.createElement('div')
      top.className = 'angelTop'
      const name = document.createElement('input')
      name.type = 'text'
      name.value = (s.serverNames && s.serverNames[k]) || (s.serverLabels && s.serverLabels[k]) || ''
      name.placeholder = (s.serverLabels && s.serverLabels[k]) || 'Name this server'
      name.setAttribute('aria-label', `Name for server ${shortKey(k)}`)
      name.onchange = () => {
        const v = name.value.trim()
        const advertised = (s.serverLabels && s.serverLabels[k]) || ''
        s.serverNames = { ...(s.serverNames || {}) }
        if (!v || v === advertised) {
          delete s.serverNames[k]
          name.value = advertised
        } else {
          s.serverNames[k] = v
        }
        save(s)
        say('Saved')
      }
      const remove = document.createElement('button')
      remove.type = 'button'
      remove.className = 'danger'
      remove.textContent = 'Remove'
      remove.setAttribute('aria-label', `Remove server ${serverName(s, k)}`)
      remove.onclick = () => removeWatcher(s, k)
      top.append(name, remove)
      const sub = document.createElement('div')
      sub.className = 'sub'
      const key = shortKey(k)
      sub.textContent = `${key} · asking…`
      grow.append(top, sub)
      item.append(dot, grow)
      list.append(item)
      lines[k] = { dot, sub, key, name }
    }
    $('serverSummary').textContent = 'Checking your servers…'
    $('serverSummary').style.color = ''
    $('checkNow').disabled = true
    const t0 = Math.floor(Date.now() / 1000)
    const checkedKeys = [...s.watchers]
    const answers = await pingWatchers(hexToBytes(s.sk), checkedKeys, s.relays, (k, ms, payload) => {
      if (!s.watchers.includes(k) || !lines[k]) return
      lines[k].dot.className = 'state ok'
      lines[k].sub.textContent = `${lines[k].key} · watching you · answered in ${(ms / 1000).toFixed(1)} s`
      const changed = noteWatcher(s, payload, Math.floor(Date.now() / 1000), k)
      if (!(s.serverNames && s.serverNames[k]) && s.serverLabels && s.serverLabels[k]) {
        lines[k].name.placeholder = s.serverLabels[k]
        if (document.activeElement !== lines[k].name) lines[k].name.value = s.serverLabels[k]
      }
      if (changed) {
        save(s)
        showInvite(s)
        showAngels(s)
        showPhones(s)
      }
    })
    $('checkNow').disabled = false
    const fresh = {}
    for (const k of Object.keys(answers)) if (s.watchers.includes(k)) fresh[k] = { at: t0, interval: (s.watching?.[k]?.interval) || 3600 }
    s.watching = mergeWatching(s, fresh)
    s.checkedAt = Date.now()
    save(s)
    for (const k of s.watchers) {
      if (k in answers || !lines[k]) continue
      const last = s.watching[k]
      lines[k].dot.className = last ? 'state warn' : 'state bad'
      lines[k].sub.textContent = `${lines[k].key} · no answer now` + (last ? ` · last answer ${ago(last.at)}` : ' · never confirmed this phone')
    }
    showWatchingSummary(s)
    ensureAngelLink(s).catch(() => { })
  }

  async function checkWatching(s) {
    if (!s || !s.watchers || !s.watchers.length) return
    try {
      const fresh = await fetchWatching(hexToBytes(s.sk), s.watchers, s.relays, 8000, (m) => noteJoined(s, m))
      for (const [k, v] of Object.entries(fresh)) noteWatcher(s, v.payload, v.at, k)
      s.watching = mergeWatching(s, fresh)
      s.checkedAt = Date.now()
      save(s)
    } catch { }
    showWatchingSummary(s)
    showJoined(s)
  }

  /** The invite screen. */
  function startJoin(s, join) {
    show('join')
    const by = join.by || 'Someone'
    const device = join.device || join.t === 'device'
    const offer = join.offer || join.t === 'offer'
    const more = offer && s.watchers && s.watchers.length > 0
    $('joinTitle').textContent = device ? 'Add this phone' : more ? 'Add a server' : 'You are invited'
    $('joinLabel').textContent = device ? 'A name for this phone' : 'Your name, as your angels will see it'
    $('joinGo').textContent = device ? 'Add this phone' : more ? 'Add this server' : 'Join'
    if (device && !$('joinName').value) $('joinName').value = /iPhone/.test(navigator.userAgent) ? 'iPhone' : 'phone'
    if (more && !$('joinName').value) $('joinName').value = (s.phones && s.phones.person) || (s.angels && s.angels.person) || ''
    $('joinText').textContent = device
      ? `This phone will send alarms for ${by}, to the same servers and angels as ${by}'s other phones.`
      : more
        ? `${by}'s server offers to watch over you as well. It gets your alarms and check-ins and sends alarms to your angels, next to the servers you have now. It sees your alarms.`
        : `${by} invites you to Sync. ${by}'s servers will watch over you and send your alarms to the angels you choose; those servers see your alarms. Nobody can follow where you are: your location goes along only with an alarm.`
    const install = iosBrowser()
    $('joinHere').hidden = install
    $('joinInstall').hidden = !install
    $('joinList').textContent = ''
    $('joinError').textContent = ''
  }

  /** Enrol using a secret (invite link, phone link, server offer, or courtesy open signup). */
  async function enroll(
    s,
    join,
    ui = { nameEl: $('joinName'), btnEl: $('joinGo'), listEl: $('joinList'), errEl: $('joinError') },
  ) {
    const name = ui.nameEl.value.trim()
    const device = join.t === 'device'
    const courtesy = join.t === 'courtesy'
    if (!name) {
      ui.errEl.textContent = device ? 'Fill in a name for this phone.' : 'Fill in your name.'
      return null
    }
    ui.errEl.textContent = ''
    ui.btnEl.disabled = true
    const lines = {}
    const list = ui.listEl
    list.textContent = ''
    for (const k of join.watchers) {
      const line = document.createElement('div')
      line.textContent = `${courtesy ? COURTESY_WATCHER.name : shortKey(k)}: asking…`
      list.append(line)
      lines[k] = line
    }
    const hash = secretHash(join.secret)
    const message = { type: 'enroll', secret: join.secret, name, device: s.device }
    const answers = await ask(hexToBytes(s.sk), join.watchers, join.relays, message, hash, (k, ms, p) => {
      const label = (typeof p.watcher === 'string' && p.watcher.trim()) || (courtesy ? COURTESY_WATCHER.name : shortKey(k))
      lines[k].textContent = p.type === 'watching' ? `${label}: watching you` : `${label}: no (${p.reason})`
      lines[k].style.color = p.type === 'watching' ? '#3fb950' : '#e5534b'
    })
    ui.btnEl.disabled = false
    for (const k of join.watchers) if (!(k in answers)) lines[k].textContent = `${courtesy ? COURTESY_WATCHER.name : shortKey(k)}: no answer`
    const accepted = join.watchers.filter((k) => answers[k] && answers[k].payload.type === 'watching')
    if (!accepted.length) {
      ui.errEl.textContent = courtesy
        ? 'Could not reach the starter server yet. Try again in a moment.'
        : device
          ? 'No server accepted this phone. Make a new link.'
          : 'No server accepted the invite. Ask for a new link.'
      return null
    }
    const t = Math.floor(Date.now() / 1000)
    // One more server for someone who already uses Sync: next to the ones they have. Their angel
    // link is made again, so it includes the new server.
    const more = join.t === 'offer' && s.watchers && s.watchers.length > 0
    // A new phone of the same person: their servers, under the name chosen for this phone.
    const next = device
      ? { ...s, watchers: accepted, relays: join.relays, device: name, watching: {} }
      : more
        ? {
          ...s,
          watchers: [...new Set([...s.watchers, ...accepted])],
          relays: [...new Set([...s.relays, ...join.relays])],
          watching: { ...(s.watching || {}) },
          angelLink: null,
          added: accepted.length,
        }
        : courtesy
          ? { ...s, watchers: accepted, relays: join.relays, courtesy: true, guideDismissed: false, watching: {} }
          : { ...s, watchers: accepted, relays: join.relays, invitedBy: join.by || 'the one who invited you', watching: {} }
    for (const k of accepted) {
      next.watching[k] = { at: t, interval: answers[k].payload.interval_secs || 3600 }
      noteWatcher(next, answers[k].payload, t, k)
    }
    if (courtesy && accepted.includes(COURTESY_HEX)) {
      if (!next.serverLabels || !next.serverLabels[COURTESY_HEX]) {
        next.serverLabels = { ...(next.serverLabels || {}), [COURTESY_HEX]: COURTESY_WATCHER.name }
      }
      next.assignedName = answers[COURTESY_HEX].payload.person || name
    }
    next.checkedAt = Date.now()
    return next
  }

  /** Protect someone: register an invite with my watchers, then show its link. */
  async function protect(s) {
    $('protectGo').disabled = true
    $('protectShareBox').hidden = true
    const list = $('protectList')
    list.textContent = 'Asking your servers…'
    const by = (s.angels && s.angels.person) || (s.phones && s.phones.person) || s.person || ''
    const angel = await prepareAngel(by)
    const secret = newInviteSecret()
    const hash = secretHash(secret)
    const expires = Math.floor(Date.now() / 1000) + PROTECT_DAYS * 86400
    const msg = { type: 'invite_created', hash, expires, angel_key: angel.pubkey }
    if (angel.push) msg.push = angel.push
    const answers = await ask(hexToBytes(s.sk), s.watchers, s.relays, msg, hash)
    $('protectGo').disabled = false
    const ok = s.watchers.filter((k) => answers[k] && answers[k].payload.type === 'invite_ok')
    list.textContent = ''
    for (const k of s.watchers) {
      if (ok.includes(k)) continue
      const line = document.createElement('div')
      line.textContent = `${serverName(s, k)}: ${answers[k] ? 'no (' + answers[k].payload.reason + ')' : 'no answer: not in this link'}`
      list.append(line)
    }
    if (!ok.length) {
      list.prepend('No server accepted, so there is no link. ')
      return
    }
    // The link holds only the secret; the details wait on the relays, encrypted to it.
    try {
      await deliver([inviteEvent(secret, { by, watchers: ok, relays: s.relays }, expires)], allRelays(s))
    } catch (e) {
      list.prepend(`Could not store the invite: ${e.message}. `)
      return
    }
    s.protectLink = joinUrl(location.href.split('#')[0], secret)
    save(s)
    sharePanel($('protectShareBox'), { link: s.protectLink, text: PROTECT_TEXT, note: 'For one person, valid for 7 days.' })
  }

  function position() {
    return new Promise((resolve) => {
      if (!navigator.geolocation) return resolve(null)
      navigator.geolocation.getCurrentPosition(
        (p) => {
          const { latitude: la, longitude: lo, accuracy } = p.coords
          const lat = la.toFixed(5)
          const lon = lo.toFixed(5)
          resolve(`https://www.openstreetmap.org/?mlat=${lat}&mlon=${lon}#map=17/${lat}/${lon} (±${Math.round(accuracy)} m)`)
        },
        () => resolve(null),
        // Never wait long: the alarm matters more than the location.
        { enableHighAccuracy: true, timeout: 3000, maximumAge: 60000 },
      )
    })
  }

  async function send(s, trigger) {
    status('…')
    try {
      const location = s.useLocation ? await position() : null
      const wraps = buildDuress(hexToBytes(s.sk), s.watchers, { device: s.device, trigger, location })
      const r = await deliver(wraps, s.relays)
      status(`OK ${r.ms} ms`)
    } catch (e) {
      status('Not sent: ' + e.message)
    }
  }

  /** Send one Envelope message (session_start / session_ok) to the watchers. */
  async function sendMessage(s, message) {
    const r = await deliver(buildWraps(hexToBytes(s.sk), s.watchers, messagePayload(message)), s.relays)
    return r.ms
  }

  /** Timer: the watchers keep the clock, so it runs on even if this phone is taken or off. */
  async function startTimer(s, minutes) {
    const session = newSession()
    const until = Date.now() + minutes * 60000
    say('Starting the timer…')
    try {
      const location = s.useLocation ? await position() : null
      const context = { device: s.device, trigger: 'walk_home' }
      if (location) context.location = location
      await sendMessage(s, { type: 'session_start', session, timeout_secs: minutes * 60, context })
      s.timer = { session, until }
      save(s)
      say('Timer started: tap Done when you are safe')
    } catch (e) {
      say('Not started: ' + e.message, { ms: 5000 })
    }
    showTimer(s)
  }

  async function stopTimer(s) {
    if (!s.timer) return
    try {
      await sendMessage(s, { type: 'session_ok', session: s.timer.session })
      s.timer = null
      save(s)
      say('Timer stopped')
    } catch (e) {
      say('Not sent: ' + e.message, { ms: 5000 })
    }
    showTimer(s)
  }

  /** More time: close the running clock and start a new one that ends later. */
  async function extendTimer(s) {
    if (!s.timer) return
    const minutes = Math.max(0, Math.ceil((s.timer.until - Date.now()) / 60000)) + EXTEND_MIN
    const old = s.timer.session
    await startTimer(s, minutes)
    if (s.timer && s.timer.session !== old) {
      try {
        await sendMessage(s, { type: 'session_ok', session: old })
      } catch { }
    }
  }

  function showTimer(s) {
    const running = !!s.timer
    $('timerStart').hidden = running
    $('timerRun').hidden = !running
    // A running timer stays visible on the start screen: forgetting it means an alarm.
    $('homeTimer').hidden = !running
    if (running) {
      const t = new Date(s.timer.until)
      const hhmm = t.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
      const text = (Date.now() > s.timer.until ? 'Timer ended ' : 'Timer until ') + hhmm
      $('timerText').textContent = text
      $('homeTimerText').textContent = text
    }
  }

  /** Send one harmless test alert to every active angel, with a short cooldown. */
  async function sendTestAlarm(s) {
    if (!confirm('Send a harmless test alert to your active angels?')) return
    const btns = [$('angelTest'), $('guideTestBtn')].filter(Boolean)
    for (const b of btns) b.disabled = true
    try {
      await sendMessage(s, { type: 'test', context: { device: s.device, trigger: 'test_app' } })
      s.testedAt = Date.now()
      save(s)
      say('Test alarm sent — check with your angel that it arrived.', { ms: 5000 })
      setTimeout(() => {
        for (const b of btns) b.disabled = false
      }, 10000)
    } catch (e) {
      for (const b of btns) b.disabled = false
      say('Not sent: ' + e.message, { ms: 5000 })
    }
  }

  let deferredInstallPrompt = null
  window.addEventListener('beforeinstallprompt', (e) => {
    e.preventDefault()
    deferredInstallPrompt = e
    const btn = $('androidInstallBtn')
    if (btn) btn.hidden = false
  })
  window.addEventListener('appinstalled', () => {
    deferredInstallPrompt = null
    const btn = $('androidInstallBtn')
    if (btn) btn.hidden = true
    if ($('installCard')) $('installCard').hidden = true
    say('Sync installed')
  })

  /** Settings: "iPhone shortcuts" card status and buttons. */
  function showShortcuts(s, focused = false) {
    const visible = !!(s && s.watchers && s.watchers.length) && !isAndroid() && !focused
    $('gShortcuts').hidden = !visible
    if (!visible) return

    const url = topicUrl(s)
    const pub = isPublicNtfyUrl(url)
    const synced = isTopicSynced(s)
    const partial = !!(s.topicSynced && s.topicSynced.url === url && !synced)
    const dot = $('shortcutStatusDot')
    const txt = $('shortcutStatusText')
    const sub = $('shortcutStatusSub')
    const syncBtn = $('syncTopicBtn')
    const offBtn = $('disableTopicBtn')

    if (!pub) {
      dot.className = 'state warn'
      txt.textContent = 'Custom ntfy server'
      sub.textContent = "Add this topic manually in your servers' config.toml (duress_ntfy)."
      syncBtn.hidden = true
      offBtn.hidden = true
    } else if (synced) {
      dot.className = 'state ok'
      txt.textContent = 'Active on your servers'
      sub.textContent = 'Your servers follow this secret topic automatically (20 s cancel window on Duress).'
      syncBtn.hidden = false
      syncBtn.textContent = 'Re-sync topic'
      offBtn.hidden = false
    } else if (partial) {
      dot.className = 'state warn'
      txt.textContent = 'Not synced to all servers yet'
      sub.textContent = 'You added a server or changed the topic. Tap Sync topic to update all servers.'
      syncBtn.hidden = false
      syncBtn.textContent = 'Sync topic to servers'
      offBtn.hidden = false
    } else {
      dot.className = 'state'
      txt.textContent = 'Not activated on servers yet'
      sub.textContent = 'Automatically activates when you tap a shortcut button below, or tap Sync topic now.'
      syncBtn.hidden = false
      syncBtn.textContent = 'Sync topic to servers'
      offBtn.hidden = true
    }
  }

  /** Register (or clear) the public ntfy.sh topic on every configured server. */
  async function syncTopic(s, { quiet = false, clear = false } = {}) {
    if (!s || !s.watchers || !s.watchers.length) return false
    const url = clear ? '' : topicUrl(s)
    if (!clear && !isPublicNtfyUrl(url)) {
      if (!quiet) say('Custom ntfy servers are configured in config.toml', { ms: 4500 })
      return false
    }
    const btns = [$('syncTopicBtn'), $('disableTopicBtn')].filter(Boolean)
    for (const b of btns) b.disabled = true
    try {
      await sendMessage(s, {
        type: 'topic_changed',
        url,
        device: s.device,
        cancel_seconds: 20,
      })
      s.topicSynced = clear ? null : { url, watchers: [...s.watchers] }
      save(s)
      showShortcuts(s)
      if (!quiet) say(clear ? 'Shortcut topic turned off on your servers' : 'Shortcut topic synced to your servers')
      return true
    } catch (e) {
      if (!quiet) say('Not synced: ' + e.message, { ms: 5000 })
      return false
    } finally {
      for (const b of btns) b.disabled = false
    }
  }

  /** Focus Settings on server + first angel until the first angel is active (or expanded). */
  function updateSetupFocus(s) {
    const ready = !!(s && s.watchers && s.watchers.length)
    const activeAngels = ((s && s.angels && s.angels.list) || []).filter((a) => a.active).length
    const focused = ready && !s.showAllSettings && !!(s.angels && activeAngels === 0)
    for (const id of ['gStatus', 'gAngels']) $(id).hidden = !ready
    for (const id of ['gTimer', 'gDevices', 'gInvite', 'gOptions']) $(id).hidden = !ready || focused
    $('moreSettingsCard').hidden = !focused
    showShortcuts(s, focused)
  }

  function fillSetup(s) {
    const ready = s.watchers.length > 0
    $('npub').textContent = npubOf(hexToBytes(s.sk))
    $('device').value = s.device
    $('watchers').value = s.watchers.join('\n')
    $('relays').value = s.relays.join('\n')
    $('autoSend').checked = s.autoSend
    $('useLocation').checked = s.useLocation
    $('topic').value = topicUrl(s)
    $('version').textContent = `Version ${APP_VERSION}`
    $('setupBack').hidden = !(ready || hasAngel())
    $('setupError').textContent = ''
    $('firstRun').hidden = ready
    if (!ready) {
      const mobile = isMobile()
      $('desktopCard').hidden = mobile
      if (!mobile) drawQr($('desktopQr'), location.href.split('#')[0])
      $('installCard').hidden = !mobile || isStandalone()
      $('installIos').hidden = !iosBrowser()
      $('installAndroid').hidden = !isAndroid() || isStandalone()
      $('androidInstallBtn').hidden = !deferredInstallPrompt
      $('courtesyCard').hidden = iosBrowser()
      $('courtesyError').textContent = ''
      $('courtesyList').textContent = ''
    }
    ;(ready ? $('pasteSlot') : $('firstRun')).append($('pasteCard'))
    $('angelCard').hidden = !(s.angels && s.angels.list.length === 0) && !$('angelCard').dataset.open
    showInvite(s)
    showAngels(s)
    showPhones(s)
    showJoined(s)
    showTimer(s)
    showWatchingSummary(s)
    updateSetupFocus(s)
  }

  /** Servers and relays as typed under Advanced. */
  function readServers() {
    const lines = (id) => $(id).value.split(/[\s,]+/).filter(Boolean)
    const watchers = [
      ...new Set(
        lines('watchers').map((w) => {
          try {
            return parsePubkey(w)
          } catch {
            throw new Error(`not a server key: ${w.slice(0, 24)}`)
          }
        }),
      ),
    ]
    const relays = lines('relays').filter((r) => r.startsWith('wss://'))
    if (!watchers.length) throw new Error('add at least one server key')
    if (!relays.length) throw new Error('add at least one relay (wss://…)')
    return { watchers, relays }
  }

  /** Sends after the button is held, so a pocket touch does nothing. */
  function holdButton(el, ms, action) {
    let timer = null
    const start = (e) => {
      e.preventDefault()
      el.classList.add('holding')
      timer = setTimeout(() => {
        el.classList.remove('holding')
        action()
      }, ms)
    }
    const cancel = () => {
      clearTimeout(timer)
      el.classList.remove('holding')
    }
    el.addEventListener('pointerdown', start)
    for (const ev of ['pointerup', 'pointerleave', 'pointercancel']) el.addEventListener(ev, cancel)
    el.addEventListener('contextmenu', (e) => e.preventDefault())
  }

  function main() {
    let s = load()
    const configured = s && s.watchers && s.watchers.length
    let angelBackTo = 'main'
    const openAngel = (from = 'main', scrollTop = false) => {
      angelBackTo = from
      showAngel(show, '', say, { scrollTop })
    }

    // Back: everything is saved already; removals waiting for Undo go out now.
    $('setupBack').onclick = () => {
      flushAll()
      $('pasteError').textContent = ''
      if (s.watchers.length) {
        show('main')
        showTimer(s)
        updateDot(s)
      } else if (hasAngel()) {
        openAngel('main', true)
      }
    }
    $('angelBack').onclick = () => {
      if (location.hash === '#angel') history.replaceState(null, '', location.pathname)
      if (angelBackTo === 'setup') {
        fillSetup(s)
        show('setup')
        window.scrollTo(0, 0)
        if (s.watchers.length && !$('watchList').children.length) liveCheck(s)
      } else if (s && s.watchers && s.watchers.length) {
        show('main')
        showTimer(s)
        updateDot(s)
      }
    }
    document.addEventListener('visibilitychange', () => {
      if (document.visibilityState === 'hidden') flushAll()
      else if (document.visibilityState === 'visible' && !$('angel').hidden) showAngel(show, '', say)
    })
    window.addEventListener('scroll', () => {
      const sc = window.scrollY > 4
      $('setupBar').classList.toggle('scrolled', sc)
      $('angelBar').classList.toggle('scrolled', sc)
    }, { passive: true })
    $('openSetup').onclick = () => {
      fillSetup(s)
      $('advanced').open = false
      show('setup')
      window.scrollTo(0, 0)
      liveCheck(s)
    }
    $('checkNow').onclick = () => liveCheck(s)
    $('homeTimerDone').onclick = () => stopTimer(s)
    const openAngelInvite = () => {
      $('angelCard').dataset.open = '1'
      $('angelCard').hidden = false
      showInvite(s)
      $('angelCard').scrollIntoView({ behavior: 'smooth', block: 'start' })
    }
    $('angelInvite').onclick = openAngelInvite
    $('guideInviteBtn').onclick = openAngelInvite
    $('angelTest').onclick = () => sendTestAlarm(s)
    $('guideTestBtn').onclick = () => sendTestAlarm(s)
    $('guideDismiss').onclick = () => {
      s.guideDismissed = true
      save(s)
      showGuide(s)
    }
    $('androidInstallBtn').onclick = async () => {
      if (!deferredInstallPrompt) return
      deferredInstallPrompt.prompt()
      try {
        await deferredInstallPrompt.userChoice
      } catch { }
      deferredInstallPrompt = null
      $('androidInstallBtn').hidden = true
    }
    $('courtesyGo').onclick = async () => {
      const typedName = $('courtesyName').value.trim()
      const next = await enroll(
        s,
        {
          secret: 'open:' + newInviteSecret(),
          watchers: [COURTESY_HEX],
          relays: COURTESY_WATCHER.relays,
          by: '',
          t: 'courtesy',
        },
        {
          nameEl: $('courtesyName'),
          btnEl: $('courtesyGo'),
          listEl: $('courtesyList'),
          errEl: $('courtesyError'),
        },
      )
      if (!next) return
      const assigned = next.assignedName || typedName
      delete next.assignedName
      s = next
      save(s)
      fillSetup(s)
      show('setup')
      window.scrollTo(0, 0)
      liveCheck(s)
      ensureAngelLink(s).catch(() => { })
      if (assigned && assigned.toLowerCase() !== typedName.toLowerCase()) {
        say(`Connected as ${assigned} (that name was taken): next invite your first angel`, { ms: 6000 })
      } else {
        say('Connected! Next: invite your first angel below.', { ms: 4000 })
      }
    }
    // Options and the device name: saved as soon as they change.
    for (const id of ['useLocation', 'autoSend']) {
      $(id).onchange = () => {
        s[id] = $(id).checked
        save(s)
        say('Saved')
      }
    }
    $('device').onchange = () => {
      s.device = $('device').value.trim() || 'phone'
      $('device').value = s.device
      save(s)
      say('Saved')
    }
    $('topic').onchange = () => {
      try {
        s.topic = parseTopic($('topic').value)
        save(s)
        updateSetupFocus(s)
        say('Saved')
      } catch (e) {
        say(e.message, { ms: 5000 })
      }
    }
    $('showAllSettingsBtn').onclick = () => {
      s.showAllSettings = true
      save(s)
      updateSetupFocus(s)
    }
    // Servers and relays: a typing error here cuts off every alarm, so only on Apply.
    $('applyServers').onclick = () => {
      try {
        const { watchers, relays } = readServers()
        const gone = s.watchers.filter((k) => !watchers.includes(k)).length
        if (gone && !confirm(`Remove ${gone} server${gone > 1 ? 's' : ''}? Alarms then go only to the ${watchers.length} left.`)) return
        const changedWatchers = watchers.length !== s.watchers.length || watchers.some((k, idx) => s.watchers[idx] !== k)
        s = { ...s, watchers, relays }
        if (!watchers.includes(COURTESY_HEX)) s.courtesy = false
        if (changedWatchers) s.angelLink = null
        s.watching = mergeWatching(s, {})
        save(s)
        $('setupError').textContent = ''
        fillSetup(s)
        say('Servers saved')
        liveCheck(s)
      } catch (e) {
        $('setupError').textContent = e.message
      }
    }
    $('protectGo').onclick = () => protect(s)
    $('phoneGo').onclick = () => addPhone(s)
    let join = null
    const useJoin = async (j) => {
      join = null
      startJoin(s, j)
      if (j.short) {
        $('joinText').textContent = 'Looking up the invite…'
        $('joinGo').disabled = true
        const full = await fetchInvite(j.secret, allRelays(s))
        if (!full) {
          $('joinText').textContent = 'This invite was not found or has expired. Ask for a new link.'
          $('joinHere').hidden = true
          $('joinInstall').hidden = true
          return
        }
        j = full
        startJoin(s, j)
        $('joinGo').disabled = false
      }
      join = j
    }
    $('joinGo').onclick = async () => {
      if (!join) return
      const next = await enroll(s, join)
      if (!next) return
      const added = next.added
      delete next.added
      s = next
      save(s)
      history.replaceState(null, '', location.pathname)
      fillSetup(s)
      show('setup')
      liveCheck(s)
      ensureAngelLink(s).catch(() => { })
      if (added) say('Server added. It does not know your angels yet: send them your angel link again.', { ms: 8000 })
    }
    $('joinLater').onclick = () => {
      history.replaceState(null, '', location.pathname)
      fillSetup(s)
      show('setup')
    }
    $('joinCopy').onclick = async () => {
      try {
        await navigator.clipboard.writeText(location.href)
        $('joinError').textContent = 'Copied.'
      } catch {
        $('joinError').textContent = 'Could not copy: copy the address from the address bar.'
      }
    }
    $('pastePaste').onclick = async () => {
      try {
        $('pasteText').value = await navigator.clipboard.readText()
      } catch {
        $('pasteError').textContent = 'Could not read the clipboard: paste into the field.'
      }
    }
    $('pasteGo').onclick = () => {
      const j = parseJoin($('pasteText').value)
      if (!j) {
        $('pasteError').textContent = 'This is not an invite link.'
        return
      }
      $('pasteError').textContent = ''
      if (j.angel) startAngelJoin(j, show, say)
      else useJoin(j)
    }
    $('angelUseApp').onclick = () => {
      s.angelMode = 'app'
      save(s)
      showInvite(s)
    }
    $('angelUseTg').onclick = () => {
      s.angelMode = 'tg'
      save(s)
      showInvite(s)
    }
    $('beAngelOpen').onclick = () => openAngel('setup', true)
    $('angelRefresh').onclick = () => showAngel(show, '', say)
    $('angelSettings').onclick = () => {
      if (location.hash === '#angel') history.replaceState(null, '', location.pathname)
      s = s || freshState()
      fillSetup(s)
      $('advanced').open = false
      show('setup')
      window.scrollTo(0, 0)
      liveCheck(s)
    }
    $('newKey').onclick = () => {
      const typed = prompt(
        'A new key cuts this phone off from all your servers until each of them gets the new key. Your angels and timer stop working from this phone.\n\nType "replace" to go ahead.',
      )
      if ((typed || '').trim().toLowerCase() !== 'replace') return
      s = { ...s, sk: freshState().sk }
      save(s)
      fillSetup(s)
      say('New key made: give it to your servers', { ms: 5000 })
    }
    // Copy what is in the field (it may be edited but not saved yet).
    const current = () => {
      try {
        return { ...s, topic: parseTopic($('topic').value) }
      } catch (e) {
        say(e.message, { ms: 5000 })
        return null
      }
    }
    $('copyTopic').onclick = () => {
      const c = current()
      if (c) copyTopic(c)
    }
    $('syncTopicBtn').onclick = () => {
      const c = current()
      if (!c) return
      if (c.topic !== s.topic) {
        s.topic = c.topic
        save(s)
      }
      syncTopic(s)
    }
    $('disableTopicBtn').onclick = async () => {
      if (confirm('Stop your servers from following this shortcut topic?')) {
        await syncTopic(s, { clear: true })
      }
    }
    for (const [id, link] of [['addDuress', SHORTCUT_LINKS.duress], ['addOk', SHORTCUT_LINKS.ok], ['addDelay', SHORTCUT_LINKS.delay]]) {
      $(id).hidden = !link
      $(id).onclick = async () => {
        const c = current()
        if (!c) return
        // Copy FIRST while the iOS user-gesture token is fresh.
        const copied = await copyTopic(c)
        if (!copied) return
        if (c.topic !== s.topic) {
          s.topic = c.topic
          save(s)
        }
        if (isPublicNtfyUrl(topicUrl(s)) && !isTopicSynced(s)) {
          await Promise.race([
            syncTopic(s, { quiet: true }),
            new Promise((resolve) => setTimeout(resolve, 3500)),
          ])
        }
        location.href = link
      }
    }
    holdButton($('sync'), 1500, () => send(s, 'duress_app'))
    for (const b of document.querySelectorAll('[data-min]')) b.onclick = () => startTimer(s, Number(b.dataset.min))
    $('timerDone').onclick = () => stopTimer(s)
    $('timerMore').onclick = () => extendTimer(s)
    setInterval(() => s && showTimer(s), 30000)

    // Older versions had no topic or timer.
    if (s && !s.topic) {
      s.topic = newTopic()
      save(s)
    }
    const linkJoin = parseJoin(location.hash)
    // A notification tap, or the app opened from one: the angel screen (never the VIP's
    // auto-send). Also when this phone is only an angel.
    window.addEventListener('hashchange', () => location.hash === '#angel' && openAngel('main', true))
    if ('serviceWorker' in navigator) navigator.serviceWorker.addEventListener('message', (e) => e.data && e.data.open === 'angel' && openAngel('main', true))
    if (linkJoin && linkJoin.angel) {
      startAngelJoin(linkJoin, show, say)
      return
    }
    if (location.hash === '#angel' || (!configured && hasAngel() && !linkJoin)) {
      openAngel('main', true)
      return
    }
    // A server offered to a phone that already uses Sync: add it to the ones it has.
    if (linkJoin && linkJoin.offer && configured) {
      useJoin(linkJoin)
      return
    }
    if (linkJoin && configured) {
      history.replaceState(null, '', location.pathname)
    }
    if (!configured) {
      s = s || freshState()
      save(s)
      if (linkJoin) {
        useJoin(linkJoin)
        return
      }
      fillSetup(s)
      // First run: most people come with an invite (paste it at the top); setting up servers by
      // hand is under Advanced.
      show('setup')
      return
    }
    show('main')
    if (linkJoin)
      status(
        linkJoin.device
          ? 'This phone already uses Sync: open the link on the new phone instead.'
          : 'This phone already uses Sync: the invite was not used.',
      )
    showTimer(s)
    showJoined(s)
    checkWatching(s)
    document.addEventListener('visibilitychange', () => document.visibilityState === 'visible' && checkWatching(s))
    const autoSend = () => {
      if (!s.autoSend || $('main').hidden || document.visibilityState !== 'visible') return
      if (Date.now() - (s.lastAuto || 0) <= AUTO_SEND_QUIET_MS) return
      s.lastAuto = Date.now()
      save(s)
      send(s, 'duress_app_open')
    }
    autoSend()
    // iOS keeps the app in memory: tapping its icon usually resumes it without a reload, so
    // "opening" must also count when the app comes back on screen.
    document.addEventListener('visibilitychange', autoSend)
    window.addEventListener('pageshow', (e) => e.persisted && autoSend())
  }

  main()

  // Offline cache (see sw.js). Only works over HTTPS; failures are harmless. When a new
  // version takes over, reload once so the new code runs.
  if ('serviceWorker' in navigator) {
    const hadController = !!navigator.serviceWorker.controller
    let reloaded = false
    navigator.serviceWorker.addEventListener('controllerchange', () => {
      if (hadController && !reloaded) {
        reloaded = true
        location.reload()
      }
    })
    navigator.serviceWorker.register('sw.js', { updateViaCache: 'none' }).catch(() => { })
  }
