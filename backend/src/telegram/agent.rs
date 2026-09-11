//! The conversational agent: every message goes to one tool-calling model that
//! reads a bounded context (rolling summary + the last few turns), calls the
//! bot's capabilities as tools, and answers in plain language. Memory
//! extraction and summary folding run in the background after each reply.

use super::*;
use crate::memory;

pub(super) fn agent_model() -> String {
    std::env::var("AGENT_MODEL").unwrap_or_else(|_| "anthropic/claude-sonnet-4.6".to_string())
}

/// Hard cap on tool round-trips per message, so a confused model can't loop forever.
pub(super) const AGENT_MAX_STEPS: usize = 8;

pub(super) fn tool_def(name: &str, description: &str, parameters: Value) -> Value {
    json!({ "type": "function", "function": { "name": name, "description": description, "parameters": parameters } })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// OpenAI-style schemas for everything the agent can do. Each name is handled in
/// `exec_tool`.
pub(super) fn agent_tools() -> Value {
    let cats = crate::vault::categories().iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", ");
    json!([
        // ---- memory ----
        tool_def("recall", "Look something up in long-term memory: people, projects, places, preferences, facts, past decisions, and links to tasks/notes. Use it whenever the user refers to something you don't see in the recent conversation.", obj(json!({
            "query": { "type": "string", "description": "Keywords or a name, e.g. 'dentist', 'Sam', 'triathlon plan'." }
        }), &["query"])),
        tool_def("remember", "Store a durable fact or preference the user states explicitly ('remember that…', 'my dentist is…'). Not for transient chatter.", obj(json!({
            "kind": { "type": "string", "enum": ["person", "organization", "project", "place", "topic", "preference", "fact", "event"] },
            "name": { "type": "string", "description": "Canonical name, or for a fact/preference the statement itself." },
            "summary": { "type": "string", "description": "One factual sentence in third person." },
            "related_to": { "type": "string", "description": "Optional name of an existing entity this links to." },
            "relation": { "type": "string", "description": "Optional relation label, e.g. 'is the dentist of'." }
        }), &["kind", "name"])),
        // ---- tasks & notes (vault) ----
        tool_def("create_task", &format!("Create a task, meeting or appointment as a note in the vault. Category is chosen automatically from: {cats}."), obj(json!({
            "text": { "type": "string", "description": "The task in natural language including any date/time and project, e.g. 'bike fit for the triathlon next tuesday 2pm'." }
        }), &["text"])),
        tool_def("complete_task", "Mark an open task done by describing it.", obj(json!({ "query": { "type": "string" } }), &["query"])),
        tool_def("list_tasks", "Show the user their open tasks (displayed directly).", obj(json!({}), &[])),
        tool_def("search_tasks", "Search tasks by keywords (displayed directly).", obj(json!({ "query": { "type": "string" } }), &["query"])),
        tool_def("save_note", "Save a note, idea or reference text to the vault (auto-titled, categorised and tagged).", obj(json!({ "text": { "type": "string" } }), &["text"])),
        tool_def("list_notes", "Show the user their newest notes (displayed directly).", obj(json!({}), &[])),
        tool_def("search_notes", "Search notes by keywords (displayed directly).", obj(json!({ "query": { "type": "string" } }), &["query"])),
        tool_def("save_memo", "Turn a long spoken/typed thought-dump into a summarised memo note (title, key points, action items, transcript). Use when the user is thinking out loud rather than asking for something.", obj(json!({
            "text": { "type": "string", "description": "The full transcript/text to summarise." }
        }), &["text"])),
        tool_def("list_summaries", "Show saved conversation/voice-memo summaries (displayed directly).", obj(json!({}), &[])),
        tool_def("show_summary", "Show one saved summary by its number from the list (displayed directly).", obj(json!({ "number": { "type": "integer" } }), &["number"])),
        // ---- reminders & email ----
        tool_def("set_reminder", "Schedule a Telegram ping at a future time.", obj(json!({
            "text": { "type": "string", "description": "What to be reminded of and when, e.g. 'pay rent tomorrow 9am'." }
        }), &["text"])),
        tool_def("send_email", "Draft and send an email on the user's behalf.", obj(json!({
            "text": { "type": "string", "description": "Who to email and what to say, in natural language." }
        }), &["text"])),
        // ---- money ----
        tool_def("log_expense", "Record money the user spent.", obj(json!({
            "amount": { "type": "number", "description": "Positive amount." },
            "merchant": { "type": "string", "description": "Store / payee." },
            "category": { "type": "string", "description": "One of: Groceries, Dining, Transport, Housing, Utilities, Health, Entertainment, Shopping, Subscriptions, Travel, Loans, Cash, Other." },
            "date": { "type": "string", "description": "YYYY-MM-DD; omit for today." }
        }), &["amount", "merchant"])),
        tool_def("log_income", "Record money the user received.", obj(json!({
            "amount": { "type": "number" },
            "source": { "type": "string" },
            "category": { "type": "string", "description": "'Salary' for payroll, otherwise 'Income'." },
            "date": { "type": "string", "description": "YYYY-MM-DD; omit for today." }
        }), &["amount", "source"])),
        tool_def("get_balance", "Show income vs expenses for a week or month (displayed directly as a table).", obj(json!({
            "period": { "type": "string", "description": "'' = this week, 'last', a number of weeks ago, or a month like 'june' / 'may 2025'." }
        }), &[])),
        tool_def("set_monthly_income", "Set the user's expected monthly income used by the balance view.", obj(json!({ "amount": { "type": "number" } }), &["amount"])),
        tool_def("list_transactions", "Show the newest ledger rows, numbered (displayed directly).", obj(json!({ "count": { "type": "integer" } }), &[])),
        tool_def("remove_transaction", "Delete a ledger row by its number from list_transactions. Asks the user to confirm.", obj(json!({ "number": { "type": "integer" } }), &["number"])),
        // ---- shopping lists ----
        tool_def("add_shopping_item", "Add item(s) to the shopping list (auto-sorted into groceries vs other).", obj(json!({ "item": { "type": "string" } }), &["item"])),
        tool_def("show_shopping_list", "Show a shopping list (displayed directly). 'checklist' is the tap-to-tick grocery version.", obj(json!({
            "which": { "type": "string", "enum": ["groceries", "to_buy", "checklist"] }
        }), &[])),
        tool_def("remove_shopping_item", "Remove one item from a shopping list by name or number.", obj(json!({
            "which": { "type": "string", "enum": ["groceries", "to_buy"] }, "item": { "type": "string" }
        }), &["which", "item"])),
        tool_def("clear_shopping_list", "Clear a whole shopping list. Asks the user to confirm.", obj(json!({
            "which": { "type": "string", "enum": ["groceries", "to_buy"] }
        }), &["which"])),
        // ---- watches & machines ----
        tool_def("watch_stock", "Watch a product URL and ping the user when it is back in stock.", obj(json!({ "url": { "type": "string" } }), &["url"])),
        tool_def("list_watches", "Show active stock watches (displayed directly).", obj(json!({}), &[])),
        tool_def("unwatch", "Stop a stock watch by number, or 'all'.", obj(json!({ "which": { "type": "string" } }), &["which"])),
        tool_def("wake_mercury", "Wake the user's HPC server 'mercury' via Wake-on-LAN.", obj(json!({}), &[])),
        // ---- settings ----
        tool_def("set_timezone", "Set the user's IANA timezone, e.g. 'America/Toronto'.", obj(json!({ "tz": { "type": "string" } }), &["tz"])),
    ])
}

