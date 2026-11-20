//! The watcher loop: follows Nostr relays for messages from protected devices, enforces
//! check-in deadlines and alerts contacts. Only makes outbound connections.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use nostr_sdk::prelude::*;
use tokio::sync::mpsc;

use crate::config::{normalize_phone, render, Config, PersonConfig, Vars};
use crate::contacts::{invite_code, AngelId, Contacts, Entry, PushSub};
use crate::phones::{clean_name, PhoneEntry, Phones, MAX_ADDED};
use crate::vips::{
    derived_angel_secret, invite_event, new_secret, secret_hash, Enrolled, InviteInfo, Refusal, Vips, MAX_ANGEL_INVITE_SECONDS,
    MAX_DEVICE_INVITE_SECONDS, MAX_INVITE_SECONDS, MAX_OFFER_SECONDS, OPEN_SIGNUP_PREFIX,
};
use crate::engine::{Alarm, AlarmKind, Engine, Persisted};
use crate::ntfy::{self, NtfyMessage};
use crate::telegram::{ButtonPress, Telegram, Update};
use crate::topics::{validate_app_topic_url, TopicEntry, Topics, MAX_CANCEL_SECONDS};

/// After Telegram reports that another watcher already polls this bot, wait this long before
/// trying again. Sending keeps working meanwhile; only button handling is left to the other.
const POLL_CONFLICT_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Right after start, relays replay old messages in any order: a peer's "sent" report may come
/// after the message that makes this watcher alarm. Hold alarms at least this long after start.
const STARTUP_HOLD: u64 = 20;

/// How often a watcher tells the VIPs' devices it watches them (also once at start). Apps
/// call a watcher offline after two intervals without word.
const WATCHING_INTERVAL: Duration = Duration::from_secs(3600);

/// Answer a ping only if it is this recent (relays replay old ones after a restart) ...
const PING_MAX_AGE: u64 = 120;
/// ... and at most this often per device (a ping costs one message to the relays).
const PING_MIN_GAP: u64 = 5;

/// Forget peers' "sent" reports after this long.
const PEER_REPORT_RETENTION: u64 = 24 * 3600;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Load the standalone watcher key from `data_dir`, creating it on first use.
pub fn load_or_create_key(data_dir: &Path) -> Result<Keys> {
    let path = data_dir.join("watcher.key");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return Keys::parse(text.trim()).context("invalid watcher.key");
    }
    std::fs::create_dir_all(data_dir)?;
    let keys = Keys::generate();
    write_private(&path, &keys.secret_key().to_bech32()?)?;
    tracing::info!("created new watcher key in {}", path.display());
    Ok(keys)
}

