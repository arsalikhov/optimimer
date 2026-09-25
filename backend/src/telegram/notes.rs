//! Vault-backed capture and lookup: `/todo`, `/note`, `/complete`, `/search_*`,
//! `/list_*` write and read Markdown files in the Obsidian vault (see
//! `crate::vault`). The `cmd-todo` / `cmd-note` agents only PARSE the text into
//! JSON (title, tags, when…); the file write happens here in Rust. Also home to
//! the ledger views `/transactions` and `/remove_transaction`.

use super::*;
use crate::vault::{self, Doc, NOTES, TASKS};

/// Output of a named node in a run (`{"text","json"}` for an LLM node).
fn node_output(result: &RunResponse, id: &str) -> Value {
    result
        .results
        .iter()
        .find(|r| r.node_id == id && r.status == "ok")
        .map(|r| r.output.clone())
        .unwrap_or(Value::Null)
}

/// The category a flow's `jev_category` node picked — only when Jev was on
/// and at least as sure as the node's `min_confidence`. Callers prefer it over
/// the AI Step's pick, which it double-checks.
pub(super) fn jev_category(result: &RunResponse) -> Option<String> {
    let out = node_output(result, "jev_category");
    (out["confident"] == Value::Bool(true)).then(|| s(&out["answer"])).filter(|c| !c.is_empty())
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").trim().to_string()
}

fn priority_badge(p: &str) -> &'static str {
    match p {
        "high" => "🔴 high",
        "low" => "🟢 low",
        _ => "🟡 medium",
    }
}

/// `/todo <text>` — parse with the cmd-todo agent, then write `tasks/<file>.md`.
pub(super) async fn handle_todo(state: &BotState, chat_id: i64, body: &str) -> Reply {
    if body.trim().is_empty() {
        return Reply::text("Usage: `/todo <what, and optionally when>` — e.g. `/todo dentist tuesday 2pm`.");
    }
    let Some(wf) = state.store.get("cmd-todo") else {
        return Reply::text("The `/todo` parser isn't installed (expected agent id `cmd-todo`). Restart the backend to re-seed it.");
    };
    let tz = tz_for(state, chat_id);
    let input = json!({
        "text": body,
        "now": now_in_tz(&tz),
        "tz": tz,
        "chat_id": chat_id.to_string(),
        "command": "todo",
        "categories": vault::categories_prompt(),
        "projects": vault::projects_prompt(),
    });
    let result = engine::run(&wf, input).await;
    let parsed = node_output(&result, "parse")["json"].clone();
    if !parsed.is_object() {
        return Reply::text(format!("Couldn't parse that task.\n{}", format_result(&wf.name, &result)));
    }
    let dt = node_output(&result, "dt");
    let title = { let t = s(&parsed["title"]); if t.is_empty() { body.trim().to_string() } else { t } };
    let kind = s(&parsed["kind"]).to_lowercase();
    let start = s(&dt["rfc3339"]);
    let end = s(&dt["rfc3339_end"]);
    // A project was chosen under the parser's category, so only overrule the
    // category of a task that has none.
    let category = match jev_category(&result) {
        Some(c) if s(&parsed["project"]).is_empty() => c,
        _ => s(&parsed["category"]),
    };
    let task = vault::NewTask {
        title: title.clone(),
        category,
        priority: s(&parsed["priority"]).to_lowercase(),
        status: s(&parsed["status"]).to_lowercase(),
        due: vault::local_iso(&start),
        due_end: if kind == "meeting" { vault::local_iso(&end) } else { String::new() },
        project: s(&parsed["project"]),
        body: body.trim().to_string(),
    };
    let doc = match vault::write_task(task) {
        Ok(d) => d,
        Err(e) => return Reply::text(format!("Couldn't write the task file: {}", vault::err_hint(&e))),
    };
    crate::charts::refresh_gantt();
    crate::memory::link_vault_doc("task", &doc);
    let esc = vault::html_escape;
    let when = if start.is_empty() {
        "no date".to_string()
    } else if kind == "meeting" && !s(&dt["human_end"]).is_empty() {
        format!("{} – {}", s(&dt["human"]), s(&dt["human_end"]))
    } else {
        s(&dt["human"])
    };
    let project = doc.str("project");
    let mut html = format!(
        "<b>{}</b><table><tr><td>Category</td><td>{}</td></tr><tr><td>Priority</td><td>{}</td></tr><tr><td>When</td><td>{}</td></tr>",
        esc(&title), esc(&doc.str("category")), priority_badge(&doc.str("priority")), esc(&when)
    );
    if !project.is_empty() {
        html.push_str(&format!("<tr><td>Project</td><td>{}</td></tr>", esc(&project)));
    }
    html.push_str(&format!("<tr><td>Status</td><td>{}</td></tr></table><i>📁 {}</i>", esc(&doc.str("status")), esc(&doc.rel)));
    let text = format!("Task saved [{}]: {title} ({when}) → {}", doc.str("category"), doc.rel);
    Reply::rich(html, text)
}

