# warn

A decentralized, silent duress alarm for people who may be forced to unlock their own devices.

**Status:** working MVP — Windows agent with built-in watcher, standalone watcher (Docker), Telegram alerts.
See [docs/setup.md](docs/setup.md) to run it and [docs/architecture.md](docs/architecture.md) for the design.

## The idea in one paragraph

Every time you log in to (or unlock) a device you care about — a laptop, a self-hosted node — a small agent
immediately publishes a signed "session started" event. Nothing appears on screen. Within a short deadline
you perform a secret action (e.g. a hotkey only you know), which publishes a signed "OK". If no OK arrives
in time, independent **watchers** running in *other* households alert your trusted contacts (Telegram first),
who follow a pre-agreed protocol: call you, and if you do not answer or do not use the agreed code word,
contact emergency services.

Someone forcing you to log in sees nothing, cannot tell whether the alarm exists, and cannot stop it by
pulling the plug afterwards: the deadline is enforced elsewhere.

## Design principles

- **No central server.** Watchers are small Docker containers run by members of a circle of trust.
- **Outbound connections only.** Nothing needs open ports; everything works behind NAT.
- **Signed events.** Nostr keys are identity; an "OK" cannot be forged, a watcher's alarm is attributable.
- **Transport-agnostic.** Events are Nostr events; Nostr relays are the default transport, others can be added.
- **Silent by default.** No visible prompt, no recognizable process names, no default hotkey.
- **Complements, never replaces, emergency services.**

## Repository layout

| Path | What | Status |
|---|---|---|
| `docs/` | Architecture, decisions, setup | |
| `proto/` | Shared message format, NIP-59 wrapping | done |
| `watcher/` | Watcher library + standalone service (Docker); also built into the agent | MVP |
| `agent-windows/` | Silent session agent for Windows | MVP |
| `webapp/` | Phone duress app (Nostr) — see `docs/iphone.md`, also for the Back Tap route | MVP |
| `scripts/` | Install helpers | |
| `agent-umbrel/` | Login detector for Umbrel | planned |

## Contributing

Issues and pull requests are welcome. Never commit real keys, npubs, relay lists, contact data or code words — only `*.example` files.

The root of this repository (`index.html`, `app.html` and their scripts) is the published website; the sources are in the folders listed above.

## Download

The phone app runs in the browser: <https://p2pfiat.github.io/angel/app.html>. The Windows agent
is on the [Releases](https://github.com/p2pfiat/angel/releases) page, with its SHA-256 checksum.
The watcher runs in Docker (see `docker-compose.example.yml` and [docs/setup.md](docs/setup.md)).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT),
at your option.
