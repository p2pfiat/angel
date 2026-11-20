// Angel mode: this phone gets a VIP's alarms (design.md, "Angels with their own key"). The
// angel has their own key (Nostr) and push key (VAPID), kept in this browser's storage apart
// from the VIP state, so one phone can be both.

import {
  DEFAULT_RELAYS,
  ask,
  buildWraps,
  deliver,
  fetchAngelMessages,
  fetchInvite,
  messagePayload,
  parseJoin,
  newSecretKey,
  newVapidKeys,
  secretHash,
  subscribePush,
} from './core.js'
import { npubEncode } from 'nostr-tools/nip19'
import { getPublicKey } from 'nostr-tools/pure'

const KEY = 'w.angel'
const $ = (id) => document.getElementById(id)
const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')
const bytes = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16))

export function loadAngel() {
  try {
    return JSON.parse(localStorage.getItem(KEY)) || null
  } catch {
    return null
  }
}

function saveAngel(a) {
  localStorage.setItem(KEY, JSON.stringify(a))
}

/** Whether this phone is an angel for anyone. */
export function hasAngel() {
  const a = loadAngel()
  return !!(a && a.vips && Object.keys(a.vips).length)
}

function iosBrowser() {
  const standalone = navigator.standalone || matchMedia('(display-mode: standalone)').matches
  return /iPhone|iPad|iPod/.test(navigator.userAgent) && !standalone
}

function shortKey(k) {
  const n = npubEncode(k)
  return `${n.slice(0, 12)}…${n.slice(-6)}`
}

function loadVipState() {
  try {
    return JSON.parse(localStorage.getItem('w.state')) || null
  } catch {
    return null
  }
}

function relaysOf(a) {
  const all = new Set(DEFAULT_RELAYS)
  for (const v of Object.values(a.vips || {})) for (const r of v.relays) all.add(r)
  const s = loadVipState()
  if (s && Array.isArray(s.relays)) for (const r of s.relays) all.add(r)
  return [...all]
}

/** Notifications on, and this phone subscribed: the `push` part of angel_join, or a reason. */
async function pushFor(a) {
  try {
    if (typeof Notification === 'undefined') return { why: 'This browser cannot show notifications. On an iPhone: open Sync from the Home Screen.' }
    const perm = await Notification.requestPermission()
    if (perm !== 'granted') return { why: 'Notifications are off: you see alarms only when you open Sync.' }
    return { push: await subscribePush(a.vapid) }
  } catch (e) {
    return { why: `No notifications on this phone (${e.message}). You see alarms when you open Sync.` }
  }
}

/** Ensure this phone has an angel key and (when possible) a Web Push subscription. */
export async function prepareAngel(defaultName = '') {
  const a = loadAngel() || { sk: hex(newSecretKey()), vapid: await newVapidKeys(), name: defaultName || '', vips: {}, acked: {} }
  if (!a.name && defaultName) a.name = defaultName
  const { push, why } = await pushFor(a)
  if (push) a.push = push
  saveAngel(a)
  return { pubkey: getPublicKey(bytes(a.sk)), push: push || a.push || null, why }
}

/** Register an invited VIP on this phone's angel state and sync push details with their watchers. */
export async function addInvitedVipToAngel(person, watchers, relays, secret, name) {
  if (!person || !watchers || !watchers.length) return
  const a = loadAngel() || { sk: hex(newSecretKey()), vapid: await newVapidKeys(), name: name || '', vips: {}, acked: {} }
  if (!a.name && name) a.name = name
  const existing = a.vips[person]
  a.vips[person] = {
    watchers,
    relays: relays && relays.length ? relays : DEFAULT_RELAYS,
    secret: secret || (existing && existing.secret) || '',
    at: Math.floor(Date.now() / 1000),
  }
  saveAngel(a)
  if (secret) {
    let push = a.push || null
    if (!push && typeof Notification !== 'undefined' && Notification.permission === 'granted') {
      try {
        push = await subscribePush(a.vapid)
        a.push = push
        saveAngel(a)
      } catch { }
    }
    join(a, { watchers, relays: a.vips[person].relays }, secret, a.name || 'Angel', push).catch(() => { })
  }
}

/** Send angel_join to a VIP's watchers; returns { ok: [watcher], person, answers }. */
async function join(a, info, secret, name, push, onLine = () => { }) {
  const hash = secretHash(secret)
  const msg = { type: 'angel_join', secret, name }
  if (push) msg.push = push
  const answers = await ask(bytes(a.sk), info.watchers, info.relays, msg, hash, (k, ms, p) =>
    onLine(k, p.type === 'angel_ok' ? 'in' : `no (${p.reason})`, p.type === 'angel_ok'),
  )
  const ok = info.watchers.filter((k) => answers[k] && answers[k].payload.type === 'angel_ok')
  // A link to be protected too (the watchers offer it to the angels of their own VIPs).
  const invite = ok.map((k) => answers[k].payload.invite).find((l) => typeof l === 'string' && parseJoin(l))
  return { ok, person: ok.length ? answers[ok[0]].payload.person : null, invite: invite || null, answers }
}