/// What a tool hands back: text for the model, and optionally a rich reply
/// that was already shown to the user (tables, checklists, buttons).
pub(super) struct ToolResult {
    pub text: String,
    pub shown: Option<Reply>,
}

fn observe(text: impl Into<String>) -> ToolResult {
    ToolResult { text: text.into(), shown: None }
}

/// Show a rich reply to the user right away and tell the model so it doesn't
/// repeat the contents.
fn display(reply: Reply) -> ToolResult {
    let preview = { let t = reply.text.trim(); if t.chars().count() > 400 { format!("{}…", t.chars().take(400).collect::<String>()) } else { t.to_string() } };
    ToolResult { text: format!("(displayed to the user)\n{preview}"), shown: Some(reply) }
}

/// Destructive action → Yes/Cancel buttons; the callback runs `action`.
fn confirm(question: &str, action: &str) -> ToolResult {
    let reply = Reply {
        text: question.to_string(),
        keyboard: Some(json!({ "inline_keyboard": [[
            { "text": "Yes", "callback_data": format!("confirm:{action}") },
            { "text": "Cancel", "callback_data": "confirm:cancel" }
        ]]})),
        rich_html: None,
    };
    ToolResult { text: "(asked the user to confirm with buttons; do not do it yourself, just say you're waiting for their tap)".into(), shown: Some(reply) }
}

