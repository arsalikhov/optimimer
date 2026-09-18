//! First-contact setup. After a chat pairs (setup code → owner, invite code →
//! member) the bot walks it through name → timezone → categories → models → machine,
//! saving each answer in `crate::config`, then hands over to the agent. The
//! current step is kept per chat in the settings table, so a restart mid-way
//! resumes where it left off. Members only get the first two steps.

use super::*;
use crate::config;

fn key(chat_id: i64) -> String {
    format!("onboarding:{chat_id}")
}

pub(super) fn active(chat_id: i64) -> bool {
    config::stored(&key(chat_id)).is_some()
}

fn set_step(chat_id: i64, step: &str) {
    config::set(&key(chat_id), step)
}

fn buttons(rows: Vec<(String, String)>) -> Value {
    json!({ "inline_keyboard": [rows.iter().map(|(t, d)| json!({ "text": t, "callback_data": d })).collect::<Vec<_>>()] })
}

fn with_buttons(text: String, rows: Vec<(String, String)>) -> Reply {
    Reply { text, keyboard: Some(buttons(rows)), rich_html: None }
}

/// Kick off (or restart) setup. `first_name` is Telegram's profile name,
/// offered as a one-tap default.
pub(super) fn start(chat_id: i64, owner: bool, first_name: &str) -> Reply {
    set_step(chat_id, "name");
    let intro = if owner {
        "Paired — this chat now owns the assistant. Let's set things up; it takes a minute, and you can change any of it later just by telling me."
    } else {
        "Paired — you can use this assistant now. Two quick questions first."
    };
    let name = first_name.trim();
    let rows = if !name.is_empty() && name.len() <= 40 {
        vec![(format!("Call me {name}"), format!("ob:name:{name}"))]
    } else {
        vec![]
    };
    with_buttons(format!("{intro}\n\nWhat should I call you?"), rows)
}

/// Handle a message while setup is active. Returns `None` only when setup
/// isn't running for this chat.
pub(super) async fn step(state: &BotState, chat_id: i64, msg: &Value) -> Option<Reply> {
    let step = config::stored(&key(chat_id))?;
    if let Some(loc) = msg.get("location").filter(|l| !l.is_null()) {
        if step == "tz" {
            let r = handle_location(state, chat_id, loc);
            if r.text.starts_with("Timezone set") {
                return Some(after_tz(state, chat_id, r.text));
            }
            return Some(r);
        }
    }
    let text = msg["text"].as_str().unwrap_or("").trim();
    if text.is_empty() {
        return Some(Reply::text("Let's finish setting up first — answer the question above, or tap a button."));
    }
    Some(answer(state, chat_id, &step, text).await)
}

/// A tapped button (`ob:` prefix already stripped).
pub(super) async fn callback(state: &BotState, chat_id: i64, data: &str) -> Reply {
    let Some(step) = config::stored(&key(chat_id)) else {
        return Reply::text("Setup is already finished — just talk to me.");
    };
    let (what, val) = data.split_once(':').unwrap_or((data, ""));
    let expected = match what { "name" => "name", "tz" => "tz", "cats" => "categories", "models" => "models", "machine" => "machine", _ => "" };
    if expected != step {
        return Reply::text("That step is done — answer the current question instead.");
    }
    answer(state, chat_id, &step, if val.is_empty() { "keep" } else { val }).await
}

fn is_owner(state: &BotState, chat_id: i64) -> bool {
    state.is_owner(chat_id)
}

async fn answer(state: &BotState, chat_id: i64, step: &str, text: &str) -> Reply {
    match step {
        "name" => {
            let name: String = text.chars().take(60).collect();
            config::set_chat_name(chat_id, &name);
            if is_owner(state, chat_id) {
                config::set(config::OWNER_NAME, &name);
            }
            set_step(chat_id, "tz");
            let current = tz_for(state, chat_id);
            with_buttons(
                format!(
                    "Nice to meet you, {name}. Which timezone are you in? Share a location pin (📎 → Location) or type it, e.g. Europe/Berlin."
                ),
                vec![(format!("Keep {current}"), "ob:tz:keep".to_string())],
            )
        }
        "tz" => {
            let name = if text == "keep" { tz_for(state, chat_id) } else { text.to_string() };
            if name.parse::<chrono_tz::Tz>().is_err() {
                return Reply::text(format!("'{name}' isn't a timezone I know. Try an IANA name like Europe/Berlin or America/New_York, or share a location pin."));
            }
            set_tz(state, chat_id, &name);
            after_tz(state, chat_id, format!("Timezone set to {name}."))
        }
        "categories" => {
            let done = if text == "keep" {
                String::new()
            } else {
                let parsed = crate::vault::parse_categories(text);
                if parsed.len() < 2 {
                    return Reply::text("I need at least two, as `Name: what belongs there, Name: …` — or tap Keep these.");
                }
                config::set(config::CATEGORIES, text);
                crate::memory::global().seed_categories();
                format!("Categories saved: {}.\n\n", parsed.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", "))
            };
            ask_models(chat_id, done).await
        }
        "models" => {
            let paid = matches!(text, "paid" | "Paid" | "claude");
            crate::llm::set_tier(if paid { crate::llm::Tier::Paid } else { crate::llm::Tier::Free });
            ask_machine(chat_id, format!("Using {} models.\n\n", if paid { "paid" } else { "free" }))
        }
        "machine" => {
            let mut prefix = String::new();
            if text != "keep" && text != "skip" {
                let parts: Vec<&str> = text.split_whitespace().collect();
                let added = match parts.as_slice() {
                    [name, mac] => config::add_machine(name, mac, ""),
                    [name, mac, iface] => config::add_machine(name, mac, iface),
                    _ => None,
                };
                match added {
                    Some(m) => prefix = format!("Registered {} ({} via {}).\n\n", m.name, m.mac, m.iface),
                    None => return Reply::text("That didn't parse. Send `name MAC [interface]` — the MAC looks like aa:bb:cc:dd:ee:ff — or tap Skip."),
                }
            }
            finish(chat_id, prefix)
        }
        _ => finish(chat_id, String::new()),
    }
}

