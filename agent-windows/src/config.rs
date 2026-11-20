use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use nostr::prelude::*;
use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Label shown to contacts, e.g. "laptop".
    pub device: String,
    /// Seconds you have after login/unlock to perform the secret action.
    pub timeout_seconds: u64,
    /// Seconds after login/unlock without check-in before a discreet reminder. 0: none.
    #[serde(default)]
    pub reminder_seconds: u64,
    /// Reminder: the system's ordinary new-mail sound.
    #[serde(default = "yes")]
    pub reminder_sound: bool,
    /// Reminder: a small notice in the corner of the screen, worded so it means nothing to an
    /// onlooker (e.g. like a mail notification).
    #[serde(default = "yes")]
    pub reminder_notice: bool,
    #[serde(default)]
    pub reminder_notice_title: String,
    #[serde(default = "default_reminder_text")]
    pub reminder_text: String,
    /// Reminder: a Telegram message to `reminder_chat` (your own chat with the bot of the
    /// built-in watcher; contacts do not get it).
    #[serde(default)]
    pub reminder_telegram: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder_chat: Option<i64>,
    #[serde(default = "default_reminder_telegram_text")]
    pub reminder_telegram_text: String,
    pub relays: Vec<String>,
    /// Other watchers' public keys (npub): a home server, a friend's agent, ...
    #[serde(default)]
    pub watchers: Vec<String>,
    /// Secret check-in hotkey, e.g. "ctrl+alt+shift+k".
    pub ok_hotkey: String,
    /// Optional hotkey that raises an alarm immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duress_hotkey: Option<String>,
    /// Optional hotkey that opens a small settings window (pause check-ins). It only opens
    /// once you have checked in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_hotkey: Option<String>,
    /// Fixed place shown to contacts, e.g. a home address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Show a small confirmation (time since unlock) after the check-in key. Never shown
    /// before the key is pressed, so an onlooker at login sees nothing.
    #[serde(default = "yes")]
    pub ok_feedback: bool,
    /// Add the position measured by Windows (Wi-Fi based) to start and duress messages.
    #[serde(default = "yes")]
    pub detect_location: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Random name for the single-instance lock.
    pub instance_id: String,
    /// Built-in watcher: this agent also watches the persons listed here (yourself included)
    /// and alerts their contacts. Same format as the standalone watcher's config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watcher: Option<warn_watcher::Config>,
}

/// Measured to accept gift wraps from new keys within a second (2026-11); the last two are
/// slower but large. Any one relay shared with a watcher is enough.
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.nostr.net",
    "wss://nostr.oxtr.dev",
    "wss://relay.snort.social",
    "wss://offchain.pub",
    "wss://nos.lol",
    "wss://relay.primal.net",
];

fn yes() -> bool {
    true
}

fn default_reminder_text() -> String {
    "Audio devices updated".into()
}

fn default_reminder_telegram_text() -> String {
    "Sync pending".into()
}

/// Accepted check-in times, in seconds.
pub const TIMEOUT_RANGE: std::ops::RangeInclusive<u64> = 30..=600;
/// A reminder comes at least this long after unlock and this long before the deadline.
const REMINDER_MARGIN: u64 = 10;
const MAX_REMINDER_TEXT: usize = 80;

/// Check-in time and reminders, as the settings window shows and edits them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CheckIn {
    pub timeout_seconds: u64,
    pub reminder_seconds: u64,
    pub sound: bool,
    pub notice: bool,
    pub notice_title: String,
    pub notice_text: String,
    pub telegram: bool,
    pub telegram_chat: Option<i64>,
    pub telegram_text: String,
}

/// The settings window's fields as typed.
pub struct CheckInForm<'a> {
    pub timeout: &'a str,
    pub reminder: &'a str,
    pub sound: bool,
    pub notice: bool,
    pub notice_title: &'a str,
    pub notice_text: &'a str,
    pub telegram: bool,
    pub telegram_chat: &'a str,
    pub telegram_text: &'a str,
}

fn short_text(label: &str, s: &str) -> Result<String> {
    let s = s.trim();
    anyhow::ensure!(s.chars().count() <= MAX_REMINDER_TEXT, "{label}: use at most {MAX_REMINDER_TEXT} characters.");
    anyhow::ensure!(!s.chars().any(char::is_control), "{label} contains an invalid character.");
    Ok(s.to_string())
}

