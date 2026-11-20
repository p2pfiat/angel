//! Shared message format and Nostr transport helpers.
//!
//! Every message is a JSON payload inside an unsigned *rumor*, sealed with the sender's
//! device key and gift-wrapped (NIP-59) to each watcher. Relays only see a gift wrap from a
//! throwaway key, with a randomized timestamp. The seal proves which device sent it.

use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use nostr::nips::nip59::{GiftWrapBuilder, UnwrappedGift};
use nostr::prelude::*;
use serde::{Deserialize, Serialize};

/// Kind of the inner rumor. Provisional: never published unwrapped.
pub const RUMOR_KIND: u16 = 4747;

/// Protocol version carried in every payload.
pub const VERSION: u8 = 1;

/// Gift wraps expire on relays after this long (NIP-40). Must exceed 2 days, because
/// NIP-59 backdates the wrap's timestamp by up to 2 days.
pub const WRAP_EXPIRATION: Duration = Duration::from_secs(3 * 24 * 3600);

/// What the device reports alongside a message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// Human-readable device label, e.g. "laptop".
    pub device: String,
    /// What caused the message: "logon", "unlock", "duress_hotkey", "test", ...
    pub trigger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// A login/unlock happened. The watcher expects `SessionOk` within `timeout_secs`.
    SessionStart {
        session: String,
        timeout_secs: u64,
        context: Context,
    },
    /// The secret action was performed for this session.
    SessionOk { session: String },
    /// Immediate alarm.
    Duress { context: Context },
    /// Harmless test alert to every active angel, requested from a VIP's device.
    Test { context: Context },
    /// Between watchers that alert the same contacts: this watcher has sent the alarm with
    /// this id, so the others need not send it again.
    AlarmSent { alarm: String },
    /// Between watchers of the same person: a Telegram chat joined (approved through the join
    /// link) or left (`/stop`). The newest change per chat wins.
    ContactChanged {
        person: String,
        chat: i64,
        name: String,
        active: bool,
        /// Gets "did not check in" alarms (duress always goes to every active angel).
        #[serde(default = "yes")]
        missed: bool,
        /// Gets "checked in late" notices.
        #[serde(default = "yes")]
        late_ok: bool,
        /// Taken off the list by the VIP (gets nothing; the join link adds them again).
        #[serde(default, skip_serializing_if = "is_false")]
        removed: bool,
        /// An angel with their own key (hex) instead of a Telegram chat; `chat` is then 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// From a watcher to every device of a VIP it watches: "I watch you". Sent at start and
    /// then every `interval_secs`, so the app can tell watched, offline and not confirmed.
    Watching {
        person: String,
        interval_secs: u64,
        /// The nonce of the `ping` this answers; absent in the periodic confirmation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        /// The VIP's join link for new angels (`t.me/<bot>?start=<code>`), when the watcher has
        /// a Telegram bot. Only ever sent to the VIP's own devices.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invite: Option<String>,
        /// The VIP's angels, for the list in the app. Only ever sent to the VIP's own devices.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        angels: Vec<Angel>,
        /// The VIP's phones (and other devices), for the list in the app. Only ever sent to the
        /// VIP's own devices.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        phones: Vec<Phone>,
        /// Optional friendly name this watcher advertises (e.g. "Home server").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        watcher: Option<String>,
    },
    /// From a device to its watchers: "do you watch me, right now?" A watcher that does answers
    /// at once with `watching` carrying this nonce. Same path as an alarm, so an answer proves
    /// that path works end to end.
    Ping { nonce: String },
    /// From a VIP's device to its watchers, or from a watcher to its peers: an invite for a
    /// new VIP (design.md, "Step 2"). `hash` is the sha256 (hex) of the secret in the link;
    /// `person` (only between watchers) is who invites.
    InviteCreated {
        hash: String,
        expires: u64,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        person: String,
        /// An invite for angels (any number may join with it) rather than for one new VIP.
        #[serde(default, skip_serializing_if = "is_false")]
        angel: bool,
        /// A link to add a phone to this VIP (design.md, "Adding a phone"): the device that
        /// enrols with it becomes one more device of `person`. Valid briefly, used once.
        #[serde(default, skip_serializing_if = "is_false")]
        device: bool,
        /// The inviter's angel key (hex) and Web Push subscription, so the inviter is an app
        /// angel of the invited VIP as soon as they enrol.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        angel_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        push: Option<PushSub>,
    },
    /// From a watcher to the inviting device: this watcher accepts the invite.
    InviteOk { hash: String },
    /// From a new device to a watcher: join as a new VIP with the secret from the link. The
    /// watcher answers `watching` (reply_to = the secret's hash) or `refused`.
    Enroll { secret: String, name: String, device: String },
    /// From a watcher: an invite or enrolment it does not accept, and why.
    Refused { reply_to: String, reason: String },
    /// From a watcher to the inviter's devices: someone accepted the invite. `invite` is the
    /// new VIP's join link, so the inviter can become their first angel.
    VipJoined {
        person: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invite: Option<String>,
        /// Secret of an angel link for `person`, so the inviter's app can register or update
        /// its angel key and push subscription without manual steps.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        angel_secret: Option<String>,
    },
    /// From an angel's app to a VIP's watchers: become an angel with the secret of the VIP's
    /// angel link. Answered with `angel_ok` or `refused` (reply_to = the secret's hash).
    AngelJoin {
        secret: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        push: Option<PushSub>,
    },
    /// From a watcher: the angel is in; `person` is the VIP. `invite`: a link with which the angel
    /// can be protected too (by these watchers), for the angels of a watcher's own VIPs.
    AngelOk {
        reply_to: String,
        person: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invite: Option<String>,
    },
    /// From a watcher to an angel's key: an alarm (or a notice about one) for `person`.
    /// `kind`: duress, missed, late_ok, test or notice.
    AngelMessage { id: String, person: String, kind: String, text: String },
    /// From an angel's app to the watchers: "I am calling" (`call`) or "Resolved" (`done`).
    Ack { alarm: String, action: String },
    /// A VIP's device added or removed: from one of the VIP's devices (remove, rename), or
    /// between watchers (to keep the same list). The newest change per device wins.
    PhoneChanged {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        person: String,
        /// The device's key (hex).
        key: String,
        name: String,
        #[serde(default, skip_serializing_if = "is_false")]
        removed: bool,
    },
    /// A VIP's phone registered or cleared its secret ntfy.sh topic for iPhone Shortcuts,
    /// or a watcher forwarding that change to peer watchers. The newest change per person wins.
    TopicChanged {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        person: String,
        /// Full `https://ntfy.sh/<topic>` URL, or empty string to stop following.
        url: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        device: String,
        #[serde(default)]
        cancel_seconds: u64,
    },
}

