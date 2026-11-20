//! VIPs that came in by invite, and the open invites (design.md, "Step 2: new VIPs by
//! invite"). Stored in `vips.json` in the data folder.
//!
//! An own VIP (one from the config file) creates an invite: a secret in a link, of which the
//! watchers keep only the hash. Whoever opens the link enrols one device under a name of their
//! choice, once, before the invite expires. VIPs that came in this way cannot invite others
//! (one hop), so a home server is never handed to strangers of strangers.
//!
//! The link is short (`<app>#j=<secret>`): the rest (who invites, which watchers and relays)
//! is an event on the relays, signed by a key derived from the secret and encrypted to it, so
//! only who has the link can find and read it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bitcoin_hashes::{sha256, Hash};
use nostr::prelude::*;
use serde::{Deserialize, Serialize};

use crate::config::PersonConfig;
use warn_proto::PushSub;

/// Longest an invite may stay valid.
pub const MAX_INVITE_SECONDS: u64 = 30 * 24 * 3600;
/// Longest an angel link may stay valid (the app makes a new one before then).
pub const MAX_ANGEL_INVITE_SECONDS: u64 = 180 * 24 * 3600;
/// Longest a link that offers this server to someone stays valid.
pub const MAX_OFFER_SECONDS: u64 = 7 * 24 * 3600;
/// Longest a link to add a phone stays valid.
pub const MAX_DEVICE_INVITE_SECONDS: u64 = 30 * 60;
/// Prefix of the synthetic secret sent by self-enrolments on an open courtesy watcher.
pub const OPEN_SIGNUP_PREFIX: &str = "open:";
/// Longest name a new VIP may choose.
const MAX_NAME_CHARS: usize = 40;

fn unique_name(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t.eq_ignore_ascii_case(base)) {
        return base.to_string();
    }
    for n in 2usize.. {
        let suffix = format!(" {n}");
        let prefix: String = base.chars().take(MAX_NAME_CHARS.saturating_sub(suffix.chars().count())).collect();
        let candidate = format!("{}{suffix}", prefix.trim_end());
        if !taken.iter().any(|t| t.eq_ignore_ascii_case(&candidate)) {
            return candidate;
        }
    }
    unreachable!()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    /// The own VIP who invites.
    pub person: String,
    pub expires: u64,
    #[serde(default)]
    pub used: bool,
    /// An angel link: any number of angels join with it, until it expires.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub angel: bool,
    /// A link to add a phone to `person`: one device, once, within half an hour.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub device: bool,
    /// The inviter's app angel key (hex) and push subscription, added automatically on enroll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub angel_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<PushSub>,
}

/// The result of enrolling a new VIP with an invite secret.
#[derive(Debug, Clone)]
pub struct Enrolled {
    pub person: PersonConfig,
    pub by: String,
    pub angel_key: Option<String>,
    pub push: Option<PushSub>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitedVip {
    pub name: String,
    /// The one device (hex key) that enrolled.
    pub device: String,
    pub invited_by: String,
    pub at: u64,
    /// Removed by the watcher's owner: watched no more. Kept so person numbers stay the same
    /// (the engine's saved timers refer to them) and the name is not given out again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Vips {
    #[serde(default)]
    invites: HashMap<String, Invite>,
    #[serde(default)]
    vips: Vec<InvitedVip>,
    #[serde(skip)]
    path: PathBuf,
}

/// Why an invite or an enrolment is refused; the text goes to the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

fn refuse<T>(why: &str) -> Result<T, Refusal> {
    Err(Refusal(why.to_string()))
}

/// The hash the watchers keep of an invite's secret (sha256, hex).
pub fn secret_hash(secret: &str) -> String {
    sha256::Hash::hash(secret.as_bytes()).to_string()
}

/// A new random secret for a link: 128 bits, base64url (22 characters).
pub fn new_secret() -> String {
    let bytes = Keys::generate().secret_key().to_secret_bytes();
    base64url(&bytes[..16])
}

/// Deterministic 22-character angel invite secret derived from a VIP invite's secret, so every
/// watcher handling the enrolment agrees on the same angel secret for the inviter.
pub fn derived_angel_secret(secret: &str) -> String {
    let h = sha256::Hash::hash(format!("warn-angel-invite:{secret}").as_bytes());
    base64url(&h.as_byte_array()[..16])
}

fn base64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Kind of the invite event (application-specific data).
pub const INVITE_KIND: u16 = 30078;

/// What the invite event holds (encrypted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteInfo {
    /// Who invites.
    pub by: String,
    /// Watcher keys (hex).
    pub w: Vec<String>,
    /// Relays.
    pub r: Vec<String>,
    /// What the link is for: empty (a new VIP), "angel" or "device" (add a phone).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub t: String,
}

