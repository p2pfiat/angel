# iPhone: silent alarm, check-ins and timer

iOS gives web apps no background time and cannot tell when the phone is unlocked, and
Apple Shortcuts cannot do Nostr cryptography. So the iPhone uses two routes; set up both,
they fail in different ways.

| | Apple Shortcuts (secret gesture or automations) | Web app "Sync" |
|---|---|---|
| Used for | discreet duress gesture; check-in or delay when opening a sensitive app | timer (walk home); duress button as a fallback |
| Visible on screen | a short Shortcuts banner at most | the app opens |
| Transport | a secret `ntfy.sh` topic the watchers follow | Nostr, same encryption as the laptop |
| Sees the content | the ntfy server | nobody but the watchers |

Neither replaces Emergency SOS (five presses on the side button; can be set to call quietly):
that one calls the emergency number. These alert your own contacts first.

## 1. Install Sync and connect a watcher

**Install:** open Sync's URL in Safari → Share → *Add to Home Screen*. Always open it
from the Home Screen: iOS gives the Home Screen app its own storage, separate from Safari,
and angel notifications arrive only in the Home Screen app.

**Connect a watcher:**
- Choose **Start with courtesy server**, paste an invite link you received, or enter your
  own watcher keys under *Advanced*;
- Keep *Send my location with an alarm* switched on;
- **Shortcut topic**: Sync generates a long random `https://ntfy.sh/...` address for this
  phone (viewable under *Advanced*). Anyone who knows this URL can send alarms and
  check-ins for you, so treat it like a password.

**Keep it alive:** iOS may clear a Home Screen app's storage when it is not used for a long
time; that deletes the key. Open Sync now and then (you can also set a weekly Shortcut
reminder or automation if you like).

## 2. Add the iPhone shortcuts (1-tap from Sync)

In Sync → **Settings** → **iPhone shortcuts**, tap **Add Duress**, **Add OK**, and
**Add Delay**:

1. Tapping a button automatically copies your secret topic URL to the clipboard, syncs that
   topic to your connected servers (`topics.json`), and opens Apple Shortcuts.
2. When Apple Shortcuts asks for the topic / URL during import, paste from your clipboard
   and finish adding the shortcut.
3. Apple Shortcuts asks once on first run for permission to contact `ntfy.sh` and read your
   location — choose **Always Allow**.

**How the three shortcuts work:**
- **Duress** — posts `duress` with your current location. Watchers apply a 20-second cancel
  window (`cancel_seconds = 20`) by default so an accidental tap can still be cancelled.
- **OK** — posts `ok`, cancelling a Duress still in its 20-second window as well as any open
  **Delay** / app-open clock.
- **Delay** — posts `start 90 app_open` with your location: starts a 90-second check-in
  clock. Send **OK** within 90 seconds to cancel it; otherwise your angels get an alarm with
  your location.

**Link each shortcut to your own trigger:**
- Choose your **own secret gesture** for **Duress** and **OK** — for example Settings →
  Accessibility → Touch → *Back Tap*, the *Action button* (iPhone 15 Pro / 16+),
  *AssistiveTouch*, Control Center, or Lock Screen controls.
- For **Delay** on banking or other sensitive apps: Shortcuts → **Automation** → **+** →
  **App** → choose the app(s) → *Is Opened* → **Run Immediately** → **New Blank
  Automation** → *Run Shortcut* → **Delay**.

**Lock Screen & iOS tips:**
- Open each imported shortcut → **ⓘ** → **Privacy** → enable **Allow Running When Locked**
  so a trigger works without unlocking first.
- If you choose **Back Tap** as your gesture, note that iOS only detects Back Tap while the
  screen is **awake** (waking to the Lock Screen is enough; a completely black sleep screen
  will not trigger it). For reliable sleep-screen triggers, use the *Action button* or Lock
  Screen controls if your iPhone has them.
- `Get Current Location` can occasionally time out or prompt if run while locked unless
  Shortcuts Location Access is allowed. Test once while locked to verify what iOS does on
  your model.
- Watchers read these commands from an `ntfy` topic: `start <seconds> [scenario] [location]`,
  `ok`, and `duress [location]` (any other text also counts as duress).

### Self-hosted ntfy servers (`config.toml`)

Sync automatically activates `https://ntfy.sh/<random-20..64-char>` topics on connected
watchers over Nostr (`topics.json`) — no `config.toml` edits or restarts needed. For
SSRF protection on shared servers, custom self-hosted `ntfy` URLs are not accepted over Nostr:
to use your own `ntfy` server, edit the topic under *Advanced* in Sync and add it manually in
every watcher's `config.toml`:

```toml
duress_ntfy = [{ url = "https://ntfy.example/your-secret-topic", device = "iphone", cancel_seconds = 20 }]
```

## 3. Building the shortcuts by hand (optional)

If you prefer not to use iCloud Shortcut links, build the three shortcuts in Apple Shortcuts
yourself. Give them inconspicuous names (e.g. "Sync 1", "Sync 2", "Sync 3"). In every
*Get Contents of URL*: URL = your topic, Method **POST**, Request Body **File**.

**Duress**
1. *Get Current Location*
2. *Text*: `duress https://www.openstreetmap.org/?mlat=` *Latitude* `&mlon=` *Longitude*
   (pick both from the Current Location variable)
3. *Get Contents of URL*, File = the *Text*

**OK**
1. *Text*: `ok`
2. *Get Contents of URL*, File = the *Text*

**Delay** (run from a Personal Automation when an app opens)
1. *Get Current Location*
2. *Text*: `start 90 app_open https://www.openstreetmap.org/?mlat=` *Latitude* `&mlon=` *Longitude*
3. *Get Contents of URL*, File = the *Text*

**Timer** (optional standalone shortcut for a walk home; `1800` = 30 minutes)
1. *Get Current Location*
2. *Text*: `start 1800 walk_home https://www.openstreetmap.org/?mlat=` *Latitude* `&mlon=` *Longitude*
3. *Get Contents of URL*, File = the *Text*

## 4. In-app Timer (walk home)

In Sync → *Settings* → *Timer* → 15, 30 or 60 min. The watchers keep the clock, so it runs on
even if the phone is taken or switched off. *Done* stops it; *+15 min* gives more time. If
it runs out, your angels get an alarm with the location where you started.

## 5. Test

1. Use **Send test alarm** in Sync first to confirm your angels receive notifications.
2. Run **Duress**, wait past the 20 s cancel window: an alarm with location arrives. Try once
   with the Lock Screen active too.
3. Run **Duress**, then run **OK** within 20 s: nothing is sent to your angels.
4. Open a monitored bank/sensitive app, run **OK** within 90 s: nothing is sent. Open it
   again and wait ~90 s: an alarm arrives.
5. Note what iOS shows on screen (Shortcuts notification banner) when the automation runs.

### Publishing the shortcuts (maintainer reference)

1. Build **Duress**, **OK**, and **Delay** using placeholder URL `https://ntfy.sh/template`.
   Never include a real secret topic before sharing.
2. In each shortcut → **ⓘ** → *Setup* → add an Import Question on *Get Contents of URL*'s
   URL ("Paste your topic").
3. Share → *Copy iCloud Link* and store the links in `SHORTCUT_LINKS` in `webapp/src/app.js`.
