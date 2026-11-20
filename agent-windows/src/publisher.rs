//! Turns local triggers into messages and delivers them to the watchers via relays.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use nostr_sdk::prelude::*;
use tokio::sync::mpsc::UnboundedReceiver;
use warn_proto::{new_session_id, Envelope, Message};

use crate::config::Config;
use crate::location::{self, Locator};
use crate::state;

#[derive(Debug, Clone, Copy)]
pub enum Trigger {
    Logon,
    Unlock,
    /// Started again by the supervisor after a crash, while the screen was unlocked.
    Restart,
    Ok,
    Duress,
    /// The settings window switched check-ins off or on (already applied in `state`).
    Paused(bool),
}

/// Keep retrying a message this long (e.g. Wi-Fi reconnecting after sleep).
const DELIVERY_PATIENCE: Duration = Duration::from_secs(15 * 60);
/// How long to wait for one relay's acknowledgement before retrying it. Relays are tried in
/// parallel, so a generous value costs nothing; too short a value makes a slow relay
/// (spam checks on new keys) time out on every attempt.
const RELAY_OK_TIMEOUT: Duration = Duration::from_secs(15);
/// An OK is only sent for sessions younger than this.
const OPEN_SESSION_MAX_AGE: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub struct Sender {
    client: Arc<Client>,
    keys: Keys,
    watchers: Vec<PublicKey>,
    relays: Vec<String>,
    /// Measures the position for each start and duress message (if enabled).
    locator: Option<Arc<Locator>>,
}

impl Sender {
    /// Connect to the relays. With `wait`, block until connected or the timeout passes.
    pub async fn connect(cfg: &Config, keys: Keys, wait: Option<Duration>) -> Result<Self> {
        let client = Client::new();
        for r in &cfg.relays {
            if let Err(e) = client.add_relay(r.as_str()).await {
                tracing::error!("bad relay {r}: {e}");
            }
        }
        match wait {
            Some(t) => client.connect().and_wait(t).await,
            None => client.connect().await,
        };
        Ok(Self {
            client: Arc::new(client),
            watchers: cfg.recipients(&keys.public_key())?,
            relays: cfg.relays.clone(),
            keys,
            locator: None,
        })
    }

    /// Add the measured position to messages, remembering the last fix in `home`.
    pub fn with_location(mut self, home: &std::path::Path) -> Self {
        self.locator = Some(Arc::new(Locator::new(home)));
        self
    }

    /// Context for a message: the configured place plus, if enabled, the measured position
    /// (waits at most `location::WAIT`, else uses the last known fix).
    pub async fn context(&self, cfg: &Config, trigger: &str) -> warn_proto::Context {
        let mut ctx = cfg.context(trigger);
        if let Some(loc) = &self.locator {
            let fix = loc.current().await;
            ctx.location = location::combine(cfg.location.as_deref(), fix.as_ref());
        }
        ctx
    }

    /// Deliver one message to every watcher. Each relay is tried in parallel and independently;
    /// this returns as soon as one relay accepted every copy, while the other relays keep
    /// trying in the background. A dead relay therefore never slows down an alarm.
    pub async fn deliver(&self, envelope: &Envelope) -> Result<()> {
        let wraps = warn_proto::wrap(&self.keys, &self.watchers, envelope)?;
        let give_up = Instant::now() + DELIVERY_PATIENCE;
        for wrap in wraps {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            for url in &self.relays {
                let (client, wrap, url, tx) = (self.client.clone(), wrap.clone(), url.clone(), tx.clone());
                tokio::spawn(async move {
                    for attempt in 1u64.. {
                        match client.send_event(&wrap).to([url.as_str()]).ok_timeout(RELAY_OK_TIMEOUT).await {
                            Ok(out) if !out.success.is_empty() => {
                                let _ = tx.send(());
                                return;
                            }
                            Ok(out) => tracing::debug!("{url} refused (attempt {attempt}): {:?}", out.failed),
                            Err(e) => tracing::debug!("{url} failed (attempt {attempt}): {e}"),
                        }
                        if Instant::now() > give_up {
                            return;
                        }
                        // Fast at first (relay still connecting), then back off to 5 s.
                        let backoff = 250u64.saturating_mul(1 << attempt.min(5)).min(5000);
                        tokio::time::sleep(Duration::from_millis(backoff)).await;
                    }
                });
            }
            drop(tx);
            if rx.recv().await.is_none() {
                anyhow::bail!("no relay accepted the message within {} minutes", DELIVERY_PATIENCE.as_secs() / 60);
            }
        }
        Ok(())
    }

