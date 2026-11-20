//! App-registered secret `ntfy.sh` topics per VIP (for iPhone Shortcuts), persisted in
//! `topics.json` inside the watcher's data directory.
//!
//! Unlike static `duress_ntfy` entries in `config.toml` (which may point at self-hosted servers),
//! topics accepted dynamically from the app over Nostr are restricted strictly to
//! `https://ntfy.sh/<topic>` with a long random topic name, preventing SSRF against LAN or
//! local services when `open_signup` is enabled.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Only public `https://ntfy.sh/<topic>` URLs are accepted dynamically from apps.
pub const PUBLIC_NTFY_PREFIX: &str = "https://ntfy.sh/";
/// Shortest allowed topic segment (20 chars matches the app's `MIN_TOPIC_LEN`).
pub const MIN_TOPIC_LEN: usize = 20;
/// Longest allowed topic segment.
pub const MAX_TOPIC_LEN: usize = 64;
/// Upper bound on cancel delay accepted from an app.
pub const MAX_CANCEL_SECONDS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicEntry {
    pub url: String,
    pub device: String,
    pub cancel_seconds: u64,
    /// When this change was made (sender's clock).
    pub at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Topics {
    /// Person name -> topic entry.
    #[serde(default)]
    persons: HashMap<String, TopicEntry>,
    #[serde(skip)]
    path: PathBuf,
}

/// Validate a shortcut topic URL sent by a VIP's app (never echoes the secret URL on failure).
pub fn validate_app_topic_url(url: &str) -> Result<String, &'static str> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let Some(topic) = trimmed.strip_prefix(PUBLIC_NTFY_PREFIX) else {
        return Err("topic URL must use https://ntfy.sh/");
    };
    if !(MIN_TOPIC_LEN..=MAX_TOPIC_LEN).contains(&topic.len()) {
        return Err("topic name length is out of range (must be 20..=64 chars)");
    }
    if !topic.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err("topic name may contain only ASCII letters, digits, '-' or '_'");
    }
    Ok(trimmed.to_string())
}

impl Topics {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("topics.json");
        let mut t: Topics = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        t.path = path;
        t
    }

    fn save(&self) {
        let text = match serde_json::to_string_pretty(self) {
            Ok(t) => t,
            Err(e) => return tracing::error!("saving topics failed: {e:#}"),
        };
        if let Err(e) = crate::service::write_private(&self.path, &text) {
            tracing::error!("saving topics failed: {e:#}");
        }
    }

    pub fn get(&self, person: &str) -> Option<&TopicEntry> {
        self.persons.get(person)
    }

    /// Record a topic update; older or identical entries do nothing. Returns whether it changed.
    pub fn apply(&mut self, person: &str, entry: TopicEntry) -> bool {
        if let Some(old) = self.persons.get(person) {
            if old.at > entry.at || *old == entry {
                return false;
            }
        }
        self.persons.insert(person.to_string(), entry);
        self.save();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_only_public_ntfy_topics() {
        let valid = "https://ntfy.sh/s0123456789abcdef0123456789abcdef01";
        assert_eq!(validate_app_topic_url(valid), Ok(valid.to_string()));
        assert_eq!(validate_app_topic_url(&format!("  {valid}/  ")), Ok(valid.to_string()));
        assert_eq!(validate_app_topic_url("   "), Ok(String::new()));

        // Reject SSRF targets and other schemes/hosts.
        assert!(validate_app_topic_url("http://ntfy.sh/s0123456789abcdef0123456789abcdef01").is_err());
        assert!(validate_app_topic_url("https://127.0.0.1/abcdefghijklmnopqrstuv").is_err());
        assert!(validate_app_topic_url("https://169.254.169.254/abcdefghijklmnopqrstuv").is_err());
        assert!(validate_app_topic_url("https://evil.example/abcdefghijklmnopqrstuv").is_err());
        assert!(validate_app_topic_url("https://ntfy.sh.evil.com/abcdefghijklmnopqrstuv").is_err());
        assert!(validate_app_topic_url("https://user:pass@ntfy.sh/abcdefghijklmnopqrstuv").is_err());
        assert!(validate_app_topic_url("https://ntfy.sh:8443/abcdefghijklmnopqrstuv").is_err());

        // Reject subpaths, queries, fragments, and bad lengths/characters.
        assert!(validate_app_topic_url("https://ntfy.sh/abcdefghijklmnopqrstuv/extra").is_err());
        assert!(validate_app_topic_url("https://ntfy.sh/abcdefghijklmnopqrstuv?foo=1").is_err());
        assert!(validate_app_topic_url("https://ntfy.sh/abcdefghijklmnopqrstuv#frag").is_err());
        assert!(validate_app_topic_url("https://ntfy.sh/tooshort").is_err());
        assert!(validate_app_topic_url(&format!("https://ntfy.sh/{}", "a".repeat(65))).is_err());
        assert!(validate_app_topic_url("https://ntfy.sh/abcdefghijklmnopqr$t").is_err());
    }

    #[test]
    fn newest_wins_and_persists() {
        let dir = std::env::temp_dir().join(format!("warn-topics-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut t = Topics::load(&dir);
        let url1 = "https://ntfy.sh/topic_one_0123456789ab".to_string();
        let url2 = "https://ntfy.sh/topic_two_0123456789ab".to_string();
        assert!(t.apply(
            "Alex",
            TopicEntry { url: url1.clone(), device: "iphone".into(), cancel_seconds: 20, at: 100 }
        ));
        // Duplicate at same timestamp does not rewrite.
        assert!(!t.apply(
            "Alex",
            TopicEntry { url: url1.clone(), device: "iphone".into(), cancel_seconds: 20, at: 100 }
        ));
        // Older message from a slow peer is ignored.
        assert!(!t.apply(
            "Alex",
            TopicEntry { url: url2.clone(), device: "iphone".into(), cancel_seconds: 20, at: 90 }
        ));
        assert_eq!(t.get("Alex").unwrap().url, url1);

        // Newer update replaces, and clearing at a newer timestamp clears.
        assert!(t.apply(
            "Alex",
            TopicEntry { url: url2.clone(), device: "iphone 2".into(), cancel_seconds: 10, at: 110 }
        ));
        let t2 = Topics::load(&dir);
        assert_eq!(t2.get("Alex").unwrap().url, url2);
        assert_eq!(t2.get("Alex").unwrap().device, "iphone 2");

        assert!(t.apply(
            "Alex",
            TopicEntry { url: String::new(), device: "iphone 2".into(), cancel_seconds: 0, at: 120 }
        ));
        let t3 = Topics::load(&dir);
        assert_eq!(t3.get("Alex").unwrap().url, "");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
