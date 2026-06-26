use crate::db::Db;
use crate::engine;
use crate::models::{RunResponse, Workflow};
use crate::store::Store;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

/// Long-polling Telegram bot — the primary way to drive agents.
///
/// Two ways to run an agent:
///   * Pick one (`/use <id>` or tap a button) and send plain text — it becomes `{{input}}`.
///   * Use a slash command (`/todo`, `/complete`, `/notify`, `/email`).
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
const COMMANDS: &[&str] = &["todo", "complete", "notify", "email", "note", "search_notes", "search_tasks", "list_todos", "list_notes", "spent", "earned"];

/// Commands handled directly in the bot (no `cmd-<name>` agent). Kept in sync
/// with the `match` in `dispatch_command`; used by `is_dispatchable` so a test
/// can prove every command the router may emit actually has a handler.
const DIRECT_COMMANDS: &[&str] = &[
    "buy_later", "groceries", "grocery_shopping", "clear_groceries", "to_buy", "clear_to_buy", "set_income", "income", "balance",
];

/// Can `dispatch_command` handle this command name?
fn is_dispatchable(cmd: &str) -> bool {
    DIRECT_COMMANDS.contains(&cmd) || COMMANDS.contains(&cmd)
}

/// Emitted by a command agent's Output when the LLM can't tell Sagemesh from
/// Personal; the bot turns it into a two-button prompt and re-runs with the answer.
const ASK_CATEGORY: &str = "ASK_CATEGORY";

/// An agent Output beginning with this marker is sent as a rich (HTML) message
/// via sendRichMessage instead of plain Markdown. The marker is stripped first.
const RICH_SENTINEL: &str = "<!rich>";

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
    active: Arc<Mutex<HashMap<i64, String>>>,
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

/// Per-chat running shopping lists (see `/buy_later`, `/groceries`). Persisted as
/// JSON next to the other bot state; no Notion/calendar involvement — these are
/// throwaway lists you add to and clear.
#[derive(Clone, Default, Serialize, Deserialize)]
struct ChatLists {
    #[serde(default)]
    groceries: Vec<String>,
    #[serde(default)]
    other: Vec<String>,
}

/// Which of a chat's two lists a command targets.
#[derive(Clone, Copy)]
enum ListKind {
    Grocery,
    Other,
}
impl ListKind {
    fn items<'a>(&self, l: &'a ChatLists) -> &'a Vec<String> {
        match self {
            ListKind::Grocery => &l.groceries,
            ListKind::Other => &l.other,
        }
    }
    fn items_mut<'a>(&self, l: &'a mut ChatLists) -> &'a mut Vec<String> {
        match self {
            ListKind::Grocery => &mut l.groceries,
            ListKind::Other => &mut l.other,
        }
    }
    fn noun(&self) -> &'static str {
        match self {
            ListKind::Grocery => "grocery",
            ListKind::Other => "to-buy",
        }
    }
    fn title(&self) -> &'static str {
        match self {
            ListKind::Grocery => "Groceries",
            ListKind::Other => "To buy",
        }
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
    // discoverable, so without this anyone could drive Notion/list commands.
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
    register_commands(&client, &api).await;
    let state = BotState {
        store,
        active: Arc::new(Mutex::new(HashMap::new())),
        pending: Arc::new(Mutex::new(HashMap::new())),
        db,
        allowed: Arc::new(allowed),
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

            let reply = handle_message(&state, chat_id, &text).await;
            send(&client, &api, chat_id, &reply).await;
        }
    }
}

async fn handle_message(state: &BotState, chat_id: i64, text: &str) -> Reply {
    if text.starts_with("/start") || text.starts_with("/agents") {
        // Rich HTML. Rich messages render like real HTML (newlines collapse), so
        // structure comes from block tags — <blockquote> and <ul>/<li> — not \n.
        // Underscores stay literal here, and Telegram auto-highlights the (now
        // valid) command tokens inside the list items.
        return agents_reply(
            &state.store,
            "<b>Optimimer</b>\
             <blockquote>Just tell me what you want in plain language — \"remind me to call Sam at 4pm\", \
             \"spent 20 on lunch\", \"what's my balance\" — and I'll route it to the right action. \
             The /commands below still work, or tap an agent to select it and send a message to run it.</blockquote>\
             <b>Capture</b>\
             <ul>\
             <li>/todo — add a calendar-synced task</li>\
             <li>/note — save a quick note</li>\
             <li>/complete — mark something done</li>\
             <li>/notify — schedule a reminder</li>\
             <li>/email — draft and send an email</li>\
             </ul>\
             <b>Search</b>\
             <ul>\
             <li>/search_notes — find notes</li>\
             <li>/search_tasks — find tasks</li>\
             <li>/list_todos — show the 5 newest tasks</li>\
             <li>/list_notes — show the 5 newest notes</li>\
             </ul>\
             <b>Money</b>\
             <ul>\
             <li>/spent — log an expense (text or a receipt photo)</li>\
             <li>/earned — log income received</li>\
             <li>/balance — income vs expenses (add 'last'/a number for past weeks, or a month like 'june')</li>\
             <li>/set_income — set your monthly income</li>\
             <li>send a .csv statement to bulk-import (duplicates skipped)</li>\
             </ul>\
             <b>Lists</b>\
             <ul>\
             <li>/buy_later — add an item (auto-sorts grocery vs other)</li>\
             <li>/groceries — show grocery list (text) · /clear_groceries — clear it</li>\
             <li>/grocery_shopping — interactive checklist, tap to cross off as you shop</li>\
             <li>/to_buy — show non-grocery list · /clear_to_buy — clear it</li>\
             </ul>",
            true,
        );
    }

    if let Some(arg) = text.strip_prefix("/use") {
        let q = arg.trim();
        return match resolve_agent(&state.store, q) {
            Some(wf) => {
                state.active.lock().unwrap().insert(chat_id, wf.id.clone());
                Reply::text(format!(
                    "Active agent: *{}*\nSend a message to run it.",
                    wf.name
                ))
            }
            None => agents_reply(&state.store, &format!("No agent matching '{q}'. Pick one:"), false),
        };
    }

    if text.starts_with("/where") {
        return Reply {
            text: format!(
                "Tap below to share your location — I'll set your timezone so reminders fire at the right time.\nCurrent timezone: *{}*",
                tz_for(state, chat_id)
            ),
            keyboard: Some(json!({
                "keyboard": [[{ "text": "Share location", "request_location": true }]],
                "resize_keyboard": true,
                "one_time_keyboard": true
            })),
            rich_html: None,
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
                Reply::text(format!("Timezone set to *{name}*."))
            }
            Err(_) => Reply::text(format!(
                "'{name}' isn't a valid IANA timezone (try e.g. `Europe/London`)."
            )),
        };
    }

    // Explicit slash command → direct handler or cmd-<name> agent.
    if let Some(rest) = text.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let body = parts.next().unwrap_or("").trim();

        // A bare agent command (no argument) gets a usage hint instead of running empty.
        if COMMANDS.contains(&cmd.as_str()) && body.is_empty() {
            return Reply::text(format!("Usage: `/{cmd} <what you want>`"));
        }
        if let Some(reply) = dispatch_command(state, chat_id, &cmd, body).await {
            return reply;
        }
        // Unknown slash command falls through to the natural-language path.
    }

    // An explicitly selected agent (via /use) owns plain text — power users can
    // still drive a hand-built agent directly without the router in the way.
    let explicit = state.active.lock().unwrap().get(&chat_id).cloned();
    if let Some(wf) = explicit.and_then(|id| state.store.get(&id)) {
        let result = engine::run(&wf, Value::String(text.to_string())).await;
        return Reply::text(format_result(&wf.name, &result));
    }

    // Experimental tool-calling agent (opt-in via AGENT_MODE): the model picks and
    // chains tools itself instead of routing to a single command.
    if agent_mode() {
        return run_agent(state, chat_id, text).await;
    }

    // Otherwise treat the message as natural language and route it to a command.
    if let Some(reply) = route_natural(state, chat_id, text).await {
        return reply;
    }

    Reply::text(
        "I couldn't tell what you wanted. Try phrasing it as an action — \
         \"remind me to call Sam at 4pm\", \"spent 20 on lunch\", \"what's my balance\" — \
         or use a /command. To run a custom agent, build one in the web UI and pick it with /use.",
    )
}

