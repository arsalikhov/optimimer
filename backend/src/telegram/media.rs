//! Non-text inputs: voice notes (transcribe → route or memo), receipt photos (OCR → /spent), CSV statements.

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

/// Receipt/invoice image → OCR to a "Spent X at Y on Z" line → run `/spent`,
/// which parses + categorizes it like any typed expense.
pub(super) async fn handle_receipt(
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
    fn memo_keyword_is_whole_word_and_stripped() {
        assert_eq!(memo_by_keyword("Memo: call the bank tomorrow").as_deref(), Some("call the bank tomorrow"));
        assert_eq!(memo_by_keyword("note to self, buy a new charger").as_deref(), Some("buy a new charger"));
        assert_eq!(memo_by_keyword("memorandum from legal"), None);
        assert_eq!(memo_by_keyword("remind me to call Sam"), None);
        assert_eq!(memo_by_keyword("memo").as_deref(), Some("memo"));
    }
}
