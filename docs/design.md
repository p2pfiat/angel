# Design: personal safety network

Status: draft for discussion. It extends [architecture.md](architecture.md) (devices, Nostr,
watchers) towards personal safety on the phone, escalation, and validated responders nearby.
Items marked *(to verify)* depend on platform behaviour we have not tested yet.

## 1. Goal

Get help to a person in danger **within minutes**, also when she cannot make a visible call
for help. Emergency services stay essential, but often arrive too late; someone close by who
shows up and makes noise often ends an assault.

### Scenarios

| | Situation | What the person can do |
|---|---|---|
| A | Assault in public or at a party (e.g. walking home) | a hidden gesture at most; maybe nothing |
| B | Forced access: robbery where she must unlock the phone or open a bank app | only things the attacker will not notice |
| C | Home invasion with coercion (laptop, server) | as in [architecture.md](architecture.md) |

### Attacker assumptions

- May take the phone, watch the screen, or force her to unlock it.
- Does not know whether this system is used, nor how it is set up.
- **May try to use the system itself**: sign up as a helper to receive the locations of
  vulnerable people. This is the most important abuse case for the design (section 6).

### Non-goals

- Resisting the attacker in the person's place: helpers are told to alert, make noise, call the
  emergency number, and not to fight.
- Replacing emergency services.

## 2. Roles