/// Dispatch a resolved command + argument to its handler — a direct handler
/// (shopping lists, balance, income) or a `cmd-<name>` agent. Returns `None`
/// when `cmd` is unknown so the caller can fall through. Shared by the
/// slash-command path and the natural-language router so both reach exactly the
/// same wiring.
async fn dispatch_command(state: &BotState, chat_id: i64, cmd: &str, body: &str) -> Option<Reply> {
    let reply = match cmd {
        // Local shopping lists (no agent, no Notion — just per-chat JSON state).
        "buy_later" => handle_buy_later(state, chat_id, body).await,
        "groceries" => list_reply(state, chat_id, ListKind::Grocery),
        "grocery_shopping" => handle_grocery_shopping(state, chat_id),
        "clear_groceries" => clear_reply(state, chat_id, ListKind::Grocery),
        "to_buy" => list_reply(state, chat_id, ListKind::Other),
        "clear_to_buy" => clear_reply(state, chat_id, ListKind::Other),
        "set_income" | "income" => handle_set_income(state, chat_id, body),
        "balance" => handle_balance(state, chat_id, body).await,
        // Money capture: parse via the cmd-<name> agent, then create + dedup in Rust.
        "spent" | "earned" => handle_money(state, chat_id, cmd, body).await,
        _ if COMMANDS.contains(&cmd) => run_command(state, chat_id, cmd, body, "").await,
        _ => return None,
    };
    Some(reply)
}

