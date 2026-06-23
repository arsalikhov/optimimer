use crate::engine;
use crate::models::{RunResponse, Workflow};
use crate::store::Store;
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Long-polling Telegram bot — the primary way to drive agents.
///
/// Two ways to run an agent:
///   * Pick one (`/use <id>` or tap a button) and send plain text — it becomes `{{input}}`.
///   * Use a slash command (`/notion`, `/remind`, `/complete`, `/notify`, `/email`).
///     Each maps to the saved agent `cmd-<name>` and receives a structured input:
///     `{ text, now, tz, category, chat_id, command }`.
///
/// Other commands:
///   /start | /agents   list agents as tappable buttons
///   /use <id|name>     set the active agent for this chat
///   /where             share your location so reminders use your current timezone
///   /tz <Area/City>    set your timezone manually (e.g. /tz Europe/London)
///
/// No-op (with a log line) when TELEGRAM_BOT_TOKEN is unset.

/// Slash commands that route to a `cmd-<name>` agent.
const COMMANDS: &[&str] = &["notion", "remind", "complete", "notify", "email", "note"];

/// Emitted by the `/notion` agent's Output when the LLM can't tell Sagemesh from
/// Personal; the bot turns it into a two-button prompt and re-runs with the answer.
const ASK_CATEGORY: &str = "ASK_CATEGORY";

/// A command awaiting the Sagemesh/Personal answer for a given chat.
#[derive(Clone)]
struct Pending {
    command: String,
    text: String,
}

#[derive(Clone)]
struct BotState {
    store: Store,
    active: Arc<Mutex<HashMap<i64, String>>>,
    tz: Arc<Mutex<HashMap<i64, String>>>,
    pending: Arc<Mutex<HashMap<i64, Pending>>>,
    tz_path: PathBuf,
}

/// A bot reply: text plus an optional reply markup (inline or reply keyboard).
struct Reply {
    text: String,
    keyboard: Option<Value>,
}
impl Reply {
    fn text(s: impl Into<String>) -> Self {
        Reply { text: s.into(), keyboard: None }
    }
}

pub async fn run_bot(store: Store) {
    let token = match std::env::var("TELEGRAM_BOT_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t,
        _ => {
            tracing::warn!("TELEGRAM_BOT_TOKEN unset — Telegram bot disabled");
            return;
        }
    };

    let api = format!("https://api.telegram.org/bot{token}");
    let client = reqwest::Client::new();
    let tz_path: PathBuf = std::env::var("OPTIMIMER_TZ_DATA")
        .unwrap_or_else(|_| "chat_tz.json".to_string())
        .into();
    let state = BotState {
        store,
        active: Arc::new(Mutex::new(HashMap::new())),
        tz: Arc::new(Mutex::new(load_tz(&tz_path))),
        pending: Arc::new(Mutex::new(HashMap::new())),
        tz_path,
    };
    let mut offset: i64 = 0;

    tracing::info!("Telegram bot started (long polling)");

    loop {
        let updates = match get_updates(&client, &api, offset).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("getUpdates failed: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        };

        for upd in updates {
            if let Some(id) = upd["update_id"].as_i64() {
                offset = id + 1;
            }

            // Button taps arrive as callback_query, not message.
            if let Some(cb) = upd.get("callback_query").filter(|c| !c.is_null()) {
                handle_callback(&client, &api, &state, cb).await;
                continue;
            }

            let msg = &upd["message"];
            let chat_id = match msg["chat"]["id"].as_i64() {
                Some(c) => c,
                None => continue,
            };

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
                    handle_voice(&client, &api, &token, &state, chat_id, fid).await;
                }
                continue;
            }

            let text = msg["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                continue;
            }

            let reply = handle_message(&state, chat_id, &text).await;
            send(&client, &api, chat_id, &reply).await;
        }
    }
}