/// One of a VIP's devices as the watchers know it (see `watching`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phone {
    /// Hex key.
    pub key: String,
    pub name: String,
    /// When it was added (0 for a device from the config file).
    #[serde(default)]
    pub at: u64,
    /// When this watcher last heard from it (0: not since it started keeping track).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seen: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Where a watcher pushes alarms to an angel's phone (Web Push), and the key it may push with.
/// The VAPID key is the angel's own: every watcher that alerts this angel gets the same one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSub {
    pub endpoint: String,
    /// The subscription's public key and auth secret (base64url, from `getKey`).
    pub p256dh: String,
    pub auth: String,
    /// The angel's VAPID key pair (base64url: private scalar, uncompressed public point).
    pub vapid_private: String,
    pub vapid_public: String,
}

fn yes() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One angel as the watchers know it (see `watching`). The VIP changes one by sending
/// `contact_changed` from one of their devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Angel {
    pub chat: i64,
    /// Set for an angel with their own key (the app); `chat` is then 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub name: String,
    pub active: bool,
    pub missed: bool,
    pub late_ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u8,
    #[serde(flatten)]
    pub msg: Message,
}

impl Envelope {
    pub fn new(msg: Message) -> Self {
        Self { v: VERSION, msg }
    }
}

/// A message after unwrapping and authentication.
#[derive(Debug, Clone)]
pub struct Received {
    /// Device key that sealed the message (authenticated).
    pub sender: PublicKey,
    /// Rumor id: unique per message, identical across all recipients' copies.
    pub id: EventId,
    /// Sender's clock when the message was created.
    pub created_at: Timestamp,
    pub envelope: Envelope,
}