/// Route a free-text natural-language message to a command via the `cmd-route`
/// agent, then dispatch it. Returns `None` when routing is unavailable or the
/// model declines to pick a command (so the caller shows help / falls back).
async fn route_natural(state: &BotState, chat_id: i64, text: &str) -> Option<Reply> {
    let wf = state.store.get("cmd-route")?;
    let tz = tz_for(state, chat_id);
    let input = json!({
        "text": text,
        "now": now_in_tz(&tz),
        "tz": tz,
        "chat_id": chat_id.to_string(),
    });
    let result = engine::run(&wf, input).await;
    let route: Value = serde_json::from_str(&output_text(&result)?).ok()?;
    let cmd = route["command"].as_str().unwrap_or("").to_lowercase();
    let body = route["text"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(text)
        .to_string();

    // Understood the message, but no command clearly fits → offer to salvage it
    // rather than silently dropping it. (Distinct from the `None` we return when
    // routing itself is unavailable — that path shows the static help instead.)
    if cmd.is_empty() || cmd == "none" {
        state.pending.lock().unwrap().insert(
            chat_id,
            Pending { command: String::new(), text: text.to_string() },
        );
        return Some(salvage_prompt());
    }

    // Router named a command we don't actually wire (a model hallucination) →
    // salvage rather than dead-ending on "that agent isn't installed".
    if !is_dispatchable(&cmd) {
        state.pending.lock().unwrap().insert(
            chat_id,
            Pending { command: String::new(), text: text.to_string() },
        );
        return Some(salvage_prompt());
    }

    // A misheard word shouldn't wipe a list — confirm destructive actions that
    // arrived via fuzzy natural language. (Typed /clear_* still runs instantly.)
    if matches!(cmd.as_str(), "clear_groceries" | "clear_to_buy") {
        return Some(confirm_clear_prompt(&cmd));
    }

    let reply = dispatch_command(state, chat_id, &cmd, &body).await?;
    Some(with_breadcrumb(&cmd, reply))
}

/// Prepend a subtle "↳ /command" breadcrumb so a natural-language reply shows
/// which command the router chose — a misroute is then visible at a glance.
/// Only applied on the routed path; typed slash commands are self-evident.
fn with_breadcrumb(cmd: &str, mut reply: Reply) -> Reply {
    reply.text = format!("↳ /{cmd}\n{}", reply.text);
    if let Some(html) = reply.rich_html.take() {
        reply.rich_html = Some(format!("<blockquote>↳ /{cmd}</blockquote>{html}"));
    }
    reply
}

/// Buttons offered when the router can't confidently place a message, so an
/// unclear capture is salvaged into a note/task instead of lost. The original
/// text is stashed in `pending` for the callback to consume.
fn salvage_prompt() -> Reply {
    Reply {
        text: "I wasn't sure which action you meant — save it as…?".into(),
        keyboard: Some(json!({ "inline_keyboard": [[
            { "text": "📝 Note", "callback_data": "salvage:note" },
            { "text": "✅ Task", "callback_data": "salvage:todo" },
            { "text": "Ignore", "callback_data": "salvage:ignore" }
        ]]})),
        rich_html: None,
    }
}

/// Yes/Cancel confirmation before a natural-language request clears a list.
fn confirm_clear_prompt(cmd: &str) -> Reply {
    let label = if cmd == "clear_groceries" { "grocery" } else { "to-buy" };
    Reply {
        text: format!("Clear your {label} list? This can't be undone."),
        keyboard: Some(json!({ "inline_keyboard": [[
            { "text": "Yes, clear it", "callback_data": format!("confirm:{cmd}") },
            { "text": "Cancel", "callback_data": "confirm:cancel" }
        ]]})),
        rich_html: None,
    }
}

/// The two category buttons shown when a command needs a "which bucket?" answer.
/// Defaults are this project's own labels — override `CATEGORY_A` / `CATEGORY_B`
/// in the env to adapt them to your domain (e.g. "Work" / "Home") without touching
/// code. This is just the button text; a command agent's Output emits
/// `ASK_CATEGORY` to trigger this prompt, and that agent is where the answer is
/// mapped onto your Notion schema — fork it to change the mapping.
fn category_labels() -> (String, String) {
    (
        std::env::var("CATEGORY_A").unwrap_or_else(|_| "Sagemesh".to_string()),
        std::env::var("CATEGORY_B").unwrap_or_else(|_| "Personal".to_string()),
    )
}

/// Run a `cmd-<name>` agent with structured input. Handles the category
/// confirmation: if the agent asks, stash the request and show two buttons.
async fn run_command(
    state: &BotState,
    chat_id: i64,
    cmd: &str,
    text: &str,
    category: &str,
) -> Reply {
    let wf = match state.store.get(&format!("cmd-{cmd}")) {
        Some(w) => w,
        None => return Reply::text(format!("The `/{cmd}` agent isn't installed (expected agent id `cmd-{cmd}`). Restart the backend to seed it, or build it in the web UI.")),
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
        state.pending.lock().unwrap().insert(
            chat_id,
            Pending {
                command: cmd.to_string(),
                text: text.to_string(),
            },
        );
        let (cat_a, cat_b) = category_labels();
        return Reply {
            text: format!("Is this for *{cat_a}* or *{cat_b}*?"),
            keyboard: Some(json!({
                "inline_keyboard": [[
                    { "text": cat_a.clone(), "callback_data": "cat:sagemesh" },
                    { "text": cat_b.clone(), "callback_data": "cat:personal" }
                ]]
            })),
            rich_html: None,
        };
    }

    // An agent can opt into a rich (HTML) reply by prefixing its Output with the
    // sentinel; it then owns the full formatting (tables, collapsible blocks, …).
    if let Some(html) = body.trim_start().strip_prefix(RICH_SENTINEL) {
        let html = html.trim_start().to_string();
        let fallback = strip_tags(&html);
        return Reply::rich(html, fallback);
    }

    Reply::text(format_result(&wf.name, &result))
}

/// Capture a hand-logged transaction (`/spent`, `/earned`, or a receipt photo).
/// The `cmd-<name>` agent only PARSES the text into JSON; the Notion create plus
/// dedup (reject an identical fingerprint, flag a same-amount near-duplicate) is
/// done in `finance::log_manual` so manual and CSV-imported rows stay consistent.
async fn handle_money(state: &BotState, chat_id: i64, cmd: &str, body: &str) -> Reply {
    if body.trim().is_empty() {
        return Reply::text(format!("Usage: `/{cmd} <amount and what it was for>`"));
    }
    let wf = match state.store.get(&format!("cmd-{cmd}")) {
        Some(w) => w,
        None => return Reply::text(format!("The `/{cmd}` parser isn't installed (expected agent id `cmd-{cmd}`). Restart the backend to re-seed it.")),
    };
    let tz = tz_for(state, chat_id);
    let now = now_in_tz(&tz);
    let input = json!({
        "text": body,
        "now": now,
        "tz": tz,
        "chat_id": chat_id.to_string(),
        "command": cmd,
    });
    let result = engine::run(&wf, input).await;
    let parsed = output_text(&result).unwrap_or_default();
    let kind = if cmd == "earned" {
        crate::finance::ManualKind::Earned
    } else {
        crate::finance::ManualKind::Spent
    };
    let today = now.get(..10).unwrap_or(&now);
    match crate::finance::log_manual(&parsed, kind, today).await {
        Ok(logged) => match logged.html {
            Some(html) => Reply::rich(html, logged.text),
            None => Reply::text(logged.text),
        },
        Err(e) => Reply::text(format!("Couldn't log that: {e}")),
    }
}

// ---- Agent mode: tool-calling loop ------------------------------------------

/// Is the experimental tool-calling agent loop enabled? Off unless AGENT_MODE is
/// truthy, so it never disturbs the default natural-language routing.
fn agent_mode() -> bool {
    matches!(
        std::env::var("AGENT_MODE").unwrap_or_default().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn agent_model() -> String {
    std::env::var("AGENT_MODEL").unwrap_or_else(|_| "anthropic/claude-sonnet-4.6".to_string())
}

/// Hard cap on tool round-trips per message, so a confused model can't loop forever.
const AGENT_MAX_STEPS: usize = 6;

fn tool_def(name: &str, description: &str, parameters: Value) -> Value {
    json!({ "type": "function", "function": { "name": name, "description": description, "parameters": parameters } })
}

/// OpenAI-style schemas for everything the agent can do. Each name is handled in
/// `exec_tool`, which dispatches to the same capabilities the slash commands use.
fn agent_tools() -> Value {
    json!([
        tool_def("log_expense", "Record money the user spent (an expense).", json!({
            "type": "object",
            "properties": {
                "amount": { "type": "number", "description": "Positive amount spent." },
                "merchant": { "type": "string", "description": "Store / payee, e.g. 'Ali Baba Shawarma'." },
                "category": { "type": "string", "description": "One of: Groceries, Dining, Transport, Housing, Utilities, Health, Entertainment, Shopping, Subscriptions, Travel, Loans, Cash, Other." },
                "date": { "type": "string", "description": "YYYY-MM-DD; omit for today." }
            },
            "required": ["amount", "merchant"]
        })),
        tool_def("log_income", "Record money the user received (income).", json!({
            "type": "object",
            "properties": {
                "amount": { "type": "number" },
                "source": { "type": "string", "description": "Where it came from, e.g. 'Paycheck'." },
                "category": { "type": "string", "description": "'Salary' for regular wages/payroll, otherwise 'Income'." },
                "date": { "type": "string", "description": "YYYY-MM-DD; omit for today." }
            },
            "required": ["amount", "source"]
        })),
        tool_def("get_balance", "Show income vs expenses for a week or a month.", json!({
            "type": "object",
            "properties": { "period": { "type": "string", "description": "'' = this week, 'last', a number of weeks ago, or a month like 'june' / 'may 2025'." } }
        })),
        tool_def("add_shopping_item", "Add an item to the shopping list (auto-sorted into groceries vs other).", json!({
            "type": "object",
            "properties": { "item": { "type": "string" } },
            "required": ["item"]
        })),
        tool_def("show_shopping_list", "Show a shopping list.", json!({
            "type": "object",
            "properties": { "which": { "type": "string", "enum": ["groceries", "to_buy"] } }
        })),
        tool_def("save_note", "Save a free-form note for later.", json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })),
        tool_def("create_task", "Create a calendar-synced task, meeting, or appointment.", json!({
            "type": "object",
            "properties": { "text": { "type": "string", "description": "The task in natural language, including any time/date." } },
            "required": ["text"]
        })),
        tool_def("set_reminder", "Schedule a Telegram reminder ping at a future time.", json!({
            "type": "object",
            "properties": { "text": { "type": "string", "description": "What to be reminded of and when, e.g. 'pay rent tomorrow 9am'." } },
            "required": ["text"]
        }))
    ])
}

/// One tool invocation → a short text observation fed back to the model. Reuses
/// the very same handlers the slash commands and router call, so behaviour
/// (dedup, list sorting, Notion writes) stays identical across entry points.
async fn exec_tool(state: &BotState, chat_id: i64, name: &str, args: &Value) -> String {
    let tz = tz_for(state, chat_id);
    let today: String = now_in_tz(&tz).chars().take(10).collect();
    let str_arg = |k: &str| args[k].as_str().unwrap_or("").to_string();
    match name {
        "log_expense" => {
            let entry = json!({ "amount": args["amount"], "merchant": args["merchant"], "category": args["category"], "date": args["date"] });
            match crate::finance::log_manual(&entry.to_string(), crate::finance::ManualKind::Spent, &today).await {
                Ok(l) => l.text,
                Err(e) => format!("Failed to log expense: {e}"),
            }
        }
        "log_income" => {
            let entry = json!({ "amount": args["amount"], "source": args["source"], "category": args["category"], "date": args["date"] });
            match crate::finance::log_manual(&entry.to_string(), crate::finance::ManualKind::Earned, &today).await {
                Ok(l) => l.text,
                Err(e) => format!("Failed to log income: {e}"),
            }
        }
        "get_balance" => handle_balance(state, chat_id, &str_arg("period")).await.text,
        "add_shopping_item" => handle_buy_later(state, chat_id, &str_arg("item")).await.text,
        "show_shopping_list" => {
            let kind = if args["which"].as_str() == Some("to_buy") { ListKind::Other } else { ListKind::Grocery };
            list_reply(state, chat_id, kind).text
        }
        "save_note" => run_command(state, chat_id, "note", &str_arg("text"), "").await.text,
        "create_task" => run_command(state, chat_id, "todo", &str_arg("text"), "").await.text,
        "set_reminder" => run_command(state, chat_id, "notify", &str_arg("text"), "").await.text,
        other => format!("(no such tool: {other})"),
    }
}

/// Run one user message through the tool-calling loop: the model decides which
/// tool(s) to call (it may chain several), we execute each and feed results
/// back, and its first tool-free turn is the reply.
async fn run_agent(state: &BotState, chat_id: i64, user_text: &str) -> Reply {
    let tz = tz_for(state, chat_id);
    let now = now_in_tz(&tz);
    let tools = agent_tools();
    let system = format!(
        "You are Optimimer, a personal assistant for ONE user over Telegram. Now: {now} ({tz}). \
         Use the provided tools to act on requests: logging expenses/income, checking finances, \
         managing the shopping list, saving notes, creating tasks, and setting reminders. Call \
         several tools in one turn when the user asks for several things. Extract concrete values \
         (amounts, merchants, dates) yourself instead of asking back. After the tools run, reply in \
         one or two short plain-text sentences confirming what you did. If no tool fits (small talk \
         or a general question), just answer briefly. Never fabricate tool results."
    );
    let mut messages = vec![
        json!({ "role": "system", "content": system }),
        json!({ "role": "user", "content": user_text }),
    ];

    for _ in 0..AGENT_MAX_STEPS {
        let msg = match crate::openrouter::chat_tools(&agent_model(), &messages, &tools).await {
            Ok(m) => m,
            Err(e) => return Reply::text(format!("Agent error: {e}")),
        };
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() {
            let content = msg["content"].as_str().unwrap_or("").trim();
            return Reply::text(if content.is_empty() { "Done." } else { content });
        }
        messages.push(msg.clone());
        for call in calls {
            let id = call["id"].as_str().unwrap_or("").to_string();
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let cargs: Value = call["function"]["arguments"]
                .as_str()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
            let observation = exec_tool(state, chat_id, &name, &cargs).await;
            messages.push(json!({ "role": "tool", "tool_call_id": id, "content": observation }));
        }
    }
    Reply::text("I couldn't finish that within a few steps — try breaking it into simpler requests.")
}

/// Crude HTML→plain-text for the rich-message fallback: drop tags, unescape the
/// entities we emit. Good enough for the rare older-client / API-error path.
fn strip_tags(html: &str) -> String {
    // Turn block boundaries into newlines so the fallback stays readable.
    let html = html
        .replace("</li>", "\n")
        .replace("</tr>", "\n")
        .replace("</blockquote>", "\n")
        .replace("</ul>", "\n")
        .replace("</table>", "\n");
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
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
            send(
                client,
                api,
                chat,
                &Reply::text(format!(
                    "Active agent: *{}*\nSend a message to run it.",
                    wf.name
                )),
            )
            .await;
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
            send(
                client,
                api,
                chat,
                &Reply::text("That prompt expired — send the command again."),
            )
            .await;
        }
    }

    // Confirm a destructive action (list clear) reached via natural language.
    if let (Some(rest), Some(chat)) = (data.strip_prefix("confirm:"), chat_id) {
        if rest == "cancel" {
            note = "Cancelled".into();
            send(client, api, chat, &Reply::text("Cancelled — nothing was changed.")).await;
        } else if let Some(reply) = dispatch_command(state, chat, rest, "").await {
            note = "Done".into();
            send(client, api, chat, &reply).await;
        }
    }

    // Salvage a message the router couldn't confidently place into note/task.
    if let (Some(action), Some(chat)) = (data.strip_prefix("salvage:"), chat_id) {
        let pending = state.pending.lock().unwrap().remove(&chat);
        if action == "ignore" {
            note = "Ignored".into();
            send(client, api, chat, &Reply::text("OK — ignored.")).await;
        } else if let Some(p) = pending {
            note = format!("Saved via /{action}");
            if let Some(reply) = dispatch_command(state, chat, action, &p.text).await {
                send(client, api, chat, &with_breadcrumb(action, reply)).await;
            }
        } else {
            send(client, api, chat, &Reply::text("That prompt expired — send it again.")).await;
        }
    }

    // Grocery checklist: toggle the tapped item's strike-through in place.
    if let (true, Some(chat)) = (data.starts_with("shop:"), chat_id) {
        let msg_id = cb["message"]["message_id"].as_i64();
        let board = &cb["message"]["reply_markup"]["inline_keyboard"];
        if let (Some(mid), Some(rows)) = (msg_id, board.as_array()) {
            let new_board: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let btns: Vec<Value> = row
                        .as_array()
                        .map(|r| r.as_slice())
                        .unwrap_or(&[])
                        .iter()
                        .map(|btn| {
                            let cd = btn["callback_data"].as_str().unwrap_or("");
                            let txt = btn["text"].as_str().unwrap_or("");
                            // Only the tapped button flips; the rest pass through.
                            let text = if cd != data {
                                txt.to_string()
                            } else if let Some(rest) = txt.strip_prefix("✅ ") {
                                format!("▫️ {}", unstrike(rest))
                            } else if let Some(rest) = txt.strip_prefix("▫️ ") {
                                format!("✅ {}", strike(rest))
                            } else {
                                txt.to_string()
                            };
                            json!({ "text": text, "callback_data": cd })
                        })
                        .collect();
                    json!(btns)
                })
                .collect();
            let _ = client
                .post(format!("{api}/editMessageReplyMarkup"))
                .json(&json!({
                    "chat_id": chat,
                    "message_id": mid,
                    "reply_markup": { "inline_keyboard": new_board }
                }))
                .send()
                .await;
            note = "✓".into();
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
async fn handle_voice(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    state: &BotState,
    chat_id: i64,
    file_id: &str,
) {
    let file_path = match get_file_path(client, api, file_id).await {
        Some(p) => p,
        None => {
            return send(
                client,
                api,
                chat_id,
                &Reply::text("Couldn't fetch that voice message."),
            )
            .await
        }
    };
    let url = format!("https://api.telegram.org/file/bot{token}/{file_path}");
    let audio = match client.get(&url).send().await.ok() {
        Some(r) => match r.bytes().await {
            Ok(b) => b.to_vec(),
            Err(_) => {
                return send(
                    client,
                    api,
                    chat_id,
                    &Reply::text("Couldn't download the audio."),
                )
                .await
            }
        },
        None => {
            return send(
                client,
                api,
                chat_id,
                &Reply::text("Couldn't download the audio."),
            )
            .await
        }
    };
    let format = file_path
        .rsplit('.')
        .next()
        .filter(|e| !e.is_empty())
        .unwrap_or("ogg")
        .to_lowercase();

    let transcript = match crate::transcribe::transcribe(audio, &format).await {
        Ok(t) => t,
        Err(e) => {
            return send(
                client,
                api,
                chat_id,
                &Reply::text(format!("Transcription failed: {e}")),
            )
            .await
        }
    };
    if transcript.trim().is_empty() {
        return send(
            client,
            api,
            chat_id,
            &Reply::text("I didn't catch that — try again?"),
        )
        .await;
    }
    send(
        client,
        api,
        chat_id,
        &Reply::text(format!("_{transcript}_")),
    )
    .await;

    // Route the transcript to a command exactly like a typed message; fall back
    // to the normal message path (active agent / help) if nothing matched.
    if let Some(reply) = route_natural(state, chat_id, &transcript).await {
        return send(client, api, chat_id, &reply).await;
    }
    let reply = handle_message(state, chat_id, &transcript).await;
    send(client, api, chat_id, &reply).await;
}

