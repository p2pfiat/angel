# Architecture

Status: draft. Decisions marked **Decided** were agreed during design; everything else is open.
Roadmap phase 1 (MVP) is implemented; see [setup.md](setup.md) for its known limitations.

**Measured (MVP, public relays, one relay down):** cold start to watcher alarm for a duress signal
≈ 1.2 s; a missed check-in alarms at deadline + grace (default 60 s + 10 s).

## 1. Problem and threat model

**Scenarios**

1. *Home invasion with coercion.* Attackers force the victim to unlock a laptop or log in to a self-hosted
   node (e.g. to move funds). The attacker watches the screen and may cut power or internet afterwards.
2. *Street robbery.* Phone taken or victim unable to act. (Phone triggers are a later phase.)

**Attacker assumptions**

- Watches the screen and keyboard during login.
- May unplug devices or cut the home internet *after* the login.
- Does not know whether this system is in use, nor how it is configured.
- Can read the source code (we assume the project is public).

**Non-goals**

- Resisting or delaying the attacker. The user's safety comes first: comply, help will be called.
- Replacing emergency services.

## 2. Overview

```
 Trigger agents               Messaging layer           Watchers (other households)      Contacts
┌──────────────────┐         ┌──────────────┐          ┌─────────────────────────┐
│ Windows agent    │ signed  │ Nostr relays │          │ watcher A (e.g. Umbrel) │──▶ Telegram
│ Umbrel agent     │────────▶│ (public, 3–5)│─────────▶│ watcher B (relative)    │──▶ (later: Signal,
│ (later: phone,   │ events  │              │          │ watcher C (VPS)         │    calls, SMS,
│  BLE button)     │         └──────▲───────┘          └───────────┬─────────────┘    web push)
└──────────────────┘                │   "alarm sent" / acks         │
                                    └───────────────────────────────┘
```

All arrows are **outbound** connections. Only relays accept inbound connections, and they are run by others.

## 3. Silent session check (core flow) — Decided

1. User logs in **or unlocks** the device.
2. The agent immediately publishes `session_start` with a deadline (default 60 s) and context
   (device, coarse/precise location if available, situation tag). **Nothing is shown on screen.**
3. Within the deadline the user performs a **secret action** (e.g. a randomly assigned hotkey), and the
   agent publishes `session_ok`.
4. Watchers track the deadline. No valid `session_ok` in time → alarm.

Why the watcher, not the agent, owns the deadline: the agent runs in the room where the attack happens.
Once `session_start` has left the device, unplugging it can no longer prevent the alarm.

**Duress login (optional):** a second account/password that looks normal but publishes `duress`
immediately — no deadline needed.

**False alarms** cost one phone call: the contact calls, the user gives the code word, done.

## 4. Messaging layer — Decided: Nostr as format, relays as default transport

- Every message is a **signed Nostr event**. Keys are identity: one key per device, so a stolen device
  can be revoked without replacing the person's identity.
- Payloads are encrypted to each recipient (NIP-44) and gift-wrapped (NIP-59) so relays see as little
  metadata as possible.
- Agents publish to several relays; watchers subscribe to several relays.
- Because the signature travels with the event, the same event may later arrive over other transports
  (Tor onion service to a watcher, ntfy, LoRa/Meshtastic as an offline fallback). A watcher accepts any
  validly signed event regardless of how it arrived.

**Alternatives considered:** Matrix (mature group E2EE, but homeserver-bound identity and heavier),
Signal (best as a *delivery* channel, not as a bus), MQTT/ntfy (no identity — would mean re-inventing
Nostr), Tailscale/Headscale (good between watchers, not for contacts), Meshtastic/Reticulum (offline
fallback, limited range).

## 5. Event types (draft)

All events are gift-wrapped (NIP-59): relays see only a kind 1059 event from a throwaway key with a
randomized timestamp. The inner rumor uses provisional kind `4747` and carries a JSON payload
(`{"v":1,"type":"session_start",...}`); the type lives in that payload. Implemented in `proto/`.
`alarm`, `ack` and `heartbeat` are not built yet (acks currently go through Telegram buttons).

| Type | Signed by | Meaning |
|---|---|---|
| `session_start` | device key | Login/unlock happened; includes `deadline`, context |
| `session_ok` | device key | Secret action performed for this session |
| `duress` | device key | Immediate alarm (duress login, panic button) |
| `alarm` | watcher key | Watcher raised an alarm for a person (missed deadline or duress) |
| `ack` | contact key | "I am calling", "I called emergency services", "resolved" |
| `heartbeat` | watcher key | Watcher is alive (lets others notice a dead watcher) |

