//! ntfy as a message source for devices that cannot speak Nostr, such as a phone shortcut.
//! Outbound only: the phone posts to a secret topic, the watcher streams that topic.
//!
//! Message text (first word is the command, the rest is the location or a note):
//!   `start <seconds> [scenario] [location]`  a clock starts (e.g. a bank app was opened)
//!   `ok`                                     check-in for this phone's open clocks
//!   `duress [location]`                      alarm at once
//! Any other text is a duress alarm with that text as location, so the simplest shortcut
//! (post the location) keeps working.

use std::time::Duration;

use anyhow::{bail, Result};
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

/// ntfy sends a keepalive every 45 s by default; silence longer than this means a dead stream.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest text taken from a message (it becomes the alarm's location).
const MAX_TEXT: usize = 500;

#[derive(Debug, Clone)]
pub struct NtfyMessage {
    pub id: String,
    /// Unix seconds, set by the ntfy server.
    pub time: u64,
    pub text: String,
}

#[derive(Deserialize)]
struct Line {
    event: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    time: u64,
    #[serde(default)]
    message: String,
}

/// What a message on a topic asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Start { seconds: u64, scenario: String, location: Option<String> },
    Ok,
    Duress { location: Option<String> },
}

/// Read the command in a message text (see the module comment).
pub fn parse_command(text: &str) -> Command {
    let rest_of = |words: &[&str]| {
        let rest = words.join(" ");
        (!rest.is_empty()).then_some(rest)
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.first().map(|w| w.to_ascii_lowercase()).as_deref() {
        Some("ok") if words.len() == 1 => Command::Ok,
        Some("start") => match words.get(1).and_then(|w| w.parse::<u64>().ok()) {
            Some(seconds) => {
                // A scenario is one plain word; a location usually starts with http.
                let (scenario, from) = match words.get(2) {
                    Some(w) if !w.contains("://") && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                        (w.to_string(), 3)
                    }
                    _ => ("app_open".to_string(), 2),
                };
                Command::Start { seconds, scenario, location: rest_of(&words[from.min(words.len())..]) }
            }
            None => Command::Duress { location: rest_of(&words) },
        },
        Some("duress") => Command::Duress { location: rest_of(&words[1..]) },
        _ => Command::Duress { location: rest_of(&words) },
    }
}

/// Parse one line of ntfy's JSON stream. Only real messages count, not keepalives.
fn parse_line(line: &str) -> Option<NtfyMessage> {
    let l: Line = serde_json::from_str(line).ok()?;
    if l.event != "message" || l.id.is_empty() {
        return None;
    }
    Some(NtfyMessage {
        id: l.id,
        time: l.time,
        text: l.message.trim().chars().take(MAX_TEXT).collect(),
    })
}

/// Follow a topic forever, reconnecting as needed. Messages newer than `since` (unix seconds)
/// are sent to `tx`, also the ones cached by ntfy while we were disconnected. Duplicates are
/// possible after a reconnect; the engine drops them by id.
pub async fn follow<T: Clone + Send + 'static>(
    url: String,
    label: String,
    since: u64,
    tag: T,
    tx: UnboundedSender<(T, NtfyMessage)>,
) {
    let http = reqwest::Client::new();
    let mut since = since;
    let mut backoff = Duration::from_secs(2);
    loop {
        match stream(&http, &url, since, &tag, &tx).await {
            Ok(last) => {
                since = since.max(last);
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                // Never log the URL: the topic name is the secret.
                tracing::warn!("ntfy source {label}: {e:#}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
        if tx.is_closed() {
            return;
        }
    }
}

/// One streaming connection. Returns the newest message time seen when the stream ends.
async fn stream<T: Clone>(
    http: &reqwest::Client,
    url: &str,
    since: u64,
    tag: &T,
    tx: &UnboundedSender<(T, NtfyMessage)>,
) -> Result<u64> {
    let mut resp = http
        .get(format!("{}/json", url.trim_end_matches('/')))
        .query(&[("since", since.to_string())])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connect failed: {}", e.without_url()))?;
    if !resp.status().is_success() {
        bail!("server answered {}", resp.status());
    }
    let mut last = since;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(READ_TIMEOUT, resp.chunk()).await {
            Err(_) => bail!("no data for {} s", READ_TIMEOUT.as_secs()),
            Ok(Err(e)) => bail!("stream broke: {}", e.without_url()),
            Ok(Ok(None)) => return Ok(last),
            Ok(Ok(Some(c))) => c,
        };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            if let Some(m) = parse_line(&String::from_utf8_lossy(&line)) {
                last = last.max(m.time);
                let _ = tx.send((tag.clone(), m));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_read_from_the_text() {
        let loc = Some("https://maps.example/?q=1,2".to_string());
        assert_eq!(parse_command("ok"), Command::Ok);
        assert_eq!(parse_command(" OK "), Command::Ok);
        assert_eq!(
            parse_command("start 90 app_open https://maps.example/?q=1,2"),
            Command::Start { seconds: 90, scenario: "app_open".into(), location: loc.clone() }
        );
        assert_eq!(
            parse_command("start 1800 https://maps.example/?q=1,2"),
            Command::Start { seconds: 1800, scenario: "app_open".into(), location: loc.clone() }
        );
        assert_eq!(parse_command("start 60 walk_home"), Command::Start { seconds: 60, scenario: "walk_home".into(), location: None });
        assert_eq!(parse_command("duress https://maps.example/?q=1,2"), Command::Duress { location: loc.clone() });
        assert_eq!(parse_command("duress"), Command::Duress { location: None });
        // Old shortcuts post only the location: still duress.
        assert_eq!(parse_command("https://maps.example/?q=1,2"), Command::Duress { location: loc });
        // "ok" with more text is not a check-in (no accidental OK from a note).
        assert!(matches!(parse_command("ok but help"), Command::Duress { .. }));
        assert!(matches!(parse_command("start soon"), Command::Duress { .. }));
    }

    #[test]
    fn parses_messages_and_skips_keepalives() {
        assert!(parse_line(r#"{"id":"k","time":1,"event":"keepalive","topic":"t"}"#).is_none());
        assert!(parse_line(r#"{"id":"o","time":1,"event":"open","topic":"t"}"#).is_none());
        let m = parse_line(r#"{"id":"abc","time":1800000000,"event":"message","topic":"t","message":" https://maps.example/?ll=1,2 \n"}"#)
            .unwrap();
        assert_eq!(m.id, "abc");
        assert_eq!(m.time, 1_800_000_000);
        assert_eq!(m.text, "https://maps.example/?ll=1,2");
    }

    #[test]
    fn long_text_is_cut() {
        let long = "x".repeat(2000);
        let m = parse_line(&format!(r#"{{"id":"a","time":1,"event":"message","message":"{long}"}}"#)).unwrap();
        assert_eq!(m.text.len(), MAX_TEXT);
    }
}