/// Download a Telegram file's bytes by file_id (getFile → download URL).
async fn download_file(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    file_id: &str,
) -> Option<Vec<u8>> {
    let file_path = get_file_path(client, api, file_id).await?;
    let url = format!("https://api.telegram.org/file/bot{token}/{file_path}");
    let bytes = client.get(&url).send().await.ok()?.bytes().await.ok()?;
    Some(bytes.to_vec())
}

/// Receipt/invoice image → OCR to a "Spent X at Y on Z" line → run `/spent`,
/// which parses + categorizes it like any typed expense.
async fn handle_receipt(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    state: &BotState,
    chat_id: i64,
    file_id: &str,
    mime: &str,
) {
    let bytes = match download_file(client, api, token, file_id).await {
        Some(b) => b,
        None => return send(client, api, chat_id, &Reply::text("Couldn't download that image.")).await,
    };
    let sentence = match crate::vision::read_receipt(bytes, mime).await {
        Ok(s) => s,
        Err(e) => return send(client, api, chat_id, &Reply::text(format!("Couldn't read the receipt: {e}"))).await,
    };
    send(client, api, chat_id, &Reply::text(format!("_{sentence}_"))).await;
    let reply = handle_money(state, chat_id, "spent", &sentence).await;
    send(client, api, chat_id, &reply).await;
}