pub(crate) fn write_private(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

struct Notifier {
    telegram: Option<Telegram>,
    /// Angels with their own key (Nostr and Web Push); none in one-shot commands.
    keyed: Option<KeyAngels>,
}

/// What reaching angels with their own key needs.
struct KeyAngels {
    client: Client,
    keys: Keys,
    http: reqwest::Client,
    /// VAPID contact (the app's address, or a mailto).
    sub: String,
    /// (person, key) of push subscriptions that are gone, for the main loop to forget.
    gone: mpsc::UnboundedSender<(String, String)>,
}

/// A notice that every watcher of a VIP would send: held like an alarm, so that with peers
/// only one sends it (the other hears `alarm_sent` with this id and skips it).
enum Note {
    Chats { id: String, chats: Vec<i64>, text: String },
    Keys { id: String, person: String, to: Vec<(String, Option<PushSub>)>, kind: &'static str, text: String },
}

impl Note {
    fn id(&self) -> &str {
        match self {
            Note::Chats { id, .. } | Note::Keys { id, .. } => id,
        }
    }
}

impl KeyAngels {
    /// Send to these angels: a gift wrap with the whole message, and a Web Push to wake the
    /// phone. Returns whether the wrap reached a relay.
    async fn tell(&self, angels: Vec<(String, Option<PushSub>)>, person: &str, id: &str, kind: &str, text: &str) -> bool {
        let to: Vec<PublicKey> = angels.iter().filter_map(|(k, _)| PublicKey::from_hex(k).ok()).collect();
        if to.is_empty() {
            return false;
        }
        let msg = warn_proto::Message::AngelMessage {
            id: id.to_string(),
            person: person.to_string(),
            kind: kind.to_string(),
            text: text.to_string(),
        };
        let mut any = false;
        match warn_proto::wrap(&self.keys, &to, &warn_proto::Envelope::new(msg)) {
            Ok(wraps) => {
                for w in wraps {
                    match self.client.send_event(&w).await {
                        Ok(_) => any = true,
                        Err(e) => tracing::error!("alarm to an angel's key not delivered: {e}"),
                    }
                }
            }
            Err(e) => tracing::error!("building the angels' message failed: {e:#}"),
        }
        let (title, body) = text.split_once('\n').unwrap_or((text, ""));
        let payload = serde_json::json!({
            "title": title,
            "body": body.trim().chars().take(2500).collect::<String>(),
            "id": id,
            "kind": kind,
            "person": person,
        })
        .to_string();
        for (key, push) in angels {
            let Some(push) = push else { continue };
            let (http, sub, gone, person, payload) =
                (self.http.clone(), self.sub.clone(), self.gone.clone(), person.to_string(), payload.clone());
            tokio::spawn(async move {
                match crate::webpush::send(&http, &push, payload.as_bytes(), &sub, now()).await {
                    crate::webpush::Outcome::Sent => {}
                    crate::webpush::Outcome::Gone => {
                        tracing::warn!("push to an angel of {person} is gone (notifications off or app removed)");
                        let _ = gone.send((person, key));
                    }
                    crate::webpush::Outcome::Failed(e) => tracing::error!("push to an angel of {person} failed: {e}"),
                }
            });
        }
        any
    }
}

impl Notifier {
    /// Tell every active angel of this person, over every channel (notices, acks).
    async fn tell_all(&self, contacts: &Contacts, person: &PersonConfig, id: &str, text: &str) {
        self.send(&contacts.chats(person), text, &[]).await;
        if let Some(k) = &self.keyed {
            k.tell(contacts.keys_for(&person.name, AlarmKind::Duress), &person.name, id, "notice", text).await;
        }
    }

    /// Returns whether at least one chat got the message.
    async fn send(&self, chats: &[i64], text: &str, buttons: &[(String, String)]) -> bool {
        let Some(tg) = &self.telegram else {
            tracing::warn!("[dry run, no telegram configured]\n{text}");
            return false;
        };
        let mut any = false;
        for chat in chats {
            // Retry a few times: this message may be the one that matters.
            for attempt in 1..=5 {
                match tg.send(*chat, text, buttons).await {
                    Ok(()) => {
                        any = true;
                        break;
                    }
                    Err(e) => {
                        tracing::error!("telegram send to {chat} failed (attempt {attempt}): {e}");
                        tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
                    }
                }
            }
        }
        any
    }
}

/// Send the test text to one person's contacts.
pub async fn test_alert(cfg: &Config, name: &str) -> Result<()> {
    let person = cfg
        .persons
        .iter()
        .find(|p| p.name == name)
        .with_context(|| format!("no person named {name} in config"))?;
    let notifier = Notifier {
        telegram: cfg.telegram.as_ref().map(|t| Telegram::new(&t.bot_token)).transpose()?,
        keyed: None,
    };
    // With the phone line of a real alarm, so contacts can try calling from it.
    let text = with_phone(cfg, person, render(&cfg.messages.test, &vars(&person.name, None, "")));
    notifier.send(&Contacts::load(cfg).chats(person), &text, &[]).await;
    println!("test alert sent for {name}");
    Ok(())
}

/// A change to one angel, made in the agent's Angels window.
#[derive(Debug, Clone)]
pub struct AngelEdit {
    pub person: String,
    pub id: AngelId,
    pub entry: Entry,
}

/// The running watcher's inbox for angel edits (none while no watcher runs).
static ANGEL_EDITS: std::sync::Mutex<Option<mpsc::UnboundedSender<AngelEdit>>> = std::sync::Mutex::new(None);

/// Hand an edit to the running watcher, which saves it and tells the other watchers. Returns
/// false when no watcher runs in this process.
pub fn edit_angel(edit: AngelEdit) -> bool {
    ANGEL_EDITS.lock().ok().and_then(|g| g.as_ref().map(|tx| tx.send(edit).is_ok())).unwrap_or(false)
}

/// A change to one of a VIP's devices, made in the agent's VIPs window.
#[derive(Debug, Clone)]
pub struct PhoneEdit {
    pub person: String,
    /// Hex key.
    pub key: String,
    pub name: String,
    pub removed: bool,
}

/// Gets the new pairing link, or why there is none.
pub type PairReply = Box<dyn FnOnce(Result<String, String>) + Send>;

/// Requests from this process (the agent's windows) to the running watcher.
enum Local {
    Phone(PhoneEdit),
    Pair { person: String, reply: PairReply },
    Offer { person: String, reply: PairReply },
}

static LOCAL: std::sync::Mutex<Option<mpsc::UnboundedSender<Local>>> = std::sync::Mutex::new(None);

fn to_local(req: Local) -> bool {
    LOCAL.lock().ok().and_then(|g| g.as_ref().map(|tx| tx.send(req).is_ok())).unwrap_or(false)
}

/// Rename or remove a device in the running watcher (which tells the others). False when no
/// watcher runs in this process.
pub fn edit_phone(edit: PhoneEdit) -> bool {
    to_local(Local::Phone(edit))
}

/// Ask the running watcher for a link to add a phone to `person`; `reply` is called with it
/// (from the watcher's thread). False when no watcher runs in this process.
pub fn request_pair(person: &str, reply: PairReply) -> bool {
    to_local(Local::Pair { person: person.to_string(), reply })
}

/// Ask the running watcher for a link that offers this server to someone, on behalf of the
/// own VIP `person`; `reply` is called with it. False when no watcher runs in this process.
pub fn request_offer(person: &str, reply: PairReply) -> bool {
    to_local(Local::Offer { person: person.to_string(), reply })
}

/// The link a new contact taps to ask for this person's alarms (`t.me/<bot>?start=<code>`).
pub async fn invite_link(cfg: &Config, name: &str) -> Result<String> {
    let person = cfg
        .persons
        .iter()
        .find(|p| p.name == name)
        .with_context(|| format!("no person named {name} in config"))?;
    let token = &cfg.telegram.as_ref().context("no [telegram] bot configured")?.bot_token;
    let bot = Telegram::new(token)?.username().await?;
    Ok(join_link(&bot, token, person))
}

fn join_link(bot: &str, token: &str, person: &PersonConfig) -> String {
    format!("https://t.me/{bot}?start={}", invite_code(token, &person.name, person.invite_round))
}

/// The join link as the running watcher knows it: none until the bot's name is known.
fn known_link(cfg: &Config, bot: &std::sync::OnceLock<String>, person: &PersonConfig) -> Option<String> {
    Some(join_link(bot.get()?, &cfg.telegram.as_ref()?.bot_token, person))
}

/// Ask Telegram for the bot's name until it answers (needed for the join links).
async fn learn_bot_name(tg: Telegram, into: std::sync::Arc<std::sync::OnceLock<String>>) {
    loop {
        match tg.username().await {
            Ok(name) => {
                let _ = into.set(name);
                return;
            }
            Err(e) => tracing::warn!("asking the bot's name failed, retrying in a minute: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

fn vars<'a>(name: &'a str, ctx: Option<&'a warn_proto::Context>, who: &'a str) -> Vars<'a> {
    Vars {
        name,
        device: ctx.map(|c| c.device.as_str()).unwrap_or(""),
        trigger: ctx.map(|c| c.trigger.as_str()).unwrap_or(""),
        location: ctx.and_then(|c| c.location.as_deref()).unwrap_or("unknown"),
        note: ctx.and_then(|c| c.note.as_deref()).unwrap_or(""),
        who,
    }
}

fn load_state(path: &Path) -> Persisted {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Run until Ctrl+C. `keys` is the identity devices send to.
pub async fn run(mut cfg: Config, keys: Keys) -> Result<()> {
    // VIPs that came in by invite come after the config's own VIPs, always in the same order.
    let own_count = cfg.persons.len();
    let mut vips = Vips::load(&cfg.data_dir);
    cfg.persons.extend(vips.persons());
    // Invited VIP -> (day, alarms sent that day).
    let mut alarms_today: HashMap<usize, (u64, u32)> = HashMap::new();
    for p in &cfg.persons {
        p.device_keys()?;
    }
    // Each VIP's devices from the config (or vips.json); phones.json adds and removes.
    let mut base: Vec<Vec<String>> = cfg.persons.iter().map(|p| p.devices.clone()).collect();
    let mut phones = Phones::load(&cfg.data_dir);
    // Map device key -> person index.
    let mut device_owner = apply_phones(&mut cfg, &base, &phones);
    // Built into the agent: the agent's own key is a device too; give it the agent's label once.
    let own_name = match (&cfg.own_device, device_owner.get(&keys.public_key())) {
        (Some(label), Some(&i)) if !label.trim().is_empty() => {
            let person = cfg.persons[i].name.clone();
            let key = keys.public_key().to_hex();
            phones.get(&person, &key).is_none().then(|| (person, key, clean_name(label)))
        }
        _ => None,
    };
    if let Some((person, key, name)) = &own_name {
        phones.apply(person, key, PhoneEntry { name: name.clone(), at: now(), removed: false });
    }
    if cfg.persons.is_empty() {
        tracing::warn!("no [[person]] configured: nothing to watch yet");
    }

    // Other watchers alerting the same contacts (never ourselves).
    let peers: Vec<PublicKey> = cfg.peer_keys()?.into_iter().filter(|k| *k != keys.public_key()).collect();
    // Alarm id -> when a peer reported it sent.
    let mut sent_by_peer: HashMap<String, u64> = HashMap::new();
    // Alarms waiting for `send_delay_seconds` (or the startup hold): (due, alarm).
    let mut held: Vec<(u64, Alarm)> = Vec::new();
    // Notices every watcher would send (an angel joined, the drill), held the same way.
    let mut held_notes: Vec<(u64, Note)> = Vec::new();
    let started = now();
    // Device key -> when we last answered its ping.
    let mut last_pong: HashMap<PublicKey, u64> = HashMap::new();

    let state_path = cfg.data_dir.join("state.json");
    let mut engine = Engine::new(cfg.grace_seconds, cfg.max_age_minutes * 60, load_state(&state_path));

    let telegram = cfg.telegram.as_ref().map(|t| Telegram::new(&t.bot_token)).transpose()?;
    let mut notifier = Notifier { telegram: telegram.clone(), keyed: None };
    let (gone_tx, mut gone_rx) = mpsc::unbounded_channel::<(String, String)>();

    let mut contacts = Contacts::load(&cfg);
    let (edit_tx, mut edit_rx) = mpsc::unbounded_channel::<AngelEdit>();
    if let Ok(mut g) = ANGEL_EDITS.lock() {
        *g = Some(edit_tx);
    }
    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<Local>();
    if let Ok(mut g) = LOCAL.lock() {
        *g = Some(local_tx);
    }

    // Button presses and commands from Telegram arrive on this channel.
    let (button_tx, mut button_rx) = mpsc::unbounded_channel::<Update>();
    let bot_name: std::sync::Arc<std::sync::OnceLock<String>> = Default::default();
    if let Some(tg) = telegram.clone() {
        tokio::spawn(learn_bot_name(tg.clone(), bot_name.clone()));
        tokio::spawn(poll_telegram(tg, button_tx));
    }

    // Duress sources without Nostr (phone shortcuts posting to a secret ntfy topic).
    // The sender is kept for the whole run, so the channel never closes.
    let (ntfy_tx, mut ntfy_rx) = mpsc::unbounded_channel::<((usize, String, String, u64), NtfyMessage)>();
    let ntfy_since = now().saturating_sub(cfg.max_age_minutes * 60);
    for (i, p) in cfg.persons.iter().enumerate() {
        for (j, s) in p.duress_ntfy.iter().enumerate() {
            let label = format!("{}#{}", p.name, j + 1);
            let tag = (i, s.device.clone(), label.clone(), s.cancel_seconds);
            tokio::spawn(ntfy::follow(s.url.clone(), label, ntfy_since, tag, ntfy_tx.clone()));
        }
    }
    let mut topics = Topics::load(&cfg.data_dir);
    let mut dynamic_ntfy: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    for i in 0..cfg.persons.len() {
        if let Some(entry) = topics.get(&cfg.persons[i].name).cloned() {
            spawn_dynamic_ntfy(&cfg, i, &entry, &mut dynamic_ntfy, &ntfy_tx);
        }
    }

    let client = Client::new();
    for r in &cfg.relays {
        if let Err(e) = client.add_relay(r.as_str()).await {
            tracing::error!("bad relay {r}: {e}");
        }
    }
    client.connect().await;
    if let Some((person, key, name)) = own_name {
        let link = PeerLink { client: &client, keys: &keys, peers: &peers };
        link.report(warn_proto::Message::PhoneChanged { person, key, name, removed: false }).await;
    }
    notifier.keyed = Some(KeyAngels {
        client: client.clone(),
        keys: keys.clone(),
        http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?,
        sub: cfg.app_url.clone().unwrap_or_else(|| "mailto:alarm@warn.invalid".into()),
        gone: gone_tx,
    });

    // Gift wraps are backdated up to 2 days, so look back that far plus our max age.
    let since = Timestamp::from_secs(now() - 2 * 24 * 3600 - cfg.max_age_minutes * 60);
    let filter = Filter::new().pubkey(keys.public_key()).kind(Kind::GiftWrap).since(since);
    client.subscribe(filter).await?;
    let mut notifications = client.notifications();

    tracing::info!(
        "watching {} person(s) as {} on {} relay(s)",
        cfg.persons.len(),
        keys.public_key().to_bech32()?,
        cfg.relays.len()
    );

    let mut alarm_owner: HashMap<String, usize> = HashMap::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    // First tick at once: confirm right after start.
    let mut watching = tokio::time::interval(WATCHING_INTERVAL);
    let mut last_prune = now();

    loop {
        let alarms: Vec<Alarm> = tokio::select! {
            Some(n) = notifications.next() => {
                let ClientNotification::Event { event, .. } = n else { continue };
                let received = warn_proto::unwrap(&keys, &event);
                // Last seen: anything a VIP's device sends.
                if let Ok(Some(r)) = &received {
                    if device_owner.contains_key(&r.sender) {
                        phones.saw(&r.sender.to_hex(), r.created_at.as_secs().min(now()));
                    }
                }
                match received {
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::AlarmSent { alarm }, .. },
                        ..
                    })) => {
                        let fresh = now().saturating_sub(created_at.as_secs()) <= cfg.max_age_minutes * 60;
                        if !peers.contains(&sender) {
                            tracing::warn!("alarm report from a watcher not in peers: {}", sender.to_bech32()?);
                        } else if fresh && sent_by_peer.insert(alarm.clone(), now()).is_none() {
                            tracing::info!("another watcher sent alarm {alarm}");
                        }
                        vec![]
                    }
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope:
                            warn_proto::Envelope {
                                msg: warn_proto::Message::ContactChanged { person, chat, name, active, missed, late_ok, removed, key },
                                ..
                            },
                        ..
                    })) => {
                        let id = AngelId::from_parts(chat, key);
                        let entry = Entry { name, active, at: created_at.as_secs(), missed, late_ok, removed, push: None };
                        let from_vip = device_owner.get(&sender).is_some_and(|&i| cfg.persons[i].name == person);
                        if peers.contains(&sender) {
                            if cfg.persons.iter().any(|p| p.name == person) && contacts.apply(&person, &id, entry) {
                                let what = if removed { "removed" } else if active { "added or changed" } else { "switched off" };
                                tracing::info!("angels of {person}: {id} {what}");
                            }
                        } else if from_vip {
                            // The VIP's app: only angels we know (a device cannot add one).
                            if contacts.get(&person, &id).is_some() {
                                tracing::info!("angels of {person}: {id} changed in the app");
                                let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                                set_angel(&mut contacts, &link, &person, &id, entry).await;
                            } else {
                                tracing::warn!("app of {person} changed {id}, which is not one of their angels");
                            }
                        } else {
                            tracing::warn!("angel change from a key that is neither a peer nor a device of {person}: {}", sender.to_bech32()?);
                        }
                        vec![]
                    }
                    // Another watcher confirming to a device of this agent: nothing to do here.
                    Ok(Some(warn_proto::Received { envelope: warn_proto::Envelope { msg: warn_proto::Message::Watching { .. }, .. }, .. })) => vec![],
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::Ping { nonce }, .. },
                        ..
                    })) => {
                        let t = now();
                        let recent = t.abs_diff(created_at.as_secs()) <= PING_MAX_AGE;
                        let spaced = last_pong.get(&sender).is_none_or(|&at| t.saturating_sub(at) >= PING_MIN_GAP);
                        // Unknown devices get no answer: that is how an app learns this watcher
                        // does not watch it.
                        if let (Some(&person), true, true) = (device_owner.get(&sender), recent, spaced) {
                            last_pong.insert(sender, t);
                            let msg = warn_proto::Message::Watching {
                                person: cfg.persons[person].name.clone(),
                                interval_secs: WATCHING_INTERVAL.as_secs(),
                                reply_to: Some(nonce),
                                invite: known_link(&cfg, &bot_name, &cfg.persons[person]),
                                angels: angels_of(&contacts, &cfg.persons[person].name),
                                phones: phones.list(&cfg.persons[person].name, &base[person]),
                                watcher: cfg.watcher_name(),
                            };
                            tokio::spawn(send_to(client.clone(), keys.clone(), vec![sender], msg));
                        }
                        vec![]
                    }
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::InviteCreated { hash, expires, person, angel, device, angel_key, push }, .. },
                        ..
                    })) => {
                        let t = now();
                        if device {
                            // A link to add a phone: from one of the VIP's devices (for that
                            // VIP), or from a peer (for `person`).
                            let from_peer = peers.contains(&sender);
                            let who = if from_peer { Some(person) } else { device_owner.get(&sender).map(|&i| cfg.persons[i].name.clone()) };
                            if let Some(p) = who.filter(|p| cfg.persons.iter().any(|q| &q.name == p && !q.devices.is_empty())) {
                                if vips.add_device_invite(&hash, &p, expires, t) {
                                    tracing::info!("link to add a phone for {p}{}", if from_peer { " (via another watcher)" } else { "" });
                                }
                                if !from_peer && t.abs_diff(created_at.as_secs()) <= ANSWER_MAX_AGE {
                                    let reply = warn_proto::Message::InviteOk { hash };
                                    tokio::spawn(send_to(client.clone(), keys.clone(), vec![sender], reply));
                                }
                            }
                        } else if angel {
                            // An angel link: any VIP of this watcher may have one (angels are not
                            // a share of the server). Only from the VIP's own devices.
                            if let Some(&i) = device_owner.get(&sender) {
                                if vips.add_angel_invite(&hash, &cfg.persons[i].name, expires, t) {
                                    tracing::info!("angel link for {}", cfg.persons[i].name);
                                }
                                if t.abs_diff(created_at.as_secs()) <= ANSWER_MAX_AGE {
                                    let reply = warn_proto::Message::InviteOk { hash };
                                    tokio::spawn(send_to(client.clone(), keys.clone(), vec![sender], reply));
                                }
                            }
                        } else if peers.contains(&sender) {
                            if cfg.persons[..own_count].iter().any(|p| p.name == person) {
                                if vips.add_invite(&hash, &person, expires, t) {
                                    tracing::info!("invite for a new VIP from {person} (via another watcher)");
                                }
                                if angel_key.is_some() || push.is_some() {
                                    vips.set_invite_angel(&hash, angel_key, push);
                                }
                            }
                        } else if let Some(&i) = device_owner.get(&sender) {
                            let inviter_name = cfg.persons[i].name.clone();
                            let refusal = if i >= own_count {
                                Some("people who were invited cannot invite others to this server")
                            } else if !cfg.accept_invites {
                                Some("this server does not take new people")
                            } else if vips.active() >= cfg.max_invited_vips {
                                Some("this server watches as many people as it can")
                            } else {
                                None
                            };
                            let reply = match refusal {
                                Some(reason) => warn_proto::Message::Refused { reply_to: hash, reason: reason.into() },
                                None => {
                                    let added = vips.add_invite(&hash, &inviter_name, expires, t);
                                    if added {
                                        tracing::info!("invite for a new VIP from {inviter_name}");
                                    }
                                    if angel_key.is_some() || push.is_some() {
                                        vips.set_invite_angel(&hash, angel_key.clone(), push);
                                    }
                                    if added && !peers.is_empty() && t.abs_diff(created_at.as_secs()) <= ANSWER_MAX_AGE {
                                        let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                                        link.report(warn_proto::Message::InviteCreated {
                                            hash: hash.clone(),
                                            expires,
                                            person: inviter_name,
                                            angel: false,
                                            device: false,
                                            angel_key,
                                            push: None,
                                        })
                                        .await;
                                    }
                                    warn_proto::Message::InviteOk { hash }
                                }
                            };
                            // Old messages (read again after a restart) are kept but not answered.
                            if t.abs_diff(created_at.as_secs()) <= ANSWER_MAX_AGE {
                                tokio::spawn(send_to(client.clone(), keys.clone(), vec![sender], reply));
                            }
                        }
                        vec![]
                    }
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::AngelJoin { secret, name, push }, .. },
                        ..
                    })) => {
                        let t = now();
                        let hash = secret_hash(&secret);
                        let name = name.trim().chars().take(40).collect::<String>();
                        let person = vips
                            .angel_invite(&secret, t)
                            .and_then(|p| cfg.persons.iter().find(|q| q.name == p && !q.devices.is_empty()).cloned());
                        let reply = match (person, name.is_empty()) {
                            (None, _) => warn_proto::Message::Refused { reply_to: hash, reason: "this angel link is not known here or has expired".into() },
                            (Some(_), true) => warn_proto::Message::Refused { reply_to: hash, reason: "fill in your name".into() },
                            (Some(person), false) => {
                                let id = AngelId::Key(sender.to_hex());
                                // Joining again keeps the name the VIP gave and their choices.
                                let at = created_at.as_secs();
                                let entry = match contacts.get(&person.name, &id) {
                                    Some(old) => Entry { active: true, removed: false, at, push: push.clone(), ..old.clone() },
                                    None => Entry { push: push.clone(), ..Entry::new(&name, at) },
                                };
                                let shown = entry.name.clone();
                                let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                                let before = contacts.list(&person.name).len();
                                set_angel(&mut contacts, &link, &person.name, &id, entry).await;
                                let fresh = t.abs_diff(at) <= ANSWER_MAX_AGE;
                                if fresh {
                                    tracing::info!("angels of {}: {shown} joined with the app{}", person.name, if push.is_some() { "" } else { " (no notifications)" });
                                    // Every watcher of this VIP gets the same join (one rumor, one
                                    // created_at), so they agree on these ids and only one sends.
                                    let tag = format!("{}-{at}", &sender.to_hex()[..16]);
                                    let due = if peers.is_empty() { 0 } else { (t + cfg.send_delay_seconds).max(started + STARTUP_HOLD) };
                                    if contacts.list(&person.name).len() > before {
                                        let text = render(&cfg.messages.join_approved, &vars(&person.name, None, &shown));
                                        held_notes.push((due, Note::Chats { id: format!("joined-{tag}"), chats: contacts.chats(&person), text }));
                                    }
                                    // The first moment is a drill: what an alarm looks like.
                                    if notifier.keyed.is_some() {
                                        let text = render(&cfg.messages.test, &vars(&person.name, None, ""));
                                        let me = contacts.get(&person.name, &id).and_then(|e| e.push.clone());
                                        held_notes.push((
                                            due,
                                            Note::Keys { id: format!("test-{tag}"), person: person.name.clone(), to: vec![(sender.to_hex(), me)], kind: "test", text },
                                        ));
                                    }
                                }
                                // The angel can be protected too, as Telegram angels are offered in
                                // their welcome (one hop: only for the angels of own VIPs).
                                let own = cfg.persons[..own_count].iter().any(|p| p.name == person.name);
                                let invite = if fresh && own { welcome_invite(&cfg, &mut vips, &link, &person.name).await } else { None };
                                warn_proto::Message::AngelOk { reply_to: hash, person: person.name.clone(), invite }
                            }
                        };
                        if t.abs_diff(created_at.as_secs()) <= ANSWER_MAX_AGE {
                            tokio::spawn(send_to(client.clone(), keys.clone(), vec![sender], reply));
                        }
                        vec![]
                    }
                    Ok(Some(warn_proto::Received {
                        sender,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::Ack { alarm, action }, .. },
                        ..
                    })) => {
                        // An angel's "I am calling" / "Resolved" from the app. The watcher that sent
                        // the alarm tells the others (so they hear it once).
                        if let Some(&i) = alarm_owner.get(&alarm) {
                            let person = &cfg.persons[i];
                            let id = AngelId::Key(sender.to_hex());
                            let who = contacts.get(&person.name, &id).filter(|e| e.active && !e.removed).map(|e| e.name.clone());
                            let template = match action.as_str() {
                                "call" => Some(&cfg.messages.ack_call),
                                "done" => Some(&cfg.messages.ack_resolved),
                                _ => None,
                            };
                            if let (Some(who), Some(template)) = (who, template) {
                                if !sent_by_peer.contains_key(&alarm) {
                                    let text = render(template, &vars(&person.name, None, &who));
                                    notifier.tell_all(&contacts, person, &format!("{alarm}-{action}"), &text).await;
                                }
                            }
                        }
                        vec![]
                    }
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::Enroll { secret, name, device }, .. },
                        ..
                    })) if vips.device_invite_person(&secret_hash(&secret)).is_some() => {
                        // A link to add a phone to a VIP.
                        let t = now();
                        let hash = secret_hash(&secret);
                        let person = vips.device_invite_person(&hash).unwrap_or_default();
                        let i = cfg.persons.iter().position(|q| q.name == person && !q.devices.is_empty());
                        let result = match (i, device_owner.get(&sender)) {
                            (None, _) => Err(Refusal(format!("{person} is not watched here any more"))),
                            // Already in (an answer that got lost, or another watcher told us).
                            (Some(i), Some(&j)) if i == j => Ok(i),
                            (Some(_), Some(_)) => Err(Refusal("this phone already sends alarms for someone else here".into())),
                            (Some(i), None) if phones.added(&person, &base[i]) >= MAX_ADDED => {
                                Err(Refusal(format!("{person} has {MAX_ADDED} added phones: remove one first")))
                            }
                            (Some(i), None) => vips.use_device_invite(&hash, t).map(|_| i),
                        };
                        let reply = match result {
                            Ok(i) => {
                                if !device_owner.contains_key(&sender) {
                                    let entry = PhoneEntry { name: clean_name(&name), at: created_at.as_secs(), removed: false };
                                    let shown = entry.name.clone();
                                    phones.apply(&person, &sender.to_hex(), entry);
                                    device_owner = apply_phones(&mut cfg, &base, &phones);
                                    tracing::info!("{person}: phone \"{shown}\" added ({device})");
                                    let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                                    link.report(warn_proto::Message::PhoneChanged {
                                        person: person.clone(),
                                        key: sender.to_hex(),
                                        name: shown.clone(),
                                        removed: false,
                                    })
                                    .await;
                                    // Every watcher adds it: only one tells the owner.
                                    let due = if peers.is_empty() { 0 } else { (t + cfg.send_delay_seconds).max(started + STARTUP_HOLD) };
                                    let text = render(&cfg.messages.phone_added, &vars(&person, None, &shown));
                                    let id = format!("phone-{}", &sender.to_hex()[..16]);
                                    held_notes.push((due, Note::Chats { id, chats: cfg.owner_chats.clone(), text }));
                                }
                                warn_proto::Message::Watching {
                                    person: cfg.persons[i].name.clone(),
                                    interval_secs: WATCHING_INTERVAL.as_secs(),
                                    reply_to: Some(hash),
                                    invite: known_link(&cfg, &bot_name, &cfg.persons[i]),
                                    angels: angels_of(&contacts, &cfg.persons[i].name),
                                    phones: phones.list(&cfg.persons[i].name, &base[i]),
                                    watcher: cfg.watcher_name(),
                                }
                            }
                            Err(Refusal(reason)) => {
                                tracing::info!("phone for {person} refused: {reason}");
                                warn_proto::Message::Refused { reply_to: hash, reason }
                            }
                        };
                        if t.abs_diff(created_at.as_secs()) <= ANSWER_MAX_AGE {
                            tokio::spawn(send_to(client.clone(), keys.clone(), vec![sender], reply));
                        }
                        vec![]
                    }
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::PhoneChanged { person, key, name, removed }, .. },
                        ..
                    })) => {
                        let at = created_at.as_secs();
                        if peers.contains(&sender) {
                            if cfg.persons.iter().any(|p| p.name == person) {
                                let entry = PhoneEntry { name: name.trim().chars().take(crate::phones::MAX_NAME_CHARS).collect(), at, removed };
                                if phones.apply(&person, &key, entry) {
                                    device_owner = apply_phones(&mut cfg, &base, &phones);
                                    tracing::info!("{person}: phone {} {} (via another watcher)", &key[..key.len().min(12)], if removed { "removed" } else { "added or renamed" });
                                }
                            }
                        } else if let Some(&i) = device_owner.get(&sender) {
                            let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                            let edit = PhoneEdit { person: cfg.persons[i].name.clone(), key, name, removed };
                            if let Err(e) = change_phone(&mut cfg, &base, &mut phones, &mut device_owner, &link, edit, at).await {
                                tracing::warn!("phone change from the app refused: {e}");
                            }
                        } else {
                            tracing::warn!("phone change from an unknown key: {}", sender.to_bech32()?);
                        }
                        vec![]
                    }
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::TopicChanged { person, url, device, cancel_seconds }, .. },
                        ..
                    })) => {
                        let at = created_at.as_secs();
                        let from_peer = peers.contains(&sender);
                        let person_idx = if from_peer {
                            cfg.persons.iter().position(|p| p.name == person && !p.devices.is_empty())
                        } else if let Some(&i) = device_owner.get(&sender) {
                            Some(i)
                        } else {
                            tracing::warn!("topic change from an unknown key: {}", sender.to_bech32()?);
                            None
                        };
                        if let Some(i) = person_idx {
                            match validate_app_topic_url(&url) {
                                Err(reason) => {
                                    tracing::warn!("topic change for {} refused: {reason}", cfg.persons[i].name);
                                }
                                Ok(clean_url) => {
                                    let person_name = cfg.persons[i].name.clone();
                                    let entry = TopicEntry {
                                        url: clean_url,
                                        device: clean_name(&device),
                                        cancel_seconds: cancel_seconds.min(MAX_CANCEL_SECONDS),
                                        at,
                                    };
                                    if topics.apply(&person_name, entry.clone()) {
                                        spawn_dynamic_ntfy(&cfg, i, &entry, &mut dynamic_ntfy, &ntfy_tx);
                                        tracing::info!(
                                            "{person_name}: shortcut topic {} ({}){}",
                                            if entry.url.is_empty() { "cleared" } else { "updated" },
                                            entry.device,
                                            if from_peer { " (via another watcher)" } else { "" }
                                        );
                                        if !from_peer && !peers.is_empty() && now().abs_diff(at) <= ANSWER_MAX_AGE {
                                            let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                                            link.report(warn_proto::Message::TopicChanged {
                                                person: person_name,
                                                url: entry.url,
                                                device: entry.device,
                                                cancel_seconds: entry.cancel_seconds,
                                            })
                                            .await;
                                        }
                                    }
                                }
                            }
                        }
                        vec![]
                    }
                    Ok(Some(warn_proto::Received {
                        sender,
                        created_at,
                        envelope: warn_proto::Envelope { msg: warn_proto::Message::Enroll { secret, name, device }, .. },
                        ..
                    })) => {
                        let t = now();
                        let hash = secret_hash(&secret);
                        let is_open = secret.starts_with(OPEN_SIGNUP_PREFIX);
                        let taken: Vec<String> = cfg.persons.iter().map(|p| p.name.clone()).collect();
                        // A device this watcher already knows (a VIP adding this server again,
                        // or an answer that got lost): under its own name only.
                        let known = device_owner.get(&sender).copied();
                        let result = if let Some(i) = known {
                            if is_open || cfg.persons[i].name.eq_ignore_ascii_case(name.trim()) {
                                Ok(Enrolled { person: cfg.persons[i].clone(), by: String::new(), angel_key: None, push: None })
                            } else {
                                Err(Refusal(format!("this phone is already watched here, as {}", cfg.persons[i].name)))
                            }
                        } else if is_open {
                            if cfg.open_signup {
                                vips.enroll_open(&name, &sender, t, &taken, cfg.max_invited_vips)
                            } else {
                                Err(Refusal("this server requires an invite link".into()))
                            }
                        } else if cfg.accept_invites {
                            vips.enroll(&secret, &name, &sender, t, &taken, cfg.max_invited_vips)
                        } else {
                            Err(Refusal("this server does not take new people".into()))
                        };
                        let reply = match result {
                            Ok(Enrolled { person: p, by, angel_key: enrolled_angel_key, push: enrolled_push }) => {
                                let i = match cfg.persons.iter().position(|q| q.name == p.name) {
                                    Some(i) => i,
                                    None => {
                                        base.push(p.devices.clone());
                                        cfg.persons.push(p);
                                        let i = cfg.persons.len() - 1;
                                        device_owner = apply_phones(&mut cfg, &base, &phones);
                                        let name = cfg.persons[i].name.clone();
                                        if by.is_empty() {
                                            tracing::info!("now also watching {name} ({device}) (open signup)");
                                            let text = format!("This watcher now also watches {name} (open signup).");
                                            notifier.send(&cfg.owner_chats, &text, &[]).await;
                                        } else {
                                            tracing::info!("now also watching {name} ({device}), invited by {by}");
                                            let text = render(&cfg.messages.vip_joined, &vars(&name, None, &by));
                                            notifier.send(&cfg.owner_chats, &text, &[]).await;

                                            let at = created_at.as_secs();
                                            let due = if peers.is_empty() { 0 } else { (t + cfg.send_delay_seconds).max(started + STARTUP_HOLD) };
                                            let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                                            auto_add_inviter_angels(
                                                &cfg,
                                                own_count,
                                                &notifier,
                                                &mut contacts,
                                                &link,
                                                &mut held_notes,
                                                due,
                                                &name,
                                                &by,
                                                &hash,
                                                at,
                                                enrolled_angel_key,
                                                enrolled_push,
                                            )
                                            .await;

                                            // Derive a shared angel secret for `name`, and notify the inviter's devices.
                                            let angel_secret = derived_angel_secret(&secret);
                                            vips.add_angel_invite(&secret_hash(&angel_secret), &name, t + MAX_ANGEL_INVITE_SECONDS, t);
                                            if let Some(inviter) = cfg.persons[..own_count].iter().find(|q| q.name == by) {
                                                let to: Vec<PublicKey> = inviter
                                                    .device_keys()
                                                    .unwrap_or_default()
                                                    .into_iter()
                                                    .filter(|k| *k != keys.public_key())
                                                    .collect();
                                                let msg = warn_proto::Message::VipJoined {
                                                    person: name,
                                                    invite: known_link(&cfg, &bot_name, &cfg.persons[i]),
                                                    angel_secret: Some(angel_secret),
                                                };
                                                if !to.is_empty() {
                                                    tokio::spawn(send_to(client.clone(), keys.clone(), to, msg));
                                                }
                                            }
                                        }
                                        i
                                    }
                                };
                                warn_proto::Message::Watching {
                                    person: cfg.persons[i].name.clone(),
                                    interval_secs: WATCHING_INTERVAL.as_secs(),
                                    reply_to: Some(hash),
                                    invite: known_link(&cfg, &bot_name, &cfg.persons[i]),
                                    angels: angels_of(&contacts, &cfg.persons[i].name),
                                    phones: phones.list(&cfg.persons[i].name, &base[i]),
                                    watcher: cfg.watcher_name(),
                                }
                            }
                            Err(Refusal(reason)) => {
                                tracing::info!("enrolment refused: {reason}");
                                warn_proto::Message::Refused { reply_to: hash, reason }
                            }
                        };
                        if t.abs_diff(created_at.as_secs()) <= ANSWER_MAX_AGE {
                            tokio::spawn(send_to(client.clone(), keys.clone(), vec![sender], reply));
                        }
                        vec![]
                    }
                    Ok(Some(msg)) => match device_owner.get(&msg.sender) {
                        Some(&person) => {
                            if let warn_proto::Message::SessionStart { timeout_secs, context, .. } = &msg.envelope.msg {
                                tracing::info!("{} started on {} ({timeout_secs} s)", context.trigger, context.device);
                            }
                            engine.handle(now(), person, &msg)
                        }
                        None => {
                            tracing::warn!("message from unknown device {}", msg.sender.to_bech32()?);
                            vec![]
                        }
                    },
                    Ok(None) => vec![],
                    Err(e) => {
                        tracing::warn!("rejected message: {e:#}");
                        vec![]
                    }
                }
            }
            Some(((person, device, source, cancel), m)) = ntfy_rx.recv() => {
                // Keys of this source's sessions share a prefix, so an `ok` finds them.
                let prefix = format!("ntfy:{source}:");
                let context = |trigger: &str, location: Option<String>| warn_proto::Context {
                    device: device.clone(),
                    trigger: trigger.into(),
                    location,
                    note: None,
                };
                match ntfy::parse_command(&m.text) {
                    ntfy::Command::Start { seconds, scenario, location } => {
                        tracing::info!("{scenario} started on {device} ({seconds} s)");
                        engine.start(now(), person, format!("{prefix}{}", m.id), m.time, seconds, context(&scenario, location))
                    }
                    ntfy::Command::Ok => engine.ok_source(now(), person, &prefix, m.time),
                    ntfy::Command::Duress { location } => {
                        let key = format!("{prefix}{}", m.id);
                        let id = format!("ntfy:{}", m.id);
                        let alarms = engine.duress_after(now(), person, key.clone(), &id, m.time, cancel, context("duress_shortcut", location));
                        if engine.duress_waiting(&key) {
                            tracing::info!("duress on {device}: alarm in {cancel} s unless cancelled by a check-in");
                        }
                        alarms
                    }
                }
            }
            _ = ticker.tick() => engine.tick(now()),
            _ = watching.tick() => {
                let persons = cfg
                    .persons
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (p.clone(), known_link(&cfg, &bot_name, p), angels_of(&contacts, &p.name), phones.list(&p.name, &base[i])))
                    .collect();
                tokio::spawn(confirm_watching(client.clone(), keys.clone(), cfg.watcher_name(), persons));
                vec![]
            }
            Some(edit) = edit_rx.recv() => {
                let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                if cfg.persons.iter().any(|p| p.name == edit.person) {
                    tracing::info!("angels of {}: {} changed in the Angels window", edit.person, edit.id);
                    set_angel(&mut contacts, &link, &edit.person, &edit.id, edit.entry).await;
                }
                vec![]
            }
            Some(update) = button_rx.recv() => {
                let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                let mut inv = Inviting { vips: &mut vips, own_count };
                handle_update(&cfg, &notifier, &alarm_owner, &mut contacts, &link, &mut inv, update).await;
                vec![]
            }
            Some(req) = local_rx.recv() => {
                let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                match req {
                    Local::Phone(edit) => {
                        if let Err(e) = change_phone(&mut cfg, &base, &mut phones, &mut device_owner, &link, edit, now()).await {
                            tracing::warn!("phone change refused: {e}");
                        }
                    }
                    Local::Pair { person, reply } => reply(pair_link(&cfg, &mut vips, &link, &person).await),
                    Local::Offer { person, reply } => reply(offer_link(&cfg, &mut vips, &link, own_count, &person).await),
                }
                vec![]
            }
            Some((person, key)) = gone_rx.recv() => {
                contacts.drop_push(&person, &key);
                vec![]
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                save_state(&state_path, &engine)?;
                return Ok(());
            }
        };

        let fresh = !alarms.is_empty();
        if fresh {
            // Save first: a crash while sending must not cause a re-alarm loop.
            save_state(&state_path, &engine)?;
            // With peers, wait our turn: a peer that sends first tells us, and we skip.
            let due = if peers.is_empty() { 0 } else { (now() + cfg.send_delay_seconds).max(started + STARTUP_HOLD) };
            for a in alarms {
                alarm_owner.insert(a.id.clone(), a.person);
                if due > now() {
                    tracing::info!("alarm {} held {} s for the other watchers", a.id, due - now());
                }
                held.push((due, a));
            }
        }
        if held.iter().any(|(due, _)| *due <= now()) {
            let (ready, wait): (Vec<_>, Vec<_>) = held.drain(..).partition(|(due, _)| *due <= now());
            held = wait;
            for (_, a) in ready {
                if sent_by_peer.contains_key(&a.id) {
                    tracing::info!("alarm {} already sent by another watcher: not sending", a.id);
                    continue;
                }
                if a.person >= own_count {
                    let day = now() / 86400;
                    let n = alarms_today.entry(a.person).or_insert((day, 0));
                    if n.0 != day {
                        *n = (day, 0);
                    }
                    n.1 += 1;
                    if n.1 > MAX_INVITED_ALARMS_PER_DAY {
                        let name = &cfg.persons[a.person].name;
                        tracing::warn!("alarm {} for {name} not sent: over today's limit", a.id);
                        if n.1 == MAX_INVITED_ALARMS_PER_DAY + 1 {
                            let text = render(&cfg.messages.alarm_limit, &vars(name, None, ""));
                            notifier.tell_all(&contacts, &cfg.persons[a.person], &format!("limit-{day}"), &text).await;
                            notifier.send(&cfg.owner_chats, &text, &[]).await;
                        }
                        continue;
                    }
                }
                if send_alarm(&cfg, &notifier, &contacts, &a).await && !peers.is_empty() {
                    // Tell the others, so they skip it. Best effort: if the report comes too
                    // late, contacts get the alarm twice, which beats not at all.
                    let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                    link.report(warn_proto::Message::AlarmSent { alarm: a.id.clone() }).await;
                }
            }
        }
        if held_notes.iter().any(|(due, _)| *due <= now()) {
            let (ready, wait): (Vec<_>, Vec<_>) = held_notes.drain(..).partition(|(due, _)| *due <= now());
            held_notes = wait;
            for (_, n) in ready {
                if sent_by_peer.contains_key(n.id()) {
                    tracing::info!("notice {} already sent by another watcher: not sending", n.id());
                    continue;
                }
                let sent = match &n {
                    Note::Chats { chats, text, .. } => notifier.send(chats, text, &[]).await,
                    Note::Keys { id, person, to, kind, text } => match &notifier.keyed {
                        Some(k) => k.tell(to.clone(), person, id, kind, text).await,
                        None => false,
                    },
                };
                if sent && !peers.is_empty() {
                    let link = PeerLink { client: &client, keys: &keys, peers: &peers };
                    link.report(warn_proto::Message::AlarmSent { alarm: n.id().to_string() }).await;
                }
            }
        }
        if !fresh && now() - last_prune > 3600 {
            last_prune = now();
            sent_by_peer.retain(|_, t| last_prune.saturating_sub(*t) < PEER_REPORT_RETENTION);
            if engine.prune(last_prune) {
                save_state(&state_path, &engine)?;
            }
        }
    }
}

