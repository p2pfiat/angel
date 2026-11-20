use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use nostr::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Built into the agent: the agent's own device label (e.g. "laptop"). The agent's key is
    /// also one of its owner's devices, and gets this name in the list of devices.
    #[serde(skip)]
    pub own_device: Option<String>,
    /// Where the watcher key and state live.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Optional friendly name advertised privately to watched devices (e.g. "Home server").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Built into the agent, this may be left empty: the agent's relays are used.
    #[serde(default)]
    pub relays: Vec<String>,
    /// Extra seconds after a deadline before alarming (network latency).
    #[serde(default = "default_grace")]
    pub grace_seconds: u64,
    /// Ignore messages older than this (e.g. replays after a long outage).
    #[serde(default = "default_max_age")]
    pub max_age_minutes: u64,
    pub telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub messages: Messages,
    #[serde(rename = "person", default)]
    pub persons: Vec<PersonConfig>,
    /// Other watchers (npub) that alert the same contacts for the same persons. Each tells the
    /// others when it sent an alarm, and they skip it, so contacts get it once. Only list
    /// watchers with the same contacts: a peer's report suppresses this watcher's alarm.
    #[serde(default)]
    pub peers: Vec<String>,
    /// Wait this long before sending an alarm, and skip it if a peer sent it meanwhile. Give
    /// the most reliable watcher (a home server) 0 and the others e.g. 10.
    #[serde(default)]
    pub send_delay_seconds: u64,
    /// Take on new VIPs invited by the VIPs above (design.md, "Step 2"). They are kept in
    /// vips.json; `remove-vip NAME` stops watching one.
    #[serde(default = "yes", skip_serializing_if = "Clone::clone")]
    pub accept_invites: bool,
    /// Allow any device that knows this watcher's key to self-enrol (courtesy starter mode).
    /// Subject to `max_invited_vips` and daily alarm limits. Default: false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_signup: bool,
    /// At most this many invited VIPs.
    #[serde(default = "default_max_invited")]
    pub max_invited_vips: usize,
    /// The phone app's address, e.g. "https://example.github.io/app/". With it, the welcome
    /// message to a new angel carries a link to protect themselves with these watchers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_url: Option<String>,
    /// Telegram chats of this watcher's owner: told when someone new is watched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_chats: Vec<i64>,
}

fn yes() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_max_invited() -> usize {
    20
}

/// Longest send delay: alarms must not wait long for a watcher that may be down.
const MAX_SEND_DELAY_SECONDS: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    pub bot_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonConfig {
    pub name: String,
    /// Device public keys (npub or hex) whose messages belong to this person.
    pub devices: Vec<String>,
    /// Telegram chats (people or groups) to alert for this person.
    #[serde(default)]
    pub telegram_chats: Vec<i64>,
    /// Duress sources for devices that cannot speak Nostr, e.g. a phone shortcut that posts
    /// to a secret ntfy topic. Every message on such a topic is a duress alarm.
    #[serde(default)]
    pub duress_ntfy: Vec<NtfySource>,
    /// Phone number shown under alarms, e.g. "+44 7700 900123". Telegram makes it tappable:
    /// contacts can call it by phone or through Telegram.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Raise by one to make the old join link invalid (when it reached the wrong people).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub invite_round: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A phone number as digits with an optional leading `+`, or `None` if it does not look like one.
pub fn normalize_phone(s: &str) -> Option<String> {
    let s = s.trim();
    let plus = s.starts_with('+');
    let rest = if plus { &s[1..] } else { s };
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || " -().".contains(c)) {
        return None;
    }
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    (6..=15).contains(&digits.len()).then(|| if plus { format!("+{digits}") } else { digits })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NtfySource {
    /// Full topic URL, e.g. "https://ntfy.sh/<long random name>". The name is the secret.
    pub url: String,
    /// Label shown to contacts.
    #[serde(default = "default_ntfy_device")]
    pub device: String,
    /// A duress from this source alarms only after this many seconds; an `ok` from the same
    /// source within that time takes it back (for gestures made by mistake). 0: at once.
    #[serde(default)]
    pub cancel_seconds: u64,
}

/// Longest cancel window for a duress: beyond this, help would come too late.
const MAX_CANCEL_SECONDS: u64 = 60;

/// A topic name shorter than this is too easy to guess (anyone who knows it can raise an alarm).
const MIN_NTFY_TOPIC_LEN: usize = 20;

fn default_ntfy_device() -> String {
    "phone".into()
}

/// Alert texts. Placeholders: {name} {device} {trigger} {location} {note} {who}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Messages {
    pub missed: String,
    pub duress: String,
    pub late_ok: String,
    pub test: String,
    pub ack_call: String,
    pub ack_resolved: String,
    pub button_call: String,
    pub button_resolved: String,
    /// Added under an alarm when the person has a phone number. Placeholders: {name} {phone}
    pub phone_line: String,
    /// Join link: to the new angel.
    pub join_welcome: String,
    /// Added to the welcome when the watcher has `app_url`. Placeholders: {name} {link}
    pub join_self: String,
    /// To `owner_chats` when someone new is watched. {who} = who invited them.
    pub vip_joined: String,
    /// Once a day when an invited VIP reaches the alarm limit.
    pub alarm_limit: String,
    /// To the owner's chats when a phone is added; {name} is the VIP, {who} the phone's name.
    #[serde(default = "default_phone_added")]
    pub phone_added: String,
    /// Join link: to the existing angels ({who} = the new angel).
    pub join_approved: String,
    /// Join link that is unknown or no longer valid.
    pub join_invalid: String,
    /// After /stop: to who left, and ({who}) to the others.
    pub leave_done: String,
    pub leave_notice: String,
}

