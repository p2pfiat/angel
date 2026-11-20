//! Deadline logic, free of I/O so every scenario can be tested.

use std::collections::HashMap;

use nostr::prelude::*;
use serde::{Deserialize, Serialize};
use warn_proto::{Context, Message, Received};

/// Accept timeouts in this range; anything else is clamped. Long enough for a walk-home timer.
const MIN_TIMEOUT: u64 = 10;
const MAX_TIMEOUT: u64 = 3 * 3600;
/// Forget handled sessions after this long (gift wraps expire after ~3 days anyway).
const RETENTION: u64 = 4 * 24 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmKind {
    /// Session started, no OK before the deadline.
    MissedCheckIn,
    /// Explicit duress signal.
    Duress,
    /// Harmless test alert requested from the app.
    Test,
    /// OK arrived after an alarm was already sent.
    LateOk,
}

#[derive(Debug, Clone)]
pub struct Alarm {
    /// Short id used in Telegram buttons.
    pub id: String,
    pub person: usize,
    pub kind: AlarmKind,
    pub context: Context,
}

#[derive(Debug, Clone)]
struct Pending {
    person: usize,
    /// When the session started (sender's clock); an `ok` only confirms earlier sessions.
    created: u64,
    due: u64,
    context: Context,
}

/// A duress that waits for its cancel window (a gesture made by mistake can be taken back).
#[derive(Debug, Clone)]
struct PendingDuress {
    /// The id the alarm is recorded under (as for an immediate duress).
    id: String,
    person: usize,
    created: u64,
    due: u64,
    context: Context,
}

/// What survives a restart, so a replay from relays does not re-alarm or forget an OK.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Persisted {
    oks: HashMap<String, u64>,
    alarmed: HashMap<String, u64>,
    duress_seen: HashMap<String, u64>,
}

pub struct Engine {
    grace: u64,
    max_age: u64,
    pending: HashMap<String, Pending>,
    /// Delayed duress signals, by key (the source's prefix + message id).
    pending_duress: HashMap<String, PendingDuress>,
    state: Persisted,
}

fn session_key(device: &PublicKey, session: &str) -> String {
    format!("{}:{}", device.to_hex(), session)
}

impl Engine {
    pub fn new(grace_secs: u64, max_age_secs: u64, state: Persisted) -> Self {
        Self {
            grace: grace_secs,
            max_age: max_age_secs,
            pending: HashMap::new(),
            pending_duress: HashMap::new(),
            state,
        }
    }

    pub fn persisted(&self) -> &Persisted {
        &self.state
    }

    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Feed one authenticated message from a known person. `now` is the watcher's clock.
    pub fn handle(&mut self, now: u64, person: usize, msg: &Received) -> Vec<Alarm> {
        let created = msg.created_at.as_secs();
        match &msg.envelope.msg {
            Message::SessionStart { session, timeout_secs, context } => {
                self.start(now, person, session_key(&msg.sender, session), created, *timeout_secs, context.clone())
            }
            Message::SessionOk { session } => self.ok(now, person, &session_key(&msg.sender, session), created),
            Message::Duress { context } => self.duress(now, person, &msg.id.to_hex(), created, context.clone()),
            Message::Test { context } => self.test(now, person, &msg.id.to_hex(), created, context.clone()),
            // Between watchers; handled by the service.
            Message::AlarmSent { .. }
            | Message::ContactChanged { .. }
            | Message::Watching { .. }
            | Message::Ping { .. }
            | Message::InviteCreated { .. }
            | Message::InviteOk { .. }
            | Message::Enroll { .. }
            | Message::Refused { .. }
            | Message::VipJoined { .. }
            | Message::AngelJoin { .. }
            | Message::AngelOk { .. }
            | Message::AngelMessage { .. }
            | Message::Ack { .. }
            | Message::PhoneChanged { .. }
            | Message::TopicChanged { .. } => vec![],
        }
    }

