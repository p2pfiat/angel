# Setup (MVP)

Two parts: a **watcher** (Docker, always on, ideally in another household) and the **agent** on
your Windows laptop. They never connect to each other directly: both only make outbound
connections to public Nostr relays.

## 1. Build

```bash
cargo build --release
```

**Windows on ARM64:** the TLS library (`ring`) needs clang. Visual Studio Build Tools ship one;
put it on the PATH for the build:

```powershell
$env:PATH = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\Llvm\ARM64\bin;$env:PATH"
cargo build --release
```

## 2. Telegram bot

One bot is enough for all watchers of a circle. Every watcher can send with it; Telegram lets
only one of them fetch button presses and `/id` at a time, so the others step back to
send-only by themselves.

1. In Telegram, talk to `@BotFather`, send `/newbot`, keep the token.
2. Put your own chat in the config: send `/id` to the bot once the watcher runs; it replies
   with the chat id for `telegram_chats`.
3. **Everyone else joins with a link**, nothing to configure: open the agent's Settings and
   press *Copy* under "Invite a contact" (or run `invite <name>` on the agent or watcher).
   Send that link, or let someone scan the QR code in the phone app (Settings → *Become my
   angel*). The contact taps it and presses *Start* and gets your alarms from then on; your
   current angels get a notice, and every watcher alerts the new angel (the watchers in
   `peers` tell each other). There is no approval: check the list (Angels window, or *My
   angels* in the phone app) and switch off or remove anyone you do not know. A contact leaves
   with `/stop`. If the link reached the wrong people, raise `invite_round` for that person:
   the old link stops working (angels who already joined stay until you remove them).
4. **Angels window** (Settings → *Angels…*): give every angel a name instead of a chat id,
   and choose per angel whether they get *No check-in* alarms and *Late check-in* notices.
   Duress alarms always go to every active angel. Untick *Active* to stop all alarms to
   someone. Changes reach the other watchers in `peers` too. The phone app has the same list
   under Settings → *My angels*, plus *Remove*. Chats from `telegram_chats`
   appear here as "Chat <id>" until you name them.

### Angels in the app (no Telegram)

In the phone app, Settings → *Become my angel* → *In this app* shows a link and QR code. The
angel opens it on their phone (on an iPhone: from Sync on the Home Screen, iOS 16.4 or newer),
fills in a name and allows notifications. Their phone gets its own key; your watchers send
alarms to it as a Web Push (straight to Apple's or Google's push service, end-to-end
encrypted) and as a Nostr message that the app shows in full, with *I am calling* and
*Resolved*. Every other angel hears those answers, on Telegram too. The angel gets a test
alarm right after joining. They show as "app" in your list (and in the Angels window); the
link is valid for 180 days and renewed by the app.

### Inviting someone to be protected

An angel can become a VIP in one tap: angels of your own VIPs get a personal invite with
their welcome (in Telegram, and in the app as a *Protect me too* card on the angel screen),
valid 30 days and once, with the same limits as below.


In the phone app, Settings → *Protect someone* makes a link (and QR code) for one person,
valid 7 days. Opening it in the app asks for a name and enrols that phone at your watchers;
they watch over the new person from then on, and you get "… accepted your invite" with a
button to become their first angel. With `app_url` in the watcher config, every new angel of
yours also gets such a link in the welcome message. On an iPhone the link must be used in the
app on the Home Screen (Safari keeps its own storage): add Sync to the Home Screen first, then
*Paste invite* in Settings if the invite is not shown.