/** The screen for an angel link (`#a=`). */
export async function startAngelJoin(link, show, say = () => {}) {
  show('angelJoin')
  $('ajText').textContent = 'Looking up the invite…'
  $('ajReassure').hidden = true
  $('ajForm').hidden = true
  $('ajInstall').hidden = true
  $('ajError').textContent = ''
  const a0 = loadAngel()
  const info = await fetchInvite(link.secret, [...new Set([...DEFAULT_RELAYS, ...((a0 && relaysOf(a0)) || [])])])
  if (!info || info.t !== 'angel') {
    $('ajText').textContent = 'This invite was not found or has expired. Ask for a new link.'
    return
  }
  const by = info.by || 'Someone'
  $('ajText').textContent =
    `${by} trusts you to be one of their angels. If ${by} ever needs urgent help, you'll get an alert here with their location.`
  $('ajReassure').hidden = false
  if (iosBrowser()) {
    $('ajInstall').hidden = false
    $('ajCopy').onclick = async () => {
      try {
        await navigator.clipboard.writeText(location.href)
        $('ajError').textContent = 'Copied.'
      } catch {
        $('ajError').textContent = 'Could not copy: copy the address from the address bar.'
      }
    }
    return
  }
  $('ajForm').hidden = false
  $('ajName').value = (a0 && a0.name) || ''
  $('ajGo').onclick = async () => {
    const name = $('ajName').value.trim()
    if (!name) {
      $('ajError').textContent = 'Fill in your name.'
      return
    }
    $('ajGo').disabled = true
    $('ajError').textContent = ''
    const a = loadAngel() || { sk: hex(newSecretKey()), vapid: await newVapidKeys(), vips: {}, acked: {} }
    const { push, why } = await pushFor(a)
    const list = $('ajList')
    list.textContent = ''
    const lines = {}
    for (const k of info.watchers) {
      const line = document.createElement('div')
      line.textContent = `${shortKey(k)}: asking…`
      list.append(line)
      lines[k] = line
    }
    const r = await join(a, info, link.secret, name, push, (k, text, good) => {
      lines[k].textContent = `${shortKey(k)}: ${text}`
      lines[k].style.color = good ? '#3fb950' : '#e5534b'
    })
    $('ajGo').disabled = false
    if (!r.ok.length) {
      $('ajError').textContent = 'No server accepted. Ask for a new link.'
      return
    }
    a.name = name
    if (push) a.push = push
    const at = Math.floor(Date.now() / 1000)
    a.vips[r.person] = { watchers: r.ok, relays: info.relays, secret: link.secret, at }
    if (r.invite) a.protect = { link: r.invite, by: r.person, at }
    saveAngel(a)
    history.replaceState(null, '', location.pathname)
    showAngel(show, why || `You are now an angel for ${r.person}. A test alarm is on its way.`, say, { scrollTop: true })
  }
}

function ago(secs) {
  const d = Math.floor(Date.now() / 1000) - secs
  if (d < 60) return 'just now'
  if (d < 3600) return `${Math.round(d / 60)} min ago`
  if (d < 86400) return `${Math.round(d / 3600)} h ago`
  return `${Math.round(d / 86400)} days ago`
}

/** Tell the VIP's watchers "I am calling" or "Resolved". */
async function sendAck(a, m, action) {
  const v = a.vips[m.person]
  if (!v) throw new Error('not an angel for ' + m.person)
  await deliver(buildWraps(bytes(a.sk), v.watchers, messagePayload({ type: 'ack', alarm: m.id, action })), v.relays)
}

/** Whether this phone already sends its own alarms (is a VIP too). */
function isVip() {
  const s = loadVipState()
  return !!(s && s.watchers && s.watchers.length)
}

/** "Want protection yourself?": hidden if already protected or during an active emergency. */
function showProtect(a, hasActiveAlarm = false) {
  if (isVip() || hasActiveAlarm) {
    $('angelProtect').hidden = true
    return
  }
  const p = a.protect
  const hasInvite = !!(p && Date.now() / 1000 - p.at < 29 * 86400)
  $('angelProtect').hidden = false
  if (hasInvite) {
    $('angelProtectText').textContent = `${p.by}'s servers can watch over you too: one tap sends an alarm to the angels you choose.`
    $('angelProtectGo').textContent = 'Protect me too'
    $('angelProtectGo').onclick = () => {
      const j = parseJoin(p.link)
      if (!j) return
      // This app, wherever it runs: the invite screen for that link.
      location.replace(`${location.pathname}#j=${j.secret}`)
      location.reload()
    }
  } else {
    $('angelProtectText').textContent = 'Free, private safety alerts for yourself in 60 seconds — no account or server setup needed.'
    $('angelProtectGo').textContent = 'Set up my own safety'
    $('angelProtectGo').onclick = () => $('angelSettings').click()
  }
}