/// CSV bank/credit-card statement → bulk import into Finances, skipping rows whose
/// fingerprint already exists (so re-imports don't duplicate).
async fn handle_csv(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    state: &BotState,
    chat_id: i64,
    file_id: &str,
    file_name: &str,
    caption: &str,
) {
    let bytes = match download_file(client, api, token, file_id).await {
        Some(b) => b,
        None => return send(client, api, chat_id, &Reply::text("Couldn't download that file.")).await,
    };
    let csv = String::from_utf8_lossy(&bytes).to_string();
    // Detect account type (Amex 'activity' = credit; BMO 'statement' = credit or
    // chequing, told apart by the header) so credits are read correctly.
    let account = crate::finance::detect_account(file_name, caption, &csv);
    let label = if account.is_empty() { "auto-detecting type".to_string() } else { format!("{account} statement") };
    send(client, api, chat_id, &Reply::text(format!("Importing transactions ({label})…"))).await;
    let tz = tz_for(state, chat_id);
    let today: String = now_in_tz(&tz).chars().take(10).collect();
    let reply = match crate::finance::import_csv(&csv, &today, account).await {
        Ok(s) if s.parsed == 0 => Reply::text("No transactions found in that CSV."),
        Ok(s) => {
            let mut msg = format!(
                "Imported *{}* transaction{} — skipped *{}* duplicate{} ({} parsed).",
                s.created,
                if s.created == 1 { "" } else { "s" },
                s.skipped,
                if s.skipped == 1 { "" } else { "s" },
                s.parsed,
            );
            if s.transfers > 0 {
                msg.push_str(&format!(
                    "\n{} card payment/transfer{} logged but excluded from /balance.",
                    s.transfers,
                    if s.transfers == 1 { "" } else { "s" }
                ));
            }
            if s.flagged > 0 {
                msg.push_str(&format!(
                    "\n⚠️ {} row{} flagged as a *likely duplicate* (same amount as an existing transaction) — check its Note in Notion and delete if redundant.",
                    s.flagged,
                    if s.flagged == 1 { "" } else { "s" }
                ));
            }
            Reply::text(msg)
        }
        Err(e) => Reply::text(format!("CSV import failed: {e}")),
    };
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
            Reply::text(format!(
                "Timezone set to *{tz}* — reminders will use this. Local time is now {}.",
                now_in_tz(&tz)
            ))
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
    let conn = state.db.lock();
    conn.query_row("SELECT tz FROM chat_tz WHERE chat_id = ?1", params![chat_id], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .unwrap_or_else(default_tz)
}

fn set_tz(state: &BotState, chat_id: i64, tz: &str) {
    let conn = state.db.lock();
    let _ = conn.execute(
        "INSERT INTO chat_tz (chat_id, tz) VALUES (?1, ?2)
         ON CONFLICT(chat_id) DO UPDATE SET tz = excluded.tz",
        params![chat_id, tz],
    );
}

/// A chat's configured monthly income (0 if unset).
fn monthly_income(state: &BotState, chat_id: i64) -> f64 {
    let conn = state.db.lock();
    conn.query_row(
        "SELECT monthly_income FROM chat_finance WHERE chat_id = ?1",
        params![chat_id],
        |r| r.get::<_, f64>(0),
    )
    .ok()
    .unwrap_or(0.0)
}

fn set_monthly_income(state: &BotState, chat_id: i64, amount: f64) {
    let conn = state.db.lock();
    let _ = conn.execute(
        "INSERT INTO chat_finance (chat_id, monthly_income) VALUES (?1, ?2)
         ON CONFLICT(chat_id) DO UPDATE SET monthly_income = excluded.monthly_income",
        params![chat_id, amount],
    );
}

/// `/set_income 5000` (or `/income` to show the current value). Drives the salary
/// slice in `/balance`.
fn handle_set_income(state: &BotState, chat_id: i64, body: &str) -> Reply {
    let raw = body.trim().trim_start_matches('$').replace(',', "");
    if raw.is_empty() {
        let cur = monthly_income(state, chat_id);
        if cur <= 0.0 {
            return Reply::text("No monthly income set yet. Set it with `/set_income 5000`.");
        }
        return Reply::text(format!(
            "Monthly income is *${cur:.2}* (≈ ${:.2}/week).\nChange it with `/set_income <amount>`.",
            cur / 4.348
        ));
    }
    match raw.parse::<f64>() {
        Ok(amount) if amount >= 0.0 => {
            set_monthly_income(state, chat_id, amount);
            Reply::text(format!(
                "Monthly income set to *${amount:.2}* (≈ ${:.2}/week). Used in /balance.\nSalary deposits in imported CSVs won't be double-counted while this is set. Use `/set_income 0` to count actual paychecks instead.",
                amount / 4.348
            ))
        }
        _ => Reply::text("That isn't a number. Try `/set_income 5000`."),
    }
}

/// `/balance` — income vs expenses for a week or a month (exact math in
/// `finance.rs`). Bare `/balance` = this week; `/balance last` / `/balance 2` =
/// past weeks; `/balance june` / `/balance may 2025` / `/balance this month` /
/// `/balance last month` = a calendar month.
async fn handle_balance(state: &BotState, chat_id: i64, body: &str) -> Reply {
    let tz = tz_for(state, chat_id);
    let income = monthly_income(state, chat_id);
    let result = if let Some((month, year)) = parse_month(body, &tz) {
        crate::finance::monthly_balance(&tz, income, month, year).await
    } else {
        crate::finance::weekly_balance(&tz, income, parse_weeks_ago(body)).await
    };
    match result {
        Ok(md) => Reply::text(md),
        Err(e) => Reply::text(format!("Couldn't compute the balance: {e}")),
    }
}

/// Parse a `/balance` argument naming a month → (month 1-12, year). Handles
/// "this month", "last month", a month name ("june"/"jun"), and an optional
/// explicit 4-digit year; with no year, a month after the current one is assumed
/// to mean last year (e.g. asking for "december" in June → last December).
fn parse_month(body: &str, tz_name: &str) -> Option<(u32, i32)> {
    use chrono::Datelike;
    let b = body.trim().to_lowercase();
    if b.is_empty() {
        return None;
    }
    let tz: chrono_tz::Tz = tz_name.parse().unwrap_or(chrono_tz::UTC);
    let now = Utc::now().with_timezone(&tz);
    let (cur_y, cur_m) = (now.year(), now.month());

    if b.contains("this month") {
        return Some((cur_m, cur_y));
    }
    if b.contains("last month") || b.contains("previous month") {
        return Some(if cur_m == 1 { (12, cur_y - 1) } else { (cur_m - 1, cur_y) });
    }
    let abbr = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let month = abbr.iter().position(|a| b.contains(a)).map(|i| i as u32 + 1)?;
    let year = b
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<i32>().ok())
        .find(|y| (2000..3000).contains(y))
        .unwrap_or(if month <= cur_m { cur_y } else { cur_y - 1 });
    Some((month, year))
}