    fn spawn_deliver(&self, envelope: Envelope) {
        let this = self.clone();
        tokio::spawn(async move {
            let kind = match &envelope.msg {
                Message::SessionStart { .. } => "session_start",
                Message::SessionOk { .. } => "session_ok",
                Message::Duress { .. } => "duress",
                Message::Test { .. } => "test",
                Message::AlarmSent { .. } => "alarm_sent",
                Message::ContactChanged { .. } => "contact_changed",
                Message::Watching { .. } => "watching",
                Message::Ping { .. } => "ping",
                Message::TopicChanged { .. } => "topic_changed",
                _ => "message",
            };
            let started = Instant::now();
            match this.deliver(&envelope).await {
                Ok(()) => tracing::info!("{kind} delivered in {} ms", started.elapsed().as_millis()),
                Err(e) => tracing::error!("{kind} NOT delivered: {e:#}"),
            }
        });
    }
}

/// Remind the user of the check-in when it has not come after the configured time.
async fn remind(c: crate::config::CheckIn, bot: Option<warn_watcher::telegram::Telegram>) {
    let after = Duration::from_secs(c.reminder_seconds);
    tokio::time::sleep(after).await;
    if !state::still_waiting(after) {
        return;
    }
    tracing::info!("reminder: no check-in after {} s", c.reminder_seconds);
    if c.sound || c.notice {
        state::remind_on_screen();
    }
    if let (true, Some(bot), Some(chat)) = (c.telegram, bot, c.telegram_chat) {
        if let Err(e) = bot.send(chat, &c.telegram_text, &[]).await {
            tracing::warn!("reminder via Telegram failed: {e}");
        }
    }
}

/// Long-running loop for `run`.
pub async fn run(cfg: Config, sender: Sender, mut rx: UnboundedReceiver<Trigger>) {
    let mut open: Vec<(String, Instant)> = Vec::new();
    // For the Telegram reminder (the built-in watcher's bot).
    let bot = cfg.bot_token().and_then(|t| warn_watcher::telegram::Telegram::new(t).ok());
    while let Some(trigger) = rx.recv().await {
        tracing::info!("trigger: {trigger:?}");
        match trigger {
            Trigger::Logon | Trigger::Unlock | Trigger::Restart => {
                if state::paused() {
                    tracing::info!("check-ins paused: no session started");
                    continue;
                }
                let name = match trigger {
                    Trigger::Logon => "logon",
                    Trigger::Restart => "restart",
                    _ => "unlock",
                };
                state::session_started();
                let session = new_session_id();
                open.push((session.clone(), Instant::now()));
                // Looking up the position may take a few seconds: do it off the loop, so an OK
                // pressed meanwhile is handled at once (watchers accept it before the start).
                let (sender, timeout_secs) = (sender.clone(), state::timeout_secs());
                let check_in = state::check_in();
                if check_in.reminder_on() {
                    tokio::spawn(remind(check_in, bot.clone()));
                }
                let ctx_cfg = cfg.clone();
                tokio::spawn(async move {
                    let context = sender.context(&ctx_cfg, name).await;
                    sender.spawn_deliver(Envelope::new(Message::SessionStart { session, timeout_secs, context }));
                });
            }
            Trigger::Ok => {
                state::checked_in();
                // Covers quick lock/unlock sequences: every open session gets its OK.
                for (session, at) in open.drain(..) {
                    if at.elapsed() < OPEN_SESSION_MAX_AGE {
                        sender.spawn_deliver(Envelope::new(Message::SessionOk { session }));
                    }
                }
            }
            Trigger::Paused(p) => {
                tracing::info!("check-ins {}", if p { "paused" } else { "enabled again" });
            }
            Trigger::Duress => {
                let (sender, ctx_cfg) = (sender.clone(), cfg.clone());
                tokio::spawn(async move {
                    let context = sender.context(&ctx_cfg, "duress_hotkey").await;
                    sender.spawn_deliver(Envelope::new(Message::Duress { context }));
                });
            }
        }
    }
}
