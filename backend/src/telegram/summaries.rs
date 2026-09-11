//! Conversation summaries: forwarded-message batches and long voice memos → summary + transcript (`/summaries`).

use super::*;

/// One forwarded message arrived: reduce it to text (transcribing audio), add it
/// to the chat's batch, and (re)arm the settle timer. `touch` runs first so a
/// timer armed by the previous forward stands down while we transcribe.
pub(super) async fn handle_forward(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    state: &BotState,
    chat_id: i64,
    msg: &Value,
) {
    convo::touch(chat_id);
    let text = match convo::audio_file_id(msg) {
        Some(fid) => match fetch_transcript(client, api, token, fid).await {
            Ok(t) => format!("🎤 {t}"),
            Err(e) => format!("[voice message — {e}]"),
        },
        None => convo::message_body(msg),
    };
    let tz = tz_for(state, chat_id);
    let item = convo::FwdItem {
        author: convo::forward_author(msg),
        at: convo::forward_time(msg, &tz),
        text,
    };
    let gen = convo::push(chat_id, item);

    let (client, api, state) = (client.clone(), api.to_string(), state.clone());
    tokio::spawn(async move {
        tokio::time::sleep(convo::settle_delay()).await;
        match convo::settle(chat_id, gen) {
            convo::Settled::Stale => {}
            convo::Settled::Ready(batch) => {
                finalize_forward_batch(&client, &api, &state, chat_id, batch).await
            }
            convo::Settled::NeedNote(desc) => {
                send(&client, &api, chat_id, &forward_note_prompt(&desc)).await
            }
        }
    });
}

/// Asked when a batch settles with no comment: the user's framing steers the
/// summary, but they can also take it as-is.
pub(super) fn forward_note_prompt(desc: &str) -> Reply {
    Reply {
        text: format!(
            "Got {desc}. What's this about, or what do you want from it? \
             Reply with a note, or tap *Summarize as-is*."
        ),
        keyboard: Some(json!({ "inline_keyboard": [[
            { "text": "Summarize as-is", "callback_data": "fwd:summarize" },
            { "text": "Discard", "callback_data": "fwd:discard" }
        ]]})),
        rich_html: None,
    }
}

pub(super) async fn finalize_forward_batch(
    client: &reqwest::Client,
    api: &str,
    state: &BotState,
    chat_id: i64,
    batch: convo::ForwardBatch,
) {
    let source = batch.describe();
    let transcript = batch.transcript();
    let note = batch.note.clone().unwrap_or_default();
    finalize_convo(client, api, state, chat_id, "forward", &source, &transcript, &note).await;
}

/// Summarize a transcript, save it locally, and reply with the summary plus a
/// collapsible transcript. Shared by forward batches and voice memos.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_convo(
    client: &reqwest::Client,
    api: &str,
    state: &BotState,
    chat_id: i64,
    kind: &str,
    source: &str,
    transcript: &str,
    note: &str,
) {
    let reply = build_convo(state, chat_id, kind, source, transcript, note).await;
    send(client, api, chat_id, &reply).await;
}

/// A typed thought-dump the agent decided to keep as a memo (`save_memo` tool).
pub(super) async fn memo_from_text(state: &BotState, chat_id: i64, text: &str) -> Option<Reply> {
    if text.trim().is_empty() {
        return None;
    }
    Some(build_convo(state, chat_id, "voice", "memo", text, "").await)
}

/// Summarize, save (SQLite index + vault file) and build the reply.
async fn build_convo(state: &BotState, chat_id: i64, kind: &str, source: &str, transcript: &str, note: &str) -> Reply {
    let (title, summary, actions) = match convo::summarize(kind, transcript, note).await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("convo summarize failed: {e}");
            let mut shown = transcript.to_string();
            if shown.len() > 3000 {
                let mut cut = 3000;
                while !shown.is_char_boundary(cut) {
                    cut -= 1;
                }
                shown.truncate(cut);
                shown.push('…');
            }
            let reply = Reply::text(format!("Couldn't summarize that — here's the transcript instead:\n\n{shown}"));
            return reply;
        }
    };
    let saved = convo::ConvoNote {
        chat_id,
        kind: kind.to_string(),
        title,
        summary,
        actions,
        transcript: transcript.to_string(),
        note: note.to_string(),
        source: source.to_string(),
        created_at: Utc::now().to_rfc3339(),
        ..Default::default()
    };
    convo::global().add(saved.clone());
    // The Markdown copy is what you'll actually read later — in Obsidian.
    let file = crate::vault::write_summary(crate::vault::NewSummary {
        title: saved.title.clone(),
        kind: kind.to_string(),
        source: source.to_string(),
        note: note.to_string(),
        summary: saved.summary.clone(),
        actions: saved.actions.clone(),
        transcript: transcript.to_string(),
    });
    let tz = tz_for(state, chat_id);
    let mut reply = Reply::rich(convo::render_html(&saved, &tz), convo::render_text(&saved));
    match file {
        Ok(doc) => {
            if let Some(h) = reply.rich_html.as_mut() {
                h.push_str(&format!("<i>📁 {}</i>", crate::vault::html_escape(&doc.rel)));
            }
            reply.text.push_str(&format!("\n📁 {}", doc.rel));
        }
        Err(e) => tracing::warn!("vault summary write failed: {e}"),
    }
    reply
}

/// Escape the characters Telegram's (legacy) Markdown treats as markup.
pub(super) fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '_' | '*' | '`' | '[') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `/summaries` — the 10 newest saved summaries, numbered for `/summary N`.
pub(super) fn summaries_reply(_state: &BotState, chat_id: i64) -> Reply {
    let notes = convo::global().recent(chat_id, 10);
    if notes.is_empty() {
        return Reply::text(format!(
            "No summaries yet. Forward me some messages, or send a voice memo of {}s or longer.",
            voice_memo_secs()
        ));
    }
    let mut text = String::from("*Summaries* (newest first)\n");
    for (i, n) in notes.iter().enumerate() {
        let icon = if n.kind == "voice" { "🎤" } else { "💬" };
        text.push_str(&format!("{}. {icon} {} — _{}_\n", i + 1, md_escape(&n.title), md_escape(&n.source)));
    }
    text.push_str("\nOpen one with `/summary <number>`.");
    Reply::text(text)
}

/// `/summary [N]` — show one saved summary in full (bare `/summary` = newest).
pub(super) fn summary_reply(state: &BotState, chat_id: i64, body: &str) -> Reply {
    let n = body
        .split_whitespace()
        .find_map(|w| w.trim_start_matches('#').parse::<usize>().ok())
        .unwrap_or(1);
    match convo::global().nth(chat_id, n) {
        Some(note) => {
            let tz = tz_for(state, chat_id);
            Reply::rich(convo::render_html(&note, &tz), convo::render_text(&note))
        }
        None => Reply::text(format!("No summary #{n}. See /summaries for the list.")),
    }
}
