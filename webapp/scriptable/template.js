// Duress signal for Scriptable (iOS). Part of the warn project.
//
// Run from the Scriptable app: a menu to show this phone's key and to test the connection.
// Run from a Shortcut (e.g. Back Tap): sends a duress alarm at once, without any screen.

// ---- settings ------------------------------------------------------------------------
// Watcher keys (npub): your laptop's device key (its built-in watcher) and any other watcher.
const WATCHERS = __WATCHERS__
// Name your contacts see for this phone.
const DEVICE = "iphone"
const USE_LOCATION = true
const RELAYS = __RELAYS__
// --------------------------------------------------------------------------------------

const KEY = "w.sync.sk"
const LAST_FIX = "w.sync.last"
/** Longest wait for a fresh position before the last known one is used. */
const LOCATION_WAIT_MS = 3000
/** Shortcuts give a script limited time; keep delivery attempts within it. */
const PATIENCE_MS = 20000
const BUNDLE = __BUNDLE__

function later(ms) {
  return new Promise((resolve) => Timer.schedule(ms, false, () => resolve(null)))
}

function within(promise, ms) {
  return Promise.race([promise.catch(() => null), later(ms)])
}

async function webview() {
  const wv = new WebView()
  await wv.loadHTML("<html><body></body></html>")
  await wv.evaluateJavaScript(BUNDLE + ";0", false)
  return wv
}

/** Call one of the bundle's async functions inside the WebView. */
async function call(wv, expr) {
  const js = `${expr}.then(r => completion({ ok: r }), e => completion({ error: String(e && e.message || e) }))`
  const r = await wv.evaluateJavaScript(js, true)
  if (r && r.error) throw new Error(r.error)
  return r.ok
}

async function secretKey(wv) {
  if (Keychain.contains(KEY)) return Keychain.get(KEY)
  const sk = await wv.evaluateJavaScript("completion(warn.newKeyHex())", true)
  Keychain.set(KEY, sk)
  return sk
}

function describe(fix, now) {
  const lat = fix.lat.toFixed(5)
  const lon = fix.lon.toFixed(5)
  const age = Math.round((now - fix.at) / 1000)
  let when = ""
  if (age > 120 && age < 3600) when = `, last known ${Math.round(age / 60)} min earlier`
  else if (age >= 3600 && age < 172800) when = `, last known ${Math.round(age / 3600)} h earlier`
  else if (age >= 172800) when = `, last known ${Math.round(age / 86400)} days earlier`
  return `https://www.openstreetmap.org/?mlat=${lat}&mlon=${lon}#map=17/${lat}/${lon} (±${Math.round(fix.acc)} m${when})`
}

/** A fresh position within LOCATION_WAIT_MS, else the last known one, else null. */
async function position() {
  if (!USE_LOCATION) return null
  try {
    Location.setAccuracyToHundredMeters()
  } catch (e) {}
  const loc = await within(Location.current(), LOCATION_WAIT_MS)
  let fix = null
  if (loc && typeof loc.latitude === "number") {
    fix = { lat: loc.latitude, lon: loc.longitude, acc: loc.horizontalAccuracy || 0, at: Date.now() }
    Keychain.set(LAST_FIX, JSON.stringify(fix))
  } else if (Keychain.contains(LAST_FIX)) {
    try {
      fix = JSON.parse(Keychain.get(LAST_FIX))
    } catch (e) {}
  }
  return fix ? describe(fix, Date.now()) : null
}

async function sendDuress(wv, sk, trigger) {
  const location = await position()
  const ctx = { device: DEVICE, trigger }
  if (location) ctx.location = location
  return call(wv, `warn.duress(${JSON.stringify(sk)}, ${JSON.stringify(WATCHERS)}, ${JSON.stringify(RELAYS)}, ${JSON.stringify(ctx)}, ${PATIENCE_MS})`)
}

async function say(title, message) {
  const a = new Alert()
  a.title = title
  a.message = message
  a.addAction("OK")
  await a.presentAlert()
}

async function menu(wv, sk) {
  const npub = await wv.evaluateJavaScript(`completion(warn.npub(${JSON.stringify(sk)}))`, true)
  const a = new Alert()
  a.title = "Sync"
  a.message = `This phone's key:\n${npub}`
  a.addAction("Copy key")
  a.addAction("Test connection (no alarm)")
  a.addDestructiveAction("Send alarm now")
  a.addCancelAction("Close")
  const choice = await a.presentSheet()
  if (choice === 0) {
    Pasteboard.copy(npub)
    await say("Copied", "Add this key to the person's devices in your watcher config.")
  } else if (choice === 1) {
    const loc = await position()
    const r = await call(wv, `warn.silentTest(${JSON.stringify(sk)}, ${JSON.stringify(WATCHERS)}, ${JSON.stringify(RELAYS)}, ${PATIENCE_MS})`)
    await say("Connection OK", `Accepted by ${r.relay} in ${r.ms} ms.\nLocation: ${loc ? "available" : "not available"}.\nNo alarm was raised.`)
  } else if (choice === 2) {
    const r = await sendDuress(wv, sk, "duress_test")
    await say("Alarm sent", `Accepted by ${r.relay} in ${r.ms} ms.`)
  }
}

async function main() {
  if (!WATCHERS.length) {
    await say("Not set up", "Edit the script: put your watcher keys in WATCHERS.")
    return
  }
  const wv = await webview()
  const sk = await secretKey(wv)
  if (config.runsInApp) {
    await menu(wv, sk)
  } else {
    // From a Shortcut / Back Tap: no questions, no screen.
    try {
      await sendDuress(wv, sk, "duress_shortcut")
      Script.setShortcutOutput("done")
    } catch (e) {
      Script.setShortcutOutput("failed")
    }
  }
}

await main()
Script.complete()