/// The key an invite's event is signed with, derived from the link's secret.
pub fn invite_keys(secret: &str) -> Result<Keys> {
    let h = sha256::Hash::hash(format!("warn-invite:{secret}").as_bytes());
    Ok(Keys::new(SecretKey::from_slice(h.as_byte_array())?))
}

/// The event that holds an invite, for the relays. It disappears after `expires` (NIP-40).
pub fn invite_event(secret: &str, info: &InviteInfo, expires: u64) -> Result<Event> {
    let k = invite_keys(secret)?;
    let content = nip44::encrypt(k.secret_key(), &k.public_key(), serde_json::to_string(info)?, nip44::Version::V2)?;
    let unsigned = EventBuilder::new(Kind::Custom(INVITE_KIND), content)
        .tags([Tag::identifier("warn-invite"), Tag::expiration(Timestamp::from_secs(expires))])
        .finalize_unsigned(k.public_key());
    Ok(k.sign_event(unsigned)?)
}

/// Read an invite event with the link's secret.
pub fn read_invite_event(secret: &str, event: &Event) -> Result<InviteInfo> {
    let k = invite_keys(secret)?;
    event.verify()?;
    anyhow::ensure!(event.pubkey == k.public_key() && event.kind == Kind::Custom(INVITE_KIND), "not this invite");
    Ok(serde_json::from_str(&nip44::decrypt(k.secret_key(), &k.public_key(), &event.content)?)?)
}