    /// A clock starts: no `ok` for `key` within `timeout_secs` of `created` → alarm.
    /// `key` must be unique per session and identical for every copy of the message.
    pub fn start(
        &mut self,
        now: u64,
        person: usize,
        key: String,
        created: u64,
        timeout_secs: u64,
        context: Context,
    ) -> Vec<Alarm> {
        let timeout = timeout_secs.clamp(MIN_TIMEOUT, MAX_TIMEOUT);
        // Stale when its deadline passed long ago (e.g. a replay after a long outage).
        let stale = now.saturating_sub(created + timeout) > self.max_age;
        if stale
            || self.pending.contains_key(&key)
            || self.state.oks.contains_key(&key)
            || self.state.alarmed.contains_key(&key)
        {
            return vec![];
        }
        // If we hear about it late (watcher was offline), still give the OK a grace window to
        // arrive before alarming.
        let due = (created + timeout).max(now) + self.grace;
        self.pending.insert(key, Pending { person, created, due, context });
        vec![]
    }

    /// The check-in for one session.
    pub fn ok(&mut self, now: u64, person: usize, key: &str, created: u64) -> Vec<Alarm> {
        let stale = now.saturating_sub(created) > self.max_age;
        if self.state.oks.insert(key.to_string(), now).is_some() {
            return vec![];
        }
        if self.pending.remove(key).is_some() {
            return vec![];
        }
        if self.state.alarmed.contains_key(key) && !stale {
            return vec![Alarm { id: short_id(key, "late"), person, kind: AlarmKind::LateOk, context: Context::default() }];
        }
        vec![]
    }

    /// A check-in from a simple source that cannot name its session (a phone shortcut): it
    /// confirms every session of that source (keys starting with `prefix`) that started
    /// before it, and reports a late check-in for one that already alarmed.
    pub fn ok_source(&mut self, now: u64, person: usize, prefix: &str, created: u64) -> Vec<Alarm> {
        let open: Vec<String> = self
            .pending
            .iter()
            .filter(|(k, p)| k.starts_with(prefix) && p.created <= created)
            .map(|(k, _)| k.clone())
            .collect();
        // Only sessions that alarmed before this OK was sent: an older OK (replayed after a
        // restart) is not a late check-in for a newer session.
        let alarmed: Vec<String> = self
            .state
            .alarmed
            .iter()
            .filter(|(k, &at)| k.starts_with(prefix) && at <= created && !self.state.oks.contains_key(*k))
            .map(|(k, _)| k.clone())
            .collect();
        let mut alarms = Vec::new();
        for key in open.iter().chain(alarmed.iter()) {
            alarms.extend(self.ok(now, person, key, created));
        }
        // A duress still in its cancel window, sent before this OK, is taken back. It is
        // recorded as seen, so a replay of it does not start the window again.
        let cancel: Vec<String> = self
            .pending_duress
            .iter()
            .filter(|(k, d)| k.starts_with(prefix) && d.created <= created && created < d.due)
            .map(|(k, _)| k.clone())
            .collect();
        for key in cancel {
            let d = self.pending_duress.remove(&key).expect("key just listed");
            tracing::info!("duress cancelled by a check-in within its window");
            self.state.duress_seen.insert(d.id, now);
        }
        alarms
    }

    /// A duress signal, from Nostr or from a simpler source (e.g. a phone shortcut via ntfy).
    /// `id` must be unique per signal and identical for every copy of it.
    pub fn duress(&mut self, now: u64, person: usize, id: &str, created: u64, context: Context) -> Vec<Alarm> {
        let stale = now.saturating_sub(created) > self.max_age;
        if stale || self.state.duress_seen.contains_key(id) {
            return vec![];
        }
        self.state.duress_seen.insert(id.to_string(), now);
        vec![Alarm {
            id: short_id(id, "duress"),
            person,
            kind: AlarmKind::Duress,
            context,
        }]
    }