/// Free vs paid models, with the account's OpenRouter balance when we can read it.
async fn ask_models(chat_id: i64, prefix: String) -> Reply {
    set_step(chat_id, "models");
    let balance = crate::openrouter::credits().await;
    let money = match balance {
        Some(b) if b <= 0.0 => "Your OpenRouter balance is $0, so only free models will work until you add credits.".to_string(),
        Some(b) => format!("Your OpenRouter balance: ${b:.2}."),
        None => "I couldn't read your OpenRouter balance.".to_string(),
    };
    with_buttons(
        format!(
            "{prefix}Which models should I use?\n\
             • Free — OpenRouter's free models. No credits needed; slower and less accurate, and voice notes or photos may misfire.\n\
             • Paid — Claude Sonnet 4.6 / Haiku 4.5 and Voxtral, billed to your OpenRouter credits. Best results.\n\
             {money} You can switch any time by saying \"use paid models\" or \"use free models\"."
        ),
        vec![("Free (default)".to_string(), "ob:models:free".to_string()), ("Paid (Claude)".to_string(), "ob:models:paid".to_string())],
    )
}

fn ask_machine(chat_id: i64, prefix: String) -> Reply {
    set_step(chat_id, "machine");
    with_buttons(
        format!(
            "{prefix}Last one, optional: is there a computer I should be able to wake over the network (Wake-on-LAN)? Send `name MAC [interface]`, e.g. `desktop aa:bb:cc:dd:ee:ff eth0`."
        ),
        vec![("Skip".to_string(), "ob:machine:skip".to_string())],
    )
}

fn after_tz(state: &BotState, chat_id: i64, prefix: String) -> Reply {
    if is_owner(state, chat_id) {
        config::set(config::TIMEZONE, &tz_for(state, chat_id));
        set_step(chat_id, "categories");
        let cats = crate::vault::categories().iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", ");
        with_buttons(
            format!(
                "{prefix}\n\nTasks and notes get filed under a category, and projects nest under those. Right now: {cats}. Keep these, or send your own as `Name: what belongs there, Name: …` — the last one is the catch-all."
            ),
            vec![("Keep these".to_string(), "ob:cats:keep".to_string())],
        )
    } else {
        finish(chat_id, format!("{prefix}\n\n"))
    }
}

fn finish(chat_id: i64, prefix: String) -> Reply {
    config::unset(&key(chat_id));
    let name = config::chat_name(chat_id);
    let hello = if name.is_empty() { "All set!".to_string() } else { format!("All set, {name}!") };
    Reply::text(format!(
        "{prefix}{hello} Just talk to me in plain language:\n\
         • \"remind me to call the dentist tomorrow at 9\"\n\
         • \"spent 12.50 on lunch\"\n\
         • \"note: ideas for the trip …\"\n\
         • forward a batch of messages, or send a voice memo, for a summary\n\n\
         Change anything later by saying so — \"call me …\", \"my timezone is …\", \"add machine …\", \"show settings\". \
         To let someone else use me, ask for an invite code."
    ))
}

/// Plain-text overview of the instance settings for the `show_settings` tool.
pub(super) fn settings_reply(state: &BotState, chat_id: i64) -> Reply {
    let cats = crate::vault::categories().iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", ");
    let machines = config::machines();
    let machines = if machines.is_empty() {
        "none".to_string()
    } else {
        machines.iter().map(|m| format!("{} ({} via {})", m.name, m.mac, m.iface)).collect::<Vec<_>>().join("; ")
    };
    let vocab = config::vocab();
    let vocab = if vocab.is_empty() { "none".to_string() } else { vocab.join(", ") };
    let chats = config::chats();
    let paired = if chats.is_empty() {
        "env allowlist only".to_string()
    } else {
        let who = chats
            .iter()
            .map(|c| {
                let n = if c.name.is_empty() { c.chat_id.to_string() } else { c.name.clone() };
                format!("{n} ({}, since {})", c.role, c.joined.chars().take(10).collect::<String>())
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!("{} — {who}", chats.len())
    };
    let name = { let n = config::chat_name(chat_id); if n.is_empty() { config::owner_name() } else { n } };
    let name = if name.is_empty() { "(not set)".to_string() } else { name };
    Reply::text(format!(
        "Name: {name}\nTimezone: {} (default {})\nModels: {}\nCategories: {cats}\nMachines: {machines}\nVoice vocabulary: {vocab}\nPaired chats: {paired}",
        tz_for(state, chat_id),
        config::get(config::TIMEZONE).unwrap_or_else(|| "UTC".into()),
        crate::llm::describe(),
    ))
}