impl Vips {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("vips.json");
        let mut v: Vips = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        v.path = path;
        v
    }

    /// A removal made with `remove-vip` while the watcher runs must not be undone when the
    /// running watcher saves: take removals from the file first.
    fn take_removals(&mut self) {
        let Some(disk) = std::fs::read_to_string(&self.path).ok().and_then(|t| serde_json::from_str::<Vips>(&t).ok()) else {
            return;
        };
        for d in disk.vips.iter().filter(|d| d.removed) {
            if let Some(v) = self.vips.iter_mut().find(|v| v.device == d.device && v.name == d.name) {
                v.removed = true;
            }
        }
    }

    fn save(&mut self) {
        self.take_removals();
        let text = match serde_json::to_string_pretty(self) {
            Ok(t) => t,
            Err(e) => return tracing::error!("saving vips failed: {e:#}"),
        };
        if let Err(e) = crate::service::write_private(&self.path, &text) {
            tracing::error!("saving vips failed: {e:#}");
        }
    }

    /// Every invited VIP as a person, in a fixed order (appended after the config's persons).
    /// Removed ones keep their place but have no devices, so nothing of theirs is accepted.
    pub fn persons(&self) -> Vec<PersonConfig> {
        self.vips.iter().map(person_of).collect()
    }

    /// How many invited VIPs are watched now.
    pub fn active(&self) -> usize {
        self.vips.iter().filter(|v| !v.removed).count()
    }

    /// Keep an invite (from an own VIP's device, or from a peer). Returns false if it was known.
    pub fn add_invite(&mut self, hash: &str, person: &str, expires: u64, now: u64) -> bool {
        self.invites.retain(|_, i| i.expires > now);
        if self.invites.contains_key(hash) {
            return false;
        }
        let expires = expires.min(now + MAX_INVITE_SECONDS);
        self.invites.insert(
            hash.to_string(),
            Invite { person: person.to_string(), expires, used: false, angel: false, device: false, angel_key: None, push: None },
        );
        self.save();
        true
    }

    /// Attach the inviter's app angel key and push subscription to a VIP invite.
    pub fn set_invite_angel(&mut self, hash: &str, angel_key: Option<String>, push: Option<PushSub>) {
        let Some(invite) = self.invites.get_mut(hash).filter(|i| !i.angel && !i.device) else {
            return;
        };
        let mut changed = false;
        if let Some(k) = angel_key.and_then(|k| PublicKey::from_hex(&k).ok()).map(|k| k.to_hex()) {
            if invite.angel_key.as_ref() != Some(&k) {
                invite.angel_key = Some(k);
                changed = true;
            }
        }
        if push.is_some() && invite.angel_key.is_some() && invite.push != push {
            invite.push = push;
            changed = true;
        }
        if changed {
            self.save();
        }
    }

    /// Keep an angel link of a VIP (from one of their devices). Returns false if it was known.
    pub fn add_angel_invite(&mut self, hash: &str, person: &str, expires: u64, now: u64) -> bool {
        self.invites.retain(|_, i| i.expires > now);
        if self.invites.contains_key(hash) {
            return false;
        }
        let expires = expires.min(now + MAX_ANGEL_INVITE_SECONDS);
        self.invites.insert(
            hash.to_string(),
            Invite { person: person.to_string(), expires, used: false, angel: true, device: false, angel_key: None, push: None },
        );
        self.save();
        true
    }

    /// Keep a link to add a phone to `person`. Returns false if it was known.
    pub fn add_device_invite(&mut self, hash: &str, person: &str, expires: u64, now: u64) -> bool {
        self.invites.retain(|_, i| i.expires > now);
        if self.invites.contains_key(hash) {
            return false;
        }
        let expires = expires.min(now + MAX_DEVICE_INVITE_SECONDS);
        self.invites.insert(
            hash.to_string(),
            Invite { person: person.to_string(), expires, used: false, angel: false, device: true, angel_key: None, push: None },
        );
        self.save();
        true
    }

    /// The VIP a link to add a phone is for (whatever its state), if it is one.
    pub fn device_invite_person(&self, hash: &str) -> Option<String> {
        self.invites.get(hash).filter(|i| i.device).map(|i| i.person.clone())
    }

    /// Use a link to add a phone: once, before it expires.
    pub fn use_device_invite(&mut self, hash: &str, now: u64) -> Result<(), Refusal> {
        let Some(i) = self.invites.get_mut(hash).filter(|i| i.device) else { return refuse("this link is not known here") };
        if i.used {
            return refuse("this link was already used: make a new one");
        }
        if i.expires <= now {
            return refuse("this link has expired: make a new one");
        }
        i.used = true;
        self.save();
        Ok(())
    }

    /// The VIP whose angel link this secret is, if valid.
    pub fn angel_invite(&self, secret: &str, now: u64) -> Option<String> {
        self.invites.get(&secret_hash(secret)).filter(|i| i.angel && i.expires > now).map(|i| i.person.clone())
    }

    /// Enrol a device with an invite's secret. `taken` are the names already in use on this
    /// watcher. Returns the new person and who invited them; the same device asking again (its
    /// answer got lost) gets the same answer.
    pub fn enroll(
        &mut self,
        secret: &str,
        name: &str,
        device: &PublicKey,
        now: u64,
        taken: &[String],
        max: usize,
    ) -> Result<Enrolled, Refusal> {
        let name = name.trim();
        let hex = device.to_hex();
        if let Some(v) = self.vips.iter().find(|v| v.device == hex && !v.removed) {
            return if v.name == name {
                Ok(Enrolled { person: person_of(v), by: v.invited_by.clone(), angel_key: None, push: None })
            } else {
                refuse("this phone already joined under another name")
            };
        }
        let hash = secret_hash(secret);
        let Some(invite) = self.invites.get(&hash).filter(|i| !i.angel && !i.device) else { return refuse("this invite is not known here") };
        if invite.used {
            return refuse("this invite was already used");
        }
        if invite.expires <= now {
            return refuse("this invite has expired");
        }
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return refuse("choose a name of 1 to 40 letters");
        }
        if taken.iter().any(|t| t.eq_ignore_ascii_case(name)) {
            return refuse("this name is taken on this server: choose another");
        }
        if self.active() >= max {
            return refuse("this server watches as many people as it can");
        }
        let by = invite.person.clone();
        let angel_key = invite.angel_key.clone();
        let push = invite.push.clone();
        self.invites.get_mut(&hash).expect("checked above").used = true;
        let v = InvitedVip { name: name.to_string(), device: hex, invited_by: by.clone(), at: now, removed: false };
        let p = person_of(&v);
        self.vips.push(v);
        self.save();
        Ok(Enrolled { person: p, by, angel_key, push })
    }

    /// Self-enrol a device on an open courtesy watcher, disambiguating duplicate display names.
    /// An already-enrolled device asking again (e.g. a lost reply) receives its assigned person.
    pub fn enroll_open(
        &mut self,
        name: &str,
        device: &PublicKey,
        now: u64,
        taken: &[String],
        max: usize,
    ) -> Result<Enrolled, Refusal> {
        let name = name.trim();
        let hex = device.to_hex();
        if let Some(v) = self.vips.iter().find(|v| v.device == hex && !v.removed) {
            return Ok(Enrolled { person: person_of(v), by: v.invited_by.clone(), angel_key: None, push: None });
        }
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return refuse("choose a name of 1 to 40 letters");
        }
        if self.active() >= max {
            return refuse("this server watches as many people as it can");
        }
        let final_name = unique_name(name, taken);
        let v = InvitedVip { name: final_name, device: hex, invited_by: String::new(), at: now, removed: false };
        let p = person_of(&v);
        self.vips.push(v);
        self.save();
        Ok(Enrolled { person: p, by: String::new(), angel_key: None, push: None })
    }

    /// Stop watching an invited VIP. Returns false if there is none by that name.
    pub fn remove(&mut self, name: &str) -> bool {
        let Some(v) = self.vips.iter_mut().find(|v| v.name.eq_ignore_ascii_case(name) && !v.removed) else {
            return false;
        };
        v.removed = true;
        self.save();
        true
    }
}