    /// A harmless test alert requested from a VIP's device, deduplicated like `duress`.
    pub fn test(&mut self, now: u64, person: usize, id: &str, created: u64, context: Context) -> Vec<Alarm> {
        let stale = now.saturating_sub(created) > self.max_age;
        if stale || self.state.duress_seen.contains_key(id) {
            return vec![];
        }
        self.state.duress_seen.insert(id.to_string(), now);
        vec![Alarm {
            id: short_id(id, "test"),
            person,
            kind: AlarmKind::Test,
            context,
        }]
    }

    /// A duress from a simple source that alarms only after `delay` seconds, unless an `ok`
    /// from the same source (keys starting with the same prefix) arrives in that window.
    /// `key` = source prefix + message id; `id` is what the alarm is recorded under.
    #[allow(clippy::too_many_arguments)]
    pub fn duress_after(
        &mut self,
        now: u64,
        person: usize,
        key: String,
        id: &str,
        created: u64,
        delay: u64,
        context: Context,
    ) -> Vec<Alarm> {
        if delay == 0 {
            return self.duress(now, person, id, created, context);
        }
        let stale = now.saturating_sub(created) > self.max_age;
        if stale || self.state.duress_seen.contains_key(id) || self.pending_duress.contains_key(&key) {
            return vec![];
        }
        let due = created + delay;
        if due <= now {
            // Heard about it only after its window (watcher was offline): alarm now.
            return self.duress(now, person, id, created, context);
        }
        self.pending_duress.insert(key, PendingDuress { id: id.to_string(), person, created, due, context });
        vec![]
    }

    /// Whether a delayed duress is waiting (for logging).
    pub fn duress_waiting(&self, key: &str) -> bool {
        self.pending_duress.contains_key(key)
    }

    /// Fire alarms for sessions whose deadline passed.
    pub fn tick(&mut self, now: u64) -> Vec<Alarm> {
        let due_duress: Vec<String> =
            self.pending_duress.iter().filter(|(_, d)| d.due <= now).map(|(k, _)| k.clone()).collect();
        let mut fired = Vec::new();
        for key in due_duress {
            let d = self.pending_duress.remove(&key).expect("key just listed");
            fired.extend(self.duress(now, d.person, &d.id, d.created, d.context));
        }
        let due: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(k, _)| k.clone())
            .collect();
        let mut alarms = fired;
        for key in due {
            let p = self.pending.remove(&key).expect("key just listed");
            self.state.alarmed.insert(key.clone(), now);
            alarms.push(Alarm {
                id: short_id(&key, "missed"),
                person: p.person,
                kind: AlarmKind::MissedCheckIn,
                context: p.context,
            });
        }
        alarms
    }

    /// Drop old bookkeeping. Returns true if anything was removed.
    pub fn prune(&mut self, now: u64) -> bool {
        let keep = |t: &mut u64| now.saturating_sub(*t) < RETENTION;
        let before = self.state.oks.len() + self.state.alarmed.len() + self.state.duress_seen.len();
        self.state.oks.retain(|_, t| keep(t));
        self.state.alarmed.retain(|_, t| keep(t));
        self.state.duress_seen.retain(|_, t| keep(t));
        before != self.state.oks.len() + self.state.alarmed.len() + self.state.duress_seen.len()
    }
}

