//! Long-polling Telegram bot — a conversational assistant.
//!
//! There are no slash commands. Every text or voice message is a turn for the
//! tool-calling agent in `agent.rs`, which reads a bounded context (rolling
//! summary + recent turns from `crate::memory`) and acts through tools: vault
//! tasks/notes/memos, the ledger, shopping lists, reminders, email, stock
//! watches, Wake-on-LAN, timezone, and long-term memory recall.
//!
//! Non-text inputs still have dedicated paths (`media.rs`): voice → transcribe
//! → agent (or memo), receipt photo → OCR → ledger, CSV → import, forwarded
//! messages → summary batch (`summaries.rs`), location pin → timezone.
//!
//! No-op (with a log line) when TELEGRAM_BOT_TOKEN is unset.

use crate::convo;
use crate::db::Db;
use crate::engine;
use crate::models::RunResponse;
use crate::store::Store;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

mod agent;
mod callbacks;
mod help;
mod lists;
mod machines;
mod media;
mod money;
mod notes;
mod prefs;
mod summaries;
mod watch;
mod workflows;

use agent::*;
use callbacks::*;
use help::*;
use lists::*;
use machines::*;
use media::*;
use money::*;
use notes::*;
use prefs::*;
use summaries::*;
use watch::*;
use workflows::*;
pub use prefs::migrate_json;

/// A chat's message awaiting a follow-up tap: either the Sagemesh/Personal
/// category answer for `command`, or the original `text` of a message the router
/// couldn't place and offered to salvage (then `command` is empty).
#[derive(Clone)]
struct Pending {
    command: String,
    text: String,
}

#[derive(Clone)]
struct BotState {
    store: Store,
    pending: Arc<Mutex<HashMap<i64, Pending>>>,
    db: Db,
    /// Chat ids permitted to use the bot. A public bot is discoverable, so every
    /// inbound update is gated against this set (built from TELEGRAM_ALLOWED_CHAT_IDS).
    /// Deny-by-default: an empty set rejects everyone.
    allowed: Arc<HashSet<i64>>,
}

impl BotState {
    fn is_allowed(&self, chat_id: i64) -> bool {
        self.allowed.contains(&chat_id)
    }
}

/// A bot reply. `text` is the plain content (also the fallback). If `rich_html`
/// is set, it's sent via `sendRichMessage` (real tables, collapsible blocks, …),
/// falling back to plain `text` if that call fails.
#[derive(Default)]
struct Reply {
    text: String,
    keyboard: Option<Value>,
    rich_html: Option<String>,
}

impl Reply {
    fn text(s: impl Into<String>) -> Self {
        Reply {
            text: s.into(),
            ..Default::default()
        }
    }

    /// A rich (HTML) reply. `fallback` is sent as plain text if the rich send
    /// fails — e.g. an older client or an API rejection.
    fn rich(html: impl Into<String>, fallback: impl Into<String>) -> Self {
        Reply {
            text: fallback.into(),
            keyboard: None,
            rich_html: Some(html.into()),
        }
    }
}

