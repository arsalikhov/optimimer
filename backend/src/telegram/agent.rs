//! The conversational agent: every message goes to one tool-calling model that
//! reads a bounded context (rolling summary + the last few turns), calls the
//! bot's capabilities as tools, and answers in plain language. Memory
//! extraction and summary folding run in the background after each reply.

use super::*;
use crate::config;
use crate::memory;

pub(super) fn agent_model() -> String {
    crate::llm::agent()
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
        tool_def("wake_machine", "Wake one of the user's registered machines via Wake-on-LAN. Omit the name when only one is registered.", obj(json!({ "name": { "type": "string" } }), &[])),
        // ---- web ----
        tool_def("web_search", "Search the web for current information (news, prices, opening hours, facts you don't know). Returns titles, URLs and snippets; call read_page on a result when the snippet isn't enough.", obj(json!({ "query": { "type": "string" } }), &["query"])),
        tool_def("read_page", "Fetch a web page and return its readable text (first few thousand characters).", obj(json!({ "url": { "type": "string" } }), &["url"])),
        // ---- settings ----
        tool_def("set_timezone", "Set the user's IANA timezone, e.g. 'Europe/Berlin'.", obj(json!({ "tz": { "type": "string" } }), &["tz"])),
        tool_def("show_settings", "Show the assistant's settings: the user's name, timezone, categories, machines, voice vocabulary, paired chats (displayed directly).", obj(json!({}), &[])),
        tool_def("set_name", "Set what the assistant calls the user.", obj(json!({ "name": { "type": "string" } }), &["name"])),
        tool_def("add_machine", "Register (or update) a machine for Wake-on-LAN by name and MAC address; the network interface defaults to eth0.", obj(json!({
            "name": { "type": "string" }, "mac": { "type": "string", "description": "e.g. aa:bb:cc:dd:ee:ff" }, "iface": { "type": "string" }
        }), &["name", "mac"])),
        tool_def("remove_machine", "Forget a Wake-on-LAN machine by name.", obj(json!({ "name": { "type": "string" } }), &["name"])),
        tool_def("set_categories", "Replace the task/note categories (owner only; the user confirms with a button). Pass 'Name: hint, Name: hint, …' with the catch-all last. Never call this just because a task doesn't fit — only when the user asks to change the categories.", obj(json!({ "list": { "type": "string" } }), &["list"])),
        tool_def("add_vocab", "Add words the voice transcriber should spell correctly (names, brands, jargon).", obj(json!({ "terms": { "type": "string", "description": "comma-separated" } }), &["terms"])),
        tool_def("invite_user", "Create a one-time invite code that lets another Telegram account use this assistant (owner only).", obj(json!({}), &[])),
        tool_def("set_models", "Switch between 'free' (OpenRouter's free models, no credits) and 'paid' (Claude Sonnet/Haiku + Voxtral) models. Owner only.", obj(json!({ "tier": { "type": "string", "enum": ["free", "paid"] } }), &["tier"])),
        tool_def("remove_user", "Revoke a member's access by their name or chat id (owner only; the owner can't be removed).", obj(json!({ "who": { "type": "string" } }), &["who"])),
        tool_def("restart_setup", "Run the first-time setup dialogue again (name, timezone, categories, machine).", obj(json!({}), &[])),
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
        "wake_machine" => observe(handle_wake(&s("name")).await.text),
        "set_timezone" => {
            let name = s("tz");
            match name.parse::<chrono_tz::Tz>() {
                Ok(_) => {
                    set_tz(state, chat_id, &name);
                    if state.is_owner(chat_id) { config::set(config::TIMEZONE, &name); }
                    observe(format!("Timezone set to {name}."))
                }
                Err(_) => observe(format!("'{name}' is not a valid IANA timezone.")),
            }
        }
        "web_search" => match crate::web::search(&s("query"), 6).await {
            Ok(hits) => observe(crate::web::render(&hits)),
            Err(e) => observe(format!("Search failed: {e}")),
        },
        "read_page" => match crate::web::read_page(&s("url"), 6000).await {
            Ok(text) => observe(text),
            Err(e) => observe(format!("Couldn't read that page: {e}")),
        },
        "show_settings" => display(settings_reply(state, chat_id)),
        "set_name" => {
            let n = s("name");
            if n.is_empty() { return observe("Name missing."); }
            config::set_chat_name(chat_id, &n);
            if state.is_owner(chat_id) { config::set(config::OWNER_NAME, &n); }
            observe(format!("Name set to {n}."))
        }
        "add_machine" => match config::add_machine(&s("name"), &s("mac"), &s("iface")) {
            Some(m) => observe(format!("Machine {} registered ({} via {}).", m.name, m.mac, m.iface)),
            None => observe("Couldn't register it: need a name and a MAC address like aa:bb:cc:dd:ee:ff."),
        },
        "remove_machine" => observe(if config::remove_machine(&s("name")) { "Machine removed." } else { "No machine by that name." }),
        "set_categories" => {
            if !state.is_owner(chat_id) { return observe("Only the owner can change the categories."); }
            let parsed = crate::vault::parse_categories(&s("list"));
            if parsed.len() < 2 { return observe("Need at least two categories as 'Name: hint, Name: hint'."); }
            let names = parsed.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", ");
            state.pending.lock().unwrap().insert(chat_id, Pending { command: "set_categories".into(), text: s("list") });
            confirm(&format!("Replace the categories with: {names}? Existing tasks keep the category they have."), "set_categories")
        }
        "add_vocab" => {
            let terms: Vec<String> = s("terms").split(',').map(|t| t.trim().to_string()).collect();
            observe(format!("Voice vocabulary is now: {}", config::add_vocab(&terms).join(", ")))
        }
        "invite_user" => {
            if !state.is_owner(chat_id) { return observe("Only the owner can invite people."); }
            let code = config::new_invite();
            observe(format!("Invite code: {code} — they message this bot and send it; it works once."))
        }
        "set_models" => {
            if !state.is_owner(chat_id) { return observe("Only the owner can change the models."); }
            let paid = s("tier") == "paid";
            crate::llm::set_tier(if paid { crate::llm::Tier::Paid } else { crate::llm::Tier::Free });
            observe(format!("Models: {}", crate::llm::describe()))
        }
        "remove_user" => {
            if !state.is_owner(chat_id) { return observe("Only the owner can remove people."); }
            let who = s("who");
            let target = config::chats().into_iter().find(|c| c.chat_id.to_string() == who || (!c.name.is_empty() && c.name.eq_ignore_ascii_case(&who)));
            match target {
                Some(c) if config::remove_chat(c.chat_id) => observe(format!("Removed {} ({}).", if c.name.is_empty() { "that chat".to_string() } else { c.name }, c.chat_id)),
                Some(_) => observe("The owner can't be removed."),
                None => observe("No paired chat matches that name or id."),
            }
        }
        "restart_setup" => display(onboarding::start(chat_id, state.is_owner(chat_id), &config::chat_name(chat_id))),
        other => observe(format!("(no such tool: {other})")),
    }
}