impl Default for Messages {
    fn default() -> Self {
        Self {
            missed: "ALARM for {name}\n\n{name} did not check in on {device} ({trigger}).\nLocation: {location}\n{note}\n\nCall {name} now. If there is no answer, or the agreed code word is not used, call emergency services.".into(),
            duress: "DURESS ALARM for {name}\n\n{name} sent a duress signal from {device}.\nLocation: {location}\n{note}\n\nCall emergency services, then call {name} and say: \"The police are already on the way.\"".into(),
            late_ok: "{name} checked in late. This does not prove {name} is safe: still call and ask for the code word.".into(),
            test: "Test from the watcher for {name}. No action needed.".into(),
            ack_call: "{who} is calling {name}.".into(),
            ack_resolved: "{who} marked the alarm for {name} as resolved.".into(),
            button_call: "I am calling".into(),
            button_resolved: "Resolved".into(),
            phone_line: "Phone {name}: {phone}".into(),
            join_welcome: "You now receive the alarms for {name}. Keep notifications for this chat switched on. Send /stop to stop.".into(),
            join_self: "Want the same protection for yourself? Open this on your phone: {link}".into(),
            vip_joined: "This watcher now also watches {name}, invited by {who}.".into(),
            phone_added: default_phone_added(),
            alarm_limit: "{name} reached the limit of alarms for today on this watcher: further alarms today are not sent from here.".into(),
            join_approved: "{who} now also receives the alarms for {name}.".into(),
            join_invalid: "This link is not valid (any more). Ask for a new one.".into(),
            leave_done: "You no longer receive the alarms for {name}.".into(),
            leave_notice: "{who} no longer receives the alarms for {name}.".into(),
        }
    }
}

pub struct Vars<'a> {
    pub name: &'a str,
    pub device: &'a str,
    pub trigger: &'a str,
    pub location: &'a str,
    pub note: &'a str,
    pub who: &'a str,
}

pub fn render(template: &str, v: &Vars) -> String {
    let text = template
        .replace("{name}", v.name)
        .replace("{device}", v.device)
        .replace("{trigger}", v.trigger)
        .replace("{location}", v.location)
        .replace("{note}", v.note)
        .replace("{who}", v.who);
    // An empty placeholder on its own line must not leave a gap.
    let mut out = String::new();
    let mut blank_run = 0;
    for line in text.trim().lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/data")
}
fn default_grace() -> u64 {
    10
}
fn default_max_age() -> u64 {
    30
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.relays.is_empty(), "watcher config: at least one relay is required");
        self.validate_persons()
    }

    /// The other watchers' public keys.
    pub fn peer_keys(&self) -> Result<Vec<PublicKey>> {
        self.peers
            .iter()
            .map(|k| PublicKey::parse(k).with_context(|| format!("invalid peer key {k}")))
            .collect()
    }

    /// Everything except the relays (which the agent fills in for its built-in watcher).
    pub fn validate_persons(&self) -> Result<()> {
        if let Some(name) = &self.name {
            let trimmed = name.trim();
            anyhow::ensure!(trimmed.chars().count() <= 40, "watcher name: use at most 40 characters");
            anyhow::ensure!(!trimmed.chars().any(char::is_control), "watcher name contains an invalid character");
        }
        self.peer_keys()?;
        anyhow::ensure!(
            self.send_delay_seconds <= MAX_SEND_DELAY_SECONDS,
            "send_delay_seconds: at most {MAX_SEND_DELAY_SECONDS}"
        );
        if let Some(url) = &self.app_url {
            anyhow::ensure!(url.starts_with("https://"), "app_url must start with https://");
        }
        for p in &self.persons {
            p.device_keys()?;
            if let Some(phone) = &p.phone {
                anyhow::ensure!(normalize_phone(phone).is_some(), "person {}: invalid phone {phone:?}", p.name);
            }
            for s in &p.duress_ntfy {
                s.validate().with_context(|| format!("person {}", p.name))?;
            }
        }
        Ok(())
    }

    /// The name this watcher reports in `Watching` messages: explicit `name`, falling back
    /// to the built-in agent's `own_device` label.
    pub fn watcher_name(&self) -> Option<String> {
        let raw = self.name.as_deref().or(self.own_device.as_deref())?;
        let cleaned: String = raw.trim().chars().filter(|c| !c.is_control()).take(40).collect();
        let cleaned = cleaned.trim();
        (!cleaned.is_empty()).then(|| cleaned.to_string())
    }
}