pub async fn run_bot(store: Store, db: Db) {
    let token = match std::env::var("TELEGRAM_BOT_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t,
        _ => {
            tracing::warn!("TELEGRAM_BOT_TOKEN unset — Telegram bot disabled");
            return;
        }
    };

    // Allowlist of chat ids that may use the bot. Telegram bots are publicly
    // discoverable, so without this anyone could drive the vault/list commands.
    // Deny-by-default: if the var is unset/empty we reject everyone and log each
    // caller's chat_id so the owner can find their own and add it.
    let allowed: HashSet<i64> = std::env::var("TELEGRAM_ALLOWED_CHAT_IDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    if allowed.is_empty() {
        tracing::warn!(
            "TELEGRAM_ALLOWED_CHAT_IDS is empty — all chats are DENIED. Message the bot, \
             find your chat_id in the 'unauthorized chat' log line below, then set the var."
        );
    } else {
        tracing::info!("Telegram allowlist: {} chat id(s) authorized", allowed.len());
    }

    let api = format!("https://api.telegram.org/bot{token}");
    let client = reqwest::Client::new();
    clear_commands(&client, &api).await;
    let state = BotState {
        store,
        pending: Arc::new(Mutex::new(HashMap::new())),
        db,
        allowed: Arc::new(allowed),
    };
    let mut offset: i64 = 0;

    tracing::info!("Telegram bot started (long polling)");

    // Updates are drained through a queue (not a plain `for`) so the plain-text
    // path can peek at what follows: Telegram delivers a forward's comment before
    // the forwards themselves.
    let mut queue: VecDeque<Value> = VecDeque::new();
    loop {
        if queue.is_empty() {
            match get_updates(&client, &api, offset, 30).await {
                Ok(u) => queue.extend(u),
                Err(e) => {
                    tracing::warn!("getUpdates failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
            }
        }

        while let Some(upd) = queue.pop_front() {
            if let Some(id) = upd["update_id"].as_i64() {
                offset = id + 1;
            }

            // Button taps arrive as callback_query, not message.
            if let Some(cb) = upd.get("callback_query").filter(|c| !c.is_null()) {
                let cb_chat = cb["message"]["chat"]["id"].as_i64().unwrap_or(0);
                if !state.is_allowed(cb_chat) {
                    tracing::warn!("unauthorized chat {cb_chat} (callback) — ignored");
                    continue;
                }
                handle_callback(&client, &api, &state, cb).await;
                continue;
            }

            let msg = &upd["message"];
            let chat_id = match msg["chat"]["id"].as_i64() {
                Some(c) => c,
                None => continue,
            };

            // Authorization gate. Reject anyone not on the allowlist before any
            // command runs, and log their chat_id so the owner can allowlist it.
            if !state.is_allowed(chat_id) {
                tracing::warn!("unauthorized chat {chat_id} — denied (add to TELEGRAM_ALLOWED_CHAT_IDS)");
                send(&client, &api, chat_id, &Reply::text("Not authorized.")).await;
                continue;
            }

            // A forwarded message (text, media, or voice) joins this chat's batch;
            // the batch is summarized once no more forwards arrive.
            if convo::is_forward(msg) {
                handle_forward(&client, &api, &token, &state, chat_id, msg).await;
                continue;
            }

            // A shared location updates this chat's timezone (used for reminders).
            if let Some(loc) = msg.get("location").filter(|l| !l.is_null()) {
                let reply = handle_location(&state, chat_id, loc);
                send(&client, &api, chat_id, &reply).await;
                continue;
            }

            // Voice / audio note → transcribe, then route to a command.
            let voice = msg
                .get("voice")
                .or_else(|| msg.get("audio"))
                .or_else(|| msg.get("video_note"))
                .filter(|v| !v.is_null());
            if let Some(v) = voice {
                if let Some(fid) = v.get("file_id").and_then(|x| x.as_str()) {
                    let secs = v["duration"].as_u64().unwrap_or(0);
                    handle_voice(&client, &api, &token, &state, chat_id, fid, secs).await;
                }
                continue;
            }

            // Photo of a receipt/invoice → OCR → log as an expense via /spent.
            if let Some(photos) = msg.get("photo").and_then(|p| p.as_array()).filter(|a| !a.is_empty()) {
                // Telegram sends multiple sizes ascending; the last is the largest.
                if let Some(fid) = photos.last().and_then(|p| p["file_id"].as_str()) {
                    handle_receipt(&client, &api, &token, &state, chat_id, fid, "image/jpeg").await;
                }
                continue;
            }

            // A document: a CSV statement → bulk import; an image → treat as a receipt.
            if let Some(doc) = msg.get("document").filter(|d| !d.is_null()) {
                let name = doc["file_name"].as_str().unwrap_or("").to_lowercase();
                let mime = doc["mime_type"].as_str().unwrap_or("").to_string();
                let caption = msg["caption"].as_str().unwrap_or("");
                if let Some(fid) = doc["file_id"].as_str() {
                    if name.ends_with(".csv") || mime.contains("csv") || mime == "text/comma-separated-values" {
                        handle_csv(&client, &api, &token, &state, chat_id, fid, &name, caption).await;
                    } else if mime.starts_with("image/") {
                        handle_receipt(&client, &api, &token, &state, chat_id, fid, &mime).await;
                    } else {
                        send(&client, &api, chat_id, &Reply::text(
                            "Send a receipt photo to log an expense, or a .csv statement to import transactions.",
                        )).await;
                    }
                }
                continue;
            }

            let text = msg["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                continue;
            }

            // A plain message may be the comment on a forward batch — one that is
            // open (still collecting, or parked waiting for exactly this note) …
            if !text.starts_with('/') {
                match convo::attach_note(chat_id, &text) {
                    convo::NoteAttach::Ready(batch) => {
                        finalize_forward_batch(&client, &api, &state, chat_id, batch).await;
                        continue;
                    }
                    convo::NoteAttach::Queued => continue,
                    convo::NoteAttach::None => {}
                }
                // … or one about to arrive. Telegram sends the comment first, so
                // peek briefly for forwards from this chat before routing the text.
                let peek = convo::peek_secs();
                if peek > 0 {
                    if queue.is_empty() {
                        if let Ok(more) = get_updates(&client, &api, offset, peek).await {
                            queue.extend(more);
                        }
                    }
                    let next_is_forward = queue
                        .front()
                        .map(|u| {
                            u["message"]["chat"]["id"].as_i64() == Some(chat_id)
                                && convo::is_forward(&u["message"])
                        })
                        .unwrap_or(false);
                    if next_is_forward {
                        convo::open_with_note(chat_id, &text);
                        continue;
                    }
                }
            }

            // Everything else is a conversation turn for the agent. A leading
            // slash from muscle memory is harmless: strip it and carry on.
            let text = text.trim_start_matches('/').trim().to_string();
            if text.is_empty() {
                continue;
            }
            if let Some(reply) = run_agent(&client, &api, &state, chat_id, &text).await {
                send(&client, &api, chat_id, &reply).await;
            }
        }
    }
}

/// Long-poll for updates. `timeout_secs` is Telegram's server-side wait; it
/// returns immediately when updates are already pending.
async fn get_updates(
    client: &reqwest::Client,
    api: &str,
    offset: i64,
    timeout_secs: u64,
) -> anyhow::Result<Vec<Value>> {
    let resp: Value = client
        .get(format!("{api}/getUpdates"))
        .query(&[
            ("offset", offset.to_string()),
            ("timeout", timeout_secs.to_string()),
            (
                "allowed_updates",
                "[\"message\",\"callback_query\"]".to_string(),
            ),
        ])
        .timeout(std::time::Duration::from_secs(timeout_secs + 10))
        .send()
        .await?
        .json()
        .await?;
    Ok(resp["result"].as_array().cloned().unwrap_or_default())
}

async fn send(client: &reqwest::Client, api: &str, chat_id: i64, reply: &Reply) {
    // Rich (HTML) reply → sendRichMessage. On any failure (older client, API
    // rejection) fall back to a plain-text message so the user still gets content.
    if let Some(html) = &reply.rich_html {
        let mut body = json!({
            "chat_id": chat_id,
            "rich_message": { "html": html }
        });
        if let Some(kb) = &reply.keyboard {
            body["reply_markup"] = kb.clone();
        }
        match client.post(format!("{api}/sendRichMessage")).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => tracing::warn!(
                "sendRichMessage rejected ({}) — falling back to plain text",
                resp.status()
            ),
            Err(e) => tracing::warn!("sendRichMessage failed ({e}) — falling back to plain text"),
        }
        // fall through to the plain-text path below using reply.text
    }

    let mut body = json!({
        "chat_id": chat_id,
        "text": reply.text,
        "parse_mode": "Markdown",
        // Show clean inline hyperlinks instead of a
        // big link-preview card under every message.
        "disable_web_page_preview": true
    });
    if let Some(kb) = &reply.keyboard {
        body["reply_markup"] = kb.clone();
    } else {
        // Dismiss any lingering custom reply keyboard (e.g. an old "Share
        // location" button from before /where was removed) so it can't sit stuck
        // in the input bar. Harmless no-op otherwise; doesn't touch inline keyboards.
        body["reply_markup"] = json!({ "remove_keyboard": true });
    }
    if let Err(e) = client
        .post(format!("{api}/sendMessage"))
        .json(&body)
        .send()
        .await
    {
        tracing::warn!("sendMessage failed: {e}");
    }
}
