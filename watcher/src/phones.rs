//! A VIP's devices beyond the config file (design.md, "Adding a phone"): phones added with a
//! pairing link, and devices taken off the list. Stored in `phones.json` in the data folder.
//!
//! The config file (or `vips.json`, for invited VIPs) gives each VIP's first devices; this file
//! adds to that and can take any of them away again, without editing the config or
//! restarting. Watchers of the same VIP tell each other every change; the newest per device wins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nostr::prelude::*;
use serde::{Deserialize, Serialize};

/// Most phones a VIP may add with links (the config's own devices do not count).
pub const MAX_ADDED: usize = 5;
/// Longest name of a phone.
pub const MAX_NAME_CHARS: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneEntry {
    pub name: String,
    /// When this change was made (the sender's clock).
    pub at: u64,
    /// Taken off the list: nothing from it is accepted any more.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Phones {
    /// Person -> device key (hex) -> entry.
    #[serde(default)]
    people: HashMap<String, HashMap<String, PhoneEntry>>,
    /// Device key (hex) -> when this watcher last heard from it. Only this watcher's view.
    #[serde(default)]
    seen: HashMap<String, u64>,
    #[serde(skip)]
    path: PathBuf,
}

/// Save "last seen" at most this often per device (it changes with every message).
const SEEN_SAVE_GAP: u64 = 600;

/// A device key from the config (npub or hex) as hex, or None if it is not a key.
pub fn hex_key(device: &str) -> Option<String> {
    PublicKey::parse(device).ok().map(|k| k.to_hex())
}

/// A device's name as shown when it has none (one put in the config by hand).
pub fn short_name(hex: &str) -> String {
    match PublicKey::from_hex(hex).ok().and_then(|k| k.to_bech32().ok()) {
        Some(n) => format!("added by hand ({}…)", &n[..12]),
        None => "added by hand".into(),
    }
}

impl Phones {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("phones.json");
        let mut p: Phones = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        p.path = path;
        p
    }

    fn save(&self) {
        let text = match serde_json::to_string_pretty(self) {
            Ok(t) => t,
            Err(e) => return tracing::error!("saving phones failed: {e:#}"),
        };
        if let Err(e) = crate::service::write_private(&self.path, &text) {
            tracing::error!("saving phones failed: {e:#}");
        }
    }

    /// Record a change; older than what is known changes nothing. Returns whether it changed.
    pub fn apply(&mut self, person: &str, key: &str, entry: PhoneEntry) -> bool {
        let list = self.people.entry(person.to_string()).or_default();
        if let Some(old) = list.get(key) {
            if old.at > entry.at || *old == entry {
                return false;
            }
        }
        list.insert(key.to_string(), entry);
        self.save();
        true
    }

    /// A device sent something at `at`.
    pub fn saw(&mut self, key: &str, at: u64) {
        let old = self.seen.get(key).copied().unwrap_or(0);
        if at <= old {
            return;
        }
        self.seen.insert(key.to_string(), at);
        if at >= old + SEEN_SAVE_GAP {
            self.save();
        }
    }

    pub fn get(&self, person: &str, key: &str) -> Option<&PhoneEntry> {
        self.people.get(person)?.get(key)
    }

    /// Phones added with a link and still on the list.
    pub fn added(&self, person: &str, base: &[String]) -> usize {
        let base: Vec<String> = base.iter().filter_map(|d| hex_key(d)).collect();
        self.people
            .get(person)
            .map(|l| l.iter().filter(|(k, e)| !e.removed && !base.contains(k)).count())
            .unwrap_or(0)
    }

    /// The devices to accept for this person (hex): `base` (from the config) without the
    /// removed ones, plus the added ones. A person without base devices (an invited VIP who was
    /// removed) has none.
    pub fn devices(&self, person: &str, base: &[String]) -> Vec<String> {
        if base.is_empty() {
            return vec![];
        }
        let list = self.people.get(person);
        let removed = |k: &str| list.and_then(|l| l.get(k)).is_some_and(|e| e.removed);
        let mut out: Vec<String> = base.iter().filter_map(|d| hex_key(d)).filter(|k| !removed(k)).collect();
        if let Some(l) = list {
            let mut extra: Vec<(&String, &PhoneEntry)> = l.iter().filter(|(k, e)| !e.removed && !out.contains(k)).collect();
            extra.sort_by_key(|(k, e)| (e.at, (*k).clone()));
            out.extend(extra.into_iter().map(|(k, _)| k.clone()));
        }
        out
    }

    /// The list for the app.
    pub fn list(&self, person: &str, base: &[String]) -> Vec<warn_proto::Phone> {
        self.devices(person, base)
            .into_iter()
            .map(|k| {
                let e = self.get(person, &k);
                let name = e.map(|e| e.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| short_name(&k));
                let seen = self.seen.get(&k).copied().unwrap_or(0);
                warn_proto::Phone { name, at: e.map(|e| e.at).unwrap_or(0), seen, key: k }
            })
            .collect()
    }
}

/// A phone's name as given, cleaned up.
pub fn clean_name(name: &str) -> String {
    let n: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    if n.is_empty() {
        "phone".into()
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_removed_and_newest_wins() {
        let dir = std::env::temp_dir().join(format!("warn-phones-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = Phones::load(&dir);
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let base = vec![a.to_bech32().unwrap()];
        assert_eq!(p.devices("M", &base), vec![a.to_hex()]);
        assert!(p.apply("M", &b.to_hex(), PhoneEntry { name: "iPhone".into(), at: 10, removed: false }));
        assert_eq!(p.devices("M", &base), vec![a.to_hex(), b.to_hex()]);
        assert_eq!(p.added("M", &base), 1);
        // The config's device taken off the list.
        assert!(p.apply("M", &a.to_hex(), PhoneEntry { name: String::new(), at: 20, removed: true }));
        assert_eq!(p.devices("M", &base), vec![b.to_hex()]);
        // An older change (from a slow watcher) does not undo it.
        assert!(!p.apply("M", &a.to_hex(), PhoneEntry { name: String::new(), at: 15, removed: false }));
        // Kept across a restart; names for the app.
        let p = Phones::load(&dir);
        let list = p.list("M", &base);
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].name.as_str(), list[0].at, list[0].seen), ("iPhone", 10, 0));
        let mut p = p;
        p.saw(&b.to_hex(), 50);
        p.saw(&b.to_hex(), 40);
        assert_eq!(p.list("M", &base)[0].seen, 50, "the newest wins");
        // A removed invited VIP (no base devices) has none.
        assert!(p.devices("M", &[]).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