/// `/note <text>` — classify with the cmd-note agent, then write `notes/<file>.md`.
pub(super) async fn handle_note(state: &BotState, chat_id: i64, body: &str) -> Reply {
    if body.trim().is_empty() {
        return Reply::text("Usage: `/note <anything worth keeping>`");
    }
    let Some(wf) = state.store.get("cmd-note") else {
        return Reply::text("The `/note` parser isn't installed (expected agent id `cmd-note`). Restart the backend to re-seed it.");
    };
    let tz = tz_for(state, chat_id);
    let mut tags: Vec<String> = Vec::new();
    for d in vault::list(NOTES) {
        if let Some(arr) = d.get("tags").and_then(|t| t.as_array()) {
            for t in arr.iter().filter_map(|t| t.as_str()) {
                if !tags.iter().any(|x| x == t) {
                    tags.push(t.to_string());
                }
            }
        }
    }
    let input = json!({
        "text": body,
        "now": now_in_tz(&tz),
        "tz": tz,
        "chat_id": chat_id.to_string(),
        "command": "note",
        "categories": vault::categories_prompt(),
        "tags": if tags.is_empty() { "(none yet)".to_string() } else { tags.join(", ") },
    });
    let result = engine::run(&wf, input).await;
    let parsed = node_output(&result, "classify")["json"].clone();
    // A failed classification still saves the note — losing a capture is worse
    // than a generic title.
    let title = { let t = s(&parsed["title"]); if t.is_empty() { first_words(body, 8) } else { t } };
    let note = vault::NewNote {
        title: title.clone(),
        category: jev_category(&result).unwrap_or_else(|| s(&parsed["category"])),
        tags: parsed["tags"].as_array().map(|a| a.iter().filter_map(|t| t.as_str()).map(str::to_string).collect()).unwrap_or_default(),
        status: s(&parsed["status"]),
        body: body.trim().to_string(),
        source: "telegram".into(),
    };
    match vault::write_note(note) {
        Ok(doc) => {
            crate::memory::link_vault_doc("note", &doc);
            let tags = doc.get("tags").and_then(|t| t.as_array()).map(|a| a.iter().filter_map(|t| t.as_str()).map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ")).unwrap_or_default();
            Reply::text(format!("Saved note *{}* [{}] ({}) {}\n📁 {}", md_escape(&title), doc.str("category"), doc.str("status"), md_escape(&tags), md_escape(&doc.rel)))
        }
        Err(e) => Reply::text(format!("Couldn't write the note file: {}", vault::err_hint(&e))),
    }
}

fn first_words(s: &str, n: usize) -> String {
    let w: Vec<&str> = s.split_whitespace().take(n).collect();
    let mut t = w.join(" ");
    if s.split_whitespace().count() > n {
        t.push('…');
    }
    t
}

/// `/complete <what>` — tick the matching open task (`status: done`). With
/// several candidates (or with Jev on, even one — the title match is loose) a
/// picker decides which was meant; when it can't tell, nothing is ticked and
/// the candidates are listed so the user (or the agent) can be specific.
pub(super) async fn handle_complete(_state: &BotState, _chat_id: i64, body: &str) -> Reply {
    if body.trim().is_empty() {
        return Reply::text("Usage: `/complete <task>`");
    }
    let mut candidates = vault::find_open_tasks(body);
    let picked = match candidates.len() {
        0 => return Reply::text(format!("Couldn't find an open task matching '{}'.", body.trim())),
        1 if !crate::jev::enabled() => Some(0),
        _ => pick_candidate(body, &candidates).await,
    };
    let Some(idx) = picked else {
        let list = candidates.iter().take(10).enumerate().map(|(i, d)| format!("{}. {}", i + 1, md_escape(&d.title()))).collect::<Vec<_>>().join("\n");
        return Reply::text(format!("Not sure which task you mean by '{}' — nothing marked done. Open tasks that look close:\n{list}", body.trim()));
    };
    let mut doc = candidates.remove(idx);
    let title = doc.title();
    match vault::complete(&mut doc) {
        Ok(()) => {
            crate::charts::refresh_gantt();
            Reply::text(format!("Marked done: *{}*", md_escape(&title)))
        }
        Err(e) => Reply::text(format!("Couldn't update the task file: {}", vault::err_hint(&e))),
    }
}

/// Jev must be at least this sure before a task is ticked on its word.
const PICK_MIN_CONFIDENCE: f64 = 0.6;

/// Which of the open tasks did the user mean? An index into `docs`, or `None`
/// when none clearly matches — a wrong tick is worse than asking.
async fn pick_candidate(query: &str, docs: &[Doc]) -> Option<usize> {
    if crate::jev::enabled() {
        let mut options: Vec<(String, String)> = docs.iter().take(crate::jev::MAX_OPTIONS - 1).enumerate().map(|(i, d)| (format!("task_{}", i + 1), d.title())).collect();
        options.push(("none_of_these".into(), "No listed task is the one the user says they finished".into()));
        let q = crate::jev::Q::choice("The user says they completed a task. Which open task do they mean?", options);
        if let Some(pick) = crate::jev::choice(json!({ "user_says_completed": query }), q).await {
            tracing::info!("complete_task pick: {}", pick.top(3));
            return pick
                .confident(PICK_MIN_CONFIDENCE)
                .and_then(|c| c.strip_prefix("task_"))
                .and_then(|n| n.parse::<usize>().ok())
                .map(|n| n - 1)
                .filter(|i| *i < docs.len());
        }
        // Jev failed: fall through to the LLM.
    }
    let list = docs
        .iter()
        .enumerate()
        .map(|(i, d)| format!("{}. {}", i + 1, d.title()))
        .collect::<Vec<_>>()
        .join("\n");
    let system = "You pick which open task the user means to mark complete. Output ONLY minified JSON {\"index\":<1-based number>} — the single best match by title, or {\"index\":0} if none of them is clearly it. No prose, no code fences.";
    let prompt = format!("User says they completed: \"{query}\"\n\nOPEN TASKS:\n{list}");
    let raw = crate::openrouter::chat(&crate::llm::parser(), system, &prompt).await.unwrap_or_default();
    let trimmed = raw.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    // An unreadable answer or 0 means "don't know" — never silently the first task.
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|v| v["index"].as_u64())
        .filter(|n| *n >= 1)
        .map(|n| n as usize - 1)
        .filter(|i| *i < docs.len())
}

