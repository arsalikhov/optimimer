//! Non-text inputs: voice notes (transcribe → route or memo), photos (read by a
//! vision model → a turn for the agent, or straight to the ledger when it's a
//! bare receipt), CSV statements.

use super::*;

/// Download a Telegram audio file and transcribe it. Errors are already
/// phrased for the user.
pub(super) async fn fetch_transcript(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    file_id: &str,
) -> Result<String, String> {
    let file_path = get_file_path(client, api, file_id)
        .await
        .ok_or_else(|| "Couldn't fetch that voice message.".to_string())?;
    let url = format!("https://api.telegram.org/file/bot{token}/{file_path}");
    let audio = match client.get(&url).send().await {
        Ok(r) => r.bytes().await.map(|b| b.to_vec()).ok(),
        Err(_) => None,
    }
    .ok_or_else(|| "Couldn't download the audio.".to_string())?;
    let format = file_path
        .rsplit('.')
        .next()
        .filter(|e| !e.is_empty())
        .unwrap_or("ogg")
        .to_lowercase();
    let transcript = crate::transcribe::transcribe(audio, &format)
        .await
        .map_err(|e| format!("Transcription failed: {e}"))?;
    if transcript.trim().is_empty() {
        return Err("I didn't catch that — try again?".to_string());
    }
    Ok(transcript)
}

