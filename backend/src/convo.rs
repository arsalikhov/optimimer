//! Conversation notes: a summary + transcript built from a batch of forwarded
//! Telegram messages, or from a long voice memo. Local-only — rows live in the
//! shared SQLite db (`convo_notes`) as an index; the Markdown copy goes to the vault.
//!
//! Forwarded messages arrive one update at a time, so this module also owns the
//! per-chat batching: each forward bumps a generation counter and the bot arms a
//! short settle timer; when the timer fires with the generation unchanged the
//! batch is complete. A note (the user's own comment) can be attached before,
//! during, or after the batch.

use crate::db::Db;
use anyhow::{anyhow, Result};
use chrono::{TimeZone, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Stored notes
// ---------------------------------------------------------------------------

/// One saved summary. `kind` is "forward" (a batch of forwarded messages) or
/// "voice" (a transcribed voice memo).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConvoNote {
    pub id: String,
    pub chat_id: i64,
    pub kind: String,
    pub title: String,
    /// Key points, one per entry.
    #[serde(default)]
    pub summary: Vec<String>,
    /// Action items / follow-ups, one per entry (may be empty).
    #[serde(default)]
    pub actions: Vec<String>,
    /// Full transcript (forward batch: one line per message; voice: the text).
    pub transcript: String,
    /// The user's own note/context that accompanied the forwards (may be empty).
    #[serde(default)]
    pub note: String,
    /// Short human description of the source, e.g. "4 messages from Alice, Bob".
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub created_at: String,
}

/// SQLite-backed note store (table `convo_notes`), mirroring `Shopper`.
#[derive(Clone)]
pub struct Convos {
    db: Db,
}

static GLOBAL: OnceLock<Convos> = OnceLock::new();

/// Initialise the process-wide store. Call once at startup.
pub fn init(db: Db) -> Convos {
    let c = Convos { db };
    let _ = GLOBAL.set(c.clone());
    c
}

/// The process-wide store; falls back to an in-memory db if `init` was never
/// called (keeps handlers safe in tests).
pub fn global() -> Convos {
    GLOBAL
        .get_or_init(|| Convos {
            db: Db::memory().expect("in-memory convo db"),
        })
        .clone()
}

impl Convos {
    /// Persist a note, assigning id/created_at if absent. Returns the id.
    pub fn add(&self, mut note: ConvoNote) -> String {
        if note.id.is_empty() {
            note.id = Uuid::new_v4().to_string();
        }
        if note.created_at.is_empty() {
            note.created_at = Utc::now().to_rfc3339();
        }
        if let Ok(json) = serde_json::to_string(&note) {
            let conn = self.db.lock();
            let _ = conn.execute(
                "INSERT INTO convo_notes (id, chat_id, json, created) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json",
                params![note.id, note.chat_id, json, note.created_at],
            );
        }
        note.id
    }