async fn handle_message(state: &BotState, chat_id: i64, text: &str) -> Reply {
    if text.starts_with("/start") || text.starts_with("/agents") {
        return agents_reply(&state.store, "👋 *Optimimer*\nTap an agent to select it, then send a message to run it.\n\nOr use a command: /notion /remind /complete /notify /email");
    }

    if let Some(arg) = text.strip_prefix("/use") {
        let q = arg.trim();
        return match resolve_agent(&state.store, q) {
            Some(wf) => {
                state.active.lock().unwrap().insert(chat_id, wf.id.clone());
                Reply::text(format!("✅ Active agent: *{}*\nSend a message to run it.", wf.name))
            }
            None => agents_reply(&state.store, &format!("No agent matching '{q}'. Pick one:")),
        };
    }

    if text.starts_with("/where") {
        return Reply {
            text: format!(
                "📍 Tap below to share your location — I'll set your timezone so reminders fire at the right time.\nCurrent timezone: *{}*",
                tz_for(state, chat_id)
            ),
            keyboard: Some(json!({
                "keyboard": [[{ "text": "📍 Share location", "request_location": true }]],
                "resize_keyboard": true,
                "one_time_keyboard": true
            })),
        };
    }

    if let Some(arg) = text.strip_prefix("/tz") {
        let name = arg.trim();
        if name.is_empty() {
            return Reply::text(format!("Your timezone is *{}*.\nSet it with `/tz Europe/London`, or send your location with /where.", tz_for(state, chat_id)));
        }
        return match name.parse::<chrono_tz::Tz>() {
            Ok(_) => {
                set_tz(state, chat_id, name);
                Reply::text(format!("✅ Timezone set to *{name}*."))
            }
            Err(_) => Reply::text(format!("'{name}' isn't a valid IANA timezone (try e.g. `Europe/London`).")),
        };
    }

    // Slash command → cmd-<name> agent.
    if let Some(rest) = text.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let body = parts.next().unwrap_or("").trim();
        if COMMANDS.contains(&cmd.as_str()) {
            if body.is_empty() {
                return Reply::text(format!("Usage: `/{cmd} <what you want>`"));
            }
            return run_command(state, chat_id, &cmd, body, "").await;
        }
        // Unknown slash command falls through to the plain-text agent path.
    }

    // Plain text → run the active agent (or the most recent one).
    let wf = match active_or_recent(state, chat_id) {
        Some(w) => w,
        None => return Reply::text("No agents yet. Build one in the web UI, then come back."),
    };
    let result = engine::run(&wf, Value::String(text.to_string())).await;
    Reply::text(format_result(&wf.name, &result))
}

/// The two category buttons shown when a command needs a "which bucket?" answer.
/// Defaults are this project's own labels — override `CATEGORY_A` / `CATEGORY_B`
/// in the env to adapt them to your domain (e.g. "Work" / "Home") without touching
/// code. This is just the button text; `agents/cmd-notion.json` is where the answer
/// is mapped onto your Notion schema — fork that agent to change the mapping.
fn category_labels() -> (String, String) {
    (
        std::env::var("CATEGORY_A").unwrap_or_else(|_| "Sagemesh".to_string()),
        std::env::var("CATEGORY_B").unwrap_or_else(|_| "Personal".to_string()),
    )
}

/// Run a `cmd-<name>` agent with structured input. Handles the category
/// confirmation: if the agent asks, stash the request and show two buttons.
async fn run_command(state: &BotState, chat_id: i64, cmd: &str, text: &str, category: &str) -> Reply {
    let wf = match state.store.get(&format!("cmd-{cmd}")) {
        Some(w) => w,
        None => return Reply::text(format!("⚠️ The `/{cmd}` agent isn't installed (expected agent id `cmd-{cmd}`). Restart the backend to seed it, or build it in the web UI.")),
    };

    let tz = tz_for(state, chat_id);
    let now = now_in_tz(&tz);
    let input = json!({
        "text": text,
        "now": now,
        "tz": tz,
        "category": category,
        "chat_id": chat_id.to_string(),
        "command": cmd,
    });

    let result = engine::run(&wf, input).await;
    let body = output_text(&result).unwrap_or_default();

    if body.trim_start().starts_with(ASK_CATEGORY) {
        state.pending.lock().unwrap().insert(chat_id, Pending { command: cmd.to_string(), text: text.to_string() });
        let (cat_a, cat_b) = category_labels();
        return Reply {
            text: format!("Is this for *{cat_a}* or *{cat_b}*?"),
            keyboard: Some(json!({
                "inline_keyboard": [[
                    { "text": format!("🏢 {cat_a}"), "callback_data": "cat:sagemesh" },
                    { "text": format!("🙂 {cat_b}"), "callback_data": "cat:personal" }
                ]]
            })),
        };
    }

    Reply::text(format_result(&wf.name, &result))
}

async fn handle_callback(client: &reqwest::Client, api: &str, state: &BotState, cb: &Value) {
    let cb_id = cb["id"].as_str().unwrap_or("");
    let chat_id = cb["message"]["chat"]["id"].as_i64();
    let data = cb["data"].as_str().unwrap_or("");

    let mut note = "OK".to_string();

    // Agent selection.
    if let (Some(id), Some(chat)) = (data.strip_prefix("use:"), chat_id) {
        if let Some(wf) = state.store.get(id) {
            state.active.lock().unwrap().insert(chat, wf.id.clone());
            note = format!("Active: {}", wf.name);
            send(client, api, chat, &Reply::text(format!("✅ Active agent: *{}*\nSend a message to run it.", wf.name))).await;
        }
    }

    // Sagemesh/Personal answer → re-run the stashed command with the category.
    if let (Some(category), Some(chat)) = (data.strip_prefix("cat:"), chat_id) {
        let pending = state.pending.lock().unwrap().remove(&chat);
        if let Some(p) = pending {
            note = format!("Category: {category}");
            let reply = run_command(state, chat, &p.command, &p.text, category).await;
            send(client, api, chat, &reply).await;
        } else {
            send(client, api, chat, &Reply::text("That prompt expired — send the command again.")).await;
        }
    }

    let _ = client
        .post(format!("{api}/answerCallbackQuery"))
        .json(&json!({ "callback_query_id": cb_id, "text": note }))
        .send()
        .await;
}