/// Parse a `/balance` argument into a week offset: "" / "this" → 0; a number → that
/// many weeks back; "last"/"previous" → 1.
fn parse_weeks_ago(body: &str) -> i64 {
    let b = body.trim().to_lowercase();
    if b.is_empty() || b.starts_with("this") {
        return 0;
    }
    if let Some(n) = b
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
    {
        return n.max(0);
    }
    if b.contains("last") || b.contains("prev") {
        return 1;
    }
    0
}

/// Read a chat's two shopping lists (empty if none stored yet).
fn get_lists(state: &BotState, chat_id: i64) -> ChatLists {
    let conn = state.db.lock();
    conn.query_row("SELECT json FROM chat_lists WHERE chat_id = ?1", params![chat_id], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|j| serde_json::from_str(&j).ok())
    .unwrap_or_default()
}

fn put_lists(state: &BotState, chat_id: i64, lists: &ChatLists) {
    if let Ok(json) = serde_json::to_string(lists) {
        let conn = state.db.lock();
        let _ = conn.execute(
            "INSERT INTO chat_lists (chat_id, json) VALUES (?1, ?2)
             ON CONFLICT(chat_id) DO UPDATE SET json = excluded.json",
            params![chat_id, json],
        );
    }
}

/// One-time import of legacy `chat_tz.json` / `lists.json` when those tables are
/// still empty. Files are left in place as a backup.
pub fn migrate_json(db: &Db, tz_path: &Path, lists_path: &Path) {
    if db.is_empty("chat_tz") && tz_path.exists() {
        if let Some(map) = std::fs::read_to_string(tz_path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<i64, String>>(&s).ok())
        {
            let conn = db.lock();
            for (chat_id, tz) in &map {
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO chat_tz (chat_id, tz) VALUES (?1, ?2)",
                    params![chat_id, tz],
                );
            }
            tracing::info!("migrated {} timezone(s) into sqlite", map.len());
        }
    }
    if db.is_empty("chat_lists") && lists_path.exists() {
        if let Some(map) = std::fs::read_to_string(lists_path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<i64, ChatLists>>(&s).ok())
        {
            let conn = db.lock();
            for (chat_id, lists) in &map {
                if let Ok(json) = serde_json::to_string(lists) {
                    let _ = conn.execute(
                        "INSERT OR REPLACE INTO chat_lists (chat_id, json) VALUES (?1, ?2)",
                        params![chat_id, json],
                    );
                }
            }
            tracing::info!("migrated {} shopping list(s) into sqlite", map.len());
        }
    }
}