    /// Newest-first notes for a chat.
    pub fn recent(&self, chat_id: i64, limit: usize) -> Vec<ConvoNote> {
        let conn = self.db.lock();
        let mut stmt = match conn.prepare(
            "SELECT json FROM convo_notes WHERE chat_id = ?1 ORDER BY created DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![chat_id, limit as i64], |r| r.get::<_, String>(0))
            .map(|rows| {
                rows.filter_map(|r| r.ok())
                    .filter_map(|j| serde_json::from_str(&j).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The `n`-th newest note (1-based, as shown by `/summaries`).
    pub fn nth(&self, chat_id: i64, n: usize) -> Option<ConvoNote> {
        if n == 0 {
            return None;
        }
        self.recent(chat_id, n).into_iter().nth(n - 1)
    }
}

// ---------------------------------------------------------------------------
// Forward batching
// ---------------------------------------------------------------------------

/// One forwarded message, already reduced to text (voice notes transcribed).
#[derive(Debug, Clone)]
pub struct FwdItem {
    pub author: String,
    /// Local time of the original message, "HH:MM" (or "" if unknown).
    pub at: String,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct ForwardBatch {
    pub items: Vec<FwdItem>,
    pub note: Option<String>,
    /// Bumped on every arrival; a settle timer only fires if it still matches.
    gen: u64,
    /// Set once the batch settled without a note and we asked the user for one.
    awaiting_note: bool,
    /// When the batch was opened / last touched, to expire abandoned prompts.
    touched: Option<Instant>,
}

impl ForwardBatch {
    /// "4 messages from Alice, Bob" — used in prompts and as the stored source.
    pub fn describe(&self) -> String {
        let mut authors: Vec<&str> = Vec::new();
        for it in &self.items {
            if !authors.contains(&it.author.as_str()) {
                authors.push(&it.author);
            }
        }
        let n = self.items.len();
        let noun = if n == 1 { "message" } else { "messages" };
        if authors.is_empty() {
            format!("{n} forwarded {noun}")
        } else {
            let shown: Vec<&str> = authors.iter().take(3).copied().collect();
            let more = if authors.len() > 3 { ", …" } else { "" };
            format!("{n} forwarded {noun} from {}{more}", shown.join(", "))
        }
    }

    /// Plain transcript: one `[HH:MM] Author: text` line per message.
    pub fn transcript(&self) -> String {
        self.items
            .iter()
            .map(|it| {
                if it.at.is_empty() {
                    format!("{}: {}", it.author, it.text)
                } else {
                    format!("[{}] {}: {}", it.at, it.author, it.text)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

static BATCHES: OnceLock<Mutex<HashMap<i64, ForwardBatch>>> = OnceLock::new();

fn batches() -> &'static Mutex<HashMap<i64, ForwardBatch>> {
    BATCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How long to wait after the last forward before treating the batch as
/// complete. Forwards from one "send" land within a second of each other.
pub fn settle_delay() -> Duration {
    let secs = std::env::var("FORWARD_SETTLE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(4);
    Duration::from_secs(secs.max(1))
}

/// How long an unanswered "what are these about?" prompt stays live before the
/// batch is dropped and the next plain message is routed normally again.
fn note_ttl() -> Duration {
    let secs = std::env::var("FORWARD_NOTE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(600);
    Duration::from_secs(secs)
}

/// Extra seconds to long-poll after a plain text message, to catch a forward
/// batch whose comment Telegram delivered first. 0 disables the peek.
pub fn peek_secs() -> u64 {
    std::env::var("FORWARD_PEEK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1)
}

/// Mark a forward as "arriving" for this chat: opens the batch if needed and
/// bumps the generation so an in-flight settle timer stands down (the item may
/// still need transcribing before it can be pushed). Returns the generation.
pub fn touch(chat_id: i64) -> u64 {
    let mut map = batches().lock().unwrap_or_else(|e| e.into_inner());
    let b = map.entry(chat_id).or_default();
    b.gen += 1;
    b.awaiting_note = false;
    b.touched = Some(Instant::now());
    b.gen
}

/// Append a reduced forward to the chat's batch. Returns the generation the
/// caller should arm its settle timer with.
pub fn push(chat_id: i64, item: FwdItem) -> u64 {
    let mut map = batches().lock().unwrap_or_else(|e| e.into_inner());
    let b = map.entry(chat_id).or_default();
    b.items.push(item);
    b.gen += 1;
    b.touched = Some(Instant::now());
    b.gen
}

/// Open a batch with the user's comment before any forward has been pushed
/// (the client sent the comment first and the forwards are queued behind it).
pub fn open_with_note(chat_id: i64, note: &str) {
    let mut map = batches().lock().unwrap_or_else(|e| e.into_inner());
    let b = map.entry(chat_id).or_default();
    b.note = Some(note.trim().to_string());
    b.gen += 1;
    b.touched = Some(Instant::now());
}

/// Outcome of a settle timer firing.
pub enum Settled {
    /// Another forward arrived after the timer was armed; do nothing.
    Stale,
    /// Batch complete and it has a note → summarize now.
    Ready(ForwardBatch),
    /// Batch complete but no note → ask the user (batch stays parked).
    NeedNote(String),
}

/// Called when a settle timer fires. Only acts if `gen` is still current.
pub fn settle(chat_id: i64, gen: u64) -> Settled {
    let mut map = batches().lock().unwrap_or_else(|e| e.into_inner());
    let Some(b) = map.get_mut(&chat_id) else { return Settled::Stale };
    if b.gen != gen || b.items.is_empty() {
        return Settled::Stale;
    }
    if b.note.is_some() {
        return Settled::Ready(map.remove(&chat_id).unwrap_or_default());
    }
    b.awaiting_note = true;
    b.touched = Some(Instant::now());
    Settled::NeedNote(b.describe())
}

/// What a plain (non-forward) text message means for an open batch.
pub enum NoteAttach {
    /// No batch is open — route the text normally.
    None,
    /// Batch is still collecting; the note is stored and the timer will finish it.
    Queued,
    /// We were waiting for exactly this note → summarize now.
    Ready(ForwardBatch),
}

/// Offer a plain text message as the note for this chat's open batch.
pub fn attach_note(chat_id: i64, text: &str) -> NoteAttach {
    let mut map = batches().lock().unwrap_or_else(|e| e.into_inner());
    let Some(b) = map.get_mut(&chat_id) else { return NoteAttach::None };
    if b.items.is_empty() {
        // A comment-only batch that never received forwards — discard it.
        map.remove(&chat_id);
        return NoteAttach::None;
    }
    let expired = b.touched.map(|t| t.elapsed() > note_ttl()).unwrap_or(true);
    if b.awaiting_note && expired {
        map.remove(&chat_id);
        return NoteAttach::None;
    }
    b.note = Some(text.trim().to_string());
    if b.awaiting_note {
        return NoteAttach::Ready(map.remove(&chat_id).unwrap_or_default());
    }
    NoteAttach::Queued
}

/// Remove and return the chat's parked batch (for the Summarize/Discard buttons).
pub fn take(chat_id: i64) -> Option<ForwardBatch> {
    let mut map = batches().lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&chat_id).filter(|b| !b.items.is_empty())
}

// ---------------------------------------------------------------------------
// Telegram message helpers
// ---------------------------------------------------------------------------

/// Is this update's message a forward? Covers Bot API 7+ `forward_origin` and
/// the legacy `forward_*` fields.
pub fn is_forward(msg: &Value) -> bool {
    !msg["forward_origin"].is_null()
        || !msg["forward_from"].is_null()
        || !msg["forward_sender_name"].is_null()
        || !msg["forward_from_chat"].is_null()
        || !msg["forward_date"].is_null()
}

/// Is the message a voice/audio/video note (own or forwarded)?
pub fn audio_file_id(msg: &Value) -> Option<&str> {
    ["voice", "audio", "video_note"]
        .iter()
        .find_map(|k| msg.get(*k).filter(|v| !v.is_null()))
        .and_then(|v| v["file_id"].as_str())
}

fn user_name(u: &Value) -> String {
    let first = u["first_name"].as_str().unwrap_or("").trim();
    let last = u["last_name"].as_str().unwrap_or("").trim();
    let name = format!("{first} {last}").trim().to_string();
    if !name.is_empty() {
        return name;
    }
    u["username"].as_str().map(|s| format!("@{s}")).unwrap_or_else(|| "Unknown".into())
}

/// Original author of a forwarded message, best effort.
pub fn forward_author(msg: &Value) -> String {
    let o = &msg["forward_origin"];
    if !o.is_null() {
        let by_type = match o["type"].as_str().unwrap_or("") {
            "user" => Some(user_name(&o["sender_user"])),
            "hidden_user" => o["sender_user_name"].as_str().map(str::to_string),
            "chat" => o["sender_chat"]["title"].as_str().map(str::to_string),
            "channel" => {
                let chat = o["chat"]["title"].as_str().unwrap_or("channel");
                Some(match o["author_signature"].as_str() {
                    Some(sig) if !sig.is_empty() => format!("{sig} ({chat})"),
                    _ => chat.to_string(),
                })
            }
            _ => None,
        };
        if let Some(n) = by_type.filter(|n| !n.trim().is_empty()) {
            return n;
        }
    }
    if !msg["forward_from"].is_null() {
        return user_name(&msg["forward_from"]);
    }
    if let Some(n) = msg["forward_sender_name"].as_str() {
        return n.to_string();
    }
    if let Some(t) = msg["forward_from_chat"]["title"].as_str() {
        return t.to_string();
    }
    "Unknown".into()
}

/// "HH:MM" of the original message in `tz_name`, or "" if no date is present.
pub fn forward_time(msg: &Value, tz_name: &str) -> String {
    let unix = msg["forward_origin"]["date"]
        .as_i64()
        .or_else(|| msg["forward_date"].as_i64())
        .or_else(|| msg["date"].as_i64());
    let Some(unix) = unix else { return String::new() };
    match tz_name.parse::<chrono_tz::Tz>() {
        Ok(tz) => tz
            .timestamp_opt(unix, 0)
            .single()
            .map(|d| d.format("%H:%M").to_string())
            .unwrap_or_default(),
        Err(_) => Utc
            .timestamp_opt(unix, 0)
            .single()
            .map(|d| d.format("%H:%M").to_string())
            .unwrap_or_default(),
    }
}

/// Text content of a message: text, caption, or a bracketed placeholder for
/// media we don't read. (Audio is handled by the caller via `audio_file_id`.)
pub fn message_body(msg: &Value) -> String {
    if let Some(t) = msg["text"].as_str().filter(|t| !t.trim().is_empty()) {
        return t.trim().to_string();
    }
    let caption = msg["caption"].as_str().unwrap_or("").trim();
    let media = if !msg["photo"].is_null() {
        "[photo]"
    } else if !msg["video"].is_null() {
        "[video]"
    } else if !msg["sticker"].is_null() {
        "[sticker]"
    } else if !msg["document"].is_null() {
        return match msg["document"]["file_name"].as_str() {
            Some(n) if caption.is_empty() => format!("[file: {n}]"),
            Some(n) => format!("[file: {n}] {caption}"),
            None if caption.is_empty() => "[file]".into(),
            None => format!("[file] {caption}"),
        };
    } else if !msg["location"].is_null() {
        "[location]"
    } else if !msg["contact"].is_null() {
        "[contact]"
    } else if !msg["poll"].is_null() {
        return format!("[poll] {}", msg["poll"]["question"].as_str().unwrap_or(""));
    } else {
        "[message]"
    };
    if caption.is_empty() {
        media.to_string()
    } else {
        format!("{media} {caption}")
    }
}

// ---------------------------------------------------------------------------
// Summarization + rendering
// ---------------------------------------------------------------------------

fn model() -> String {
    std::env::var("CONVO_MODEL").unwrap_or_else(|_| "anthropic/claude-sonnet-4.6".to_string())
}

/// Ask the model for a title, key points, and action items. `note` is the
/// user's own framing (what the material is / what they want from it) and
/// steers the summary when present.
pub async fn summarize(kind: &str, transcript: &str, note: &str) -> Result<(String, Vec<String>, Vec<String>)> {
    let what = match kind {
        "voice" => "a transcript of a voice memo the user recorded",
        _ => "a transcript of chat messages the user forwarded (one message per line, '[time] Author: text')",
    };
    let system = format!(
        "You summarize {what}. Output ONLY minified JSON — no prose, no code fences — with keys: \
         title (short, specific, max 8 words), summary (array of 2-7 concise bullet strings covering \
         the key points, decisions and context — write them as complete, self-contained statements), \
         actions (array of follow-ups / to-dos / open questions mentioned or clearly implied; empty \
         array if none). Keep the user's language (reply in the language the transcript is in). \
         If the user supplied a NOTE, treat it as the brief: prioritise what it asks for and answer \
         any question it poses inside the summary bullets."
    );
    let mut prompt = String::new();
    if !note.trim().is_empty() {
        prompt.push_str("NOTE FROM USER:\n");
        prompt.push_str(note.trim());
        prompt.push_str("\n\n");
    }
    prompt.push_str("TRANSCRIPT:\n");
    prompt.push_str(transcript.trim());

    let raw = crate::openrouter::chat(&model(), &system, &prompt).await?;
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    // Be tolerant of a stray sentence before the JSON.
    let json_str = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(a), Some(b)) if b > a => &trimmed[a..=b],
        _ => trimmed,
    };
    let v: Value = serde_json::from_str(json_str).map_err(|e| anyhow!("bad summary JSON ({e}): {raw}"))?;
    let title = v["title"].as_str().unwrap_or("Summary").trim().to_string();
    let summary = str_vec(&v["summary"]);
    let actions = str_vec(&v["actions"]);
    if summary.is_empty() {
        return Err(anyhow!("model returned no summary bullets: {raw}"));
    }
    Ok((title, summary, actions))
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

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn created_label(note: &ConvoNote, tz_name: &str) -> String {
    let parsed = chrono::DateTime::parse_from_rfc3339(&note.created_at).ok();
    let Some(dt) = parsed else { return String::new() };
    match tz_name.parse::<chrono_tz::Tz>() {
        Ok(tz) => dt.with_timezone(&tz).format("%b %-d, %H:%M").to_string(),
        Err(_) => dt.format("%b %-d, %H:%M UTC").to_string(),
    }
}

/// Rich (HTML) rendering: heading, source line, bullets, actions, and the
/// transcript folded into a collapsible block.
pub fn render_html(note: &ConvoNote, tz_name: &str) -> String {
    let mut h = format!("<h3>{}</h3>", html_escape(&note.title));
    let mut meta = html_escape(&note.source);
    let when = created_label(note, tz_name);
    if !when.is_empty() {
        meta.push_str(" · ");
        meta.push_str(&when);
    }
    h.push_str(&format!("<blockquote>{meta}</blockquote>"));
    if !note.note.trim().is_empty() {
        h.push_str(&format!("<p><i>Note: {}</i></p>", html_escape(&note.note)));
    }
    h.push_str("<ul>");
    for b in &note.summary {
        h.push_str(&format!("<li>{}</li>", html_escape(b)));
    }
    h.push_str("</ul>");
    if !note.actions.is_empty() {
        h.push_str("<b>Action items</b><ul>");
        for a in &note.actions {
            h.push_str(&format!("<li>{}</li>", html_escape(a)));
        }
        h.push_str("</ul>");
    }
    let lines: Vec<&str> = note.transcript.lines().filter(|l| !l.trim().is_empty()).collect();
    let label = if note.kind == "voice" {
        "Transcript".to_string()
    } else {
        format!("Transcript ({} messages)", lines.len())
    };
    h.push_str(&format!("<details><summary>{label}</summary>"));
    for l in lines {
        h.push_str(&format!("<p>{}</p>", html_escape(l)));
    }
    h.push_str("</details>");
    h
}

/// Plain-text (Markdown) fallback, transcript truncated to stay under Telegram's
/// 4096-char message limit.
pub fn render_text(note: &ConvoNote) -> String {
    let mut t = format!("*{}*\n_{}_\n", note.title, note.source);
    if !note.note.trim().is_empty() {
        t.push_str(&format!("Note: {}\n", note.note));
    }
    for b in &note.summary {
        t.push_str(&format!("• {b}\n"));
    }
    if !note.actions.is_empty() {
        t.push_str("\nAction items:\n");
        for a in &note.actions {
            t.push_str(&format!("☐ {a}\n"));
        }
    }
    t.push_str("\nTranscript:\n");
    let budget = 3500usize.saturating_sub(t.len());
    if note.transcript.len() > budget {
        let mut cut = budget;
        while !note.transcript.is_char_boundary(cut) {
            cut -= 1;
        }
        t.push_str(&note.transcript[..cut]);
        t.push('…');
    } else {
        t.push_str(&note.transcript);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_forwards_old_and_new_shapes() {
        assert!(is_forward(&json!({"forward_origin": {"type": "user", "date": 1, "sender_user": {"first_name": "A"}}})));
        assert!(is_forward(&json!({"forward_from": {"first_name": "A"}, "forward_date": 1})));
        assert!(!is_forward(&json!({"text": "hi"})));
    }

    #[test]
    fn author_from_each_origin_type() {
        let user = json!({"forward_origin": {"type": "user", "sender_user": {"first_name": "Ada", "last_name": "L"}}});
        assert_eq!(forward_author(&user), "Ada L");
        let hidden = json!({"forward_origin": {"type": "hidden_user", "sender_user_name": "Bob"}});
        assert_eq!(forward_author(&hidden), "Bob");
        let chan = json!({"forward_origin": {"type": "channel", "chat": {"title": "News"}, "author_signature": "Ed"}});
        assert_eq!(forward_author(&chan), "Ed (News)");
        let legacy = json!({"forward_sender_name": "Cy"});
        assert_eq!(forward_author(&legacy), "Cy");
    }

    #[test]
    fn body_prefers_text_then_caption_then_placeholder() {
        assert_eq!(message_body(&json!({"text": " hi "})), "hi");
        assert_eq!(message_body(&json!({"photo": [{}], "caption": "look"})), "[photo] look");
        assert_eq!(message_body(&json!({"sticker": {}})), "[sticker]");
        assert_eq!(message_body(&json!({"document": {"file_name": "a.pdf"}})), "[file: a.pdf]");
    }

    #[test]
    fn batch_lifecycle_note_after() {
        let chat = 9001;
        let g = touch(chat);
        let g2 = push(chat, FwdItem { author: "A".into(), at: "10:00".into(), text: "x".into() });
        assert!(g2 > g);
        assert!(matches!(settle(chat, g), Settled::Stale), "old generation must not settle");
        match settle(chat, g2) {
            Settled::NeedNote(desc) => assert!(desc.contains("1 forwarded message from A")),
            _ => panic!("expected NeedNote"),
        }
        match attach_note(chat, "planning") {
            NoteAttach::Ready(b) => {
                assert_eq!(b.note.as_deref(), Some("planning"));
                assert_eq!(b.transcript(), "[10:00] A: x");
            }
            _ => panic!("expected Ready"),
        }
        assert!(take(chat).is_none());
    }

    #[test]
    fn batch_lifecycle_note_before() {
        let chat = 9002;
        open_with_note(chat, "context");
        touch(chat);
        let g = push(chat, FwdItem { author: "B".into(), at: "".into(), text: "y".into() });
        match settle(chat, g) {
            Settled::Ready(b) => {
                assert_eq!(b.note.as_deref(), Some("context"));
                assert_eq!(b.transcript(), "B: y");
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn comment_without_forwards_is_dropped() {
        let chat = 9003;
        open_with_note(chat, "orphan");
        assert!(matches!(attach_note(chat, "next"), NoteAttach::None));
        assert!(take(chat).is_none());
    }

    #[test]
    fn store_round_trips_and_orders_newest_first() {
        let c = Convos { db: Db::memory().unwrap() };
        c.add(ConvoNote { chat_id: 1, kind: "voice".into(), title: "first".into(), created_at: "2026-01-01T00:00:00Z".into(), ..Default::default() });
        c.add(ConvoNote { chat_id: 1, kind: "forward".into(), title: "second".into(), created_at: "2026-01-02T00:00:00Z".into(), ..Default::default() });
        c.add(ConvoNote { chat_id: 2, title: "other chat".into(), ..Default::default() });
        let r = c.recent(1, 10);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "second");
        assert_eq!(c.nth(1, 2).unwrap().title, "first");
        assert!(c.nth(1, 3).is_none());
    }

    #[test]
    fn html_escapes_and_folds_transcript() {
        let n = ConvoNote {
            title: "A & B".into(),
            summary: vec!["<x>".into()],
            transcript: "[1] A: hi\n[2] B: yo".into(),
            source: "2 forwarded messages".into(),
            ..Default::default()
        };
        let h = render_html(&n, "UTC");
        assert!(h.contains("<h3>A &amp; B</h3>"));
        assert!(h.contains("<li>&lt;x&gt;</li>"));
        assert!(h.contains("<details><summary>Transcript (2 messages)</summary>"));
        assert!(render_text(&n).starts_with("*A & B*"));
    }
}