/// `remove-vip NAME`: stop watching an invited VIP (takes effect when the watcher restarts).
pub fn remove_from(data_dir: &Path, name: &str) -> Result<()> {
    let mut v = Vips::load(data_dir);
    anyhow::ensure!(v.remove(name), "no invited VIP named {name} (VIPs from the config file are removed there)");
    Ok(())
}

fn person_of(v: &InvitedVip) -> PersonConfig {
    PersonConfig {
        name: v.name.clone(),
        devices: if v.removed { vec![] } else { vec![v.device.clone()] },
        telegram_chats: vec![],
        duress_ntfy: vec![],
        phone: None,
        invite_round: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("warn-vips-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_invite_lets_one_device_in_once() {
        let dir = temp_dir("a");
        let mut v = Vips::load(&dir);
        let secret = new_secret();
        assert_eq!(secret.len(), 22);
        assert!(secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        // The web app computes the same (webapp/src/core.js, secretHash).
        assert_eq!(secret_hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(v.add_invite(&secret_hash(&secret), "Alex", 1000, 100));
        assert!(!v.add_invite(&secret_hash(&secret), "Alex", 1000, 100), "known");
        let anna = Keys::generate().public_key();
        let taken = vec!["Alex".to_string()];
        assert_eq!(v.enroll("wrong", "Anna", &anna, 200, &taken, 20).unwrap_err().0, "this invite is not known here");
        assert!(v.enroll(&secret, "alex", &anna, 200, &taken, 20).is_err(), "name taken");
        let enrolled = v.enroll(&secret, " Anna ", &anna, 200, &taken, 20).unwrap();
        assert_eq!((enrolled.person.name.as_str(), enrolled.by.as_str()), ("Anna", "Alex"));
        assert_eq!(enrolled.person.device_keys().unwrap(), vec![anna]);
        // The answer got lost: asking again gives the same.
        assert_eq!(v.enroll(&secret, "Anna", &anna, 210, &taken, 20).unwrap().person.name, "Anna");
        // Someone else with the same link: used.
        let bo = Keys::generate().public_key();
        assert_eq!(v.enroll(&secret, "Bo", &bo, 220, &taken, 20).unwrap_err().0, "this invite was already used");
        // Survives a restart; removing keeps the place but drops the device.
        let mut v = Vips::load(&dir);
        assert_eq!(v.persons().len(), 1);
        assert!(v.remove("anna"));
        assert!(v.persons()[0].devices.is_empty());
        assert_eq!(v.active(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_phone_link_works_once_and_briefly() {
        let dir = temp_dir("c");
        let mut v = Vips::load(&dir);
        let h = secret_hash("p1");
        assert!(v.add_device_invite(&h, "Alex", u64::MAX, 100));
        assert_eq!(v.invites[&h].expires, 100 + MAX_DEVICE_INVITE_SECONDS);
        assert_eq!(v.device_invite_person(&h).as_deref(), Some("Alex"));
        // Not a VIP invite.
        let k = Keys::generate().public_key();
        assert!(v.enroll("p1", "Anna", &k, 200, &[], 20).is_err());
        assert!(v.use_device_invite(&h, 200).is_ok());
        assert_eq!(v.use_device_invite(&h, 210).unwrap_err().0, "this link was already used: make a new one");
        v.add_device_invite(&secret_hash("p2"), "Alex", 300, 100);
        assert!(v.use_device_invite(&secret_hash("p2"), 300).is_err(), "expired");
        assert!(v.device_invite_person(&secret_hash("nope")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn base64url_matches_the_standard() {
        assert_eq!(base64url(b"\xfb\xff\x00\x10hello world!!!!!"), "-_8AEGhlbGxvIHdvcmxkISEhISE");
        assert_eq!(base64url(&[0u8; 16]).len(), 22);
    }

    #[test]
    fn invite_event_round_trip() {
        let info = InviteInfo { by: "Alex".into(), w: vec!["ab".repeat(32)], r: vec!["wss://nos.lol".into()], t: String::new() };
        let ev = invite_event("secret-1", &info, 2_000_000_000).unwrap();
        assert_eq!(read_invite_event("secret-1", &ev).unwrap(), info);
        assert!(read_invite_event("secret-2", &ev).is_err());
    }

    /// Prints an invite event for the web app to read (see webapp/test/interop.mjs).
    #[test]
    #[ignore]
    fn print_invite_for_the_web_app() {
        let info = InviteInfo { by: "Zoë".into(), w: vec!["ab".repeat(32)], r: vec!["wss://nos.lol".into()], t: String::new() };
        println!("INVITE {}", serde_json::to_string(&invite_event("BBBBBBBBBBBBBBBBBBBBBB", &info, 4_000_000_000).unwrap()).unwrap());
    }

    #[test]
    fn reads_an_invite_made_by_the_web_app() {
        // Written by `node webapp/test/interop.mjs fixture`.
        let text = include_str!("../../proto/tests/fixtures/js_invite.json");
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        let ev: Event = serde_json::from_value(v["event"].clone()).unwrap();
        let info = read_invite_event(v["secret"].as_str().unwrap(), &ev).unwrap();
        assert_eq!(info.by, "Zoë");
        assert_eq!(info.r, vec!["wss://nos.lol".to_string()]);
    }

    #[test]
    fn invites_expire_and_servers_fill_up() {
        let dir = temp_dir("b");
        let mut v = Vips::load(&dir);
        v.add_invite(&secret_hash("s1"), "M", 300, 100);
        let k = Keys::generate().public_key();
        assert_eq!(v.enroll("s1", "A", &k, 300, &[], 20).unwrap_err().0, "this invite has expired");
        v.add_invite(&secret_hash("s2"), "M", 1000, 100);
        assert!(v.enroll("s2", "A", &k, 300, &[], 0).is_err(), "full");
        // Never longer than the maximum.
        v.add_invite(&secret_hash("s3"), "M", u64::MAX, 100);
        assert_eq!(v.invites[&secret_hash("s3")].expires, 100 + MAX_INVITE_SECONDS);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invite_carries_inviter_angel_key_and_derives_consistent_angel_secret() {
        let dir = temp_dir("d");
        let mut v = Vips::load(&dir);
        let secret = new_secret();
        let hash = secret_hash(&secret);
        let angel_key = Keys::generate().public_key().to_hex();
        let push = PushSub {
            endpoint: "https://push.example/1".into(),
            p256dh: "p".into(),
            auth: "a".into(),
            vapid_private: "d".into(),
            vapid_public: "q".into(),
        };
        v.add_invite(&hash, "Alex", 1000, 100);
        // Invalid key is ignored; valid key + push are saved and returned on enroll.
        v.set_invite_angel(&hash, Some("not-hex".into()), Some(push.clone()));
        assert!(v.invites[&hash].angel_key.is_none());
        v.set_invite_angel(&hash, Some(angel_key.clone()), Some(push.clone()));
        // A peer forwarding without push does not overwrite existing push.
        v.set_invite_angel(&hash, Some(angel_key.clone()), None);
        let loaded = Vips::load(&dir);
        assert_eq!(loaded.invites[&hash].push.as_ref(), Some(&push));

        let anna = Keys::generate().public_key();
        let enrolled = v.enroll(&secret, "Anna", &anna, 200, &[], 20).unwrap();
        assert_eq!(enrolled.angel_key.as_deref(), Some(angel_key.as_str()));
        assert_eq!(enrolled.push, Some(push));

        let a1 = derived_angel_secret(&secret);
        let a2 = derived_angel_secret(&secret);
        assert_eq!(a1, a2);
        assert_eq!(a1.len(), 22);
        assert_ne!(a1, secret);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_signup_allows_multiple_users_and_disambiguates_duplicate_names() {
        let dir = temp_dir("open");
        let mut v = Vips::load(&dir);
        let d1 = Keys::generate().public_key();
        let d2 = Keys::generate().public_key();
        let d3 = Keys::generate().public_key();
        let mut taken = vec!["Owner".to_string()];

        let e1 = v.enroll_open("  Alex  ", &d1, 100, &taken, 3).unwrap();
        assert_eq!(e1.person.name, "Alex");
        assert!(e1.by.is_empty());
        taken.push(e1.person.name.clone());

        let e2 = v.enroll_open("alex", &d2, 110, &taken, 3).unwrap();
        assert_eq!(e2.person.name, "alex 2");
        taken.push(e2.person.name.clone());

        // Idempotent retry by d2 still returns its assigned unique name even if it asks as "alex".
        let e2_retry = v.enroll_open("alex", &d2, 115, &taken, 3).unwrap();
        assert_eq!(e2_retry.person.name, "alex 2");

        // 40-char collision truncates cleanly to stay within MAX_NAME_CHARS.
        let long = "a".repeat(MAX_NAME_CHARS);
        taken.push(long.clone());
        let e3 = v.enroll_open(&long, &d3, 120, &taken, 3).unwrap();
        assert!(e3.person.name.chars().count() <= MAX_NAME_CHARS);
        assert!(e3.person.name.ends_with(" 2"));

        // Max cap enforced.
        let d4 = Keys::generate().public_key();
        assert_eq!(
            v.enroll_open("Sam", &d4, 130, &taken, 3).unwrap_err().0,
            "this server watches as many people as it can"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