impl NtfySource {
    fn validate(&self) -> Result<()> {
        let url = self.url.trim_end_matches('/');
        anyhow::ensure!(
            url.starts_with("https://") || url.starts_with("http://"),
            "duress_ntfy url must start with https://"
        );
        let topic = url.rsplit('/').next().unwrap_or("");
        anyhow::ensure!(
            topic.len() >= MIN_NTFY_TOPIC_LEN && topic.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "duress_ntfy topic must be at least {MIN_NTFY_TOPIC_LEN} letters, digits, - or _ (it is the secret)"
        );
        anyhow::ensure!(
            self.cancel_seconds <= MAX_CANCEL_SECONDS,
            "duress_ntfy cancel_seconds: at most {MAX_CANCEL_SECONDS}"
        );
        Ok(())
    }
}

impl PersonConfig {
    pub fn device_keys(&self) -> Result<Vec<PublicKey>> {
        self.devices
            .iter()
            .map(|d| PublicKey::parse(d).with_context(|| format!("person {}: invalid device key {d}", self.name)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_note_leaves_no_gap() {
        let v = Vars { name: "A", device: "d", trigger: "t", location: "x", note: "", who: "" };
        let out = render(&Messages::default().missed, &v);
        assert!(!out.contains("\n\n\n"), "{out}");
        assert!(out.contains("Location: x\n\nCall A now"), "{out}");
    }

    #[test]
    fn peers_and_send_delay_are_checked() {
        let mut cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.peers = vec![Keys::generate().public_key().to_bech32().unwrap()];
        cfg.send_delay_seconds = 10;
        assert!(cfg.validate().is_ok());
        cfg.send_delay_seconds = 61;
        assert!(cfg.validate().is_err());
        cfg.send_delay_seconds = 10;
        cfg.peers.push("not a key".into());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(!cfg.relays.is_empty());
        assert_eq!(cfg.persons.len(), 1);
    }

    #[test]
    fn phone_numbers() {
        assert_eq!(normalize_phone(" +44 (0)7700-900 123 ").as_deref(), Some("+4407700900123"));
        assert_eq!(normalize_phone("07700900123").as_deref(), Some("07700900123"));
        for bad in ["", "+", "12345", "+44 7700 abc", "1+2345678", "+1234567890123456"] {
            assert_eq!(normalize_phone(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn ntfy_topic_must_be_long_enough() {
        let src = |url: &str| NtfySource { url: url.into(), device: "phone".into(), cancel_seconds: 0 };
        assert!(src("https://ntfy.sh/short").validate().is_err());
        assert!(src("ntfy.sh/abcdefghijklmnopqrstuvwxyz").validate().is_err());
        assert!(src("https://ntfy.sh/abcdefghij_KLMNOPQRST-123/").validate().is_ok());
    }

    #[test]
    fn watcher_name_uses_explicit_name_or_falls_back_to_own_device() {
        let mut cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!(cfg.watcher_name(), None);
        cfg.own_device = Some("  laptop  ".into());
        assert_eq!(cfg.watcher_name().as_deref(), Some("laptop"));
        cfg.name = Some("  Home server  ".into());
        assert_eq!(cfg.watcher_name().as_deref(), Some("Home server"));
        assert!(cfg.validate().is_ok());
        cfg.name = Some("bad\nname".into());
        assert!(cfg.validate_persons().is_err());
        cfg.name = Some("x".repeat(41));
        assert!(cfg.validate_persons().is_err());
    }
}

fn default_phone_added() -> String {
    "A phone was added for {name}: \"{who}\". Not you? Remove it in Sync (Settings, My devices).".into()
}
