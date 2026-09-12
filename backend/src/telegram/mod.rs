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
//! Access: a chat pairs by sending the setup code (owner) or an invite code
//! (member) — see `crate::config` — and is then walked through `onboarding.rs`.
//! `TELEGRAM_ALLOWED_CHAT_IDS` still works as a static allowlist.
//!
//! No-op (with a log line) when TELEGRAM_BOT_TOKEN is unset.

use crate::config;
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
mod onboarding;
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
use onboarding::*;
use prefs::*;
use summaries::*;
use watch::*;
use workflows::*;
pub use prefs::migrate_json;

/// A confirmation the agent proposed that carries a payload too big for a
/// callback (e.g. `set_categories`): `command` names the action, `text` the data.
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
    /// Static allowlist from TELEGRAM_ALLOWED_CHAT_IDS, on top of the chats
    /// paired in the database. Deny-by-default: unknown chats only get to pair.
    allowed: Arc<HashSet<i64>>,
    /// One lock per chat: agent turns run off the polling loop (so a slow model
    /// can't freeze the bot) but stay in order within a chat.
    turns: Arc<Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>>,
}

impl BotState {
    fn is_allowed(&self, chat_id: i64) -> bool {
        self.allowed.contains(&chat_id) || config::chat_role(chat_id).is_some()
    }

    /// The owner: the chat that paired with the setup code — or, before anyone
    /// has, any env-allowlisted chat (single-user installs from before pairing).
    fn is_owner(&self, chat_id: i64) -> bool {
        config::is_owner(chat_id) || (config::owner_chat().is_none() && self.allowed.contains(&chat_id))
    }

    fn turn_lock(&self, chat_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        self.turns.lock().unwrap().entry(chat_id).or_default().clone()
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

    // Telegram bots are publicly discoverable, so access is deny-by-default:
    // a chat is in if it paired (setup/invite code → `chats` table) or is in
    // the optional static allowlist.
    let allowed: HashSet<i64> = std::env::var("TELEGRAM_ALLOWED_CHAT_IDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    match config::setup_code() {
        Some(code) => {
            tracing::info!("No owner paired yet. Open Telegram, message the bot and send the setup code: {code}");
            crate::setup::print_setup_code(&code);
        }
        None => tracing::info!(
            "Telegram access: {} paired chat(s), {} from TELEGRAM_ALLOWED_CHAT_IDS",
            config::chats().len(),
            allowed.len()
        ),
    }

    let api = format!("https://api.telegram.org/bot{token}");
    let client = reqwest::Client::new();
    clear_commands(&client, &api).await;
    let state = BotState {
        store,
        pending: Arc::new(Mutex::new(HashMap::new())),
        db,
        allowed: Arc::new(allowed),
        turns: Arc::new(Mutex::new(HashMap::new())),
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

            // Authorization gate. An unknown chat's only move is to pair: the
            // setup code makes it the owner, an invite code a member. Anything
            // else is refused (and logged, so the owner can spot strangers).
            if !state.is_allowed(chat_id) {
                let text = msg["text"].as_str().unwrap_or("").trim();
                let first_name = msg["from"]["first_name"].as_str().unwrap_or("");
                match config::try_pair(chat_id, text, first_name) {
                    Some(role) => {
                        tracing::info!("chat {chat_id} paired as {role}");
                        let owner = role == config::ROLE_OWNER;
                        send(&client, &api, chat_id, &onboarding::start(chat_id, owner, first_name)).await;
                    }
                    None => {
                        tracing::warn!("unauthorized chat {chat_id} — denied (needs the setup or an invite code)");
                        send(&client, &api, chat_id, &Reply::text(
                            "This assistant is private. If it's yours, send the setup code the installer printed \
                             (it's also in the server log); otherwise ask the owner for an invite code.",
                        )).await;
                    }
                }
                continue;
            }

            // While setup is running it owns every message (text, pins, buttons).
            if onboarding::active(chat_id) {
                if let Some(reply) = onboarding::step(&state, chat_id, msg).await {
                    send(&client, &api, chat_id, &reply).await;
                }
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
            // Off the polling loop: other chats (and pins, buttons, forwards)
            // keep flowing while this turn waits on the model.
            let (client, api, state) = (client.clone(), api.clone(), state.clone());
            tokio::spawn(async move {
                let _turn = state.turn_lock(chat_id).lock_owned().await;
                if let Some(reply) = run_agent(&client, &api, &state, chat_id, &text).await {
                    send(&client, &api, chat_id, &reply).await;
                }
            });
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
    match client.post(format!("{api}/sendMessage")).json(&body).send().await {
        Err(e) => tracing::warn!("sendMessage failed: {e}"),
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            // Telegram said no. The usual reason is Markdown it can't parse (a
            // stray `_` or `*` in user-supplied text); resend as plain text so
            // the user still gets the message, and log everything else.
            let status = resp.status();
            let desc = resp
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v["description"].as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            if desc.to_lowercase().contains("parse") {
                if let Some(o) = body.as_object_mut() {
                    o.remove("parse_mode");
                }
                match client.post(format!("{api}/sendMessage")).json(&body).send().await {
                    Ok(r) if r.status().is_success() => tracing::info!("sendMessage: Markdown rejected ({desc}) — resent as plain text"),
                    Ok(r) => tracing::warn!("sendMessage rejected twice ({}, then {})", desc, r.status()),
                    Err(e) => tracing::warn!("sendMessage retry failed: {e}"),
                }
            } else {
                tracing::warn!("sendMessage rejected ({status}): {desc}");
            }
        }
    }
}