impl CheckIn {
    /// Check what was typed in the settings window. `has_bot`: a Telegram bot is configured.
    pub fn parse(f: &CheckInForm, has_bot: bool) -> Result<Self> {
        let timeout: u64 =
            f.timeout.trim().parse().map_err(|_| anyhow::anyhow!("Check-in time: enter a number of seconds."))?;
        anyhow::ensure!(
            TIMEOUT_RANGE.contains(&timeout),
            "Check-in time: between {} and {} seconds.",
            TIMEOUT_RANGE.start(),
            TIMEOUT_RANGE.end()
        );
        let reminder: u64 = match f.reminder.trim() {
            "" => 0,
            r => r.parse().map_err(|_| anyhow::anyhow!("Reminder: enter a number of seconds (0 = none)."))?,
        };
        anyhow::ensure!(
            reminder == 0 || (REMINDER_MARGIN..=timeout - REMINDER_MARGIN).contains(&reminder),
            "Reminder: 0, or between {REMINDER_MARGIN} and {} seconds.",
            timeout - REMINDER_MARGIN
        );
        let notice_title = short_text("Notice title", f.notice_title)?;
        let notice_text = short_text("Notice text", f.notice_text)?;
        anyhow::ensure!(!f.notice || !notice_text.is_empty(), "Notice: enter a text, or switch the notice off.");
        let telegram_text = short_text("Telegram text", f.telegram_text)?;
        let telegram_chat = match f.telegram_chat.trim() {
            "" => None,
            c => Some(c.parse::<i64>().ok().filter(|&id| id != 0).ok_or_else(|| {
                anyhow::anyhow!("Telegram: the chat id is a number (send /id to the bot to get it).")
            })?),
        };
        if f.telegram {
            anyhow::ensure!(has_bot, "Telegram: no bot is configured for the built-in watcher.");
            anyhow::ensure!(telegram_chat.is_some(), "Telegram: enter your chat id (send /id to the bot to get it).");
            anyhow::ensure!(!telegram_text.is_empty(), "Telegram: enter a text, or switch it off.");
        }
        Ok(Self {
            timeout_seconds: timeout,
            reminder_seconds: reminder,
            sound: f.sound,
            notice: f.notice,
            notice_title,
            notice_text,
            telegram: f.telegram,
            telegram_chat,
            telegram_text,
        })
    }

    /// Whether any reminder will be given.
    pub fn reminder_on(&self) -> bool {
        self.reminder_seconds > 0 && (self.sound || self.notice || self.telegram)
    }
}

impl Config {
    pub fn path(home: &Path) -> PathBuf {
        home.join("config.toml")
    }

    pub fn load(home: &Path) -> Result<Self> {
        let path = Self::path(home);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {} (run `init` first)", path.display()))?;
        let cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.watcher_keys()?;
        anyhow::ensure!(
            !cfg.watchers.is_empty() || cfg.watcher.is_some(),
            "{}: add watcher keys to `watchers`, or a [watcher] section, or both",
            path.display()
        );
        if let Some(w) = cfg.local_watcher(home) {
            w.validate()?;
        }
        crate::hotkey::Hotkey::parse(&cfg.ok_hotkey)?;
        for h in [&cfg.duress_hotkey, &cfg.settings_hotkey].into_iter().flatten() {
            crate::hotkey::Hotkey::parse(h)?;
        }
        Ok(cfg)
    }

    pub fn watcher_keys(&self) -> Result<Vec<PublicKey>> {
        self.watchers
            .iter()
            .map(|w| PublicKey::parse(w).with_context(|| format!("invalid watcher key {w}")))
            .collect()
    }

    /// The built-in watcher's config, completed with this agent's relays and folder.
    pub fn local_watcher(&self, home: &Path) -> Option<warn_watcher::Config> {
        let mut w = self.watcher.clone()?;
        if w.relays.is_empty() {
            w.relays = self.relays.clone();
        }
        w.data_dir = home.to_path_buf();
        w.own_device = Some(self.device.clone());
        Some(w)
    }

    /// Everyone a message goes to: the configured watchers, plus this device itself when it
    /// runs the built-in watcher.
    pub fn recipients(&self, own: &PublicKey) -> Result<Vec<PublicKey>> {
        let mut keys = self.watcher_keys()?;
        if self.watcher.is_some() && !keys.contains(own) {
            keys.push(*own);
        }
        Ok(keys)
    }

    /// Index of the built-in watcher's person this device belongs to: the one whose
    /// `devices` include the device's own key.
    fn own_person(&self, own: &PublicKey) -> Option<usize> {
        let w = self.watcher.as_ref()?;
        w.persons
            .iter()
            .position(|p| p.device_keys().map(|keys| keys.contains(own)).unwrap_or(false))
    }

