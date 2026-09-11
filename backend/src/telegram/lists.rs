//! Per-chat shopping lists (`/buy_later`, `/groceries`, `/to_buy`, …) — local SQLite state.

use super::*;

/// Per-chat running shopping lists (see `/buy_later`, `/groceries`). Persisted as
/// JSON next to the other bot state; not in the vault — these are
/// throwaway lists you add to and clear.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct ChatLists {
    #[serde(default)]
    groceries: Vec<String>,
    #[serde(default)]
    other: Vec<String>,
}

/// Which of a chat's two lists a command targets.
#[derive(Clone, Copy)]
pub(super) enum ListKind {
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

/// Read a chat's two shopping lists (empty if none stored yet).
pub(super) fn get_lists(state: &BotState, chat_id: i64) -> ChatLists {
    let conn = state.db.lock();
    conn.query_row("SELECT json FROM chat_lists WHERE chat_id = ?1", params![chat_id], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|j| serde_json::from_str(&j).ok())
    .unwrap_or_default()
}

pub(super) fn put_lists(state: &BotState, chat_id: i64, lists: &ChatLists) {
    if let Ok(json) = serde_json::to_string(lists) {
        let conn = state.db.lock();
        let _ = conn.execute(
            "INSERT INTO chat_lists (chat_id, json) VALUES (?1, ?2)
             ON CONFLICT(chat_id) DO UPDATE SET json = excluded.json",
            params![chat_id, json],
        );
    }
}

/// `/buy_later <stuff>` — split the input into items, classify each as a grocery
/// or not, and append to the matching per-chat list.
pub(super) async fn handle_buy_later(state: &BotState, chat_id: i64, body: &str) -> Reply {
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
pub(super) async fn classify_items(text: &str) -> (Vec<String>, Vec<String>) {
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

pub(super) fn str_vec(v: &Value) -> Vec<String> {
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
pub(super) fn list_reply(state: &BotState, chat_id: i64, kind: ListKind) -> Reply {
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
/// item; tapping toggles a ✅/▫️ check so you can tick things off while you shop
/// without spamming the chat. (Buttons can't carry real strikethrough, and the
/// combining-character fake mangles on mobile, so a check prefix it is.) State
/// lives in the message's own keyboard (see `shop:` in `handle_callback`).
pub(super) fn handle_grocery_shopping(state: &BotState, chat_id: i64) -> Reply {
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

pub(super) fn clear_reply(state: &BotState, chat_id: i64, kind: ListKind) -> Reply {
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

/// `/remove_grocery <item-or-number>` — drop a single entry from a list, either by
/// its 1-based position (as shown by `/groceries`) or by name (case-insensitive,
/// first match wins). Keeps the rest of the list intact, unlike `/clear_groceries`.
pub(super) fn remove_reply(state: &BotState, chat_id: i64, kind: ListKind, body: &str) -> Reply {
    let arg = body.trim();
    if arg.is_empty() {
        return Reply::text(format!(
            "Usage: `/remove_{} milk` or `/remove_{0} 2` (the number from /{}).",
            if matches!(kind, ListKind::Grocery) { "grocery" } else { "to_buy" },
            if matches!(kind, ListKind::Grocery) { "groceries" } else { "to_buy" },
        ));
    }
    let mut lists = get_lists(state, chat_id);
    let items = kind.items_mut(&mut lists);
    if items.is_empty() {
        return Reply::text(format!("Your {} list is empty.", kind.noun()));
    }
    // Prefer an exact 1-based index when the whole arg is a number, else match by name.
    let idx = match arg.parse::<usize>() {
        Ok(n) if n >= 1 && n <= items.len() => Some(n - 1),
        Ok(_) => return Reply::text(format!("No item {arg} — the list has {} item(s).", items.len())),
        Err(_) => items.iter().position(|it| it.eq_ignore_ascii_case(arg)),
    };
    match idx {
        Some(i) => {
            let removed = items.remove(i);
            put_lists(state, chat_id, &lists);
            Reply::text(format!("Removed from {}: {removed}", kind.noun()))
        }
        None => Reply::text(format!("`{arg}` isn't on your {} list.", kind.noun())),
    }
}
