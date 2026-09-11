//! Inline-button taps (`callback_query`): onboarding steps, confirmations, forward-batch prompts, checklists.

use super::*;

pub(super) async fn handle_callback(client: &reqwest::Client, api: &str, state: &BotState, cb: &Value) {
    let cb_id = cb["id"].as_str().unwrap_or("");
    let chat_id = cb["message"]["chat"]["id"].as_i64();
    let data = cb["data"].as_str().unwrap_or("");

    let mut note = "OK".to_string();

    // Onboarding buttons (one-tap defaults: name, keep timezone/categories, skip machine).
    if let (Some(rest), Some(chat)) = (data.strip_prefix("ob:"), chat_id) {
        let reply = onboarding::callback(state, chat, rest).await;
        send(client, api, chat, &reply).await;
    }

    // Confirm a destructive action the agent proposed (see `agent::confirm`).
    if let (Some(rest), Some(chat)) = (data.strip_prefix("confirm:"), chat_id) {
        let reply = match rest {
            "cancel" => {
                note = "Cancelled".into();
                Reply::text("Cancelled — nothing was changed.")
            }
            "clear_groceries" => {
                note = "Done".into();
                clear_reply(state, chat, ListKind::Grocery)
            }
            "clear_to_buy" => {
                note = "Done".into();
                clear_reply(state, chat, ListKind::Other)
            }
            r if r.starts_with("remove_transaction:") => {
                note = "Done".into();
                remove_transaction_reply(&r["remove_transaction:".len()..])
            }
            "set_categories" => {
                let pending = state.pending.lock().unwrap().remove(&chat);
                match pending {
                    Some(p) if p.command == "set_categories" => {
                        note = "Done".into();
                        crate::config::set(crate::config::CATEGORIES, &p.text);
                        crate::memory::global().seed_categories();
                        let names = crate::vault::categories().iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", ");
                        Reply::text(format!("Categories are now: {names}."))
                    }
                    _ => Reply::text("That confirmation expired — ask again."),
                }
            }
            _ => Reply::text("That confirmation is no longer valid."),
        };
        send(client, api, chat, &reply).await;
    }

    // Forward batch parked without a note: summarize as-is, or drop it.
    if let (Some(action), Some(chat)) = (data.strip_prefix("fwd:"), chat_id) {
        match (action, convo::take(chat)) {
            ("discard", _) => {
                note = "Discarded".into();
                send(client, api, chat, &Reply::text("OK — dropped those forwards.")).await;
            }
            ("summarize", Some(batch)) => {
                note = "Summarizing…".into();
                finalize_forward_batch(client, api, state, chat, batch).await;
            }
            _ => {
                send(client, api, chat, &Reply::text("That prompt expired — forward the messages again.")).await;
            }
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
                                format!("▫️ {rest}")
                            } else if let Some(rest) = txt.strip_prefix("▫️ ") {
                                format!("✅ {rest}")
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