    /// The name contacts see in alerts about this device's owner.
    pub fn own_name(&self, own: &PublicKey) -> Option<String> {
        self.own_details(own).map(|d| d.name)
    }

    /// What the settings window shows for this device's owner.
    pub fn own_details(&self, own: &PublicKey) -> Option<OwnerDetails> {
        let i = self.own_person(own)?;
        let p = &self.watcher.as_ref()?.persons[i];
        Some(OwnerDetails {
            name: p.name.clone(),
            phone: p.phone.clone().unwrap_or_default(),
        })
    }

    pub fn check_in(&self) -> CheckIn {
        CheckIn {
            timeout_seconds: self.timeout_seconds,
            reminder_seconds: self.reminder_seconds,
            sound: self.reminder_sound,
            notice: self.reminder_notice,
            notice_title: self.reminder_notice_title.clone(),
            notice_text: self.reminder_text.clone(),
            telegram: self.reminder_telegram,
            telegram_chat: self.reminder_chat,
            telegram_text: self.reminder_telegram_text.clone(),
        }
    }

    /// The bot token of the built-in watcher, used for the Telegram reminder.
    pub fn bot_token(&self) -> Option<&str> {
        self.watcher.as_ref()?.telegram.as_ref().map(|t| t.bot_token.as_str())
    }

    pub fn context(&self, trigger: &str) -> warn_proto::Context {
        warn_proto::Context {
            device: self.device.clone(),
            trigger: trigger.into(),
            location: self.location.clone(),
            note: self.note.clone(),
        }
    }

    pub fn new_default(device: &str) -> Self {
        let mut rng = rand::rng();
        // Letters that are not used by common Ctrl+Alt+Shift shortcuts.
        let letters = ['B', 'G', 'H', 'J', 'K', 'M', 'P', 'Q', 'U', 'Y'];
        let ok = *letters.choose(&mut rng).expect("non-empty");
        Self {
            device: device.into(),
            timeout_seconds: 60,
            reminder_seconds: 0,
            reminder_sound: true,
            reminder_notice: true,
            reminder_notice_title: String::new(),
            reminder_text: default_reminder_text(),
            reminder_telegram: false,
            reminder_chat: None,
            reminder_telegram_text: default_reminder_telegram_text(),
            relays: DEFAULT_RELAYS.iter().map(|r| r.to_string()).collect(),
            watchers: vec![],
            watcher: None,
            ok_hotkey: format!("ctrl+alt+shift+{}", ok.to_ascii_lowercase()),
            duress_hotkey: None,
            settings_hotkey: None,
            location: None,
            detect_location: true,
            ok_feedback: true,
            note: None,
            instance_id: format!("{:016x}", rand::random::<u64>()),
        }
    }
}

/// Longest name accepted from the settings window.
const MAX_NAME_CHARS: usize = 40;

/// The owner's details that contacts see in alerts. Empty phone: none.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnerDetails {
    pub name: String,
    pub phone: String,
}

/// Save the details of this device's owner in the config file, keeping its comments and
/// layout. Returns them as saved.
pub fn set_own_details(home: &Path, own: &PublicKey, d: &OwnerDetails) -> Result<OwnerDetails> {
    let d = OwnerDetails { name: d.name.trim().into(), phone: d.phone.trim().into() };
    anyhow::ensure!(!d.name.is_empty(), "Enter a name.");
    anyhow::ensure!(d.name.chars().count() <= MAX_NAME_CHARS, "Name: use at most {MAX_NAME_CHARS} characters.");
    anyhow::ensure!(!d.name.chars().any(char::is_control), "The name contains an invalid character.");
    anyhow::ensure!(
        d.phone.is_empty() || warn_watcher::config::normalize_phone(&d.phone).is_some(),
        "Phone: use digits, optionally starting with + and the country code."
    );

    let path = Config::path(home);
    let text = std::fs::read_to_string(&path)?;
    let cfg: Config = toml::from_str(&text)?;
    let i = cfg
        .own_person(own)
        .context("This device is not listed under any person in the built-in watcher.")?;
    let new_text = edit_person(&text, i, &d)?;
    // Never write a config the agent could not start with.
    let check: Config = toml::from_str(&new_text)?;
    if let Some(w) = &check.watcher {
        w.validate_persons()?;
    }
    anyhow::ensure!(check.own_details(own).as_ref() == Some(&d), "could not update the config");

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, new_text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(d)
}