fn save_state(path: &Path, engine: &Engine) -> Result<()> {
    write_private(path, &serde_json::to_string(engine.persisted())?)
}

/// Returns whether at least one contact got it.
async fn send_alarm(cfg: &Config, notifier: &Notifier, contacts: &Contacts, a: &Alarm) -> bool {
    let person = &cfg.persons[a.person];
    let template = match a.kind {
        AlarmKind::MissedCheckIn => &cfg.messages.missed,
        AlarmKind::Duress => &cfg.messages.duress,
        AlarmKind::Test => &cfg.messages.test,
        AlarmKind::LateOk => &cfg.messages.late_ok,
    };
    tracing::warn!("alarm {:?} for {} ({})", a.kind, person.name, a.id);
    let text = with_phone(cfg, person, render(template, &vars(&person.name, Some(&a.context), "")));
    let buttons = match a.kind {
        AlarmKind::LateOk | AlarmKind::Test => vec![],
        _ => vec![
            (cfg.messages.button_call.clone(), format!("call:{}", a.id)),
            (cfg.messages.button_resolved.clone(), format!("done:{}", a.id)),
        ],
    };
    let mut any = notifier.send(&contacts.chats_for(person, a.kind), &text, &buttons).await;
    if let Some(k) = &notifier.keyed {
        let kind = match a.kind {
            AlarmKind::MissedCheckIn => "missed",
            AlarmKind::Duress => "duress",
            AlarmKind::Test => "test",
            AlarmKind::LateOk => "late_ok",
        };
        any |= k.tell(contacts.keys_for(&person.name, a.kind), &person.name, &a.id, kind, &text).await;
    }
    any
}