/// Recordings at least this long are memos to summarize, not commands to route.
pub(super) fn voice_memo_secs() -> u64 {
    std::env::var("VOICE_MEMO_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(45)
}

/// Spoken openers that mark a recording as a memo regardless of length. The
/// trigger is stripped so it doesn't end up in the transcript.
const MEMO_TRIGGERS: &[&str] = &["note to self", "voice note", "voice memo", "memo"];

/// If the transcript opens with a memo trigger, return the rest of it.
pub(super) fn memo_by_keyword(transcript: &str) -> Option<String> {
    let t = transcript.trim();
    let lower = t.to_lowercase();
    for trig in MEMO_TRIGGERS {
        if let Some(rest) = lower.strip_prefix(trig) {
            // Only a whole-word match: "memorandum from…" isn't a trigger.
            if !rest.starts_with(|c: char| c.is_alphanumeric()) {
                let rest = t[trig.len()..].trim_start_matches(|c: char| !c.is_alphanumeric());
                return Some(if rest.is_empty() { t.to_string() } else { rest.to_string() });
            }
        }
    }
    None
}

/// Jev must be this sure a short recording is a memo before it skips the agent.
const MEMO_MIN: f64 = 0.8;

/// Is this transcript a thought-dump to keep (a memo) rather than a request
/// for the assistant? False when Jev is off or unsure — the agent can still
/// call `save_memo` itself.
async fn is_thought_dump(transcript: &str) -> bool {
    let q = crate::jev::Q::noul(
        "The user sent this voice note to their personal assistant. Is it a thought-dump to be kept as a memo, rather than a request or question for the assistant to act on?",
        "Rambling thoughts, ideas, reflections or notes to self, with nothing asked of the assistant",
        "A request, command or question for the assistant (add a task, set a reminder, log spending, look something up, …)",
    );
    match crate::jev::noul(json!(transcript), q).await {
        Some(p) => {
            tracing::info!("voice memo check: {p:.2}");
            p >= MEMO_MIN
        }
        None => false,
    }
}

pub(super) fn fmt_duration(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Hold a transcribed recording for the rest of this turn, so that whatever the
/// turn saves can file the words verbatim under `transcripts/`.
fn hold_voice(state: &BotState, chat_id: i64, text: &str, source: &str) {
    if let Ok(mut slot) = state.voice.lock() {
        slot.insert(chat_id, VoiceTake { text: text.to_string(), source: source.to_string() });
    }
}

/// Claim the recording behind this turn, if there was one — called by the code
/// that has just written a note or a summary. Taking it clears the slot, so one
/// recording is filed once even when a turn saves several things.
pub(super) fn take_voice(state: &BotState, chat_id: i64) -> Option<VoiceTake> {
    state.voice.lock().ok()?.remove(&chat_id)
}

/// Drop an unclaimed recording at the end of a turn: a spoken command is not
/// something to keep.
fn drop_voice(state: &BotState, chat_id: i64) {
    if let Ok(mut slot) = state.voice.lock() {
        slot.remove(&chat_id);
    }
}

/// Download a voice note and transcribe it, then decide between "memo" (summarize
/// and save a note) and a conversation turn for the agent. Memo gates: a spoken
/// opener ("memo …", "note to self …") or a recording at least `VOICE_MEMO_SECS`
/// long; anything else goes to the agent, which can still call `save_memo`.
pub(super) async fn handle_voice(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    state: &BotState,
    chat_id: i64,
    file_id: &str,
    duration_secs: u64,
) {
    let transcript = match fetch_transcript(client, api, token, file_id).await {
        Ok(t) => t,
        Err(e) => return send(client, api, chat_id, &Reply::text(e)).await,
    };
    let source = format!("voice memo, {}", fmt_duration(duration_secs));
    // Offered to every path below; only one that saves a note or summary keeps it.
    hold_voice(state, chat_id, &transcript, &source);

    if let Some(body) = memo_by_keyword(&transcript) {
        return finalize_convo(client, api, state, chat_id, "voice", &source, &body, "").await;
    }
    if duration_secs >= voice_memo_secs() {
        return finalize_convo(client, api, state, chat_id, "voice", &source, &transcript, "").await;
    }
    // No opener and short: Jev tells a spoken thought-dump from a request.
    if is_thought_dump(&transcript).await {
        return finalize_convo(client, api, state, chat_id, "voice", &source, &transcript, "").await;
    }

    // Everything else is a normal conversation turn: echo the transcript so
    // the user can see what was heard, then let the agent act on it.
    send(client, api, chat_id, &Reply::text(format!("_{transcript}_"))).await;
    if let Some(reply) = run_agent(client, api, state, chat_id, &transcript).await {
        send(client, api, chat_id, &reply).await;
    }
    drop_voice(state, chat_id);
}

/// Download a Telegram file's bytes by file_id (getFile → download URL).
pub(super) async fn download_file(
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

/// Hold the photo behind this turn, so whatever the turn saves can file the
/// image itself in the vault next to the text. Mirrors `hold_voice`.
fn hold_photo(state: &BotState, chat_id: i64, bytes: &[u8], mime: &str) {
    if let Ok(mut slot) = state.photo.lock() {
        slot.insert(chat_id, PhotoTake { bytes: bytes.to_vec(), mime: mime.to_string() });
    }
}

/// Claim the photo behind this turn — called by the code that has just written
/// a note, a task or a summary. Taking it clears the slot, so one photo is
/// filed once even when a turn saves several things.
pub(super) fn take_photo(state: &BotState, chat_id: i64) -> Option<PhotoTake> {
    state.photo.lock().ok()?.remove(&chat_id)
}

/// Drop an unclaimed photo at the end of a turn: a picture the user only asked
/// a question about is not something to keep.
fn drop_photo(state: &BotState, chat_id: i64) {
    if let Ok(mut slot) = state.photo.lock() {
        slot.remove(&chat_id);
    }
}

/// File the photo behind this turn alongside the doc it became, so the note,
/// task or summary shows the picture it was made from. A no-op when the turn
/// had no photo, which is the usual case.
pub(super) fn file_photo(state: &BotState, chat_id: i64, doc: &crate::vault::Doc) {
    let Some(p) = take_photo(state, chat_id) else { return };
    if let Err(e) = crate::vault::write_photo(crate::vault::NewPhoto {
        title: doc.title(),
        linked: doc.rel.clone(),
        bytes: p.bytes,
        mime: p.mime,
    }) {
        tracing::warn!("vault photo write failed: {e}");
    }
}

/// A photo the user sent: read it with a vision model, then hand the reading to
/// the agent as an ordinary turn, so the picture can become a note, a task, a
/// memory, a summary or an answer — whatever the caption asks for.
///
/// The one shortcut is a receipt sent on its own, with nothing said about it:
/// that still goes straight to the ledger, the way it always has. A receipt
/// *with* a caption goes to the agent instead, which gets the same
/// `Spent … at … on …` line and can log it or do what was actually asked.
pub(super) async fn handle_photo(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    state: &BotState,
    chat_id: i64,
    file_id: &str,
    mime: &str,
    caption: &str,
) {
    let bytes = match download_file(client, api, token, file_id).await {
        Some(b) => b,
        None => return send(client, api, chat_id, &Reply::text("Couldn't download that image.")).await,
    };
    typing(client, api, chat_id).await;
    let look = match crate::vision::look(&bytes, mime, caption).await {
        Ok(l) => l,
        Err(e) => return send(client, api, chat_id, &Reply::text(format!("Couldn't read that image: {e}"))).await,
    };

    if look.is_receipt() && caption.trim().is_empty() {
        send(client, api, chat_id, &Reply::text(format!("_{}_", look.expense))).await;
        let reply = handle_money(state, chat_id, "spent", &look.expense).await;
        return send(client, api, chat_id, &reply).await;
    }

    // Offered to every tool the turn calls; only one that saves a file keeps it.
    hold_photo(state, chat_id, &bytes, mime);
    let turn = photo_turn(&look, caption);
    if let Some(reply) = run_agent(client, api, state, chat_id, &turn).await {
        send(client, api, chat_id, &reply).await;
    }
    drop_photo(state, chat_id);
}

/// The message the agent sees for a photo. The model can't see the image, so
/// the description stands in for it — marked as a description, not as the
/// user's own words, and with the caption kept separate underneath.
fn photo_turn(look: &crate::vision::Look, caption: &str) -> String {
    let mut t = format!(
        "[The user sent a photo ({}). You can't see it; this is a description of what it shows, \
         not something they said:\n{}\n]",
        look.kind,
        look.text.trim()
    );
    if !look.expense.is_empty() {
        t.push_str(&format!("\n[It reads as a receipt for: {}]", look.expense));
    }
    let caption = caption.trim();
    if caption.is_empty() {
        t.push_str("\n\n(Sent without a caption.)");
    } else {
        t.push_str(&format!("\n\n{caption}"));
    }
    t
}

/// A photo inside a forwarded batch: describe it so the summary is about what
/// was actually shared, not about "[photo]". Failure degrades to a marker —
/// one unreadable image shouldn't sink the batch.
pub(super) async fn describe_forward_photo(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    file_id: &str,
    caption: &str,
) -> String {
    let Some(bytes) = download_file(client, api, token, file_id).await else {
        return "[photo — couldn't download it]".to_string();
    };
    match crate::vision::look(&bytes, "image/jpeg", caption).await {
        Ok(look) => {
            let mut t = format!("🖼 {}", look.text.trim());
            if !caption.trim().is_empty() {
                t.push_str(&format!("\n{}", caption.trim()));
            }
            t
        }
        Err(e) => {
            tracing::warn!("forwarded photo unreadable: {e}");
            format!("[photo — {e}]")
        }
    }
}

/// CSV bank/credit-card statement → bulk import into Finances, skipping rows whose
/// fingerprint already exists (so re-imports don't duplicate).
pub(super) async fn handle_csv(
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
    let mut account = crate::finance::detect_account(file_name, caption, &csv);
    if account.is_empty() && crate::jev::enabled() {
        account = crate::finance::infer_account(&csv).await;
    }
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
                    "\n⚠️ {} row{} flagged as a *likely duplicate* (same amount as an existing transaction) — see /transactions to review and remove it.",
                    s.flagged,
                    if s.flagged == 1 { "" } else { "s" }
                ));
            }
            if s.uncertain > 0 {
                msg.push_str(&format!(
                    "\n❓ {} row{} with an *unsure category* — tagged in the note; see /transactions.",
                    s.uncertain,
                    if s.uncertain == 1 { "" } else { "s" }
                ));
            }
            Reply::text(msg)
        }
        Err(e) => Reply::text(format!("CSV import failed: {e}")),
    };
    send(client, api, chat_id, &reply).await;
}