| Role | Who | Gets | Must have |
|---|---|---|---|
| **VIP** (protected person) | the user (e.g. a daughter) | — | the phone setup (section 4) |
| **Angels** (tier 1) | 2–5 people she chooses | every alarm, exact live location | an invite link, accepted |
| **Circle** (tier 2) | people her angels or she add | alarms nobody in tier 1 took on | an invite link, accepted |
| **Responders** (tier 3) | validated members near the alarm | coarse alarm first; exact location only after "I'm going" | validation (section 6) and the responder setup |
| **Watchers** | always-on devices in the circle (a home server, an angel's PC) | encrypted alarm messages | the watcher program; outbound connections only |

A person can be all of these for different people. Whoever runs a watcher for others (the
technical uncle) has no role of their own: running it *is* the watcher role.

**A VIP has devices.** The VIP is one identity with several devices (phone,
laptop, a watch later); each device has its own key and sends its own messages, and watchers
map every device key to the VIP. Alarms are about the VIP and name the device ("on the
laptop"). This exists already (`devices` per `[[person]]` in the watcher config, which keeps
that name); onboarding makes it explicit: adding a
device is scanning a QR code with an already linked device, and a lost device is removed
without touching the others.

**An angel is one identity with several channels.** The same split as VIP and devices: the
angel is a person (a friendly name, which alarms they get), reachable over one or more
channels (Telegram chat, Web Push subscription, phone number for SMS and calls). Today an
angel is still keyed by their Telegram chat (`contacts.json`, synced between watchers with
`contact_changed`). With the circle on Nostr (phase 2) every angel gets a key, created when
they accept an invite: the key signs their consent and their acknowledgements ("I'm on it"),
and the channels hang under it. Then an angel can add or change a channel themselves, a
phone change does not lose them, and the same angel can be listed for several VIPs. The
angel's key is never shown to them; like the VIP's, it lives in the app.

**Per angel, the VIP chooses** which alarms they get: missed check-ins, late check-ins
(duress always goes to every active angel). Later the angel adds their own limits (quiet
hours for missed check-ins, never for duress).

## 3. One model for all use cases

Every use case is built from the same four parts; only how they are filled in differs.

```
trigger (device) ──▶ message ──▶ watcher: clock + escalation policy ──▶ recipients per tier
```

1. **Trigger:** what happens on a device (unlock, app opened, gesture, timer started).
2. **Message:** one of a few kinds, the same for every device and transport.
3. **Escalation policy:** which tiers get the alarm, in which order, after how long. Chosen by
   the message's *scenario*.
4. **Recipients and channels:** angels, circle, responders; Telegram now, push and SMS later.

### Use cases in this model

| Use case | Trigger | Message | Deadline | OK | Policy |
|---|---|---|---|---|---|
| Home invasion (laptop) | unlock / logon | `start`, scenario `unlock` | e.g. 120 s | secret hotkey | angels → circle |
| Forced access (bank app) | app opened | `start`, scenario `app_open` | e.g. 90 s | secret gesture | angels → circle → responders |
| Walk me home | one tap | `start`, scenario `walk_home` | e.g. 20 min | arriving home, or gesture | angels → responders |
| Assault, hidden gesture | Back Tap, Action button, Watch | `duress`, scenario `gesture` | — | — | all tiers at once |
| Forced cancel | duress PIN on the cancel screen | `duress`, scenario `duress_pin` | — | — | all tiers at once |

A new use case (a button on a key ring, a car-related trigger, a server login) is a new
trigger plus the choice of a policy; nothing else changes.

### Messages

| Kind | Meaning | Exists |
|---|---|---|
| `start` | a clock runs until `deadline`; no `ok` by then → alarm | yes (`session_start`) |
| `ok` | stops that clock | yes (`session_ok`) |
| `duress` | alarm at once | yes |
| `extend` | moves the deadline of a running clock (walk me home: "I'm later") | new |
| `cancel` | withdraws an alarm with the PIN; the duress PIN is sent as `duress` instead, and looks the same on the phone | new |

Every message carries a **scenario** (above) next to the existing context (device, trigger,
location, note). Messages reach the watchers through neutral meeting points (section 7):
Nostr relays for apps and agents that can sign, a secret ntfy topic for iPhone Shortcuts.

### Escalation policy

A policy is a short list of steps, set per person and per scenario, with sensible defaults:

```toml
[policy.gesture]            # duress: everyone at once
steps = [{ tiers = ["angels", "circle", "responders"], after = 0 }]

[policy.app_open]           # missed check-in: angels call first
steps = [
  { tiers = ["angels"],     after = 0 },
  { tiers = ["circle"],     after = 120, unless = "acknowledged" },
  { tiers = ["responders"], after = 240, unless = "acknowledged" },
]
```

`unless = "acknowledged"` stops the step when someone pressed "I'm on it" or "Resolved".
The engine runs one state machine per alarm: it walks the steps, records every delivery and
acknowledgement, and shows them to everyone already alerted.

### Channels

A channel is a plug-in with one job: deliver an alarm (text, location, buttons) to one
recipient and report back delivered / failed / acknowledged. Telegram is the first; push
notifications (with critical alerts in the native app) and SMS follow without touching the
engine. Every delivery is logged, also when it succeeds, so "did it arrive?" always has an
answer.

### What exists and what is new

| Part | Today | Needed |
|---|---|---|
| Triggers | Windows unlock/logon, hotkeys, web app, ntfy | iPhone Shortcuts (gesture, app open, walk home), Watch |
| Messages | `start`, `ok`, `duress` with context | scenario field, `extend`, `cancel` |
| Trust | device keys; watcher lists its persons in a config file | signed invitations and consent, encrypted circle lists, signed acknowledgements |
| Transport | Nostr; ntfy for duress only | ntfy topics also carry `start` and `ok` (iPhone Shortcuts) |
| Engine | clocks and alarms to everyone | policies and the escalation state machine |
| Channels | Telegram, failures logged | channel interface, delivery log, push, SMS |

## 4. Triggers on the iPhone

iOS cannot detect an unlock and stops background web apps, so every trigger runs through iOS
itself (Shortcuts, Back Tap, the Action button, Apple Watch) and sends one HTTPS request to a
secret ntfy topic that the watchers follow (section 7). The web app signs Nostr messages itself.

| Trigger | How | Scenario |
|---|---|---|
| **Duress gesture** | Back Tap (triple tap on the back) or the Action button runs the "duress" shortcut | A |
| **Apple Watch** | a shortcut on the watch face *(to verify: runs without opening anything)* | A |
| **Walk-me-home timer** | start: one tap; stop: arriving home (location automation) *(to verify: runs without confirmation)* or the secret gesture | A |
| **Forced access** | automation "when [bank app] is opened" starts a session; the secret gesture is the OK | B |
| **Web app** | hold the button, or open the app with auto-send on | A (fallback) |

Notes:

- Since iOS 17 automations can run without confirmation, but iOS shows a short banner that
  cannot be switched off. An attacker may see it; the wording is fixed by iOS.
- **The OK must be a gesture nobody notices**: the same Back Tap pattern as everyday use (e.g.
  double tap = OK, triple tap = duress), never a button on screen.
- **Cancel = duress PIN.** A cancel screen asks for a PIN; the duress PIN looks like a normal
  cancel but escalates silently.
- **Live location:** a shortcut sends the position with each trigger. For angels, iOS's own
  location sharing (Find My) is the reliable live view; the alarm message links to it.
- A **native app** becomes necessary for two things a web app and Shortcuts cannot do:
  location updates in the background, and *critical alerts* that sound through Do Not Disturb.
  Critical alerts need an Apple entitlement for safety apps; apply early.

## 5. Escalation

The watcher runs one state machine per alarm, following the policy of the alarm's scenario
(section 3). Every step is logged and visible to the angels. The defaults:

```
alarm ─┬─▶ angels (all)                 ── someone presses "I'm on it"? ──▶ others: "X is on it"
       │                                       │ no, after T1 (e.g. 2 min)
       │                                       ▼
       ├─▶ circle (tier 2)              ── same, after T2
       │
       └─▶ responders nearby (tier 3)   ── at once for duress (scenario A),
                                            after T1 for a missed check-in
```

- Each tier has buttons: **I'm on it**, **I'm calling the emergency number**, **Resolved (code word OK)**.
- Angels also get **Alert my angels**: the angel forwards this alarm to *their own* angels,
  people who may be physically closer (a neighbour of the angel, a friend in the same town).
  This is how tier 2 grows without anyone keeping a list: each angel brings their own circle,
  and only when an alarm needs it. Rules: the VIP allows forwarding at setup (on
  by default, can be switched off); the forwarded alarm says who forwarded it and carries the
  same location and buttons; everyone already alerted sees "X alerted their angels";
  forwarding goes one step only (a forwarded alarm cannot be forwarded again), so an alarm
  never reaches strangers of strangers. Needs the angel's own circle, so it comes with the
  circle lists of phase 2 (section 10).
- "Resolved" by an angel needs the agreed code word; a cancel from the person's own phone
  needs her PIN (a duress PIN escalates, section 4).
- Duress in scenario A goes to all tiers at once: minutes matter more than avoiding noise.
  A missed check-in (scenarios B, C) starts with the angels, who call first.

## 6. Responders: validation and abuse

The design must keep a predator from becoming a "responder" and receiving the locations of
vulnerable people.

**Levels**

| Level | How | May receive |
|---|---|---|
| 0 invited | invite link from a member | nothing yet |
| 1 vouched | signed vouches from **two** level-2 members who know them in person | alarms in a pilot circle |
| 2 verified | identity check (e.g. a national eID) + level 1 + accepted code of conduct | alarms from anyone nearby |

Vouches are signed statements kept by the responder and shown, encrypted, only to the
watchers that must check them (section 7): they can be checked and withdrawn, but the network
of responders is not visible to outsiders.

**Location disclosure**

1. Alarm: responders within the radius get "emergency, about 600 m from you", no address.
2. "I'm going": that responder gets the exact live location and the angels see who is coming.
3. At most N responders (e.g. 5) get the exact location per alarm.

**Safeguards**

- A responder's position **never leaves the responder's phone**: the alarm goes, encrypted,
  to the responders of the area, and each responder's app decides itself whether the alarm is
  close enough to show (section 7).
- Every disclosure is logged; the VIP and her angels can see who received her
  location.
- Patterns are flagged: accepting without going, often accepting alarms of the same person,
  being near the same person repeatedly.
- The VIP can choose: responders of any gender, or only women; only level 2.
- Removing a responder removes them everywhere at once (revoked vouch, key blocked).

**Responder safety**

- Every alarm instructs: call the emergency number first, go with at least two people, make
  noise, do not fight.
- Each responder sees how many others are on their way (from their signed "I'm going").

## 7. Architecture

**Decided: no server of our own, and no device with an open port.** Every program makes
outbound connections only; devices meet at neutral, replaceable meeting points. There is no
central service that can fail, be blocked or be shut down: as long as one watcher of the
circle runs, an alarm goes out.

```
 iPhone (Shortcuts) ──HTTPS──▶ ┌──────────────────────────────┐ ◀── watchers follow both
 web app, agents ──Nostr────▶ │ meeting points               │      (home server, angels' PCs:
                               │  • Nostr relays (many,       │       outbound only)
                               │    different operators)      │
                               │  • ntfy servers (secret       │ ──▶ alarms to angels:
                               │    topics)                   │      ntfy app, Telegram, Nostr DM
                               └──────────────────────────────┘
```

| Layer | What | Where |
|---|---|---|
| **Trust and identity** | one key per person and device; signed invitations and consent; circle lists; signed acknowledgements; vouches | Nostr events, encrypted where private |
| **Meeting points** | alarm messages (`start`, `ok`, `duress`, `extend`, `cancel`), acknowledgements | Nostr relays (signed, encrypted); secret ntfy topics for iPhone Shortcuts, which cannot sign |
| **Watchers** | clocks, escalation, delivery log | every always-on device of the circle; each one works alone |
| **Channels** | getting an alarm to a person and their answer back | ntfy app (default), Telegram, Nostr direct messages, later SMS |

### Watchers: several, each enough on its own

- A watcher runs on any always-on device of the circle: a home server, an angel's PC or mini
  PC, later an Android phone. iPhones cannot be watchers: iOS stops background apps.
- **Every watcher keeps every clock.** Two or three watchers in different homes are more
  resilient than any server; the circle needs at least two.
- **No duplicate alarms:** a watcher that sends an alarm announces it (signed, on Nostr); the
  others wait a few seconds, see it, and only add what is missing (e.g. the next escalation
  step). If the announcement does not come, they send themselves.
- Watchers catch up after an outage by reading recent messages from the meeting points, and
  keep their state on disk, so a restart repeats no alarm.
- **A watcher's key is an address, not a permission.** Anyone can put it in an app; the
  watcher only handles devices of VIPs it watches. So the VIP's app must not assume
  protection from a key in its settings: a duress that relays accept but no watcher handles
  would leave her believing help is coming. "Delivered" alone means only "a relay has it".
- **Watchers confirm, and keep confirming.** A watcher sends every device of every VIP it
  watches a signed `watching` message (gift-wrapped like all others): at start and then every
  hour, with that interval in it. The main screen shows how many watchers were heard within
  2 intervals, and warns plainly when none was. Shortcuts cannot check this; the web app on
  the same phone is where the VIP sees it.
- **Settings asks live.** Opening Settings sends a `ping` to every watcher; a watcher that
  watches this device answers at once (`watching` with the ping's nonce). That is the alarm's
  own path (phone → relays → watcher → relays → phone), so an answer proves it works now.
  Each watcher shows *watching you (answered in 0.6 s)* or *NO ANSWER* with its last sign of
  life. Unknown devices get no answer; a watcher answers a device at most every 5 s, and only
  pings of the last 2 minutes (relays replay old ones).

### Why Nostr for trust, and not as a database

Nostr relays keep events as long as they choose; reliability comes only from using many of
them, and they cannot be queried like a database. What Nostr does well is **signed, portable
statements** that no single server owns. That fits trust:

- **Invitation and consent.** She invites an angel with an encrypted message; the angel
  accepts with a signed reply. Both signatures together are the relation. A watcher can check
  a relation, but cannot invent one, and nobody can add someone to her circle unnoticed.
- **Circle lists** are kept as encrypted lists (NIP-51 style), readable only by her keys and
  her watchers: available on a new phone and to every watcher, so a new watcher can take over.
- **Acknowledgements** ("I'm on it", "Resolved") are signed by the person who pressed them, so
  it is always clear who said what; every watcher sees them.
- **Vouches** for responders are signed statements held by the responder, shown encrypted to
  the watchers that need to check them: verifiable, without a public map of responders.

**Never public:** who protects whom, who is a responder, and where responders are. Nostr's
public follow lists are therefore not used for these relations (at most as suggestions of
people to invite).

Ordinary users never see keys: the app or web app creates them and keeps them on the device.

### iPhone Shortcuts and ntfy

Shortcuts cannot sign Nostr events, so they post plain commands (`start 90 app_open`, `ok`,
`duress`) with the location to a **secret ntfy topic** of that phone. Watchers follow the topic
with an outbound connection. Anyone who knows the topic name can send to it, so it is long and
random, and treated as a secret. Later: a second ntfy server for redundancy, and a short
authentication code computed by the Shortcut (Shortcuts can hash).

### Channels to angels

Ordered by how little the angel has to do:

| Channel | The angel does | Notes |
|---|---|---|
| **SMS** | nothing | through an old Android phone that is also a watcher (an SMS gateway app); for angels without smartphone habits |
| **Web app + Web Push (default)** | opens the invite link, adds it to the home screen, allows notifications | iOS 16.4+ and Android. Watchers send straight to Apple's / Google's push service: no server of ours. Content is end-to-end encrypted (RFC 8291), so better than Telegram. The same app for every role. Each angel's app makes its own push key (VAPID) and gives it to the watchers it accepts alarms from (see "Angels with their own key") |
| **ntfy app** | installs it, scans a QR code | reliable and simple; ntfy.sh sees the content |
| **Telegram** | taps a join link, presses Start | the circle's own bot. The join link (`t.me/<bot>?start=<code>`) replaces looking up chat ids. Creating the bot stays one task for whoever sets up the watchers |
| **Nostr direct messages** | uses a Nostr app | for those who already do |

No channel gets through silent mode except critical alerts, which need a native app with
Apple's permission (section 10). A shared project-wide Telegram bot is not an option: every
watcher would hold its token, and anyone with the token can read and send as the bot.

### Responders nearby, without a central list of positions

1. The alarm goes, encrypted, to every responder of the area (the pilot's responder list).
   It carries only a coarse position.
2. Each responder's app compares it with its own position, on the phone, and shows the alarm
   only when it is within the radius. No one else ever learns where responders are.
3. "I'm going" is a signed message; the watchers of the VIP check the responder's
   vouches and send that responder the exact live location, encrypted.

### What this costs

- **At least one always-on device in the circle**, better two in different homes. For people
  without a home server: a Windows watcher for an angel's PC, or a small ready-made box.
- **Meeting points and channels are third-party services** (relays, ntfy, Telegram). They can
  fail, so we use several of each; none of them can stop the system alone.
- **Push on iPhones always goes through Apple.** Notification apps (ntfy, Telegram) run their
  own small push relays for that. A native responder app will need one too: it only wakes the
  phone and knows no clocks, circles or positions.
- Coordinating several watchers is more work than one server (duplicates, shared
  acknowledgements).

## 8. Onboarding (target: one scan or one link)

**One mechanism: an invite, as a link or as a QR code.** The link is sent through any
messenger (WhatsApp, iMessage) and works at a distance; the QR code is the same link for when
people are together, and also proves who is in front of you. Opening it shows who invites you
and for what, then one "Join" button.

**The configuration lives with the VIP, not in files.** Her app keeps the
circle (devices, angels, watchers, policy) and publishes it as an encrypted list on Nostr to
her watchers (section 7). Watchers are paired once and then follow that list; nobody edits a
`config.toml` or looks up a chat id. Until then, step 1 below removes the worst manual steps.

Steps, in order of effort:

1. **Quick wins (now).** Telegram join link: a contact taps it and presses Start; the watcher
   adds the chat on every watcher at once (no approval: the VIP sees every angel in the app
   and switches off or removes the ones they do not want). The iPhone
   Shortcuts as iCloud links: "Add Shortcut" asks for the topic, which the web app has put on
   the clipboard. Back Tap stays a 30-second manual setting; the bank-app automation cannot be
   shared as a link and stays an advanced step.
2. **The circle on Nostr.** The invite link for angels, devices and watchers; angel mode in
   the web app with Web Push; "Alert my angels".
3. **An Umbrel app** in a community app store: install with one click, pair by scanning.
4. **SMS through an Android watcher**, then perhaps a native iOS app (Back Tap and Action button
   list it directly, lock-screen widget, Watch, critical alerts for angels). The native app
   costs a developer account and App Store review, and the publisher's name is public: it
   needs a publisher that is not a private person.

The target flow per role:

- **VIP:** open the web app → it creates her key (she never sees it) → shows a
  QR code / link for each Shortcut (iCloud link, one tap) and an invite link for her angels.
- **Angel:** taps the invite link, sees who invites them, accepts. Accepting creates the
  angel's key and the signed consent (section 7), and links a channel: Web Push in the same
  app by default, or SMS, ntfy or Telegram (section 7, channels to angels).
- **Watcher:** a home server, or the Windows watcher on an angel's PC; set up by scanning the
  circle's QR code. The circle needs at least two.
- **Laptop:** the installer shows a QR code; scanning it with the phone links the laptop to the
  same person.
- **Responder:** invite link → vouches → (level 2) identity check → the responder app.

### Invites from the web app: four loops

Every invite is a link sent with the phone's share sheet (`navigator.share`: one tap opens
WhatsApp, iMessage, ...) or the same link as a QR code. The secret sits after `#`, so
messenger link previews never send it to their servers. The preview text says who asks and
for what ("Alex asks you to be his angel").

| Loop | Who invites | Result |
|---|---|---|
| **A. Be my angel** | the VIP | the invited person becomes her angel. The main loop: every VIP invites 3–5 people, and for an angel it is the lowest step (open the link, allow notifications). |
| **B. Angel becomes VIP** | an angel, from the app | "Want angels of your own? Alex is already your first." One tap. Angels have seen it work, so this is the strongest conversion. |
| **C. Protect someone** | the worried one (a parent, a partner, an adult child) | a new VIP, with the inviter as her first angel and the inviter's watchers. Usually the one who pays. |
| **D. Let's protect each other** | anyone, for a group (friends, students, colleagues) | one link: everyone is VIP and angel for the others; each can leave per person. |

Rules:

- **The VIP decides who her angels are.** In loop C the inviter's own angels are offered as
  *suggestions*; only the inviter is ticked by default. A suggested angel gets a request
  ("Alex asks you to also protect Anna") and is added only after accepting.
- **Nobody can follow a VIP.** Location travels only with an alarm or a check-in, never
  continuously; the VIP sees all her angels and removes anyone at any time. This is the guard
  against "protect someone" used for control by a partner, and a selling point: unlike a
  family-locator app, a teenager can accept it.
- **Watchers are shared by default** (loop C, and D for the group's watchers): without a
  watcher a new VIP has no protection, so the invite carries the inviter's watchers. A watcher
  accepts a VIP invited by one of its own VIPs automatically, and tells its owner ("now also
  watching Anna, invited by Alex"). One hop only: a VIP added that way cannot pass the
  watcher on, so a home server is never handed to strangers of strangers. Later a watcher can
  announce limits in the circle (`full`, `paid only`, a maximum number of VIPs); the app then
  asks the VIP to add another watcher or a paid one, and never drops below one working
  watcher without warning.
- **The angel's first moment is a drill:** right after accepting, a test alarm shows what a
  real one looks like. It builds trust, keeps notifications on, and is where loop B is offered.

### Step 2: new VIPs by invite, before the circle on Nostr

Loops B and C with what runs today (watchers with a config file, angels on Telegram). The
circle on Nostr replaces the watcher-side list later; the link and the app flow stay.

**The link.** `<app>#j=<secret>`: 22 characters (128 bits, base64url), about 55 in all, so
the QR code stays simple. Everything after `#` stays in the phone: messenger previews and
the web server never see it. The details (who invites, the watcher keys, the relays) are an
event on the relays (kind 30078, NIP-40 expiration), signed by a key derived from the secret
(sha256 of `warn-invite:<secret>`) and encrypted to that key with NIP-44. The app derives the
same key, asks its standard relays for that author's event and decrypts it; relays see only
an encrypted event from a key used nowhere else. The secret also lets the new VIP in at the
watchers, once, before it expires. A link shortener would see the secret, and a watcher has
no open port to ask, so the relays are the place. If no relay kept the event the app says
"not found: ask for a new link".

**Who may invite, one hop.** Only a VIP from a watcher's config file (an "own" VIP) can
create an invite on that watcher. A VIP who came in by invite cannot pass the watcher on, so
a home server is never handed to strangers of strangers.

**Creating an invite.** Two ways, both ending in the same link:

- *Protect someone* in the VIP's app (loop C): the app makes the secret and sends
  `invite_created {hash, expires}` (sha256 of the secret, 7 days) to each of its watchers.
  Each watcher answers `invite_ok {hash}` or `invite_refused {hash, reason}` (not an own VIP,
  full). Only watchers that said yes go into the link; with none, the app says so and shows no
  link.
- *The welcome message to a new angel* (loop B): the watcher that handles the join makes the
  secret itself (on behalf of that VIP, 30 days), tells its `peers` with the same
  `invite_created`, and puts the link in the welcome: "Want the same protection for yourself?
  `<link>`". Needs `app_url` in the watcher config; without it the line is left out.

**Joining.** Opening the link in the web app (no key yet, or a key without watchers):

1. Shows who invites and what it means: "Alex invites you. His servers will watch over you
   and send your alarms to the angels you choose. Alex's servers see your alarms."
2. Asks one thing: the name angels will see ("Anna"). Creates the device key.
3. Sends `enroll {secret, name, device}` to each watcher in the link, waits like the live
   check. A watcher checks the hash, the expiry, that it was not used, and that the name is
   free on that watcher; then it adds Anna with this device, answers with `watching` (carrying
   Anna's own angel link) and marks the secret used. Otherwise `refused {reason}`.
4. Opens Settings with Anna's *Become my angel* card on top: "First send this to Alex".

**The inviter becomes the first angel**, without knowing a chat id: when Anna enrols, the
watcher sends Alex's devices `vip_joined {person, invite}`. Alex's app shows "Anna
accepted: tap to become her angel"; the tap opens Anna's Telegram link, Start, done (the join
needs no approval, section above). Alex's own angels are not added: Anna invites whom she
wants.

**On the watcher.** VIPs that came by invite live in `vips.json` next to `contacts.json`
(name, devices, invited by, when), loaded with the config's persons at start and added while
running. Angels, timers, alarms and deduplication between `peers` work for them as for own
VIPs, because both watchers enrolled the same device under the same name. Limits in the
config: `accept_invites` (default true) and `max_invited_vips` (default 20). The owner sees
who joined in the log and, with `owner_chats`, on Telegram ("now also watching Anna, invited
by Alex"), and removes one with `remove-vip NAME`.

**What it costs the owner.** Alarms of invited VIPs go through the owner's bot and server,
and the server sees them. Limits keep it small: the cap above, and at most 30 alarms a day per
invited VIP (beyond that one notice to the owner and the VIP's angels). Locations are never
written to the log.

**On the iPhone.** A web app added to the Home Screen has its own storage, separate from
Safari. So the link opened in Safari does not enrol yet: it shows "Add to Home Screen, then
open Sync" and copies the code to the clipboard; the app, opened without setup, offers
"Paste invite". Whether Add to Home Screen keeps the `#` part (which would skip the paste) is
tested first.

**Not in step 2:** suggested angels from the inviter's list (needs angels with their own
key), the group link (loop D), a watcher announcing "full" or "paid only" in advance (now it
refuses at enrol time), and a VIP leaving a watcher from the app.

### Angels with their own key (built before the circle on Nostr)

An angel becomes a key in the web app, reachable by Web Push, next to (later instead of)
Telegram. Nothing new runs anywhere: watchers send pushes outbound, like their Telegram
messages.

**Angel mode in the same app.** The VIP's *Become my angel* card shows a second link,
`<app>#a=<secret>` (short, like the VIP invite: the details are an encrypted event on the
relays: the VIP's name, watchers and relays). Opening it:

1. "Alex asks you to be his angel. If he is in trouble you get an alarm on this phone."
   On an iPhone: add to the Home Screen first (Web Push only works there, iOS 16.4+).
2. A name ("Sam") and *Allow notifications* (the permission prompt needs this tap).
3. The app makes the angel key (Nostr), a push key (VAPID, P-256) and a push subscription
   with that key, and sends `angel_join {secret, name, push}` to each watcher in the link.
   `push` holds the subscription (endpoint, p256dh, auth) and the VAPID private key.
4. A watcher checks the secret like a Telegram join link (same code, derived from its
   invite round), adds the angel at once (no approval, as with Telegram) and answers.
5. A test alarm follows at once: the angel's first moment is a drill.

**Why the push key is the angel's.** A browser has one push subscription per app, bound to
one VAPID public key, and only the holder of the matching private key can push to it. A key
per circle would let an angel follow one VIP only. With the angel's own key, the same angel
can serve several VIPs: every watcher that alerts this angel gets the same key. If it leaks,
the harm is pushes to this one angel, who removes it by joining again (new key).

**Alarms to such an angel** go two ways at once: a Web Push with the alarm text (encrypted
to the subscription, RFC 8291: Apple and Google see only ciphertext) that shows a
notification even when the app is closed, and the same alarm as a gift wrap to the angel's
key, which the app shows in full with the buttons *I am calling* and *Resolved*. Those go
back to the watchers as `ack {alarm, action}` signed by the angel, and every angel of that VIP
hears it, over every channel. A push endpoint that answers "gone" (404/410) is dropped and
the VIP sees that angel as unreachable.

**The VIP's list** shows both kinds, with a mark for the channel (Telegram, app). Names,
alarm choices, *Active* and *Remove* work the same; changes go between peers as now, with the
angel's key in place of a chat id. Push details go to each watcher only from the angel
itself.

**Not yet:** one angel moving between phones (a new join replaces the old key), quiet
hours, "Alert my angels", and the angel's own view of all VIPs they serve (each join adds
the VIP to the list in the app; that list is what the app shows, nothing more).

### Adding a phone

A VIP's devices used to live only in the config file: a new or replaced phone meant pasting its
key into every watcher's config and restarting each one. Now a short link adds one:
`<app>#d=<secret>`, built like the invite links (the details are an encrypted event on the
relays, `t: "device"`). It is valid for 30 minutes and works once.

**Who makes one:** the VIP in Sync (Settings → *My devices* → *Add a phone or tablet*, from a phone that
already works), the owner in Telegram (`/pair` or `/pair NAME`, only from `owner_chats`, for
when no working phone is left), or the laptop agent (Settings → *VIPs…* → *Add phone or tablet*,
with a QR code). The watcher that makes it tells its peers with
`invite_created {device: true, person}`; from a VIP's device the person is that VIP.

**Opening it:** the new phone sends `enroll {secret, name, device}` as for a new VIP; the
watcher sees the secret belongs to a phone link and adds the phone to that VIP in
`phones.json`, without touching the config and without a restart. Each watcher in the link
does this itself; they also tell each other (`phone_changed`), and only one tells the owner
("A phone was added for …"), with the same hold as alarms.

**The list:** `watching` carries `phones [{key, name, at, seen}]` to the VIP's own devices;
`seen` is when that watcher last heard from the device (any message), so an old one stands out. In Sync
and in the agent's *VIPs* window (every VIP of that watcher, with their devices) the VIP renames a device or removes a lost one
(`phone_changed {key, name, removed}`); the newest change per device wins, and the last device
cannot be removed. A removed device is no longer accepted: its alarms and check-ins are
ignored, and it gets no answer to a ping.

**Limits:** at most 5 added phones per VIP (the config's own devices do not count); an invited
VIP can add phones only for themselves.

### Offering a server

The other way round from *Protect someone*: the owner of a watcher makes the link
(`<app>#w=<secret>`, `t: "offer"`), with `/offer` in Telegram or *Offer this server…* in the
agent's VIPs window. On the watcher it is a VIP invite on behalf of one of the owner's own VIPs
(one hop still holds), valid 7 days, told to the peers as `invite_created`. In Sync it differs
only for a phone that is already set up: the accepting servers are added to the ones it has,
instead of replacing them, and the app makes a new angel link so that it includes them. The
new server has no angels for this VIP until they join again; the app says so. A device that a
watcher already knows may enrol again only under its own name (an answer that got lost).

### Free and paid

The core stays free: alarms, angels, check-ins, watchers on own devices or in the family.
Paid, typically by the one who protects (loop C):

- a **managed watcher**: always on, no home server or laptop needed. It is one more watcher,
  outbound only like the others; the family's watchers keep working without it, so it adds
  redundancy and is never a single point of failure;
- **SMS and voice calls** to angels (they cost money; a call is the most reliable wake-up);
- later, the **responder network** nearby.

A family subscription of about €5–8 a month, comparable to existing safety and locator apps.

## 9. False alarms

A responder network survives only if alarms are almost always real.

- A cancel window for gestures that can be made by accident (e.g. 10 s, with the PIN).
- Reminders before a missed check-in becomes an alarm (as on Windows).
- Angels first for check-ins; responders at once only for an explicit duress gesture.
- Monthly test alarm per circle, so everyone knows what an alarm looks like and that it arrives.

## 10. Phases

1. **Phone and angels.** iPhone Shortcuts over ntfy (duress gesture, forced access) and a
   walk-me-home timer in the web app; watchers on a home server and an angel's PC; alarms to
   angels; live location via Find My link.
   Done since: watchers avoid duplicate alarms (`peers`, `send_delay_seconds`).
2. **Circles on Nostr:** invite links and QR codes with signed consent, circle config
   published by the VIP, angel mode in the web app with Web Push, signed
   acknowledgements (section 8).
3. **Escalation** to the circle (tier 2), starting with "Alert my angels".
4. **Responder pilot** in one trusted area: level 1 (vouched) responders, coarse alarm, exact
   location after "I'm going", disclosure log.
5. **Native app** (background location, critical alerts), level 2 verification, opening up.

## 11. Open questions

- Which identity check for level 2, and who performs it?
- Legal: processing location and data about victims (data protection), liability towards
  responders and VIPs. Needs advice before phase 3.
- Partnership with emergency services or alarm centres: can an alarm reach them directly?
- A watcher for people without an always-on device: an angel's PC, a ready-made box?
- Funding the SMS fallback, and a push relay for the native app.
- Default radius and number of responders per alarm.