/// Add the person's phone number under `text`. Telegram makes it tappable: contacts can call
/// it by phone or through Telegram in one tap.
fn with_phone(cfg: &Config, person: &PersonConfig, text: String) -> String {
    match person.phone.as_deref().and_then(normalize_phone) {
        Some(phone) => {
            let line = cfg.messages.phone_line.replace("{name}", &person.name).replace("{phone}", &phone);
            format!("{text}\n\n{line}")
        }
        None => text,
    }
}

/// Answers to the app (invites, enrolment) only for messages this recent.
const ANSWER_MAX_AGE: u64 = 600;
/// Alarms an invited VIP may cause per day (against misuse of the owner's bot).
const MAX_INVITED_ALARMS_PER_DAY: u32 = 30;

/// Tell every device of every VIP that this watcher watches it. Best effort, in the background.
#[allow(clippy::type_complexity)]
async fn confirm_watching(
    client: Client,
    keys: Keys,
    watcher_name: Option<String>,
    persons: Vec<(PersonConfig, Option<String>, Vec<warn_proto::Angel>, Vec<warn_proto::Phone>)>,
) {
    let mut sent = 0;
    for (p, invite, angels, phones) in persons {
        // Not to ourselves: a laptop's built-in watcher shares the laptop's key.
        let devices: Vec<PublicKey> = match p.device_keys() {
            Ok(d) => d.into_iter().filter(|k| *k != keys.public_key()).collect(),
            Err(_) => continue,
        };
        if devices.is_empty() {
            continue;
        }
        let msg = warn_proto::Message::Watching {
            person: p.name.clone(),
            interval_secs: WATCHING_INTERVAL.as_secs(),
            reply_to: None,
            invite,
            angels,
            phones,
            watcher: watcher_name.clone(),
        };
        match warn_proto::wrap(&keys, &devices, &warn_proto::Envelope::new(msg)) {
            Ok(wraps) => {
                for w in wraps {
                    if client.send_event(&w).await.is_ok() {
                        sent += 1;
                    }
                }
            }
            Err(e) => tracing::error!("building watching message failed: {e:#}"),
        }
    }
    tracing::info!("told {sent} device(s) they are watched");
}