/// Download a voice note, transcribe it, then route the transcript to a command
/// via the `cmd-route` agent (falling back to the active agent if routing fails).
async fn handle_voice(client: &reqwest::Client, api: &str, token: &str, state: &BotState, chat_id: i64, file_id: &str) {
    let file_path = match get_file_path(client, api, file_id).await {
        Some(p) => p,
        None => return send(client, api, chat_id, &Reply::text("⚠️ Couldn't fetch that voice message.")).await,
    };
    let url = format!("https://api.telegram.org/file/bot{token}/{file_path}");
    let audio = match client.get(&url).send().await.ok() {
        Some(r) => match r.bytes().await {
            Ok(b) => b.to_vec(),
            Err(_) => return send(client, api, chat_id, &Reply::text("⚠️ Couldn't download the audio.")).await,
        },
        None => return send(client, api, chat_id, &Reply::text("⚠️ Couldn't download the audio.")).await,
    };
    let format = file_path.rsplit('.').next().filter(|e| !e.is_empty()).unwrap_or("ogg").to_lowercase();

    let transcript = match crate::transcribe::transcribe(audio, &format).await {
        Ok(t) => t,
        Err(e) => return send(client, api, chat_id, &Reply::text(format!("⚠️ Transcription failed: {e}"))).await,
    };
    if transcript.trim().is_empty() {
        return send(client, api, chat_id, &Reply::text("🎤 I didn't catch that — try again?")).await;
    }
    send(client, api, chat_id, &Reply::text(format!("🎤 _{transcript}_"))).await;

    // Route to a command via the cmd-route agent (returns {command, text}).
    if let Some(wf) = state.store.get("cmd-route") {
        let tz = tz_for(state, chat_id);
        let input = json!({ "text": transcript, "now": now_in_tz(&tz), "tz": tz, "chat_id": chat_id.to_string() });
        let result = engine::run(&wf, input).await;
        if let Ok(route) = serde_json::from_str::<Value>(&output_text(&result).unwrap_or_default()) {
            let cmd = route["command"].as_str().unwrap_or("").to_lowercase();
            let text = route["text"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(transcript.as_str());
            if COMMANDS.contains(&cmd.as_str()) {
                let reply = run_command(state, chat_id, &cmd, text, "").await;
                return send(client, api, chat_id, &reply).await;
            }
        }
    }
    // Fallback: treat the transcript as a normal message.
    let reply = handle_message(state, chat_id, &transcript).await;
    send(client, api, chat_id, &reply).await;
}

async fn get_file_path(client: &reqwest::Client, api: &str, file_id: &str) -> Option<String> {
    let resp: Value = client
        .get(format!("{api}/getFile"))
        .query(&[("file_id", file_id)])
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    resp["result"]["file_path"].as_str().map(|s| s.to_string())
}

/// Map a shared location to an IANA timezone (offline, no API key) and store it.
fn handle_location(state: &BotState, chat_id: i64, loc: &Value) -> Reply {
    let lat = loc["latitude"].as_f64();
    let lng = loc["longitude"].as_f64();
    match (lat, lng) {
        (Some(lat), Some(lng)) => {
            let tz = finder().get_tz_name(lng, lat).to_string();
            if tz.is_empty() {
                return Reply::text("Couldn't resolve a timezone from that location.");
            }
            set_tz(state, chat_id, &tz);
            Reply::text(format!("📍 Timezone set to *{tz}* — reminders will use this. Local time is now {}.", now_in_tz(&tz)))
        }
        _ => Reply::text("That location didn't include coordinates."),
    }
}

// ---- timezone helpers ---------------------------------------------------------

/// tzf-rs finder is moderately expensive to build (embedded boundary data); reuse one.
fn finder() -> &'static tzf_rs::DefaultFinder {
    static F: OnceLock<tzf_rs::DefaultFinder> = OnceLock::new();
    F.get_or_init(tzf_rs::DefaultFinder::new)
}

fn default_tz() -> String {
    std::env::var("DEFAULT_TZ").unwrap_or_else(|_| "UTC".to_string())
}