/// Short id (fits in Telegram callback data). The same on every watcher and every build, so
/// watchers can tell each other which alarm they sent.
fn short_id(key: &str, salt: &str) -> String {
    // FNV-1a, 64 bit: fixed by its definition, unlike std's hasher.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in salt.bytes().chain([b':']).chain(key.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use warn_proto::Envelope;

    const T0: u64 = 1_800_000_000;

    fn device() -> PublicKey {
        Keys::generate().public_key()
    }

    fn msg(sender: PublicKey, at: u64, m: Message) -> Received {
        Received {
            sender,
            id: EventId::from_byte_array([0u8; 32]),
            created_at: Timestamp::from_secs(at),
            envelope: Envelope::new(m),
        }
    }

    fn start(sender: PublicKey, at: u64, session: &str) -> Received {
        msg(
            sender,
            at,
            Message::SessionStart {
                session: session.into(),
                timeout_secs: 60,
                context: Context {
                    device: "laptop".into(),
                    trigger: "unlock".into(),
                    ..Default::default()
                },
            },
        )
    }

    fn ok(sender: PublicKey, at: u64, session: &str) -> Received {
        msg(sender, at, Message::SessionOk { session: session.into() })
    }

    fn engine() -> Engine {
        Engine::new(10, 30 * 60, Persisted::default())
    }

    #[test]
    fn ok_in_time_means_no_alarm() {
        let (mut e, d) = (engine(), device());
        e.handle(T0, 0, &start(d, T0, "s1"));
        e.handle(T0 + 20, 0, &ok(d, T0 + 20, "s1"));
        assert!(e.tick(T0 + 3600).is_empty());
    }

    #[test]
    fn missing_ok_alarms_after_deadline_plus_grace() {
        let (mut e, d) = (engine(), device());
        e.handle(T0, 0, &start(d, T0, "s1"));
        assert!(e.tick(T0 + 69).is_empty(), "not before deadline + grace");
        let a = e.tick(T0 + 70);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, AlarmKind::MissedCheckIn);
        assert!(e.tick(T0 + 200).is_empty(), "alarm only once");
    }

    #[test]
    fn duplicate_start_from_other_relay_is_ignored() {
        let (mut e, d) = (engine(), device());
        e.handle(T0, 0, &start(d, T0, "s1"));
        e.handle(T0 + 1, 0, &start(d, T0, "s1"));
        assert_eq!(e.pending_count(), 1);
        assert_eq!(e.tick(T0 + 70).len(), 1);
    }

    #[test]
    fn ok_arriving_before_start_on_replay() {
        let (mut e, d) = (engine(), device());
        e.handle(T0 + 300, 0, &ok(d, T0 + 20, "s1"));
        e.handle(T0 + 300, 0, &start(d, T0, "s1"));
        assert!(e.tick(T0 + 3600).is_empty());
    }

    #[test]
    fn late_delivery_still_alarms_but_waits_for_grace() {
        // Watcher was offline; hears about a 5-minute-old session.
        let (mut e, d) = (engine(), device());
        e.handle(T0 + 300, 0, &start(d, T0, "s1"));
        assert!(e.tick(T0 + 305).is_empty(), "grace window for the OK");
        assert_eq!(e.tick(T0 + 310).len(), 1);
    }

    #[test]
    fn stale_session_is_ignored() {
        // Deadline (60 s) passed more than max age (30 min) ago.
        let (mut e, d) = (engine(), device());
        e.handle(T0 + 32 * 60, 0, &start(d, T0, "s1"));
        assert_eq!(e.pending_count(), 0);
    }

    fn ctx(trigger: &str) -> Context {
        Context { device: "iphone".into(), trigger: trigger.into(), location: None, note: None }
    }

    #[test]
    fn long_timer_survives_a_watcher_restart() {
        // Walk-home timer of 1 h, heard 40 min after it started (watcher was down).
        let mut e = engine();
        e.start(T0 + 40 * 60, 0, "ntfy:p#1:a".into(), T0, 3600, ctx("walk_home"));
        assert!(e.tick(T0 + 3600).is_empty());
        assert_eq!(e.tick(T0 + 3610)[0].kind, AlarmKind::MissedCheckIn);
    }

    #[test]
    fn simple_source_ok_confirms_only_earlier_sessions() {
        let mut e = engine();
        e.start(T0, 0, "ntfy:p#1:a".into(), T0, 90, ctx("app_open"));
        assert!(e.ok_source(T0 + 30, 0, "ntfy:p#1:", T0 + 30).is_empty());
        assert!(e.tick(T0 + 1000).is_empty(), "confirmed");

        // An old OK replayed after a reconnect must not confirm a newer session.
        e.start(T0 + 2000, 0, "ntfy:p#1:b".into(), T0 + 2000, 90, ctx("app_open"));
        e.ok_source(T0 + 2001, 0, "ntfy:p#1:", T0 + 30);
        assert_eq!(e.tick(T0 + 2100).len(), 1, "still alarms");

        // Another source's OK does not count either.
        e.start(T0 + 3000, 0, "ntfy:p#1:c".into(), T0 + 3000, 90, ctx("app_open"));
        e.ok_source(T0 + 3010, 0, "ntfy:p#2:", T0 + 3010);
        assert_eq!(e.tick(T0 + 3100).len(), 1);
    }

    #[test]
    fn alarm_ids_are_the_same_on_every_watcher_and_build() {
        // Watchers tell each other which alarm they sent by this id, so it must never change
        // with the Rust version or platform.
        assert_eq!(short_id("abc", "duress"), "edb43712d813");
        let mut a = engine();
        let mut b = engine();
        let c = ctx("duress_shortcut");
        assert_eq!(a.duress(T0, 0, "x", T0, c.clone())[0].id, b.duress(T0, 0, "x", T0, c)[0].id);
    }

    #[test]
    fn delayed_duress_is_cancelled_by_ok_in_its_window() {
        let mut e = engine();
        let c = ctx("duress_shortcut");
        assert!(e.duress_after(T0, 0, "ntfy:p#1:a".into(), "ntfy:a", T0, 20, c.clone()).is_empty());
        assert!(e.tick(T0 + 10).is_empty(), "waits");
        e.ok_source(T0 + 15, 0, "ntfy:p#1:", T0 + 15);
        assert!(e.tick(T0 + 100).is_empty(), "cancelled");
        // A replay of the same duress does not start again.
        assert!(e.duress_after(T0 + 101, 0, "ntfy:p#1:a".into(), "ntfy:a", T0, 20, c).is_empty());
        assert!(e.tick(T0 + 200).is_empty());
    }

    #[test]
    fn delayed_duress_alarms_after_its_window() {
        let mut e = engine();
        let c = ctx("duress_shortcut");
        e.duress_after(T0, 0, "ntfy:p#1:a".into(), "ntfy:a", T0, 20, c.clone());
        assert!(e.tick(T0 + 19).is_empty());
        assert_eq!(e.tick(T0 + 20)[0].kind, AlarmKind::Duress);
        // An OK after the window, an OK sent before the duress, or another source's OK do
        // not cancel.
        e.duress_after(T0 + 100, 0, "ntfy:p#1:b".into(), "ntfy:b", T0 + 100, 20, c.clone());
        e.ok_source(T0 + 101, 0, "ntfy:p#1:", T0 + 99);
        e.ok_source(T0 + 102, 0, "ntfy:p#2:", T0 + 102);
        assert_eq!(e.tick(T0 + 120).len(), 1);
        // Heard only after the window: alarms at once.
        assert_eq!(e.duress_after(T0 + 300, 0, "ntfy:p#1:c".into(), "ntfy:c", T0 + 250, 20, c).len(), 1);
    }

    #[test]
    fn replayed_older_ok_is_not_a_late_ok() {
        // After a restart the watcher reads recent messages again: an OK sent before a
        // session that later alarmed must not count as that session's late check-in.
        let mut e = engine();
        e.start(T0 + 50, 0, "ntfy:p#1:b".into(), T0 + 50, 90, ctx("app_open"));
        assert_eq!(e.tick(T0 + 150).len(), 1);
        assert!(e.ok_source(T0 + 700, 0, "ntfy:p#1:", T0 + 30).is_empty());
        // A real late OK still is.
        assert_eq!(e.ok_source(T0 + 701, 0, "ntfy:p#1:", T0 + 160).len(), 1);
    }

    #[test]
    fn simple_source_late_ok_is_reported_once() {
        let mut e = engine();
        e.start(T0, 0, "ntfy:p#1:a".into(), T0, 90, ctx("app_open"));
        assert_eq!(e.tick(T0 + 100).len(), 1);
        let late = e.ok_source(T0 + 120, 0, "ntfy:p#1:", T0 + 120);
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].kind, AlarmKind::LateOk);
        assert!(e.ok_source(T0 + 130, 0, "ntfy:p#1:", T0 + 130).is_empty());
    }

    #[test]
    fn ok_from_another_device_does_not_count() {
        let (mut e, d, other) = (engine(), device(), device());
        e.handle(T0, 0, &start(d, T0, "s1"));
        e.handle(T0 + 5, 0, &ok(other, T0 + 5, "s1"));
        assert_eq!(e.tick(T0 + 70).len(), 1);
    }

    #[test]
    fn late_ok_after_alarm_is_reported_once() {
        let (mut e, d) = (engine(), device());
        e.handle(T0, 0, &start(d, T0, "s1"));
        e.tick(T0 + 70);
        let a = e.handle(T0 + 90, 0, &ok(d, T0 + 90, "s1"));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, AlarmKind::LateOk);
        assert!(e.handle(T0 + 91, 0, &ok(d, T0 + 90, "s1")).is_empty());
    }

    #[test]
    fn duress_alarms_immediately_once() {
        let (mut e, d) = (engine(), device());
        let m = msg(d, T0, Message::Duress { context: Context::default() });
        assert_eq!(e.handle(T0, 0, &m)[0].kind, AlarmKind::Duress);
        assert!(e.handle(T0 + 1, 0, &m).is_empty());
    }

    #[test]
    fn restart_does_not_realarm_on_replay() {
        let (mut e, d) = (engine(), device());
        e.handle(T0, 0, &start(d, T0, "s1"));
        e.tick(T0 + 70);
        let saved = serde_json::to_string(e.persisted()).unwrap();
        let mut e2 = Engine::new(10, 30 * 60, serde_json::from_str(&saved).unwrap());
        e2.handle(T0 + 100, 0, &start(d, T0, "s1"));
        assert!(e2.tick(T0 + 1000).is_empty());
    }

    #[test]
    fn absurd_timeout_is_clamped() {
        let (mut e, d) = (engine(), device());
        let m = msg(
            d,
            T0,
            Message::SessionStart {
                session: "s1".into(),
                timeout_secs: 999_999,
                context: Context::default(),
            },
        );
        e.handle(T0, 0, &m);
        assert_eq!(e.tick(T0 + MAX_TIMEOUT + 10).len(), 1);
    }

    #[test]
    fn duress_from_simple_source_alarms_once_and_ignores_stale() {
        let mut e = Engine::new(10, 1800, Persisted::default());
        let ctx = Context { device: "phone".into(), trigger: "duress_shortcut".into(), location: None, note: None };
        assert_eq!(e.duress(T0 + 5, 0, "ntfy:a", T0, ctx.clone()).len(), 1);
        assert!(e.duress(T0 + 6, 0, "ntfy:a", T0, ctx.clone()).is_empty(), "same id again");
        assert!(e.duress(T0 + 4000, 0, "ntfy:b", T0, ctx).is_empty(), "older than max age");
    }

    #[test]
    fn test_alert_fires_once_and_ignores_replay() {
        let (mut e, d) = (engine(), device());
        let m = msg(d, T0, Message::Test { context: ctx("test_app") });
        let alarms = e.handle(T0, 0, &m);
        assert_eq!(alarms.len(), 1);
        assert_eq!(alarms[0].kind, AlarmKind::Test);
        assert!(e.handle(T0 + 1, 0, &m).is_empty(), "replay ignored");
    }
}