fn short_date(iso: &str) -> String {
    let d = iso.get(..10).unwrap_or(iso);
    chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
        .map(|d| d.format("%b %-d").to_string())
        .unwrap_or_else(|_| d.to_string())
}

/// One row per doc: title + a small meta line (status/due for tasks, tags for notes).
fn docs_table(docs: &[Doc], sub: &str) -> (String, String) {
    let esc = vault::html_escape;
    let mut html = String::from("<table>");
    let mut text = String::new();
    for d in docs {
        let meta = if sub == TASKS {
            let due = d.str("due");
            let mut m = d.str("status");
            if !due.is_empty() {
                m.push_str(&format!(" · due {}", due.replace('T', " ")));
            }
            let c = d.str("category");
            let p = d.str("project");
            match (c.is_empty(), p.is_empty()) {
                (false, false) => m.push_str(&format!(" · {c} / {p}")),
                (false, true) => m.push_str(&format!(" · {c}")),
                (true, false) => m.push_str(&format!(" · {p}")),
                _ => {}
            }
            m
        } else {
            let tags = d.get("tags").and_then(|t| t.as_array()).map(|a| a.iter().filter_map(|t| t.as_str()).map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ")).unwrap_or_default();
            format!("{} {}", short_date(&d.created()), tags).trim().to_string()
        };
        html.push_str(&format!("<tr><td>{}<br><i>{}</i></td></tr>", esc(&d.title()), esc(&meta)));
        text.push_str(&format!("• {} — {}\n", d.title(), meta));
    }
    html.push_str("</table>");
    (html, text)
}

pub(super) fn search_reply(sub: &str, query: &str) -> Reply {
    let noun = if sub == TASKS { "tasks" } else { "notes" };
    if query.trim().is_empty() {
        return Reply::text(format!("Usage: `/search_{noun} <words>`"));
    }
    let hits = vault::search(sub, query, 8);
    if hits.is_empty() {
        return Reply::text(format!("No {noun} matching '{}'.", query.trim()));
    }
    let (table, text) = docs_table(&hits, sub);
    let head = format!("{} matching “{}”", if sub == TASKS { "Tasks" } else { "Notes" }, query.trim());
    Reply::rich(format!("<b>{}</b>{table}", vault::html_escape(&head)), format!("{head}\n{text}"))
}

pub(super) fn list_docs_reply(sub: &str) -> Reply {
    let docs: Vec<Doc> = if sub == TASKS {
        vault::open_tasks().into_iter().take(5).collect()
    } else {
        vault::list(NOTES).into_iter().take(5).collect()
    };
    if docs.is_empty() {
        return Reply::text(if sub == TASKS { "No open tasks." } else { "No notes yet." });
    }
    let (table, text) = docs_table(&docs, sub);
    let head = if sub == TASKS { "Open tasks (newest 5)" } else { "Last 5 notes" };
    Reply::rich(format!("<b>{head}</b>{table}"), format!("{head}\n{text}"))
}

/// `/transactions [n]` — newest ledger rows, numbered.
pub(super) fn transactions_reply(body: &str) -> Reply {
    let n = body.split_whitespace().find_map(|w| w.parse::<usize>().ok()).unwrap_or(10).clamp(1, 50);
    let (html, text) = crate::finance::recent_listing(n);
    Reply::rich(html, text)
}

/// `/remove_transaction N` — delete the N-th row from `/transactions`.
pub(super) fn remove_transaction_reply(body: &str) -> Reply {
    let Some(n) = body.split_whitespace().find_map(|w| w.trim_start_matches('#').parse::<usize>().ok()) else {
        return Reply::text("Usage: `/remove_transaction <number from /transactions>`");
    };
    match crate::finance::remove_nth(n) {
        Some(t) => Reply::text(format!("Removed: {} {} ${:.2} ({})", t.date, md_escape(&t.name), t.amount, t.category)),
        None => Reply::text(format!("No transaction #{n}. See /transactions.")),
    }
}
