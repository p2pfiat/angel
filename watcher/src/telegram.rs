//! Minimal Telegram Bot API client. Outbound only: replies are fetched with long polling.

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    base: String,
}

/// A button press from a contact.
#[derive(Debug)]
pub struct ButtonPress {
    pub callback_id: String,
    pub data: String,
    pub who: String,
    /// The chat the button was pressed in.
    pub chat_id: i64,
}

pub enum Update {
    Button(ButtonPress),
    /// `/id`: we answer with the chat id so it can be configured by hand.
    ChatIdRequest { chat_id: i64 },
    /// `/start`, with the code of a join link if it came through one.
    Start { chat_id: i64, who: String, code: Option<String> },
    /// `/stop`: stop receiving alarms.
    Stop { chat_id: i64, who: String },
    /// `/pair [name]`: a link to add a phone (only from the owner's chats).
    Pair { chat_id: i64, name: Option<String> },
    /// `/offer [name]`: a link that lets someone add this server (only from the owner's chats).
    Offer { chat_id: i64, name: Option<String> },
}

impl Telegram {
    pub fn new(token: &str) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(70)).build()?;
        Ok(Self {
            http,
            base: format!("https://api.telegram.org/bot{token}"),
        })
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        // Never log the URL: it contains the bot token. reqwest puts it in its errors.
        let hide = |e: reqwest::Error| anyhow::anyhow!("telegram {method}: {}", e.without_url());
        let resp: Value = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .map_err(hide)?
            .json()
            .await
            .map_err(hide)?;
        if resp["ok"].as_bool() != Some(true) {
            bail!("telegram {method} failed: {}", resp["description"]);
        }
        Ok(resp["result"].clone())
    }

    /// Send a plain-text message, optionally with one row of buttons (label, callback data).
    pub async fn send(&self, chat_id: i64, text: &str, buttons: &[(String, String)]) -> Result<()> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if !buttons.is_empty() {
            let row: Vec<Value> = buttons
                .iter()
                .map(|(label, data)| json!({ "text": label, "callback_data": data }))
                .collect();
            body["reply_markup"] = json!({ "inline_keyboard": [row] });
        }
        self.call("sendMessage", body).await.map(|_| ())
    }

    /// The bot's @username, for join links.
    pub async fn username(&self) -> Result<String> {
        let me = self.call("getMe", json!({})).await?;
        me["username"].as_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("bot has no username"))
    }

    /// A chat's display name (a person's first name), for messages about them.
    pub async fn chat_name(&self, chat_id: i64) -> String {
        match self.call("getChat", json!({ "chat_id": chat_id })).await {
            Ok(c) => c["first_name"].as_str().or(c["title"].as_str()).unwrap_or("Someone").to_string(),
            Err(_) => "Someone".into(),
        }
    }

    pub async fn answer_button(&self, callback_id: &str) -> Result<()> {
        self.call("answerCallbackQuery", json!({ "callback_query_id": callback_id }))
            .await
            .map(|_| ())
    }

    /// Long-poll for updates. Returns the updates and the next offset.
    pub async fn poll(&self, offset: i64) -> Result<(Vec<Update>, i64)> {
        let result = self
            .call(
                "getUpdates",
                json!({ "offset": offset, "timeout": 50, "allowed_updates": ["message", "callback_query"] }),
            )
            .await?;
        let mut next = offset;
        let mut out = Vec::new();
        for u in result.as_array().cloned().unwrap_or_default() {
            if let Some(id) = u["update_id"].as_i64() {
                next = next.max(id + 1);
            }
            if let Some(cb) = u.get("callback_query") {
                let who = cb["from"]["first_name"].as_str().unwrap_or("Someone").to_string();
                let chat_id = cb["message"]["chat"]["id"].as_i64().unwrap_or(0);
                if let (Some(id), Some(data)) = (cb["id"].as_str(), cb["data"].as_str()) {
                    out.push(Update::Button(ButtonPress {
                        callback_id: id.into(),
                        data: data.into(),
                        who,
                        chat_id,
                    }));
                }
            } else if let (Some(text), Some(chat_id)) =
                (u["message"]["text"].as_str(), u["message"]["chat"]["id"].as_i64())
            {
                let who = u["message"]["from"]["first_name"].as_str().unwrap_or("Someone").to_string();
                let mut words = text.split_whitespace();
                // "/start@BotName" in groups.
                match words.next().map(|c| c.split('@').next().unwrap_or(c)) {
                    Some("/start") => {
                        out.push(Update::Start { chat_id, who, code: words.next().map(str::to_string) })
                    }
                    Some("/stop") => out.push(Update::Stop { chat_id, who }),
                    Some("/id") => out.push(Update::ChatIdRequest { chat_id }),
                    Some(c @ ("/pair" | "/offer")) => {
                        let name = words.collect::<Vec<_>>().join(" ");
                        let name = (!name.is_empty()).then_some(name);
                        out.push(if c == "/pair" { Update::Pair { chat_id, name } } else { Update::Offer { chat_id, name } })
                    }
                    _ => {}
                }
            }
        }
        Ok((out, next))
    }
}
