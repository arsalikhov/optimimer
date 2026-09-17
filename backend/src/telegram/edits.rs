//! Changing what is already saved: notes and summaries in the vault, and nodes
//! in the knowledge graph.
//!
//! Notes and summaries are rewritten by the small parser model rather than by
//! the agent itself: it is handed the file as it stands plus the user's
//! instruction ("add that it's €30", "drop the last point", "call it Bike fit")
//! and returns the whole thing again. That way the agent never has to carry a
//! long note in its context to change one line of it.
//!
//! Two guards make a bad rewrite harmless rather than destructive: an empty
//! result never overwrites non-empty content, and a summary's transcript is
//! rebuilt verbatim — it is a record of what was said, not something to edit.
//! Memory nodes need no model at all; the agent passes the corrected text.

use super::*;
use crate::memory;
use crate::vault::{self, Doc, NOTES};

/// Ask the parser model for one JSON object. `None` for anything unusable.
async fn rewrite(system: &str, prompt: &str) -> Option<Value> {
    let raw = crate::openrouter::chat(&crate::llm::parser(), system, prompt).await.ok()?;
    let trimmed = raw.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    serde_json::from_str::<Value>(trimmed).ok().filter(Value::is_object)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        return "(none)".into();
    }
    items.iter().map(|b| format!("- {b}")).collect::<Vec<_>>().join("\n")
}

fn tag_names(doc: &Doc) -> Vec<String> {
    doc.get("tags")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter_map(|t| t.as_str()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Locate the note the user means: a single search hit is taken as given,
/// several are narrowed by the same picker `/complete` uses.
async fn find_note(query: &str) -> Result<Doc, String> {
    if query.trim().is_empty() {
        return Err("Which note? Give me a few words from it.".into());
    }
    let mut hits = vault::search(NOTES, query, 8);
    match hits.len() {
        0 => Err(format!("No note matching '{}'.", query.trim())),
        1 => Ok(hits.remove(0)),
        _ => match pick_candidate(query, &hits).await {
            Some(i) => Ok(hits.remove(i)),
            // Editing the wrong note is worse than asking again.
            None => Err(format!(
                "Not sure which note you mean by '{}'. Closest: {}.",
                query.trim(),
                hits.iter().take(5).map(|d| d.title()).collect::<Vec<_>>().join(", ")
            )),
        },
    }
}

/// One note as a card: title, a metadata line, the body, and the vault path.
/// `head` is an optional lead-in for the metadata line, e.g. "updated".
fn note_card(doc: &Doc, head: &str) -> Reply {
    let esc = vault::html_escape;
    let tags = tag_names(doc).iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ");
    let meta = [head.to_string(), doc.str("category"), doc.str("status"), tags]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let body = clip(doc.body.trim(), 3000);
    let mut html = format!("<h3>{}</h3><blockquote>{}</blockquote>", esc(&doc.title()), esc(&meta));
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        html.push_str(&format!("<p>{}</p>", esc(line.trim())));
    }
    html.push_str(&format!("<blockquote>📁 {}</blockquote>", esc(&doc.rel)));
    let text = format!("{}\n{}\n\n{}\n\n📁 {}", doc.title(), meta, body, doc.rel);
    Reply::rich(html, text)
}

/// `read_note` — show one saved note in full, so an edit can be checked against
/// what was actually there.
pub(super) async fn read_note_reply(query: &str) -> Reply {
    match find_note(query).await {
        Ok(doc) => note_card(&doc, ""),
        Err(e) => Reply::text(e),
    }
}

/// `edit_note` — apply a natural-language instruction to a saved note and
/// rewrite its file. Renaming the note renames the file with it, and the
/// knowledge graph is repointed at the new path.
pub(super) async fn edit_note_reply(query: &str, instruction: &str) -> Reply {
    if instruction.trim().is_empty() {
        return Reply::text("What should I change about it?");
    }
    let mut doc = match find_note(query).await {
        Ok(d) => d,
        Err(e) => return Reply::text(e),
    };
    let was = doc.rel.clone();
    let cats = vault::categories().iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", ");
    let system = format!(
        "You apply one edit to a saved note and return the whole note again. Output ONLY minified JSON \
         {{\"title\":\"…\",\"body\":\"…\",\"tags\":[\"…\"],\"category\":\"…\",\"status\":\"…\"}} — no prose, no code fences. \
         Keep everything the instruction does not mention exactly as it was: do not summarise, reword, reorder or drop \
         anything. The body is Markdown. Category is one of: {cats}. Status is one of: inbox, draft, final."
    );
    let prompt = format!(
        "CURRENT NOTE\ntitle: {}\ncategory: {}\nstatus: {}\ntags: {}\nbody:\n{}\n\nINSTRUCTION FROM THE USER\n{}",
        doc.title(),
        doc.str("category"),
        doc.str("status"),
        tag_names(&doc).join(", "),
        clip(&doc.body, 6000),
        instruction.trim()
    );
    let Some(v) = rewrite(&system, &prompt).await else {
        return Reply::text(format!("Couldn't work out that edit to “{}” — nothing changed.", doc.title()));
    };

    // An empty body from a confused model must never wipe the file.
    let body = v["body"].as_str().map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
    let mut props: Vec<(String, Value)> = Vec::new();
    if let Some(c) = v["category"].as_str().map(str::trim).filter(|c| !c.is_empty()) {
        props.push(("category".into(), Value::String(vault::clamp_category(c))));
    }
    if let Some(st) = v["status"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        props.push(("status".into(), Value::String(st.to_lowercase())));
    }
    if let Some(arr) = v["tags"].as_array() {
        let tags: Vec<Value> = arr.iter().filter_map(|t| t.as_str()).map(|t| Value::String(vault::slug(t))).collect();
        props.push(("tags".into(), Value::Array(tags)));
    }
    let title = v["title"].as_str().map(str::to_string);

    match vault::edit_doc(&mut doc, title, body, props) {
        Ok(d) => {
            memory::global().repoint(&was, &d.rel, &d.title());
            note_card(&d, "updated")
        }
        Err(e) => Reply::text(format!("Couldn't write the note file: {}", vault::err_hint(&e))),
    }
}