/** Extract map URL and phone tel: link from alarm text while keeping clean prose. */
export function parseAlarmDetails(text) {
  const src = String(text || '')
  const mapMatch = src.match(/https:\/\/www\.openstreetmap\.org\/\S+/)
  const mapUrl = mapMatch ? mapMatch[0] : null
  const phoneMatch = src.match(/Phone\s+[^:\n]+:\s*(\+?\d[\d\s()-]{5,18}\d)/i)
  const telHref = phoneMatch ? `tel:${phoneMatch[1].replace(/[^\d+]/g, '')}` : null
  const body = src
    .replace(/Location:\s*https:\/\/www\.openstreetmap\.org\/\S+\s*(\([^)]+\))?/gi, (_, acc) => (acc ? `Location: shared ${acc}` : ''))
    .replace(/https:\/\/www\.openstreetmap\.org\/\S+/g, '')
    .replace(/\n{3,}/g, '\n\n')
    .trim()
  return { body, mapUrl, telHref }
}

let loadSeq = 0

/** The angel screen: who I am an angel for, notifications, and the latest alarms. */
export async function showAngel(show, note = '', say = () => {}, { scrollTop = false } = {}) {
  const a = loadAngel()
  if (!a) return
  const seq = ++loadSeq
  show('angel')
  $('angelBack').hidden = !isVip()
  if (scrollTop) window.scrollTo(0, 0)
  if (note) say(note, { ms: 3500 })
  const names = Object.keys(a.vips)
  $('angelFor').textContent = names.length ? `You get alarms for ${names.join(', ')}.` : 'You are not an angel for anyone yet.'
  const perm = typeof Notification === 'undefined' ? 'unsupported' : Notification.permission
  $('angelNotifyRow').hidden = perm === 'granted' || perm === 'unsupported'
  $('angelNotifyDot').className = 'state ' + (perm === 'granted' ? 'ok' : perm === 'default' ? 'warn' : 'bad')
  $('angelNotifyState').textContent =
    perm === 'granted'
      ? 'Notifications are on.'
      : perm === 'unsupported'
        ? 'This browser cannot show notifications: open Sync from the Home Screen.'
        : 'Notifications are off: alarms reach you only when you open Sync.'
  $('angelNotify').onclick = async () => {
    const { push, why } = await pushFor(a)
    if (!push) {
      say(why, { ms: 4000 })
      return
    }
    a.push = push
    saveAngel(a)
    // Give every VIP's watchers the new subscription (joining again keeps everything else).
    for (const [person, v] of Object.entries(a.vips)) {
      if (!v.secret) continue
      await join(a, { watchers: v.watchers, relays: v.relays }, v.secret, a.name || 'Angel', push).catch(() => { })
      void person
    }
    showAngel(show, 'Notifications are on.', say)
  }
  showProtect(a, false)
  const box = $('angelAlarms')
  const refreshBtn = $('angelRefresh')
  if (box.dataset.loaded !== '1') box.textContent = 'Loading…'
  refreshBtn.disabled = true
  refreshBtn.textContent = 'Refreshing…'
  let msgs = []
  try {
    msgs = await fetchAngelMessages(bytes(a.sk), relaysOf(a))
  } catch { } finally {
    if (seq === loadSeq) {
      refreshBtn.disabled = false
      refreshBtn.textContent = 'Refresh'
    }
  }
  if (seq !== loadSeq) return
  box.dataset.loaded = '1'
  const vipState = loadVipState()
  const myWatchers = (vipState && vipState.watchers) || []
  let discovered = false
  for (const m of msgs) {
    if (!a.vips[m.person] && myWatchers.includes(m.from)) {
      a.vips[m.person] = {
        watchers: myWatchers,
        relays: vipState.relays && vipState.relays.length ? vipState.relays : DEFAULT_RELAYS,
        secret: '',
        at: m.at,
      }
      discovered = true
    }
  }
  if (discovered) {
    saveAngel(a)
    const updatedNames = Object.keys(a.vips)
    $('angelFor').textContent = updatedNames.length ? `You get alarms for ${updatedNames.join(', ')}.` : 'You are not an angel for anyone yet.'
  }
  // Only from the servers of a VIP I am an angel for.
  msgs = msgs.filter((m) => a.vips[m.person] && a.vips[m.person].watchers.includes(m.from))
  // The same alarm comes from several servers: show it once.
  const seen = new Set()
  msgs = msgs.filter((m) => !seen.has(m.id) && seen.add(m.id))
  const hasActiveAlarm = msgs.some((m) => (m.kind === 'duress' || m.kind === 'missed') && (a.acked || {})[m.id] !== 'done')
  showProtect(a, hasActiveAlarm)
  box.textContent = ''
  if (!msgs.length) {
    const empty = document.createElement('div')
    empty.className = 'card'
    const p = document.createElement('p')
    p.className = 'hint'
    p.textContent = 'No alarms. When one comes, it shows here.'
    empty.append(p)
    box.append(empty)
    return
  }
  for (const m of msgs.slice(0, 20)) {
    const ack = (a.acked || {})[m.id]
    const card = document.createElement('div')
    let badgeText = ''
    let badgeCls = ''
    if ((m.kind === 'duress' || m.kind === 'missed') && ack !== 'done') {
      card.className = `card ${m.kind === 'duress' ? 'alarm-duress' : 'alarm-missed'}`
      ;[badgeText, badgeCls] = m.kind === 'duress' ? ['Duress', 'bad'] : ['Missed', 'warn']
    } else if (ack === 'done') {
      card.className = 'card alarm-quiet'
      ;[badgeText, badgeCls] = ['Resolved', 'ok']
    } else if (m.kind === 'late_ok') {
      card.className = 'card'
      ;[badgeText, badgeCls] = ['Late OK', 'warn']
    } else {
      card.className = 'card alarm-quiet'
      ;[badgeText, badgeCls] = m.kind === 'test' ? ['Test', ''] : ['Notice', '']
    }
    const [title, ...rest] = m.text.split('\n')
    const head = document.createElement('div')
    head.className = 'alarmHead'
    const h = document.createElement('h2')
    h.textContent = title
    const badge = document.createElement('span')
    badge.className = `badge${badgeCls ? ' ' + badgeCls : ''}`
    badge.textContent = badgeText
    head.append(h, badge)
    const meta = document.createElement('div')
    meta.className = 'sub'
    meta.textContent = ago(m.at)
    card.append(head, meta)

    const { body: cleanBody, mapUrl, telHref } = parseAlarmDetails(rest.join('\n'))
    if (cleanBody) {
      const body = document.createElement('p')
      body.className = 'alarmText'
      body.style.marginTop = '10px'
      body.textContent = cleanBody
      card.append(body)
    }

    if (telHref || mapUrl) {
      const linkRow = document.createElement('div')
      linkRow.className = 'row3'
      linkRow.style.marginTop = '10px'
      if (telHref) {
        const call = document.createElement('a')
        call.className = 'btnLink'
        call.href = telHref
        call.textContent = `Call ${m.person}`
        linkRow.append(call)
      }
      if (mapUrl) {
        const map = document.createElement('a')
        map.className = 'btnLink'
        map.href = mapUrl
        map.target = '_blank'
        map.rel = 'noopener'
        map.textContent = 'Open map'
        linkRow.append(map)
      }
      card.append(linkRow)
    }

    if (m.kind === 'duress' || m.kind === 'missed') {
      const sendAction = async (action, btn) => {
        btn.disabled = true
        try {
          await sendAck(a, m, action)
          a.acked = { ...(a.acked || {}), [m.id]: action }
          saveAngel(a)
          showAngel(show, action === 'call' ? 'The others know you are calling.' : 'Marked as resolved.', say)
        } catch (e) {
          btn.disabled = false
          say('Not sent: ' + e.message, { ms: 4000 })
        }
      }
      if (ack === 'call') {
        const p = document.createElement('p')
        p.className = 'sub'
        p.style.marginTop = '8px'
        p.textContent = 'You said you are calling.'
        const row = document.createElement('div')
        row.className = 'row3'
        row.style.marginTop = '8px'
        const done = document.createElement('button')
        done.type = 'button'
        done.textContent = 'Resolved'
        done.onclick = () => sendAction('done', done)
        row.append(done)
        card.append(p, row)
      } else if (!ack) {
        const row = document.createElement('div')
        row.className = 'row3'
        row.style.marginTop = '10px'
        for (const [action, label, primary] of [['call', 'I am calling', true], ['done', 'Resolved', false]]) {
          const b = document.createElement('button')
          b.type = 'button'
          if (primary) b.className = 'primary'
          b.textContent = label
          b.onclick = () => sendAction(action, b)
          row.append(b)
        }
        card.append(row)
      } else if (ack === 'done') {
        const p = document.createElement('p')
        p.className = 'sub'
        p.style.marginTop = '8px'
        p.textContent = 'You marked it resolved.'
        card.append(p)
      }
    }
    box.append(card)
  }
}