People who came in by invite cannot invite others to your watchers, and each watcher takes at
most `max_invited_vips` of them. Their alarms go through your bot and your watchers see them.
Remove one with `warn-watcher remove-vip NAME` (or `warn-agent remove-vip NAME` for the
laptop's built-in watcher) and restart.

### A new or replaced phone

No config editing needed. Make a link, open it on the new phone, give the phone a name:

- in Sync on a phone that already works: Settings → *My devices* → *Add a phone or tablet*;
- on the laptop: Settings → *VIPs…* → *Add phone or tablet* next to the VIP (scan the QR code);
- with no working phone left: send `/pair` to the bot from one of the `owner_chats`
  (`/pair NAME` when the watcher has more than one VIP of its own). Needs `app_url`.

The link works once, within 30 minutes. On an iPhone, open it in Sync on the Home Screen
(Copy, then *Paste invite* in Settings). A lost phone: remove it under *My devices* (or in the
laptop's *VIPs* window) and Save; every watcher stops accepting it.

### Offering your server to someone

A watcher's owner can hand out a link that lets someone add that watcher (and its peers):

- in Telegram: send `/offer` to the bot from one of the `owner_chats` (`/offer NAME` when the
  watcher has more than one VIP of its own; the link is made on that VIP's behalf);
- on the laptop: Settings → *VIPs…* → *Offer this server…* (QR code and link).

The link works once, within 7 days. Someone new who opens it in Sync is set up with your
servers, as with *Protect someone*. Someone who already uses Sync with other servers gets
yours **next to** theirs; they then send their angel link again, so your server knows their
angels too. Either way they count as invited VIPs (`max_invited_vips`, no passing it on).
Needs `app_url` and `accept_invites = true`.

## 3. Watcher

```bash
mkdir data
cp watcher/config.example.toml data/config.toml   # edit: relays, bot token, persons
cp docker-compose.example.yml docker-compose.yml
docker compose up -d --build
docker compose run --rm watcher pubkey             # the watcher's key, for the agent
docker compose run --rm watcher test-alert Alex    # checks Telegram delivery
```

Without Docker: `warn-watcher --config data/config.toml run`.

Leave out the `[telegram]` section for a dry run: alarms are only logged.

**Several watchers, one alarm.** A home server and the laptop's built-in watcher both alert
the same contacts. To send each alarm once, give each the other's key in `peers` and set the
order with `send_delay_seconds`: the home server 0, the laptop 10. The first to send tells the
others over Nostr; they skip it. If that report does not arrive in time (relays slow, or a
watcher just started), contacts get the alarm twice rather than not at all. Right after a
start, a watcher holds alarms 20 s, because relays replay old messages in any order.

## 4. Agent (Windows)

The agent has a **built-in watcher**: one installation both reports your sessions and watches
the persons in its `[watcher]` section (yourself, and others whose agents add your device key
to their `watchers`). It uses the device key as its identity, so there is one key per
installation. A watcher on the same laptop cannot help if the laptop is switched off within
the timeout: keep at least one watcher elsewhere (a home server, a friend's agent).

```powershell
.\warn-agent.exe init --device laptop | Out-Host
```

`init` creates `%LOCALAPPDATA%\warn` (outside OneDrive) with a device key and a config, and prints:

- the **device key** → add it to the person's `devices` in the watcher config;
- the **secret check-in hotkey** → memorize it.

Then edit `%LOCALAPPDATA%\warn\config.toml`:

- other watchers' keys in `watchers`;
- optionally a built-in watcher (it uses the agent's relays):

  ```toml
  [watcher.telegram]
  bot_token = "..."

  [[watcher.person]]
  name = "Me"
  devices = ["<your device key>"]
  telegram_chats = [123456789]
  ```

- optionally `location` (a fixed place such as a home address), `note`, `duress_hotkey` and
  `timeout_seconds`;
- `detect_location` (default on): adds the position from the Windows location service to
  start and duress messages. A message waits at most 3 s for it; otherwise the last known
  position goes along, marked "last known … earlier". Windows shows its location icon
  briefly and lists the program under Privacy → Location. The last fix is kept in
  `last_location.json` (overwritten, no history);
- `ok_feedback` (default on): after the check-in key, a small note in the bottom-right corner
  shows the seconds since unlock (or that it came too late and contacts were alerted). Nothing
  is shown before the key is pressed;
- `reminder_seconds` (0 = off): when no check-in came after this many seconds, a discreet
  reminder: the ordinary new-mail sound (`reminder_sound`), a small notice worded to mean
  nothing to others (`reminder_notice`, `reminder_notice_title`, `reminder_text`), and/or a
  Telegram message to your own chat (`reminder_telegram`, `reminder_chat`,
  `reminder_telegram_text`). All of it, and `timeout_seconds`, can be changed in the
  settings window;
- optionally `settings_hotkey`: opens a small window to pause check-ins (e.g. while someone
  else uses the laptop). It only opens after you checked in, a pause ends at your next
  sign-in, and the duress key keeps working while paused. With a built-in watcher it also
  lets you change the name and phone number your contacts see in alerts (Telegram makes
  the number tappable, to call by phone or through Telegram); the built-in watcher picks it up
  at once.

`run` keeps the agent alive: it starts the agent as a child process and starts it again within
seconds if it crashes. If the screen is unlocked at that moment, the restarted agent asks for a
check-in (trigger `restart`), so a crash while someone else uses the laptop still ends in an
alarm. The position lookup runs in its own short-lived process for the same reason.

`warn-agent test-alert Me` checks Telegram delivery through the built-in watcher.

> `| Out-Host` makes PowerShell wait for the output: the agent is a windowless program.

Test the chain:

```powershell
.\warn-agent.exe test --ok     | Out-Host   # start + OK: watchers stay quiet
.\warn-agent.exe test          | Out-Host   # no OK: alarm after the timeout
.\warn-agent.exe test --duress | Out-Host   # immediate alarm
```

## 5. Start the agent at every logon

Rename the executable to something unremarkable, then:

```powershell
.\scripts\install-agent-task.ps1 -ExePath <path to exe> -TaskName "<unremarkable name>"
```

From then on: after every logon and every unlock, press your hotkey within the timeout.
Nothing appears on screen; the agent logs to `%LOCALAPPDATA%\warn\agent.log`.

## 6. Phone

See [iphone.md](iphone.md): a Back Tap shortcut (via a secret ntfy topic) and a home-screen
web app (Nostr) for duress alarms from the phone.

## Known limitations (MVP)

- If the laptop has no internet at all after unlock, `session_start` cannot leave the device and no
  alarm follows. The agent keeps retrying for 15 minutes.
- A restart of the agent (crash, task restart) counts as a new logon: check in again.
- Hotkeys do not work while the screen is locked; check in after unlocking.
- An alarm held for `send_delay_seconds` is lost if that watcher crashes in those seconds;
  the other watcher normally sent it already.
- A button press reaches whichever watcher currently polls the bot. If that watcher did not
  raise the alarm itself (it watches other persons), the press is ignored.
- The device key is stored unencrypted in `%LOCALAPPDATA%\warn` (DPAPI protection is planned).
- Umbrel login detection is not built yet.