/// `edit_summary` — apply an instruction to summary #N: its title, key points
/// and action items. Both copies are rewritten, the SQLite row and the vault
/// file; the transcript is carried over untouched.
pub(super) async fn edit_summary_reply(state: &BotState, chat_id: i64, number: usize, instruction: &str) -> Reply {
    if instruction.trim().is_empty() {
        return Reply::text("What should I change in it?");
    }
    let n = if number == 0 { 1 } else { number };
    let Some(mut note) = convo::global().nth(chat_id, n) else {
        return Reply::text(format!("No summary #{n}. Ask me to list your summaries first."));
    };
    let system = "You apply one edit to a saved summary and return it whole. Output ONLY minified JSON \
                  {\"title\":\"…\",\"summary\":[\"key point\",…],\"actions\":[\"action item\",…]} — no prose, no code \
                  fences. Keep every point the instruction does not mention exactly as it was. Never state anything the \
                  transcript does not support. `actions` may be an empty list.";
    let prompt = format!(
        "CURRENT SUMMARY\ntitle: {}\nkey points:\n{}\naction items:\n{}\n\nTRANSCRIPT (context only — it is never edited)\n{}\n\nINSTRUCTION FROM THE USER\n{}",
        note.title,
        bullets(&note.summary),
        bullets(&note.actions),
        clip(&note.transcript, 4000),
        instruction.trim()
    );
    let Some(v) = rewrite(system, &prompt).await else {
        return Reply::text(format!("Couldn't work out that edit to “{}” — nothing changed.", note.title));
    };
    let points = strings(&v["summary"]);
    if points.is_empty() {
        return Reply::text("That edit would leave the summary with no key points, so I left it alone.");
    }
    if let Some(t) = v["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
        note.title = t.to_string();
    }
    note.summary = points;
    note.actions = strings(&v["actions"]);

    // The Markdown copy follows the row. A missing file (older summary, or a
    // failed write at the time) just means there is nothing to rewrite.
    if !note.file.is_empty() {
        let path = vault::dir().join(format!("{}.md", note.file));
        if let Some(mut doc) = vault::read(&path) {
            let fresh = vault::NewSummary {
                title: note.title.clone(),
                kind: note.kind.clone(),
                source: note.source.clone(),
                note: note.note.clone(),
                summary: note.summary.clone(),
                actions: note.actions.clone(),
                transcript: note.transcript.clone(),
            };
            match vault::update_summary(&mut doc, &fresh) {
                Ok(d) => note.file = d.rel.clone(),
                Err(e) => tracing::warn!("vault summary rewrite failed: {e}"),
            }
        }
    }
    convo::global().add(note.clone());

    let tz = tz_for(state, chat_id);
    let mut reply = Reply::rich(convo::render_html(&note, &tz), convo::render_text(&note));
    if !note.file.is_empty() {
        if let Some(h) = reply.rich_html.as_mut() {
            h.push_str(&format!("<i>📁 {}</i>", vault::html_escape(&note.file)));
        }
        reply.text.push_str(&format!("\n📁 {}", note.file));
    }
    reply
}