/// `/buy_later <stuff>` — split the input into items, classify each as a grocery
/// or not, and append to the matching per-chat list.
async fn handle_buy_later(state: &BotState, chat_id: i64, body: &str) -> Reply {
    if body.is_empty() {
        return Reply::text("Usage: `/buy_later milk, eggs, usb cable`");
    }
    let (groceries, other) = classify_items(body).await;
    let mut lists = get_lists(state, chat_id);
    lists.groceries.extend(groceries.iter().cloned());
    lists.other.extend(other.iter().cloned());
    put_lists(state, chat_id, &lists);

    let mut lines = Vec::new();
    if !groceries.is_empty() {
        lines.push(format!("Added to groceries: {}", groceries.join(", ")));
    }
    if !other.is_empty() {
        lines.push(format!("Added to to-buy: {}", other.join(", ")));
    }
    Reply::text(lines.join("\n"))
}

/// Ask a cheap model to split `text` into individual items and sort them into
/// (groceries, other). Falls back to treating the whole input as one grocery
/// line if the model is unavailable or returns something unparseable.
async fn classify_items(text: &str) -> (Vec<String>, Vec<String>) {
    let system = "Split the shopping input into individual items and classify each as a grocery \
        (food, drinks, produce, pantry staples, and household consumables like paper towels, dish \
        soap, toilet paper) or other (anything non-grocery: electronics, clothing, tools, gifts, \
        furniture, etc.). Output ONLY minified JSON: {\"groceries\":[...],\"other\":[...]}. Each item \
        a short lowercase phrase. No prose, no code fences.";
    let raw = crate::openrouter::chat("anthropic/claude-haiku-4.5", system, text)
        .await
        .unwrap_or_default();
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        let g = str_vec(&v["groceries"]);
        let o = str_vec(&v["other"]);
        if !g.is_empty() || !o.is_empty() {
            return (g, o);
        }
    }
    // Couldn't classify — keep the item rather than dropping it.
    (vec![text.trim().to_string()], vec![])
}

fn str_vec(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A plain-text list — deliberately NOT a rich card, so calling `/groceries`
/// repeatedly doesn't clutter the chat with big boxes. For an interactive,
/// tap-to-check version use `/grocery_shopping`.
fn list_reply(state: &BotState, chat_id: i64, kind: ListKind) -> Reply {
    let lists = get_lists(state, chat_id);
    let items = kind.items(&lists);
    if items.is_empty() {
        return Reply::text(format!("Your {} list is empty.", kind.noun()));
    }
    let mut text = format!("*{}* ({})\n", kind.title(), items.len());
    for (i, it) in items.iter().enumerate() {
        text.push_str(&format!("{}. {}\n", i + 1, it));
    }
    Reply::text(text.trim_end().to_string())
}

/// `/grocery_shopping` — an interactive checklist: one inline button per grocery
/// item; tapping toggles a strike-through so you can tick things off while you
/// shop without spamming the chat. State lives in the message's own keyboard
/// (see the `shop:` branch in `handle_callback`), so no extra storage is needed.
fn handle_grocery_shopping(state: &BotState, chat_id: i64) -> Reply {
    let lists = get_lists(state, chat_id);
    let items = &lists.groceries;
    if items.is_empty() {
        return Reply::text("Your grocery list is empty. Add items with /buy_later first.");
    }
    let rows: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(i, it)| json!([{ "text": format!("▫️ {it}"), "callback_data": format!("shop:{i}") }]))
        .collect();
    Reply {
        text: format!("🛒 Shopping list ({}) — tap an item to check it off:", items.len()),
        keyboard: Some(json!({ "inline_keyboard": rows })),
        rich_html: None,
    }
}

/// Overlay each character with a combining strike (U+0336) so a checklist item
/// reads as visibly crossed-out inside an inline-keyboard button (which can't
/// carry rich formatting). `unstrike` reverses it.
fn strike(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        out.push(c);
        out.push('\u{0336}');
    }
    out
}
fn unstrike(s: &str) -> String {
    s.chars().filter(|c| *c != '\u{0336}').collect()
}

fn clear_reply(state: &BotState, chat_id: i64, kind: ListKind) -> Reply {
    let mut lists = get_lists(state, chat_id);
    let v = kind.items_mut(&mut lists);
    let n = v.len();
    v.clear();
    if n > 0 {
        put_lists(state, chat_id, &lists);
    }
    if n == 0 {
        Reply::text(format!("Your {} list was already empty.", kind.noun()))
    } else {
        Reply::text(format!(
            "Cleared {} list ({n} item{}).",
            kind.noun(),
            if n == 1 { "" } else { "s" }
        ))
    }
}

/// Human + machine friendly "now" string for the LLM to resolve relative dates.
/// e.g. "2026-06-22 15:30 -04:00 (Sunday)". Falls back to UTC on a bad tz.
fn now_in_tz(tz_name: &str) -> String {
    match tz_name.parse::<chrono_tz::Tz>() {
        Ok(tz) => Utc::now()
            .with_timezone(&tz)
            .format("%Y-%m-%d %H:%M %:z (%A)")
            .to_string(),
        Err(_) => Utc::now().format("%Y-%m-%d %H:%M +00:00 (%A)").to_string(),
    }
}