/// Set the details of the `index`-th `[[watcher.person]]` in a config text.
fn edit_person(text: &str, index: usize, d: &OwnerDetails) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let person = doc
        .get_mut("watcher")
        .and_then(|w| w.get_mut("person"))
        .and_then(|p| p.as_array_of_tables_mut())
        .and_then(|a| a.get_mut(index))
        .context("person not found in the config")?;
    person["name"] = toml_edit::value(&d.name);
    if d.phone.is_empty() {
        person.remove("phone");
    } else {
        person["phone"] = toml_edit::value(&d.phone);
    }
    Ok(doc.to_string())
}

/// Save the check-in time and reminders in the config file, keeping its comments and layout.
pub fn set_check_in(home: &Path, c: &CheckIn) -> Result<()> {
    let path = Config::path(home);
    let text = std::fs::read_to_string(&path)?;
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    doc["timeout_seconds"] = toml_edit::value(c.timeout_seconds as i64);
    doc["reminder_seconds"] = toml_edit::value(c.reminder_seconds as i64);
    doc["reminder_sound"] = toml_edit::value(c.sound);
    doc["reminder_notice"] = toml_edit::value(c.notice);
    doc["reminder_notice_title"] = toml_edit::value(c.notice_title.as_str());
    doc["reminder_text"] = toml_edit::value(c.notice_text.as_str());
    doc["reminder_telegram"] = toml_edit::value(c.telegram);
    match c.telegram_chat {
        Some(id) => doc["reminder_chat"] = toml_edit::value(id),
        None => {
            doc.remove("reminder_chat");
        }
    }
    doc["reminder_telegram_text"] = toml_edit::value(c.telegram_text.as_str());
    let new_text = doc.to_string();
    // Never write a config the agent could not start with.
    let check: Config = toml::from_str(&new_text)?;
    anyhow::ensure!(check.check_in() == *c, "could not update the config");
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, new_text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_or_create_key(home: &Path) -> Result<Keys> {
    let path = home.join("device.key");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return Keys::parse(text.trim()).context("invalid device.key");
    }
    if Config::path(home).exists() {
        bail!("config exists but device.key is missing: refusing to create a new identity silently");
    }
    let keys = Keys::generate();
    std::fs::write(&path, keys.secret_key().to_bech32()?)?;
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELF_WATCHING: &str = r#"
device = "laptop"
timeout_seconds = 60
relays = ["wss://nos.lol"]
ok_hotkey = "ctrl+alt+h"
instance_id = "abc"

[watcher]
[[watcher.person]]
name = "Me"
devices = ["npub1trl7y6nxegm5hg0qgqj2srdd59u8zy3p6klxeatapsnapnpsulqswnluue"]
telegram_chats = [1]
"#;

    #[test]
    fn built_in_watcher_uses_agent_relays_and_home_and_receives_own_messages() {
        let cfg: Config = toml::from_str(SELF_WATCHING).unwrap();
        let w = cfg.local_watcher(Path::new("home")).unwrap();
        assert_eq!(w.relays, vec!["wss://nos.lol".to_string()]);
        assert_eq!(w.data_dir, PathBuf::from("home"));
        w.validate().unwrap();

        let own = Keys::generate().public_key();
        assert_eq!(cfg.recipients(&own).unwrap(), vec![own]);
    }

    fn two_persons(own: &PublicKey) -> String {
        format!(
            r#"# my config
device = "laptop"
timeout_seconds = 60
relays = ["wss://nos.lol"]
ok_hotkey = "ctrl+alt+h"
instance_id = "abc"

[watcher]
[[watcher.person]]
name = "Friend" # someone else
devices = ["{other}"]
telegram_chats = [1]

[[watcher.person]]
name = "Me"
devices = ["{own}"]
telegram_chats = [2, 3]
"#,
            other = Keys::generate().public_key().to_bech32().unwrap(),
            own = own.to_bech32().unwrap(),
        )
    }

    #[test]
    fn renaming_changes_only_the_own_person_and_keeps_comments() {
        let home = std::env::temp_dir().join(format!("warn-agent-test-{:x}", rand::random::<u64>()));
        std::fs::create_dir_all(&home).unwrap();
        let own = Keys::generate().public_key();
        std::fs::write(Config::path(&home), two_persons(&own)).unwrap();

        let details = |name: &str, phone: &str| OwnerDetails { name: name.into(), phone: phone.into() };
        let set_own_name =
            |home: &Path, own: &PublicKey, name: &str| set_own_details(home, own, &details(name, "")).map(|d| d.name);
        assert_eq!(set_own_name(&home, &own, "  Alex B  ").unwrap(), "Alex B");
        let text = std::fs::read_to_string(Config::path(&home)).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.own_name(&own).as_deref(), Some("Alex B"));
        assert_eq!(cfg.watcher.as_ref().unwrap().persons[0].name, "Friend");
        assert!(text.contains("# my config") && text.contains("# someone else"));

        // Names that would read badly or break the file are refused, and nothing changes.
        for bad in ["", "   ", "a\nb", &"x".repeat(41)] {
            assert!(set_own_name(&home, &own, bad).is_err(), "{bad:?}");
        }
        // Quotes are stored safely.
        set_own_name(&home, &own, r#"Al "the" ex"#).unwrap();
        let cfg: Config = toml::from_str(&std::fs::read_to_string(Config::path(&home)).unwrap()).unwrap();
        assert_eq!(cfg.own_name(&own).as_deref(), Some(r#"Al "the" ex"#));

        let stranger = Keys::generate().public_key();
        assert!(set_own_name(&home, &stranger, "X").is_err());

        // The phone number is saved, checked, and removed when left empty.
        let saved = set_own_details(&home, &own, &details("Alex", " +44 7700 900123 ")).unwrap();
        assert_eq!(saved, details("Alex", "+44 7700 900123"));
        let cfg: Config = toml::from_str(&std::fs::read_to_string(Config::path(&home)).unwrap()).unwrap();
        assert_eq!(cfg.own_details(&own), Some(saved));
        assert!(set_own_details(&home, &own, &details("Alex", "call me")).is_err());
        set_own_details(&home, &own, &details("Alex", "")).unwrap();
        let text = std::fs::read_to_string(Config::path(&home)).unwrap();
        assert!(!text.contains("phone"), "{text}");
        let _ = std::fs::remove_dir_all(&home);
    }

    fn form<'a>(timeout: &'a str, reminder: &'a str, telegram: bool, chat: &'a str) -> CheckInForm<'a> {
        CheckInForm {
            timeout,
            reminder,
            sound: true,
            notice: true,
            notice_title: " Outlook ",
            notice_text: " New message ",
            telegram,
            telegram_chat: chat,
            telegram_text: "Sync pending",
        }
    }

    #[test]
    fn check_in_settings_are_checked_and_saved() {
        let c = CheckIn::parse(&form(" 120 ", "60", true, " 123456 "), true).unwrap();
        assert_eq!((c.timeout_seconds, c.reminder_seconds, c.telegram_chat), (120, 60, Some(123456)));
        assert_eq!((c.notice_title.as_str(), c.notice_text.as_str()), ("Outlook", "New message"));
        assert!(c.reminder_on());
        assert!(!CheckIn::parse(&form("120", "", false, ""), true).unwrap().reminder_on());
        let bad = [("abc", "0"), ("10", "0"), ("601", "0"), ("120", "115"), ("120", "5"), ("120", "x")];
        for (timeout, reminder) in bad {
            assert!(CheckIn::parse(&form(timeout, reminder, false, ""), true).is_err(), "{timeout} {reminder}");
        }
        // Telegram needs a bot and a chat id; a chat id must be a number.
        assert!(CheckIn::parse(&form("120", "60", true, "123"), false).is_err());
        assert!(CheckIn::parse(&form("120", "60", true, ""), true).is_err());
        assert!(CheckIn::parse(&form("120", "60", false, "abc"), true).is_err());

        let home = std::env::temp_dir().join(format!("warn-agent-test-{:x}", rand::random::<u64>()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(Config::path(&home), SELF_WATCHING).unwrap();
        set_check_in(&home, &c).unwrap();
        let text = std::fs::read_to_string(Config::path(&home)).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.check_in(), c);
        // The keys stay above the [watcher] table, where TOML needs them.
        assert!(text.find("reminder_chat").unwrap() < text.find("[watcher]").unwrap(), "{text}");
        let off = CheckIn { telegram: false, telegram_chat: None, ..c };
        set_check_in(&home, &off).unwrap();
        let cfg: Config = toml::from_str(&std::fs::read_to_string(Config::path(&home)).unwrap()).unwrap();
        assert_eq!(cfg.check_in(), off);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn without_built_in_watcher_only_configured_watchers_receive() {
        let mut cfg: Config = toml::from_str(SELF_WATCHING).unwrap();
        cfg.watcher = None;
        assert!(cfg.recipients(&Keys::generate().public_key()).unwrap().is_empty());
    }
}
