//! A VIP's angels as the watchers know them: one entry per Telegram chat or per angel key,
//! with a friendly name and which alarms it gets.
//!
//! Angels come from three places: `telegram_chats` in the config (added as entries named
//! "Chat <id>", which can then be renamed or switched off), the Telegram join link
//! (`t.me/<bot>?start=<code>`), and the app's angel link (an angel with their own key, reached
//! by Web Push and Nostr; design.md, "Angels with their own key"). Every change (join, `/stop`,
//! an edit in the agent's Angels window or the VIP's app) is told to the other watchers over
//! Nostr (`ContactChanged`); the newest change per angel wins, so all watchers end up the
//! same. Push details are not shared: each watcher gets them from the angel. Stored in
//! `contacts.json` in the data folder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bitcoin_hashes::{sha256, Hash};
use serde::{Deserialize, Serialize};

use crate::config::{Config, PersonConfig};
use crate::engine::AlarmKind;
pub use warn_proto::PushSub;

fn yes() -> bool {
    true
}

/// Who an angel is: a Telegram chat, or their own key (hex).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AngelId {
    Chat(i64),
    Key(String),
}

impl AngelId {
    /// The key in contacts.json: the chat id as before, or `k:<hex>`.
    fn store_key(&self) -> String {
        match self {
            AngelId::Chat(c) => c.to_string(),
            AngelId::Key(k) => format!("k:{k}"),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.strip_prefix("k:") {
            Some(k) => Some(AngelId::Key(k.to_string())),
            None => s.parse().ok().map(AngelId::Chat),
        }
    }

    /// As in `ContactChanged` and `Angel`: (chat, key).
    pub fn parts(&self) -> (i64, Option<String>) {
        match self {
            AngelId::Chat(c) => (*c, None),
            AngelId::Key(k) => (0, Some(k.clone())),
        }
    }

    pub fn from_parts(chat: i64, key: Option<String>) -> Self {
        match key {
            Some(k) => AngelId::Key(k),
            None => AngelId::Chat(chat),
        }
    }
}

impl std::fmt::Display for AngelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AngelId::Chat(c) => write!(f, "chat {c}"),
            AngelId::Key(k) => write!(f, "angel key {}…", &k[..k.len().min(12)]),
        }
    }
}

/// One angel's latest state. Changes are ordered by `at` (the changer's clock).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub active: bool,
    pub at: u64,
    /// Gets "did not check in" alarms. Duress alarms always go to every active angel.
    #[serde(default = "yes")]
    pub missed: bool,
    /// Gets "checked in late" notices.
    #[serde(default = "yes")]
    pub late_ok: bool,
    /// Taken off the list by the VIP: gets nothing and is not shown. Kept (rather than deleted)
    /// so the removal wins over older copies on other watchers; the join link adds them again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// Web Push for an angel with their own key; only from the angel, never from a peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<PushSub>,
}

impl Entry {
    pub fn new(name: &str, at: u64) -> Self {
        Self { name: name.to_string(), active: true, at, missed: true, late_ok: true, removed: false, push: None }
    }