pub(super) async fn exec_tool(state: &BotState, chat_id: i64, name: &str, args: &Value) -> ToolResult {
    let tz = tz_for(state, chat_id);
    let today: String = now_in_tz(&tz).chars().take(10).collect();
    let s = |k: &str| args[k].as_str().unwrap_or("").trim().to_string();
    match name {
        // ---- memory ----
        "recall" => observe(memory::recall(&s("query"))),
        "remember" => {
            let m = memory::global();
            let node = m.upsert_node(&s("kind"), &s("name"), &s("summary"), "");
            let related = s("related_to");
            if !related.is_empty() {
                // Link to whichever kind already holds that name; else make a topic.
                let target = m.all_nodes().into_iter().find(|n| n.name.eq_ignore_ascii_case(&related))
                    .unwrap_or_else(|| m.upsert_node("topic", &related, "", ""));
                let rel = { let r = s("relation"); if r.is_empty() { "related to".into() } else { r } };
                m.add_edge(&node.id, &target.id, &rel, &format!("chat:{chat_id}"));
                let _ = crate::vault::mirror_memory_node(&target, &m.neighbors(&target.id, 12));
            }
            let _ = crate::vault::mirror_memory_node(&node, &m.neighbors(&node.id, 12));
            observe(format!("Remembered [{}] {}.", node.kind, node.name))
        }
        // ---- vault ----
        "create_task" => observe(handle_todo(state, chat_id, &s("text")).await.text),
        "complete_task" => observe(handle_complete(state, chat_id, &s("query")).await.text),
        "list_tasks" => display(list_docs_reply(crate::vault::TASKS)),
        "search_tasks" => display(search_reply(crate::vault::TASKS, &s("query"))),
        "save_note" => observe(handle_note(state, chat_id, &s("text")).await.text),
        "list_notes" => display(list_docs_reply(crate::vault::NOTES)),
        "search_notes" => display(search_reply(crate::vault::NOTES, &s("query"))),
        "save_memo" => match memo_from_text(state, chat_id, &s("text")).await {
            Some(reply) => display(reply),
            None => observe("Couldn't summarise that."),
        },
        "list_summaries" => display(summaries_reply(state, chat_id)),
        "show_summary" => display(summary_reply(state, chat_id, &args["number"].to_string())),
        // ---- reminders & email (still workflow agents) ----
        "set_reminder" => observe(run_command(state, chat_id, "notify", &s("text"), "").await.text),
        "send_email" => observe(run_command(state, chat_id, "email", &s("text"), "").await.text),
        // ---- money ----
        "log_expense" | "log_income" => {
            let (kind, entry) = if name == "log_expense" {
                (crate::finance::ManualKind::Spent, json!({ "amount": args["amount"], "merchant": args["merchant"], "category": args["category"], "date": args["date"] }))
            } else {
                (crate::finance::ManualKind::Earned, json!({ "amount": args["amount"], "source": args["source"], "category": args["category"], "date": args["date"] }))
            };
            match crate::finance::log_manual(&entry.to_string(), kind, &today).await {
                Ok(l) => match l.html {
                    Some(html) => display(Reply::rich(html, l.text)),
                    None => observe(l.text),
                },
                Err(e) => observe(format!("Failed to log: {e}")),
            }
        }
        "get_balance" => display(handle_balance(state, chat_id, &s("period")).await),
        "set_monthly_income" => observe(handle_set_income(state, chat_id, &args["amount"].to_string()).text),
        "list_transactions" => display(transactions_reply(&args["count"].to_string())),
        "remove_transaction" => {
            let n = args["number"].as_u64().unwrap_or(0);
            if n == 0 { observe("Need the transaction number from list_transactions.") } else {
                confirm(&format!("Delete transaction #{n}?"), &format!("remove_transaction:{n}"))
            }
        }
        // ---- lists ----
        "add_shopping_item" => observe(handle_buy_later(state, chat_id, &s("item")).await.text),
        "show_shopping_list" => match s("which").as_str() {
            "to_buy" => display(list_reply(state, chat_id, ListKind::Other)),
            "checklist" => display(handle_grocery_shopping(state, chat_id)),
            _ => display(list_reply(state, chat_id, ListKind::Grocery)),
        },
        "remove_shopping_item" => {
            let kind = if s("which") == "to_buy" { ListKind::Other } else { ListKind::Grocery };
            observe(remove_reply(state, chat_id, kind, &s("item")).text)
        }
        "clear_shopping_list" => {
            let which = if s("which") == "to_buy" { "to_buy" } else { "groceries" };
            confirm(&format!("Clear the whole {} list? This can't be undone.", which.replace('_', "-")), &format!("clear_{which}"))
        }
        // ---- watches & machines ----
        "watch_stock" => observe(handle_watch(chat_id, &s("url")).text),
        "list_watches" => display(watches_reply(chat_id)),
        "unwatch" => observe(handle_unwatch(chat_id, &s("which")).text),
        "wake_mercury" => observe(handle_wake_mercury().await.text),
        "set_timezone" => {
            let name = s("tz");
            match name.parse::<chrono_tz::Tz>() {
                Ok(_) => { set_tz(state, chat_id, &name); observe(format!("Timezone set to {name}.")) }
                Err(_) => observe(format!("'{name}' is not a valid IANA timezone.")),
            }
        }
        other => observe(format!("(no such tool: {other})")),
    }
}