fn system_prompt(state: &BotState, chat_id: i64) -> String {
    let tz = tz_for(state, chat_id);
    let now = now_in_tz(&tz);
    let cats = crate::vault::categories().iter().map(|(n, h)| format!("{n} ({h})")).collect::<Vec<_>>().join("; ");
    let name = { let n = config::chat_name(chat_id); if n.is_empty() { config::owner_name() } else { n } };
    let who = if name.is_empty() { String::new() } else { format!(" The user's name is {name}.") };
    let machines = config::machines();
    let machines = if machines.is_empty() {
        "No machines are registered for Wake-on-LAN (offer add_machine if asked to wake one).".to_string()
    } else {
        format!("Machines you can wake: {}.", machines.iter().map(|m| m.name.clone()).collect::<Vec<_>>().join(", "))
    };
    format!(
        "You are Optimimer, the personal assistant of ONE user, chatting over Telegram. Now: {now} ({tz}).{who}\n\
         You act through tools: tasks, notes and memos live as Markdown files in the user's Obsidian vault; money in a ledger; \
         plus shopping lists, reminders, email, stock watches, machines, settings, and long-term memory. \
         Task/note categories: {cats}. {machines}\n\
         Rules:\n\
         - Do things, don't describe how to. When the request is clear, call the tool(s) right away, several in one turn if needed. \
           Extract amounts, dates, names yourself; ask a question only when a required detail is genuinely missing.\n\
         - Before answering questions about people, plans, preferences or anything not visible in this conversation, call `recall`. \
           Use `remember` when the user tells you something worth keeping.\n\
         - For anything current or outside your knowledge (news, prices, weather, hours, recent events), call `web_search`, \
           then `read_page` on the best hit if the snippets aren't enough; mention the source URL in your answer.\n\
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
    // "typing…" only lasts ~5s per call; keep it alive for as long as the model takes.
    let keep_typing = {
        let (c, a) = (client.clone(), api.to_string());
        tokio::spawn(async move {
            loop {
                typing(&c, &a, chat_id).await;
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
        })
    };

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
    // What tools actually saved this turn — handed to the memory extractor so it
    // doesn't restate a task/note/expense as a "fact".
    let mut recorded: Vec<String> = Vec::new();
    for step in 0..AGENT_MAX_STEPS {
        let msg = match crate::openrouter::chat_tools(&agent_model(), &messages, &tools).await {
            Ok(m) => m,
            Err(e) => {
                final_text = format!("I couldn't get an answer from the model — {e}");
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
            if matches!(name.as_str(), "create_task" | "save_note" | "save_memo" | "log_expense" | "log_income" | "set_reminder" | "remember" | "complete_task") {
                recorded.push(format!("{name}: {}", result.text.lines().next().unwrap_or("")));
            }
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
        memory::extract(chat_id, &u, &a, &recorded, assistant_id).await;
        memory::maybe_roll_summary(chat_id).await;
    });
    let _ = user_id;

    keep_typing.abort();
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