    /// Whether this angel gets an alarm of this kind.
    pub fn gets(&self, kind: AlarmKind) -> bool {
        self.active
            && !self.removed
            && match kind {
                AlarmKind::Duress | AlarmKind::Test => true,
                AlarmKind::MissedCheckIn => self.missed,
                AlarmKind::LateOk => self.late_ok,
            }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Contacts {
    /// Person name -> angel (chat id, or `k:<hex>`) -> entry.
    persons: HashMap<String, HashMap<String, Entry>>,
    #[serde(skip)]
    path: PathBuf,
}

impl Contacts {
    /// Load, and add the chats from the config that are not known yet. They are added with
    /// time 0, so any real change (from any watcher) wins over them.
    pub fn load(cfg: &Config) -> Self {
        Self::load_from(&cfg.data_dir, &cfg.persons)
    }

    pub fn load_from(data_dir: &Path, persons: &[PersonConfig]) -> Self {
        let path = data_dir.join("contacts.json");
        let mut c: Contacts = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        c.path = path;
        for p in persons {
            let chats = c.persons.entry(p.name.clone()).or_default();
            for chat in &p.telegram_chats {
                chats.entry(chat.to_string()).or_insert_with(|| Entry::new(&format!("Chat {chat}"), 0));
            }
        }
        c
    }

    fn save(&self) -> Result<()> {
        crate::service::write_private(&self.path, &serde_json::to_string_pretty(self)?)
    }

    /// Apply a change if it is newer than what we know. Returns whether anything changed. A
    /// change without push details keeps the ones we have (peers never send them).
    pub fn apply(&mut self, person: &str, id: &AngelId, mut entry: Entry) -> bool {
        let angels = self.persons.entry(person.to_string()).or_default();
        let old = angels.get(&id.store_key());
        if old.is_some_and(|e| e.at >= entry.at) {
            return false;
        }
        if entry.push.is_none() {
            entry.push = old.and_then(|e| e.push.clone());
        }
        angels.insert(id.store_key(), entry);
        if let Err(e) = self.save() {
            tracing::error!("saving contacts failed: {e:#}");
        }
        true
    }

    /// Forget an angel's push details (the push service said the subscription is gone).
    pub fn drop_push(&mut self, person: &str, key: &str) {
        if let Some(e) = self.persons.get_mut(person).and_then(|m| m.get_mut(&AngelId::Key(key.to_string()).store_key())) {
            e.push = None;
            if let Err(e) = self.save() {
                tracing::error!("saving contacts failed: {e:#}");
            }
        }
    }

    fn angels(&self, person: &str) -> impl Iterator<Item = (AngelId, &Entry)> {
        self.persons.get(person).into_iter().flatten().filter_map(|(k, e)| AngelId::parse(k).map(|id| (id, e)))
    }

    /// Every active Telegram angel of this person (for buttons, join requests and notices).
    pub fn chats(&self, person: &PersonConfig) -> Vec<i64> {
        self.chats_for(person, AlarmKind::Duress)
    }

    /// The Telegram angels that get this kind of alarm.
    pub fn chats_for(&self, person: &PersonConfig, kind: AlarmKind) -> Vec<i64> {
        let mut out: Vec<i64> = self
            .angels(&person.name)
            .filter(|(_, e)| e.gets(kind))
            .filter_map(|(id, _)| match id {
                AngelId::Chat(c) => Some(c),
                AngelId::Key(_) => None,
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// The angels with their own key that get this kind of alarm: (key hex, push details).
    pub fn keys_for(&self, person: &str, kind: AlarmKind) -> Vec<(String, Option<PushSub>)> {
        let mut out: Vec<(String, Option<PushSub>)> = self
            .angels(person)
            .filter(|(_, e)| e.gets(kind))
            .filter_map(|(id, e)| match id {
                AngelId::Key(k) => Some((k, e.push.clone())),
                AngelId::Chat(_) => None,
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn get(&self, person: &str, id: &AngelId) -> Option<&Entry> {
        self.persons.get(person).and_then(|m| m.get(&id.store_key()))
    }

    /// Any stored Web Push subscription for an angel with this key (reused when the angel is
    /// automatically added to a new VIP).
    pub fn angel_push(&self, key: &str) -> Option<PushSub> {
        let store_key = AngelId::Key(key.to_string()).store_key();
        self.persons
            .values()
            .filter_map(|m| m.get(&store_key))
            .filter(|e| !e.removed && e.push.is_some())
            .max_by_key(|e| e.at)
            .and_then(|e| e.push.clone())
    }

    /// An active app angel key matching `name` across any watched person, with its push details.
    pub fn key_for_name(&self, name: &str) -> Option<(String, Option<PushSub>)> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        self.persons
            .values()
            .flat_map(|m| m.iter())
            .filter_map(|(k, e)| match AngelId::parse(k) {
                Some(AngelId::Key(hex)) if e.active && !e.removed && e.name.eq_ignore_ascii_case(name) => {
                    Some((hex, e))
                }
                _ => None,
            })
            .max_by_key(|(_, e)| (e.push.is_some(), e.at))
            .map(|(hex, e)| (hex, e.push.clone()))
    }

    /// Active Telegram chat ids matching `name` across any watched person.
    pub fn chats_for_name(&self, name: &str) -> Vec<i64> {
        let name = name.trim();
        if name.is_empty() {
            return vec![];
        }
        let mut out: Vec<i64> = self
            .persons
            .values()
            .flat_map(|m| m.iter())
            .filter_map(|(k, e)| match AngelId::parse(k) {
                Some(AngelId::Chat(c)) if e.active && !e.removed && e.name.eq_ignore_ascii_case(name) => Some(c),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// All angels of this person, active or not but not removed, sorted by name (for the
    /// Angels window and the app).
    pub fn list(&self, person: &str) -> Vec<(AngelId, Entry)> {
        let mut out: Vec<(AngelId, Entry)> =
            self.angels(person).filter(|(_, e)| !e.removed).map(|(id, e)| (id, e.clone())).collect();
        out.sort_by(|a, b| (!a.1.active, a.1.name.to_lowercase()).cmp(&(!b.1.active, b.1.name.to_lowercase())));
        out
    }
}

/// The secret in a person's join link. Derived from the bot token, so every watcher sharing
/// the bot knows it without storing anything; `round` (the person's `invite_round`) changes it
/// when a link leaked. Telegram allows [A-Za-z0-9_-], at most 64 characters.
pub fn invite_code(bot_token: &str, person: &str, round: u32) -> String {
    let h = sha256::Hash::hash(format!("warn-invite:{round}:{person}:{bot_token}").as_bytes());
    h.to_string()[..20].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(chats: Vec<i64>) -> PersonConfig {
        toml::from_str(&format!("name = \"A\"\ndevices = []\ntelegram_chats = {chats:?}")).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("warn-contacts-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const C5: AngelId = AngelId::Chat(5);
    const C7: AngelId = AngelId::Chat(7);

    #[test]
    fn newer_change_wins_and_replays_do_nothing() {
        let dir = temp_dir("a");
        let p = person(vec![1]);
        let mut c = Contacts::load_from(&dir, std::slice::from_ref(&p));
        assert!(c.apply("A", &C5, Entry::new("Bo", 100)));
        assert!(!c.apply("A", &C5, Entry::new("Bo", 100)), "replay");
        assert_eq!(c.chats(&p), vec![1, 5]);
        assert!(c.apply("A", &C5, Entry { active: false, ..Entry::new("Bo", 200) }));
        assert!(!c.apply("A", &C5, Entry::new("Bo", 150)), "older join after the leave");
        assert_eq!(c.chats(&p), vec![1]);
        // Survives a restart.
        assert_eq!(Contacts::load_from(&dir, std::slice::from_ref(&p)).chats(&p), vec![1]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn configured_chats_become_angels_that_can_be_renamed_and_switched_off() {
        let dir = temp_dir("b");
        let p = person(vec![7]);
        let mut c = Contacts::load_from(&dir, std::slice::from_ref(&p));
        assert_eq!(c.get("A", &C7).unwrap().name, "Chat 7");
        c.apply("A", &C7, Entry { missed: false, ..Entry::new("Partner", 10) });
        // Loading again does not reset the change to the config's defaults.
        let c = Contacts::load_from(&dir, std::slice::from_ref(&p));
        assert_eq!(c.get("A", &C7).unwrap().name, "Partner");
        assert!(c.chats_for(&p, AlarmKind::MissedCheckIn).is_empty());
        assert_eq!(c.chats_for(&p, AlarmKind::LateOk), vec![7]);
        assert_eq!(c.chats_for(&p, AlarmKind::Duress), vec![7], "duress always");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_files_without_choices_get_everything() {
        let e: Entry = serde_json::from_str(r#"{"name":"Bo","active":true,"at":1}"#).unwrap();
        assert!(e.missed && e.late_ok);
        let off = Entry { active: false, ..e };
        assert!(!off.gets(AlarmKind::Duress));
    }

    #[test]
    fn removed_angels_get_nothing_and_are_not_listed() {
        let dir = temp_dir("c");
        let p = person(vec![7]);
        let mut c = Contacts::load_from(&dir, std::slice::from_ref(&p));
        c.apply("A", &C7, Entry { removed: true, ..Entry::new("Bo", 10) });
        assert!(c.chats_for(&p, AlarmKind::Duress).is_empty());
        assert!(c.list("A").is_empty());
        // The config does not bring a removed chat back.
        let c = Contacts::load_from(&dir, std::slice::from_ref(&p));
        assert!(c.list("A").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn angels_with_a_key_keep_their_push_details() {
        let dir = temp_dir("d");
        let p = person(vec![7]);
        let mut c = Contacts::load_from(&dir, std::slice::from_ref(&p));
        let leen = AngelId::Key("ab".repeat(32));
        let push = PushSub {
            endpoint: "https://push.example/1".into(),
            p256dh: "p".into(),
            auth: "a".into(),
            vapid_private: "d".into(),
            vapid_public: "q".into(),
        };
        c.apply("A", &leen, Entry { push: Some(push.clone()), ..Entry::new("Sam", 10) });
        // A peer renames her: no push details in that change, ours stay.
        c.apply("A", &leen, Entry { missed: false, ..Entry::new("Sam B", 20) });
        let mut c = Contacts::load_from(&dir, std::slice::from_ref(&p));
        assert_eq!(c.keys_for("A", AlarmKind::Duress), vec![("ab".repeat(32), Some(push))]);
        assert!(c.keys_for("A", AlarmKind::MissedCheckIn).is_empty());
        assert_eq!(c.chats(&p), vec![7], "Telegram list unchanged");
        assert_eq!(c.list("A").len(), 2);
        assert_eq!(c.get("A", &leen).unwrap().name, "Sam B");
        c.drop_push("A", &"ab".repeat(32));
        assert_eq!(c.keys_for("A", AlarmKind::Duress), vec![("ab".repeat(32), None)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn looks_up_existing_angel_keys_and_chats_by_name() {
        let dir = temp_dir("e");
        let p = person(vec![]);
        let mut c = Contacts::load_from(&dir, std::slice::from_ref(&p));
        let key = "cd".repeat(32);
        let push = PushSub {
            endpoint: "https://push.example/2".into(),
            p256dh: "p".into(),
            auth: "a".into(),
            vapid_private: "d".into(),
            vapid_public: "q".into(),
        };
        c.apply("A", &AngelId::Key(key.clone()), Entry { push: Some(push.clone()), ..Entry::new("Alex", 10) });
        c.apply("A", &C5, Entry::new("Alex", 10));
        c.apply("A", &C7, Entry { active: false, ..Entry::new("Alex", 10) });
        assert_eq!(c.angel_push(&key), Some(push.clone()));
        assert_eq!(c.key_for_name("alex"), Some((key, Some(push))));
        assert_eq!(c.chats_for_name("ALEX"), vec![5]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invite_code_is_fixed_secret_and_rotates() {
        let a = invite_code("123:abc", "Alex", 0);
        assert_eq!(a.len(), 20);
        assert_eq!(a, invite_code("123:abc", "Alex", 0));
        assert_ne!(a, invite_code("123:abc", "Alex", 1));
        assert_ne!(a, invite_code("123:xyz", "Alex", 0));
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