fn system_prompt(state: &BotState, chat_id: i64) -> String {
    let tz = tz_for(state, chat_id);
    let now = now_in_tz(&tz);
    let cats = crate::vault::categories().iter().map(|(n, h)| format!("{n} ({h})")).collect::<Vec<_>>().join("; ");
    format!(
        "You are Optimimer, the personal assistant of ONE user, chatting over Telegram. Now: {now} ({tz}).\n\
         You act through tools: tasks, notes and memos live as Markdown files in the user's Obsidian vault; money in a ledger; \
         plus shopping lists, reminders, email, stock watches, and long-term memory. Task/note categories: {cats}.\n\
         Rules:\n\
         - Do things, don't describe how to. When the request is clear, call the tool(s) right away, several in one turn if needed. \
           Extract amounts, dates, names yourself; ask a question only when a required detail is genuinely missing.\n\
         - Before answering questions about people, plans, preferences or anything not visible in this conversation, call `recall`. \
           Use `remember` when the user tells you something worth keeping.\n\
         - When a tool says it was displayed to the user, do not repeat its contents; reply with one short sentence or nothing at all.\n\
         - Confirmations for destructive actions are handled by buttons; never assume they were tapped.\n\
         - Voice transcripts arrive as plain text; if one is clearly a thought-dump rather than a request, use `save_memo`.\n\
         - Reply in plain text (no Markdown headers, no bullet spam), one to three short sentences, in the user's language. \
           Never invent tool results. If something failed, say so plainly."
    )
}