/// Build one gift wrap per recipient for the same message.
pub fn wrap(keys: &Keys, recipients: &[PublicKey], envelope: &Envelope) -> Result<Vec<Event>> {
    if recipients.is_empty() {
        bail!("no recipients configured");
    }
    let json = serde_json::to_string(envelope)?;
    let mut rumor = EventBuilder::new(Kind::Custom(RUMOR_KIND), json).finalize_unsigned(keys.public_key());
    rumor.ensure_id();
    recipients
        .iter()
        .map(|r| {
            GiftWrapBuilder::new(*r, rumor.clone())
                .expiration(WRAP_EXPIRATION)
                .finalize(keys)
                .context("building gift wrap")
        })
        .collect()
}

/// Unwrap a gift wrap addressed to `keys`. Returns `Ok(None)` for events that are not ours
/// (wrong kind, not decryptable, other rumor kind) and `Err` for malformed payloads.
pub fn unwrap(keys: &Keys, event: &Event) -> Result<Option<Received>> {
    if event.kind != Kind::GiftWrap {
        return Ok(None);
    }
    let Ok(UnwrappedGift { sender, rumor }) = UnwrappedGift::from_gift_wrap(keys, event) else {
        return Ok(None);
    };
    if rumor.kind != Kind::Custom(RUMOR_KIND) {
        return Ok(None);
    }
    if rumor.pubkey != sender {
        bail!("rumor author does not match seal signer");
    }
    let envelope: Envelope = serde_json::from_str(&rumor.content).context("parsing payload")?;
    if envelope.v != VERSION {
        bail!("unsupported protocol version {}", envelope.v);
    }
    let id = rumor.id.context("rumor without id")?;
    Ok(Some(Received {
        sender,
        id,
        created_at: rumor.created_at,
        envelope,
    }))
}