/// `forget_memory` — drop an entry outright. For things that should never have
/// been stored (a shopping-list item, a passing bit of state), not for facts
/// that merely changed — those are `edit_memory`'s job.
pub(super) fn forget_memory_reply(name: &str) -> Reply {
    if name.trim().is_empty() {
        return Reply::text("Which memory should I drop? Give me the name it's filed under.");
    }
    let m = memory::global();
    let Some(node) = m.by_name(name) else {
        return Reply::text(format!("Nothing in memory is filed under '{}'.", name.trim()));
    };
    if node.kind == "category" {
        return Reply::text(format!("'{}' is one of your categories, not a memory — change those in settings.", node.name));
    }
    // A node that mirrors a task or note is that file's entry in the graph;
    // dropping it would just desync the two.
    if !node.path.is_empty() {
        return Reply::text(format!("“{}” is the entry for the file {}. Delete the file itself if you don't want it.", node.name, node.path));
    }
    // Whoever linked to it names it in their "Related" list — rewrite those, or
    // Obsidian keeps drawing it as a ghost node.
    let neighbors = m.neighbor_ids(&node.id);
    let _ = vault::trash_memory(&node, &m.neighbors(&node.id, 12), "you asked me to forget it");
    if m.delete_node(&node.id) {
        m.remirror(&neighbors);
        Reply::text(format!("Forgotten: {}. It's in the vault's bin for {} days if you want it back.", node.name, vault::trash_days()))
    } else {
        Reply::text(format!("Couldn't drop “{}”.", node.name))
    }
}

/// `list_trash` — what is still recoverable, newest first.
pub(super) fn trash_reply() -> Reply {
    let docs = vault::trash();
    if docs.is_empty() {
        return Reply::text(format!("The bin is empty. Anything I drop stays there for {} days.", vault::trash_days()));
    }
    let esc = vault::html_escape;
    let mut html = format!("<h3>In the bin ({} days to recover)</h3><table>", vault::trash_days());
    let mut text = String::from("In the bin\n");
    for d in docs.iter().take(20) {
        let when = vault::local_iso(&d.str("deleted"));
        let meta = format!("{} · {} · {}", d.str("kind"), d.str("reason"), when.get(..10).unwrap_or(&when));
        html.push_str(&format!("<tr><td>{}<br><i>{}</i></td></tr>", esc(&d.title()), esc(&meta)));
        text.push_str(&format!("• {} — {}\n", d.title(), meta));
    }
    html.push_str("</table>");
    Reply::rich(html, text)
}

/// `restore_memory` — put one back in the graph, with whatever of its relations
/// still have something on the other end.
pub(super) fn restore_memory_reply(name: &str) -> Reply {
    if name.trim().is_empty() {
        return Reply::text("Which one? Ask me what's in the bin.");
    }
    let Some(doc) = vault::trashed(name) else {
        return Reply::text(format!("Nothing called '{}' is in the bin.", name.trim()));
    };
    let m = memory::global();
    let title = doc.title();
    if m.by_name(&title).is_some() {
        return Reply::text(format!("“{title}” is already back in memory."));
    }
    // The body's first paragraph is the summary; the note about when it was
    // dropped is ours and doesn't come back.
    let summary = doc.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
    let node = m.upsert_node(&doc.str("kind"), &title, &summary, "");
    let mut relinked = 0;
    for (rel, other) in vault::trashed_relations(&doc) {
        if let Some(target) = m.by_name(&other) {
            m.add_edge(&node.id, &target.id, &rel, "restored");
            m.remirror(&[target.id]);
            relinked += 1;
        }
    }
    let _ = vault::mirror_memory_node(&node, &m.neighbors(&node.id, 12));
    vault::untrash(&doc);
    let links = match relinked {
        0 => String::new(),
        1 => ", with 1 link".to_string(),
        n => format!(", with {n} links"),
    };
    Reply::text(format!("Restored [{}] {}{links}.", node.kind, node.name))
}

/// `edit_memory` — correct what long-term memory holds about something: its
/// summary, its name, or the kind of thing it is. The vault mirror is rewritten
/// to match, and a rename drops the note filed under the old name.
pub(super) fn edit_memory_reply(name: &str, summary: &str, new_name: &str, kind: &str) -> Reply {
    if name.trim().is_empty() {
        return Reply::text("Which memory? Give me the name it's filed under.");
    }
    if summary.trim().is_empty() && new_name.trim().is_empty() && kind.trim().is_empty() {
        return Reply::text("Nothing to change — I need a new summary, name or kind.");
    }
    let m = memory::global();
    match m.edit_node(name, new_name, summary, kind) {
        memory::Edit::Missing => Reply::text(format!(
            "Nothing in memory is filed under '{}'. Search for it first, or store it fresh.",
            name.trim()
        )),
        memory::Edit::Conflict(other) => Reply::text(format!(
            "'{other}' is already a separate memory. Pick another name, or tell me which of the two to keep."
        )),
        memory::Edit::Updated { before, after } => {
            if !before.name.eq_ignore_ascii_case(&after.name) {
                let _ = vault::remove_memory_mirror(&before.name);
                // A rename moves the file, so everything linking to it has to
                // be rewritten too.
                m.remirror(&m.neighbor_ids(&after.id));
            }
            let _ = vault::mirror_memory_node(&after, &m.neighbors(&after.id, 12));
            let renamed = if before.name == after.name {
                String::new()
            } else {
                format!(" (was “{}”)", before.name)
            };
            let summary = if after.summary.is_empty() { "no summary".to_string() } else { after.summary.clone() };
            Reply::text(format!("Memory updated — [{}] {}{}: {}", after.kind, after.name, renamed, summary))
        }
    }
}