pub(super) async fn get_file_path(client: &reqwest::Client, api: &str, file_id: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_photo_turn_keeps_the_reading_apart_from_the_caption() {
        let board = crate::vision::Look {
            kind: "whiteboard".into(),
            text: "Three columns: To do, Doing, Done.".into(),
            expense: String::new(),
        };
        let turn = photo_turn(&board, "save this");
        assert!(turn.contains("Three columns"), "the reading is there: {turn}");
        assert!(turn.trim_end().ends_with("save this"), "the caption is last, unbracketed: {turn}");

        let receipt = crate::vision::Look {
            kind: "receipt".into(),
            text: "A till receipt from Joe's.".into(),
            expense: "Spent 12.00 at Joe's on 2026-09-18".into(),
        };
        let turn = photo_turn(&receipt, "");
        assert!(turn.contains("Spent 12.00 at Joe's on 2026-09-18"), "the agent can still log it: {turn}");
        assert!(turn.contains("without a caption"), "and knows nothing was asked: {turn}");
    }

    #[test]
    fn memo_keyword_is_whole_word_and_stripped() {
        assert_eq!(memo_by_keyword("Memo: call the bank tomorrow").as_deref(), Some("call the bank tomorrow"));
        assert_eq!(memo_by_keyword("note to self, buy a new charger").as_deref(), Some("buy a new charger"));
        assert_eq!(memo_by_keyword("memorandum from legal"), None);
        assert_eq!(memo_by_keyword("remind me to call Sam"), None);
        assert_eq!(memo_by_keyword("memo").as_deref(), Some("memo"));
    }
}