/// Send one message to some keys, in the background.
async fn send_to(client: Client, keys: Keys, to: Vec<PublicKey>, msg: warn_proto::Message) {
    match warn_proto::wrap(&keys, &to, &warn_proto::Envelope::new(msg)) {
        Ok(wraps) => {
            for w in wraps {
                if let Err(e) = client.send_event(&w).await {
                    tracing::warn!("answer not delivered: {e}");
                }
            }
        }
        Err(e) => tracing::error!("building answer failed: {e:#}"),
    }
}

/// What is needed to tell the other watchers about a change.
struct PeerLink<'a> {
    client: &'a Client,
    keys: &'a Keys,
    peers: &'a [PublicKey],
}

impl PeerLink<'_> {
    async fn report(&self, msg: warn_proto::Message) {
        if self.peers.is_empty() {
            return;
        }
        match warn_proto::wrap(self.keys, self.peers, &warn_proto::Envelope::new(msg)) {
            Ok(wraps) => {
                for w in wraps {
                    if let Err(e) = self.client.send_event(&w).await {
                        tracing::warn!("report to other watchers not delivered: {e}");
                    }
                }
            }
            Err(e) => tracing::error!("building report failed: {e:#}"),
        }
    }
}

/// Everything that reaches the bot: buttons under alarms and join requests, and commands.
async fn handle_update(
    cfg: &Config,
    notifier: &Notifier,
    alarm_owner: &HashMap<String, usize>,
    contacts: &mut Contacts,
    link: &PeerLink<'_>,
    inv: &mut Inviting<'_>,
    update: Update,
) {
    let (Some(tg), Some(token)) = (&notifier.telegram, cfg.telegram.as_ref().map(|t| t.bot_token.as_str())) else {
        return;
    };
    let msg = |template: &str, person: &str, who: &str| render(template, &vars(person, None, who));
    match update {
        Update::Button(press) => {
            if let Err(e) = tg.answer_button(&press.callback_id).await {
                tracing::warn!("answering button failed: {e}");
            }
            // Approve / Decline under old join requests: joining needs no approval any more.
            if !press.data.starts_with("join:") && !press.data.starts_with("nojoin:") {
                handle_button(cfg, notifier, contacts, alarm_owner, press).await;
            }
        }
        Update::ChatIdRequest { chat_id } => {
            let _ = tg.send(chat_id, &format!("Chat id: {chat_id}"), &[]).await;
        }
        Update::Start { chat_id, who, code } => {
            let found = code.as_deref().and_then(|code| {
                // Removed invited VIPs have no devices: their links no longer work.
                cfg.persons
                    .iter()
                    .enumerate()
                    .find(|(_, p)| !p.devices.is_empty() && invite_code(token, &p.name, p.invite_round) == code)
            });
            let Some((i, person)) = found else {
                let _ = tg.send(chat_id, &cfg.messages.join_invalid, &[]).await;
                return;
            };
            if contacts.chats(person).contains(&chat_id) {
                let _ = tg.send(chat_id, &msg(&cfg.messages.join_welcome, &person.name, &who), &[]).await;
                return;
            }
            // Whoever has the link joins at once; the VIP sees them in the app and can switch
            // them off or remove them.
            let own = i < inv.own_count;
            add_contact(cfg, notifier, tg, contacts, link, person, chat_id, &who).await;
            // The new angel can protect themselves too, with these watchers (one hop: only for
            // the angels of own VIPs).
            if own {
                if let Some(url) = welcome_invite(cfg, inv.vips, link, &person.name).await {
                    let text = cfg.messages.join_self.replace("{name}", &person.name).replace("{link}", &url);
                    let _ = tg.send(chat_id, &text, &[]).await;
                }
            }
        }
        Update::Pair { chat_id, name } => {
            if !cfg.owner_chats.contains(&chat_id) {
                let _ = tg.send(chat_id, "Only the owner of this server can add phones.", &[]).await;
                return;
            }
            let watched: Vec<&PersonConfig> = cfg.persons.iter().filter(|p| !p.devices.is_empty()).collect();
            let own: Vec<&PersonConfig> = cfg.persons[..inv.own_count].iter().filter(|p| !p.devices.is_empty()).collect();
            let person = match &name {
                Some(n) => watched.iter().find(|p| p.name.eq_ignore_ascii_case(n.trim())).copied(),
                None if own.len() == 1 => Some(own[0]),
                None => None,
            };
            let Some(person) = person else {
                let names: Vec<&str> = watched.iter().map(|p| p.name.as_str()).collect();
                let _ = tg.send(chat_id, &format!("For whom? Send /pair and a name: {}", names.join(", ")), &[]).await;
                return;
            };
            let text = match pair_link(cfg, inv.vips, link, &person.name).await {
                Ok(url) => format!(
                    "To add a phone for {}: open this link on that phone within {} minutes. It works once.\n\n{url}",
                    person.name,
                    MAX_DEVICE_INVITE_SECONDS / 60
                ),
                Err(e) => format!("No link: {e}"),
            };
            let _ = tg.send(chat_id, &text, &[]).await;
        }
        Update::Offer { chat_id, name } => {
            if !cfg.owner_chats.contains(&chat_id) {
                let _ = tg.send(chat_id, "Only the owner of this server can offer it.", &[]).await;
                return;
            }
            // On behalf of an own VIP (one hop: invited VIPs cannot pass the server on).
            let own: Vec<&PersonConfig> = cfg.persons[..inv.own_count].iter().filter(|p| !p.devices.is_empty()).collect();
            let person = match &name {
                Some(n) => own.iter().find(|p| p.name.eq_ignore_ascii_case(n.trim())).copied(),
                None if own.len() == 1 => Some(own[0]),
                None => None,
            };
            let Some(person) = person else {
                let names: Vec<&str> = own.iter().map(|p| p.name.as_str()).collect();
                let _ = tg.send(chat_id, &format!("On behalf of whom? Send /offer and a name: {}", names.join(", ")), &[]).await;
                return;
            };
            let text = match offer_link(cfg, inv.vips, link, inv.own_count, &person.name).await {
                Ok(url) => format!(
                    "A link for someone who wants this server to watch over them. It works once, within {} days. They open it in Sync on their phone.\n\n{url}",
                    MAX_OFFER_SECONDS / 86400
                ),
                Err(e) => format!("No link: {e}"),
            };
            let _ = tg.send(chat_id, &text, &[]).await;
        }
        Update::Stop { chat_id, who } => {
            let mut any = false;
            for person in &cfg.persons {
                let Some(known) = contacts.get(&person.name, &AngelId::Chat(chat_id)).filter(|e| e.active && !e.removed).cloned() else { continue };
                any = true;
                // Keep the name the VIP gave this angel.
                let name = known.name.clone();
                set_angel(contacts, link, &person.name, &AngelId::Chat(chat_id), Entry { active: false, at: now(), ..known }).await;
                tracing::info!("contacts of {}: chat {chat_id} left", person.name);
                let _ = tg.send(chat_id, &msg(&cfg.messages.leave_done, &person.name, &who), &[]).await;
                let notice = msg(&cfg.messages.leave_notice, &person.name, &name);
                notifier.tell_all(contacts, person, &format!("left-{chat_id}"), &notice).await;
            }
            if !any {
                let _ = tg.send(chat_id, &cfg.messages.join_invalid, &[]).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn add_contact(
    cfg: &Config,
    notifier: &Notifier,
    tg: &Telegram,
    contacts: &mut Contacts,
    link: &PeerLink<'_>,
    person: &PersonConfig,
    chat: i64,
    name: &str,
) {
    let before = contacts.chats(person);
    // Someone who left and comes back keeps the name the VIP gave them, and their choices.
    let entry = match contacts.get(&person.name, &AngelId::Chat(chat)) {
        Some(old) => Entry { active: true, removed: false, at: now(), ..old.clone() },
        None => Entry::new(name, now()),
    };
    let name = &entry.name.clone();
    set_angel(contacts, link, &person.name, &AngelId::Chat(chat), entry).await;
    tracing::info!("contacts of {}: chat {chat} joined", person.name);
    let _ = tg.send(chat, &render(&cfg.messages.join_welcome, &vars(&person.name, None, name)), &[]).await;
    let text = render(&cfg.messages.join_approved, &vars(&person.name, None, name));
    notifier.send(&before, &text, &[]).await;
}

/// What handling a join needs to make an invite for the new angel.
struct Inviting<'a> {
    vips: &'a mut Vips,
    own_count: usize,
}

/// Make an invite on behalf of an own VIP, tell the peers, and return the app link. None when
/// this watcher takes no new VIPs or has no `app_url`.
async fn welcome_invite(cfg: &Config, vips: &mut Vips, link: &PeerLink<'_>, person: &str) -> Option<String> {
    let app = cfg.app_url.as_deref()?;
    if !cfg.accept_invites || vips.active() >= cfg.max_invited_vips {
        return None;
    }
    let secret = new_secret();
    let hash = secret_hash(&secret);
    let expires = now() + MAX_INVITE_SECONDS;
    vips.add_invite(&hash, person, expires, now());
    link.report(warn_proto::Message::InviteCreated { hash, expires, person: person.to_string(), angel: false, device: false, angel_key: None, push: None }).await;
    // The link holds only the secret; the details go on the relays, encrypted to it.
    let mut watchers = vec![link.keys.public_key().to_hex()];
    watchers.extend(link.peers.iter().map(|k| k.to_hex()));
    let info = InviteInfo { by: person.to_string(), w: watchers, r: cfg.relays.clone(), t: String::new() };
    let event = match invite_event(&secret, &info, expires) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("building the invite event failed: {e:#}");
            return None;
        }
    };
    if let Err(e) = link.client.send_event(&event).await {
        tracing::warn!("invite event not stored on the relays: {e}");
        return None;
    }
    // Everything after `#` stays on the phone.
    Some(format!("{app}#j={secret}"))
}

/// A link to add a phone to `person` (`<app>#d=<secret>`): kept here, told to the other
/// watchers, and its details put on the relays.
async fn pair_link(cfg: &Config, vips: &mut Vips, link: &PeerLink<'_>, person: &str) -> Result<String, String> {
    let app = cfg.app_url.as_deref().ok_or("this server has no app_url in its config")?;
    if !cfg.persons.iter().any(|p| p.name == person && !p.devices.is_empty()) {
        return Err(format!("{person} is not watched here"));
    }
    let secret = new_secret();
    let hash = secret_hash(&secret);
    let expires = now() + MAX_DEVICE_INVITE_SECONDS;
    vips.add_device_invite(&hash, person, expires, now());
    link.report(warn_proto::Message::InviteCreated { hash, expires, person: person.to_string(), angel: false, device: true, angel_key: None, push: None }).await;
    let mut watchers = vec![link.keys.public_key().to_hex()];
    watchers.extend(link.peers.iter().map(|k| k.to_hex()));
    let info = InviteInfo { by: person.to_string(), w: watchers, r: cfg.relays.clone(), t: "device".into() };
    let event = invite_event(&secret, &info, expires).map_err(|e| format!("building the link failed: {e}"))?;
    link.client.send_event(&event).await.map_err(|e| format!("the relays did not take the link: {e}"))?;
    tracing::info!("link to add a phone for {person}");
    Ok(format!("{app}#d={secret}"))
}

/// A link that offers this server (and its peers) to someone (`<app>#w=<secret>`), on behalf of
/// the own VIP `person`: whoever opens it is watched from then on, as an invited VIP. Someone
/// who already uses Sync with other servers adds these to theirs.
async fn offer_link(cfg: &Config, vips: &mut Vips, link: &PeerLink<'_>, own_count: usize, person: &str) -> Result<String, String> {
    let app = cfg.app_url.as_deref().ok_or("this server has no app_url in its config")?;
    if !cfg.persons[..own_count].iter().any(|p| p.name == person) {
        return Err(format!("{person} is not one of this server's own VIPs"));
    }
    if !cfg.accept_invites {
        return Err("this server does not take new people (accept_invites = false)".into());
    }
    if vips.active() >= cfg.max_invited_vips {
        return Err("this server watches as many people as it can (max_invited_vips)".into());
    }
    let secret = new_secret();
    let hash = secret_hash(&secret);
    let expires = now() + MAX_OFFER_SECONDS;
    vips.add_invite(&hash, person, expires, now());
    link.report(warn_proto::Message::InviteCreated { hash, expires, person: person.to_string(), angel: false, device: false, angel_key: None, push: None }).await;
    let mut watchers = vec![link.keys.public_key().to_hex()];
    watchers.extend(link.peers.iter().map(|k| k.to_hex()));
    let info = InviteInfo { by: person.to_string(), w: watchers, r: cfg.relays.clone(), t: "offer".into() };
    let event = invite_event(&secret, &info, expires).map_err(|e| format!("building the link failed: {e}"))?;
    link.client.send_event(&event).await.map_err(|e| format!("the relays did not take the link: {e}"))?;
    tracing::info!("link offering this server, for {person}");
    Ok(format!("{app}#w={secret}"))
}

/// Every VIP's devices from `base` and the phones file; returns device key -> person index.
fn apply_phones(cfg: &mut Config, base: &[Vec<String>], phones: &Phones) -> HashMap<PublicKey, usize> {
    let mut owner = HashMap::new();
    for (i, p) in cfg.persons.iter_mut().enumerate() {
        p.devices = phones.devices(&p.name, &base[i]);
        for k in p.devices.iter().filter_map(|d| PublicKey::from_hex(d).ok()) {
            owner.insert(k, i);
        }
    }
    owner
}

/// Rename or remove one of a VIP's own devices (from the VIP's app or the agent's window), and
/// tell the other watchers. The last device cannot be removed.
async fn change_phone(
    cfg: &mut Config,
    base: &[Vec<String>],
    phones: &mut Phones,
    device_owner: &mut HashMap<PublicKey, usize>,
    link: &PeerLink<'_>,
    edit: PhoneEdit,
    at: u64,
) -> Result<(), String> {
    let i = cfg.persons.iter().position(|p| p.name == edit.person).ok_or("unknown person")?;
    if !cfg.persons[i].devices.contains(&edit.key) {
        return Err(format!("not a device of {}", edit.person));
    }
    if edit.removed && cfg.persons[i].devices.len() <= 1 {
        return Err("the last device cannot be removed".into());
    }
    let old = phones.get(&edit.person, &edit.key).map(|e| e.name.clone()).unwrap_or_default();
    let name = if edit.name.trim().is_empty() { old } else { clean_name(&edit.name) };
    let entry = PhoneEntry { name: name.clone(), at, removed: edit.removed };
    if phones.apply(&edit.person, &edit.key, entry) {
        *device_owner = apply_phones(cfg, base, phones);
        tracing::info!("{}: phone {} {}", edit.person, &edit.key[..12], if edit.removed { "removed" } else { "renamed" });
        link.report(warn_proto::Message::PhoneChanged { person: edit.person, key: edit.key, name, removed: edit.removed }).await;
    }
    Ok(())
}

/// A VIP's angels as the app shows them.
fn angels_of(contacts: &Contacts, person: &str) -> Vec<warn_proto::Angel> {
    contacts
        .list(person)
        .into_iter()
        .map(|(id, e)| {
            let (chat, key) = id.parts();
            warn_proto::Angel { chat, key, name: e.name, active: e.active, missed: e.missed, late_ok: e.late_ok }
        })
        .collect()
}

/// Change one angel here and tell the other watchers.
async fn set_angel(contacts: &mut Contacts, link: &PeerLink<'_>, person: &str, id: &AngelId, entry: Entry) {
    let (chat, key) = id.parts();
    let msg = warn_proto::Message::ContactChanged {
        person: person.to_string(),
        chat,
        name: entry.name.clone(),
        active: entry.active,
        missed: entry.missed,
        late_ok: entry.late_ok,
        removed: entry.removed,
        key,
    };
    if contacts.apply(person, id, entry) {
        link.report(msg).await;
    }
}

/// Telegram chat ids belonging to an own VIP who invited someone.
fn collect_inviter_chats(cfg: &Config, own_count: usize, contacts: &Contacts, by: &str) -> Vec<i64> {
    let inviter = cfg.persons[..own_count].iter().find(|q| q.name == by);
    let mut chats = Vec::new();
    let mut push = |c: i64| {
        if !chats.contains(&c) {
            chats.push(c);
        }
    };
    if let Some(inv) = inviter {
        for &c in &inv.telegram_chats {
            push(c);
        }
    }
    for &c in &cfg.owner_chats {
        push(c);
    }
    for c in contacts.chats_for_name(by) {
        push(c);
    }
    if chats.is_empty() {
        if let Some(inv) = inviter {
            let existing = contacts.chats(inv);
            if existing.len() == 1 {
                chats.push(existing[0]);
            }
        }
    }
    chats
}

/// Immediately add the inviter as both a webapp angel and a Telegram angel for the new VIP,
/// and queue held welcome notices for both channels.
#[allow(clippy::too_many_arguments)]
async fn auto_add_inviter_angels(
    cfg: &Config,
    own_count: usize,
    notifier: &Notifier,
    contacts: &mut Contacts,
    link: &PeerLink<'_>,
    held_notes: &mut Vec<(u64, Note)>,
    due: u64,
    person_name: &str,
    by: &str,
    hash: &str,
    at: u64,
    enrolled_angel_key: Option<String>,
    enrolled_push: Option<PushSub>,
) {
    if by.is_empty() {
        return;
    }
    let app_angel = match enrolled_angel_key {
        Some(k) => {
            let push = enrolled_push.or_else(|| contacts.angel_push(&k));
            Some((k, push))
        }
        None => contacts.key_for_name(by),
    };
    if let Some((k, push)) = app_angel {
        let id = AngelId::Key(k.clone());
        let entry = Entry { push: push.clone(), ..Entry::new(by, at) };
        set_angel(contacts, link, person_name, &id, entry).await;
        if notifier.keyed.is_some() {
            let text = render(&cfg.messages.join_welcome, &vars(person_name, None, by));
            held_notes.push((
                due,
                Note::Keys {
                    id: format!("welcome-key-{}", &hash[..16]),
                    person: person_name.to_string(),
                    to: vec![(k, push)],
                    kind: "notice",
                    text,
                },
            ));
        }
    }

    let inviter_chats = collect_inviter_chats(cfg, own_count, contacts, by);
    for &chat_id in &inviter_chats {
        let default_label = format!("Chat {chat_id}");
        let chat_name = if inviter_chats.len() > 1 {
            contacts
                .get(by, &AngelId::Chat(chat_id))
                .map(|e| e.name.as_str())
                .filter(|n| !n.is_empty() && *n != default_label)
                .unwrap_or(by)
        } else {
            by
        };
        set_angel(contacts, link, person_name, &AngelId::Chat(chat_id), Entry::new(chat_name, at)).await;
    }
    if !inviter_chats.is_empty() {
        let text = render(&cfg.messages.join_welcome, &vars(person_name, None, by));
        held_notes.push((
            due,
            Note::Chats {
                id: format!("welcome-tg-{}", &hash[..16]),
                chats: inviter_chats,
                text,
            },
        ));
    }
}

async fn handle_button(
    cfg: &Config,
    notifier: &Notifier,
    contacts: &Contacts,
    alarm_owner: &HashMap<String, usize>,
    press: ButtonPress,
) {
    let Some((action, id)) = press.data.split_once(':') else { return };
    let Some(&person_idx) = alarm_owner.get(id) else {
        tracing::warn!("button for unknown alarm {id} (watcher restarted?)");
        return;
    };
    let person = &cfg.persons[person_idx];
    let template = match action {
        "call" => &cfg.messages.ack_call,
        "done" => &cfg.messages.ack_resolved,
        _ => return,
    };
    let text = render(template, &vars(&person.name, None, &press.who));
    // Tell everyone, so contacts do not all call at once.
    notifier.tell_all(contacts, person, &format!("{id}-{action}"), &text).await;
}

/// Telegram lets only one client fetch a bot's updates. Several watchers can share one bot:
/// they all send, and whichever polls first handles buttons and `/id`. The others notice the
/// conflict and step back.
async fn poll_telegram(tg: Telegram, tx: mpsc::UnboundedSender<Update>) {
    let mut offset = 0;
    let mut stepped_back = false;
    loop {
        match tg.poll(offset).await {
            Err(e) if e.to_string().contains("Conflict") => {
                if !stepped_back {
                    tracing::info!("another watcher handles this bot's buttons; this one only sends");
                    stepped_back = true;
                }
                tokio::time::sleep(POLL_CONFLICT_BACKOFF).await;
            }
            Ok((updates, next)) => {
                if stepped_back {
                    tracing::info!("now handling this bot's buttons");
                    stepped_back = false;
                }
                offset = next;
                for u in updates {
                    let _ = tx.send(u);
                }
            }
            Err(e) => {
                tracing::warn!("telegram poll failed: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Abort any prior app-registered `ntfy` stream for this person and spawn a new one unless
/// cleared or already followed via static `config.toml` (`duress_ntfy`).
fn spawn_dynamic_ntfy(
    cfg: &Config,
    person_idx: usize,
    entry: &TopicEntry,
    dynamic_ntfy: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    ntfy_tx: &mpsc::UnboundedSender<((usize, String, String, u64), NtfyMessage)>,
) {
    let person = &cfg.persons[person_idx];
    if let Some(handle) = dynamic_ntfy.remove(&person.name) {
        handle.abort();
    }
    if entry.url.is_empty() || person.devices.is_empty() {
        return;
    }
    let norm = entry.url.trim_end_matches('/');
    if person.duress_ntfy.iter().any(|s| s.url.trim_end_matches('/') == norm) {
        return;
    }
    let label = format!("{}#app", person.name);
    let since = now().saturating_sub(cfg.max_age_minutes * 60);
    let tag = (person_idx, entry.device.clone(), label.clone(), entry.cancel_seconds);
    let handle = tokio::spawn(ntfy::follow(entry.url.clone(), label, since, tag, ntfy_tx.clone()));
    dynamic_ntfy.insert(person.name.clone(), handle);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_link_waits_for_the_bot_name() {
        let mut cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.telegram = Some(toml::from_str("bot_token = \"123:abc\"").unwrap());
        let bot = std::sync::OnceLock::new();
        assert_eq!(known_link(&cfg, &bot, &cfg.persons[0]), None);
        bot.set("w4rnbot".to_string()).unwrap();
        let link = known_link(&cfg, &bot, &cfg.persons[0]).unwrap();
        let code = invite_code("123:abc", &cfg.persons[0].name, 0);
        assert_eq!(link, format!("https://t.me/w4rnbot?start={code}"));
        cfg.telegram = None;
        assert_eq!(known_link(&cfg, &bot, &cfg.persons[0]), None);
    }

    #[test]
    fn collects_deduplicated_inviter_chats() {
        let dir = std::env::temp_dir().join(format!("warn-service-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.data_dir = dir.clone();
        cfg.owner_chats = vec![111111111, 222222222];
        let mut contacts = Contacts::load(&cfg);
        contacts.apply("Other", &AngelId::Chat(333333333), Entry::new("Alex", 10));
        assert_eq!(collect_inviter_chats(&cfg, 1, &contacts, "Alex"), vec![111111111, 222222222, 333333333]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