/// Run one user message through the agent. Returns `None` when the model has
/// nothing to add (everything was displayed by tools).
pub(super) async fn run_agent(client: &reqwest::Client, api: &str, state: &BotState, chat_id: i64, user_text: &str) -> Option<Reply> {
    let mem = memory::global();
    let user_id = mem.add_message(chat_id, "user", user_text);
    typing(client, api, chat_id).await;

    let tools = agent_tools();
    let mut messages = vec![json!({ "role": "system", "content": system_prompt(state, chat_id) })];
    let (summary, _) = mem.summary(chat_id);
    if !summary.is_empty() {
        messages.push(json!({ "role": "system", "content": format!("Summary of the conversation so far:\n{summary}") }));
    }
    // Recent turns (excluding the one we just stored, added last).
    let mut recent = mem.recent_messages(chat_id, memory::window() + 1);
    if recent.last().map(|(_, t)| t == user_text).unwrap_or(false) {
        recent.pop();
    }
    for (role, text) in recent {
        let role = if role == "assistant" { "assistant" } else { "user" };
        messages.push(json!({ "role": role, "content": clip(&text, 1500) }));
    }
    messages.push(json!({ "role": "user", "content": user_text }));

    let mut final_text = String::new();
    let mut shown_any = false;
    for step in 0..AGENT_MAX_STEPS {
        let msg = match crate::openrouter::chat_tools(&agent_model(), &messages, &tools).await {
            Ok(m) => m,
            Err(e) => {
                final_text = format!("Something went wrong talking to the model: {e}");
                break;
            }
        };
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() {
            final_text = msg["content"].as_str().unwrap_or("").trim().to_string();
            break;
        }
        messages.push(msg.clone());
        for call in calls {
            let id = call["id"].as_str().unwrap_or("").to_string();
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let cargs: Value = call["function"]["arguments"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_else(|| json!({}));
            tracing::info!("agent tool {name} {}", cargs);
            let result = exec_tool(state, chat_id, &name, &cargs).await;
            if let Some(reply) = result.shown {
                send(client, api, chat_id, &reply).await;
                shown_any = true;
                typing(client, api, chat_id).await;
            }
            messages.push(json!({ "role": "tool", "tool_call_id": id, "content": result.text }));
        }
        if step == AGENT_MAX_STEPS - 1 {
            final_text = "I got stuck in a loop — try breaking that into smaller requests.".into();
        }
    }

    // Persist the assistant turn (what was said, or what was shown) and learn
    // from the exchange in the background.
    let stored = if final_text.is_empty() { if shown_any { "(showed the requested information)".to_string() } else { "Done.".to_string() } } else { final_text.clone() };
    let assistant_id = mem.add_message(chat_id, "assistant", &stored);
    let (u, a) = (user_text.to_string(), stored.clone());
    tokio::spawn(async move {
        memory::extract(chat_id, &u, &a, assistant_id).await;
        memory::maybe_roll_summary(chat_id).await;
    });
    let _ = user_id;

    if final_text.is_empty() {
        if shown_any { None } else { Some(Reply::text("Done.")) }
    } else {
        Some(Reply::text(final_text))
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// "typing…" indicator while the model works.
async fn typing(client: &reqwest::Client, api: &str, chat_id: i64) {
    let _ = client.post(format!("{api}/sendChatAction")).json(&json!({ "chat_id": chat_id, "action": "typing" })).send().await;
}

/// Crude HTML→plain-text for the rich-message fallback: drop tags, unescape the
/// entities we emit. Good enough for the rare older-client / API-error path.
pub(super) fn strip_tags(html: &str) -> String {
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
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}