/// Random identifier for a session (hex).
pub fn new_session_id() -> String {
    Keys::generate().public_key().to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            device: "laptop".into(),
            trigger: "unlock".into(),
            location: Some("home".into()),
            note: None,
        }
    }

    #[test]
    fn roundtrip_to_each_recipient() {
        let device = Keys::generate();
        let w1 = Keys::generate();
        let w2 = Keys::generate();
        let env = Envelope::new(Message::SessionStart {
            session: new_session_id(),
            timeout_secs: 60,
            context: ctx(),
        });
        let wraps = wrap(&device, &[w1.public_key(), w2.public_key()], &env).unwrap();
        assert_eq!(wraps.len(), 2);
        // Relays never see the device key.
        assert!(wraps.iter().all(|w| w.pubkey != device.public_key()));

        let r1 = unwrap(&w1, &wraps[0]).unwrap().unwrap();
        let r2 = unwrap(&w2, &wraps[1]).unwrap().unwrap();
        assert_eq!(r1.sender, device.public_key());
        assert_eq!(r1.envelope, env);
        assert_eq!(r1.id, r2.id, "same message id for every recipient");
    }

    #[test]
    fn other_recipient_cannot_read() {
        let device = Keys::generate();
        let w1 = Keys::generate();
        let outsider = Keys::generate();
        let env = Envelope::new(Message::Duress { context: ctx() });
        let wraps = wrap(&device, &[w1.public_key()], &env).unwrap();
        assert!(unwrap(&outsider, &wraps[0]).unwrap().is_none());
    }

    #[test]
    fn payload_json_shape() {
        let env = Envelope::new(Message::SessionOk { session: "abc".into() });
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"v":1,"type":"session_ok","session":"abc"}"#);
        let env = Envelope::new(Message::AlarmSent { alarm: "0123456789ab".into() });
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"v":1,"type":"alarm_sent","alarm":"0123456789ab"}"#);
        // The web app reads this one (webapp/src/core.js, fetchWatching).
        let env = Envelope::new(Message::Watching { person: "A".into(), interval_secs: 21600, reply_to: None, invite: None, angels: vec![], phones: vec![], watcher: None });
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"v":1,"type":"watching","person":"A","interval_secs":21600}"#);
        let env = Envelope::new(Message::Watching {
            person: "A".into(),
            interval_secs: 3600,
            reply_to: Some("n1".into()),
            invite: Some("https://t.me/b?start=c".into()),
            angels: vec![Angel { chat: 7, key: None, name: "Bo".into(), active: true, missed: false, late_ok: true }],
            phones: vec![],
            watcher: Some("Home server".into()),
        });
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"type":"watching","person":"A","interval_secs":3600,"reply_to":"n1","invite":"https://t.me/b?start=c","angels":[{"chat":7,"name":"Bo","active":true,"missed":false,"late_ok":true}],"watcher":"Home server"}"#
        );
        // The web app sends this one to change an angel.
        let edit: Envelope = serde_json::from_str(
            r#"{"v":1,"type":"contact_changed","person":"A","chat":7,"name":"Bo","active":true,"missed":true,"late_ok":false,"removed":true}"#,
        )
        .unwrap();
        assert!(matches!(edit.msg, Message::ContactChanged { removed: true, late_ok: false, .. }));
        // The web app sends these when inviting and joining.
        let m: Envelope = serde_json::from_str(r#"{"v":1,"type":"invite_created","hash":"h","expires":5}"#).unwrap();
        assert_eq!(m.msg, Message::InviteCreated { hash: "h".into(), expires: 5, person: String::new(), angel: false, device: false, angel_key: None, push: None });
        let m: Envelope = serde_json::from_str(
            r#"{"v":1,"type":"invite_created","hash":"h","expires":5,"angel_key":"ab","push":{"endpoint":"https://push.example/x","p256dh":"p","auth":"a","vapid_private":"d","vapid_public":"q"}}"#,
        )
        .unwrap();
        assert!(matches!(m.msg, Message::InviteCreated { angel_key: Some(_), push: Some(_), .. }));
        // Adding a phone.
        let m: Envelope = serde_json::from_str(r#"{"v":1,"type":"invite_created","hash":"h","expires":5,"device":true}"#).unwrap();
        assert!(matches!(m.msg, Message::InviteCreated { device: true, angel: false, .. }));
        let m: Envelope = serde_json::from_str(r#"{"v":1,"type":"phone_changed","key":"ab","name":"old phone","removed":true}"#).unwrap();
        assert_eq!(m.msg, Message::PhoneChanged { person: String::new(), key: "ab".into(), name: "old phone".into(), removed: true });
        let json = serde_json::to_string(&Envelope::new(Message::Watching {
            person: "A".into(),
            interval_secs: 1,
            reply_to: None,
            invite: None,
            angels: vec![],
            phones: vec![Phone { key: "ab".into(), name: "iPhone".into(), at: 9, seen: 12 }],
            watcher: None,
        }))
        .unwrap();
        assert_eq!(json, r#"{"v":1,"type":"watching","person":"A","interval_secs":1,"phones":[{"key":"ab","name":"iPhone","at":9,"seen":12}]}"#);
        // Angels with their own key.
        let m: Envelope = serde_json::from_str(
            r#"{"v":1,"type":"angel_join","secret":"s","name":"Sam","push":{"endpoint":"https://push.example/x","p256dh":"p","auth":"a","vapid_private":"d","vapid_public":"q"}}"#,
        )
        .unwrap();
        assert!(matches!(m.msg, Message::AngelJoin { push: Some(_), .. }));
        let m: Envelope = serde_json::from_str(r#"{"v":1,"type":"ack","alarm":"0123","action":"call"}"#).unwrap();
        assert_eq!(m.msg, Message::Ack { alarm: "0123".into(), action: "call".into() });
        let json = serde_json::to_string(&Envelope::new(Message::AngelMessage {
            id: "i".into(),
            person: "A".into(),
            kind: "duress".into(),
            text: "t".into(),
        }))
        .unwrap();
        assert_eq!(json, r#"{"v":1,"type":"angel_message","id":"i","person":"A","kind":"duress","text":"t"}"#);
        let m: Envelope =
            serde_json::from_str(r#"{"v":1,"type":"enroll","secret":"s","name":"Anna","device":"phone"}"#).unwrap();
        assert_eq!(m.msg, Message::Enroll { secret: "s".into(), name: "Anna".into(), device: "phone".into() });
        // And reads these.
        let json = serde_json::to_string(&Envelope::new(Message::Refused { reply_to: "h".into(), reason: "full".into() })).unwrap();
        assert_eq!(json, r#"{"v":1,"type":"refused","reply_to":"h","reason":"full"}"#);
        let json = serde_json::to_string(&Envelope::new(Message::VipJoined { person: "Anna".into(), invite: None, angel_secret: None })).unwrap();
        assert_eq!(json, r#"{"v":1,"type":"vip_joined","person":"Anna"}"#);
        let json = serde_json::to_string(&Envelope::new(Message::VipJoined {
            person: "Anna".into(),
            invite: Some("https://t.me/b?start=c".into()),
            angel_secret: Some("sec".into()),
        }))
        .unwrap();
        assert_eq!(json, r#"{"v":1,"type":"vip_joined","person":"Anna","invite":"https://t.me/b?start=c","angel_secret":"sec"}"#);
        let json = serde_json::to_string(&Envelope::new(Message::InviteOk { hash: "h".into() })).unwrap();
        assert_eq!(json, r#"{"v":1,"type":"invite_ok","hash":"h"}"#);
        // The web app sends this one.
        let ping: Envelope = serde_json::from_str(r#"{"v":1,"type":"ping","nonce":"n1"}"#).unwrap();
        assert_eq!(ping.msg, Message::Ping { nonce: "n1".into() });
        let test_env = Envelope::new(Message::Test {
            context: Context { device: "phone".into(), trigger: "test_app".into(), location: None, note: None },
        });
        let test_json = serde_json::to_string(&test_env).unwrap();
        assert_eq!(test_json, r#"{"v":1,"type":"test","context":{"device":"phone","trigger":"test_app"}}"#);
        let parsed: Envelope = serde_json::from_str(&test_json).unwrap();
        assert_eq!(parsed, test_env);
        // App registers or clears an ntfy.sh topic; watchers forward it with `person`.
        let topic_app: Envelope = serde_json::from_str(
            r#"{"v":1,"type":"topic_changed","url":"https://ntfy.sh/s0123456789abcdef0123456789abcdef01","device":"iphone","cancel_seconds":20}"#,
        )
        .unwrap();
        assert_eq!(
            topic_app.msg,
            Message::TopicChanged {
                person: String::new(),
                url: "https://ntfy.sh/s0123456789abcdef0123456789abcdef01".into(),
                device: "iphone".into(),
                cancel_seconds: 20,
            }
        );
        let topic_peer = Envelope::new(Message::TopicChanged {
            person: "A".into(),
            url: "https://ntfy.sh/s0123456789abcdef0123456789abcdef01".into(),
            device: "iphone".into(),
            cancel_seconds: 20,
        });
        let topic_peer_json = serde_json::to_string(&topic_peer).unwrap();
        assert_eq!(
            topic_peer_json,
            r#"{"v":1,"type":"topic_changed","person":"A","url":"https://ntfy.sh/s0123456789abcdef0123456789abcdef01","device":"iphone","cancel_seconds":20}"#
        );
        let topic_clear: Envelope = serde_json::from_str(r#"{"v":1,"type":"topic_changed","url":""}"#).unwrap();
        assert_eq!(
            topic_clear.msg,
            Message::TopicChanged {
                person: String::new(),
                url: String::new(),
                device: String::new(),
                cancel_seconds: 0,
            }
        );
    }
}