fn tz_for(state: &BotState, chat_id: i64) -> String {
    state.tz.lock().unwrap().get(&chat_id).cloned().unwrap_or_else(default_tz)
}

fn set_tz(state: &BotState, chat_id: i64, tz: &str) {
    let mut map = state.tz.lock().unwrap();
    map.insert(chat_id, tz.to_string());
    if let Ok(json) = serde_json::to_string_pretty(&*map) {
        let _ = std::fs::write(&state.tz_path, json);
    }
}

fn load_tz(path: &PathBuf) -> HashMap<i64, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Human + machine friendly "now" string for the LLM to resolve relative dates.
/// e.g. "2026-06-22 15:30 -04:00 (Sunday)". Falls back to UTC on a bad tz.
fn now_in_tz(tz_name: &str) -> String {
    match tz_name.parse::<chrono_tz::Tz>() {
        Ok(tz) => Utc::now().with_timezone(&tz).format("%Y-%m-%d %H:%M %:z (%A)").to_string(),
        Err(_) => Utc::now().format("%Y-%m-%d %H:%M +00:00 (%A)").to_string(),
    }
}

// ---- shared helpers (unchanged behaviour) ------------------------------------

fn active_or_recent(state: &BotState, chat_id: i64) -> Option<Workflow> {
    let wf_id = state.active.lock().unwrap().get(&chat_id).cloned();
    wf_id
        .and_then(|id| state.store.get(&id))
        .or_else(|| state.store.list().into_iter().find(|w| !w.id.starts_with("cmd-")))
}

/// Build a reply listing agents as inline buttons (one per row). Command agents
/// (`cmd-*`) are hidden — they're driven by slash commands, not selection.
fn agents_reply(store: &Store, header: &str) -> Reply {
    let list: Vec<Workflow> = store.list().into_iter().filter(|w| !w.id.starts_with("cmd-")).collect();
    if list.is_empty() {
        return Reply::text(format!("{header}\n\nNo agents saved yet — build one in the web UI first."));
    }
    let rows: Vec<Value> = list
        .iter()
        .map(|w| json!([{ "text": format!("🤖 {}", w.name), "callback_data": format!("use:{}", w.id) }]))
        .collect();
    Reply {
        text: header.to_string(),
        keyboard: Some(json!({ "inline_keyboard": rows })),
    }
}

/// Resolve an agent by exact id, id prefix, or case-insensitive name.
fn resolve_agent(store: &Store, q: &str) -> Option<Workflow> {
    if q.is_empty() {
        return None;
    }
    if let Some(w) = store.get(q) {
        return Some(w);
    }
    let ql = q.to_lowercase();
    store
        .list()
        .into_iter()
        .find(|w| w.id.starts_with(q) || w.name.to_lowercase() == ql)
}

/// The raw Output-node value (or last successful node output) as a string.
fn output_text(result: &RunResponse) -> Option<String> {
    let v = result
        .results
        .iter()
        .rev()
        .find(|r| r.node_type == "output" && r.status == "ok")
        .map(|r| r.output["value"].clone())
        .or_else(|| {
            result
                .results
                .iter()
                .rev()
                .find(|r| r.status == "ok")
                .map(|r| r.output.clone())
        })?;
    Some(match v {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(&other).unwrap_or_default(),
    })
}

/// Human reply from a run: the Output node's value, or the last successful node.
fn format_result(name: &str, result: &RunResponse) -> String {
    if result.status == "error" {
        let err = result.results.iter().find_map(|r| r.error.clone()).unwrap_or_default();
        return format!("⚠️ *{name}* finished with an error:\n{err}");
    }
    let body = output_text(result).filter(|s| !s.is_empty()).unwrap_or_else(|| "(no output)".to_string());
    format!("🤖 *{name}*\n\n{body}")
}

async fn get_updates(client: &reqwest::Client, api: &str, offset: i64) -> anyhow::Result<Vec<Value>> {
    let resp: Value = client
        .get(format!("{api}/getUpdates"))
        .query(&[
            ("offset", offset.to_string()),
            ("timeout", "30".to_string()),
            ("allowed_updates", "[\"message\",\"callback_query\"]".to_string()),
        ])
        .timeout(std::time::Duration::from_secs(40))
        .send()
        .await?
        .json()
        .await?;
    Ok(resp["result"].as_array().cloned().unwrap_or_default())
}

async fn send(client: &reqwest::Client, api: &str, chat_id: i64, reply: &Reply) {
    let mut body = json!({
        "chat_id": chat_id,
        "text": reply.text,
        "parse_mode": "Markdown"
    });
    if let Some(kb) = &reply.keyboard {
        body["reply_markup"] = kb.clone();
    }
    if let Err(e) = client.post(format!("{api}/sendMessage")).json(&body).send().await {
        tracing::warn!("sendMessage failed: {e}");
    }
}