// ---- shared helpers (unchanged behaviour) ------------------------------------

/// Build a reply listing agents as inline buttons (one per row). Command agents
/// (`cmd-*`) are hidden — they're driven by slash commands, not selection.
fn agents_reply(store: &Store, header: &str, rich: bool) -> Reply {
    let list: Vec<Workflow> = store
        .list()
        .into_iter()
        .filter(|w| !w.id.starts_with("cmd-"))
        .collect();
    if list.is_empty() {
        let full = format!("{header}\n\nNo new agents saved yet — build one in the web UI first.");
        return if rich {
            let fallback = strip_tags(&full);
            Reply::rich(full, fallback)
        } else {
            Reply::text(full)
        };
    }
    let rows: Vec<Value> = list
        .iter()
        .map(|w| json!([{ "text": w.name, "callback_data": format!("use:{}", w.id) }]))
        .collect();
    let keyboard = Some(json!({ "inline_keyboard": rows }));
    if rich {
        Reply {
            text: strip_tags(header),
            keyboard,
            rich_html: Some(header.to_string()),
        }
    } else {
        Reply {
            text: header.to_string(),
            keyboard,
            rich_html: None,
        }
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
        let err = result
            .results
            .iter()
            .find_map(|r| r.error.clone())
            .unwrap_or_default();
        return format!("*{name}* finished with an error:\n{err}");
    }
    let body = output_text(result)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no output)".to_string());
    format!("*{name}*\n\n{body}")
}

async fn get_updates(
    client: &reqwest::Client,
    api: &str,
    offset: i64,
) -> anyhow::Result<Vec<Value>> {
    let resp: Value = client
        .get(format!("{api}/getUpdates"))
        .query(&[
            ("offset", offset.to_string()),
            ("timeout", "30".to_string()),
            (
                "allowed_updates",
                "[\"message\",\"callback_query\"]".to_string(),
            ),
        ])
        .timeout(std::time::Duration::from_secs(40))
        .send()
        .await?
        .json()
        .await?;
    Ok(resp["result"].as_array().cloned().unwrap_or_default())
}

/// Register the command list with Telegram (setMyCommands) so the "/" menu and
/// autocomplete show them with descriptions. Names must be [a-z0-9_]; the
/// underscores (not hyphens) keep multi-word commands fully tappable.
async fn register_commands(client: &reqwest::Client, api: &str) {
    let commands = json!({
        "commands": [
            { "command": "todo",            "description": "Add a calendar-synced task" },
            { "command": "note",            "description": "Save a quick note" },
            { "command": "complete",        "description": "Mark something done" },
            { "command": "notify",          "description": "Schedule a reminder" },
            { "command": "email",           "description": "Draft and send an email" },
            { "command": "search_notes",    "description": "Find notes" },
            { "command": "search_tasks",    "description": "Find tasks" },
            { "command": "list_todos",      "description": "Show the 5 newest tasks" },
            { "command": "list_notes",      "description": "Show the 5 newest notes" },
            { "command": "spent",           "description": "Log an expense (text or receipt photo)" },
            { "command": "earned",          "description": "Log income received" },
            { "command": "balance",         "description": "This week's income vs expenses" },
            { "command": "set_income",      "description": "Set your monthly income" },
            { "command": "buy_later",       "description": "Add an item (auto-sorts grocery vs other)" },
            { "command": "groceries",       "description": "Show the grocery list (text)" },
            { "command": "grocery_shopping","description": "Interactive checklist — tap to cross off" },
            { "command": "clear_groceries", "description": "Clear the grocery list" },
            { "command": "to_buy",          "description": "Show the non-grocery to-buy list" },
            { "command": "clear_to_buy",    "description": "Clear the to-buy list" },
            { "command": "where",           "description": "Share location to set your timezone" }
        ]
    });
    match client.post(format!("{api}/setMyCommands")).json(&commands).send().await {
        Ok(resp) if resp.status().is_success() => tracing::info!("registered bot commands"),
        Ok(resp) => tracing::warn!("setMyCommands rejected: {}", resp.status()),
        Err(e) => tracing::warn!("setMyCommands failed: {e}"),
    }
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
        // Show clean inline hyperlinks (e.g. "Link to Notion page") instead of a
        // big link-preview card under every message.
        "disable_web_page_preview": true
    });
    if let Some(kb) = &reply.keyboard {
        body["reply_markup"] = kb.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull the command keywords the router is told it may emit out of the
    /// bundled cmd-route agent's system prompt (the "- name: …" bullets).
    fn router_commands() -> Vec<String> {
        let wf: Value = serde_json::from_str(include_str!("../agents/cmd-route.json")).unwrap();
        let sys = wf["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "router")
            .unwrap()["data"]["system"]
            .as_str()
            .unwrap();
        sys.lines()
            .filter_map(|l| l.trim().strip_prefix("- "))
            .filter_map(|l| l.split(':').next())
            .map(|s| s.trim().to_string())
            .collect()
    }

    /// Guard against drift: if someone teaches the router a new command but
    /// forgets to wire it (or renames a handler), this fails instead of the bot
    /// silently replying "that agent isn't installed".
    #[test]
    fn every_routed_command_is_dispatchable() {
        let cmds = router_commands();
        assert!(cmds.contains(&"todo".to_string()), "prompt parse found nothing");
        for cmd in cmds {
            assert!(
                cmd == "none" || is_dispatchable(&cmd),
                "router may emit '{cmd}' but dispatch_command can't handle it"
            );
        }
    }

    #[test]
    fn breadcrumb_marks_text_and_rich() {
        let r = with_breadcrumb("spent", Reply::text("Logged expense"));
        assert!(r.text.starts_with("↳ /spent"));
        assert!(r.text.contains("Logged expense"));

        let r = with_breadcrumb("balance", Reply::rich("<b>$5</b>", "$5"));
        let html = r.rich_html.unwrap();
        assert!(html.starts_with("<blockquote>↳ /balance</blockquote>"));
        assert!(html.contains("<b>$5</b>"));
        assert!(r.text.starts_with("↳ /balance")); // fallback also marked
    }

    #[test]
    fn confirm_targets_the_named_list() {
        let r = confirm_clear_prompt("clear_groceries");
        assert!(r.text.contains("grocery"));
        let data = r.keyboard.unwrap()["inline_keyboard"][0][0]["callback_data"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(data, "confirm:clear_groceries");
    }
}