Events carry an expiration (NIP-40) so relays can drop them.

## 6. Watchers — Decided

- A small Docker container. Runs anywhere that is always on: Umbrel, Raspberry Pi, NAS, VPS, an old
  Android phone (can also place calls/SMS with a SIM).
- **At least one watcher outside the user's home.** A watcher in the same house fails together with it.
- Responsibilities: subscribe to relays, verify signatures, track deadlines, deduplicate, deliver alerts,
  read acknowledgements, publish its own heartbeat.
- Only outbound connections. Telegram replies are read with long polling (`getUpdates`), not webhooks.

**Coordination between watchers:** every watcher detects the missed deadline. After a short random delay,
the first one publishes `alarm`; others see it and suppress duplicates on shared channels. Recipients
also deduplicate on the alarm id.

**Failure modes**

| Watcher misbehaves by… | Effect | Mitigation |
|---|---|---|
| Forging an OK | Impossible | OK must be signed by the device key |
| Staying silent | One alarm path lost | Multiple independent watchers |
| Raising false alarms | Annoyance | Alarm is signed by the watcher → attributable; rate limits |
| Going offline | One alarm path lost | Watcher heartbeats; others notice |

## 7. Delivery channels

| Channel | Send | Receive acks without inbound ports | Phase |
|---|---|---|---|
| Telegram bot | yes | yes (long polling) | 1 |
| Signal (signal-cli) | yes | yes | later |
| WhatsApp Cloud API | yes (templates, paid) | no (webhook only) | later, send-only |
| Voice call / SMS | via provider or Android SIM | via Android SIM | later |
| Web push to contacts' web app | yes | yes (acks as Nostr events) | later |

## 8. Contact protocol — Decided (wording per user)

The alert message tells contacts what to do, agreed in advance:

> Call me. If I do not answer, or do not use our code word, call emergency services — something is wrong.

- **The code word never appears in any message, config file or this repository.**
- The first contact who acts publishes an `ack` so others know someone is on it.
- Open choice per alarm type: for forced-login alarms a ringing phone may escalate the situation;
  a user may prefer "contact emergency services directly" for that type.

## 9. Circles of trust

| Circle | Who | Receives |
|---|---|---|
| Inner | household, 3–8 close people | full context, precise location, night-time escalation |
| Neighbourhood | nearby trusted people | coarse "incident nearby", no personal data |
| Second degree | contacts of contacts | later phase; rate-limited, coarse |

Circles are signed lists (NIP-51). Joining happens **in person** via QR key exchange.

## 10. Opsec rules

**For the project**

- No default hotkey; it is randomly chosen at install time.
- Process, service and scheduled-task names are user-chosen and innocuous.
- Only `*.example` config in git; no real keys, npubs, relay lists, contacts or code words.

**For users**

- Keys and real config live outside cloud-synced folders (e.g. `%LOCALAPPDATA%`, DPAPI-protected).
- Publishing under a pseudonym is recommended: being publicly linked to "holds funds at home" is the
  real risk, not the code being public.

## 11. Open questions

- How to detect a login on Umbrel reliably (umbreld logs? needs investigation on a device).
- Secret action on Umbrel: hotkey is not available in a browser on someone else's screen — phone shortcut?
- Default deadline (60 s?) and grace behaviour.
- Group encryption: per-recipient gift wraps first; MLS-based groups later?
- Event kind numbers.

## 12. Roadmap

1. **MVP:** Windows agent (logon + unlock trigger, secret hotkey, publish to Nostr) + one watcher
   container with Telegram delivery and ack via long polling.
2. Umbrel login detection; second watcher in another household; duress account.
3. Contacts' web app (view alarm, ack, own panic button, circle management via QR).
4. More channels (Signal, calls/SMS), neighbourhood and second-degree circles, offline fallback.

## Prior art (none covers this combination)

- Phone check-in / dead man's switch apps (daily or hourly check-ins, centralized).
- Self-hosted dead man's switches (Still Alive Bot, deadman, LastSignal, Aeterna): days/weeks scale,
  single instance, no context, no mutual watching.
- Uptime Kuma push monitors: periodic, not event-triggered.
