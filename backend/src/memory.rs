//! Conversation memory: what was said, and a small knowledge graph distilled
//! from it, so the chat agent can look things up instead of carrying the whole
//! history in its context window.
//!
//! * `chat_messages` — every turn, per chat. The agent sees only the last few
//!   plus a rolling `chat_summaries` digest of everything older.
//! * `mem_nodes` / `mem_edges` — entities (people, projects, places, topics,
//!   preferences, facts, plus tasks/notes the bot created) and typed relations
//!   between them, with an FTS5 index over names and summaries. Extraction runs
//!   in the background after each exchange with a cheap model.
//! * Every node is mirrored to `memory/<kind>/<slug>.md` in the vault with
//!   wikilinks for its edges, so the graph is visible in Obsidian's graph view.
//!
//! `recall(query)` is the agent's tool: FTS hits + one hop of neighbours,
//! ranked by mentions and recency, rendered as a short bullet list.

use crate::db::Db;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub summary: String,
    /// Vault path (without .md) when the node is a task/note the bot wrote.
    pub path: String,
    pub mentions: i64,
    pub updated: String,
}

#[derive(Clone)]
pub struct Memory {
    db: Db,
}

static GLOBAL: OnceLock<Memory> = OnceLock::new();

pub fn init(db: Db) -> Memory {
    let m = Memory { db };
    let _ = GLOBAL.set(m.clone());
    m
}

pub fn global() -> Memory {
    GLOBAL
        .get_or_init(|| Memory { db: Db::memory().expect("in-memory memory db") })
        .clone()
}

/// How many recent turns the agent sees verbatim.
pub fn window() -> usize {
    std::env::var("MEMORY_WINDOW").ok().and_then(|s| s.parse().ok()).unwrap_or(12)
}

fn model() -> String {
    std::env::var("MEMORY_MODEL").unwrap_or_else(|_| "anthropic/claude-haiku-4.5".to_string())
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn node_id(kind: &str, name: &str) -> String {
    format!("{}:{}", norm(kind), crate::vault::slug(name))
}

fn row_node(r: &rusqlite::Row) -> rusqlite::Result<Node> {
    Ok(Node {
        id: r.get(0)?,
        kind: r.get(1)?,
        name: r.get(2)?,
        summary: r.get(3)?,
        path: r.get(4)?,
        mentions: r.get(5)?,
        updated: r.get(6)?,
    })
}

const NODE_COLS: &str = "id, kind, name, summary, path, mentions, updated";

impl Memory {
    // ---- conversation ------------------------------------------------------

    pub fn add_message(&self, chat_id: i64, role: &str, text: &str) -> i64 {
        let conn = self.db.lock();
        let _ = conn.execute(
            "INSERT INTO chat_messages (chat_id, role, text, created) VALUES (?1, ?2, ?3, ?4)",
            params![chat_id, role, text, Utc::now().to_rfc3339()],
        );
        conn.last_insert_rowid()
    }

    /// The last `n` turns, oldest first.
    pub fn recent_messages(&self, chat_id: i64, n: usize) -> Vec<(String, String)> {
        let conn = self.db.lock();
        let Ok(mut stmt) = conn.prepare("SELECT role, text FROM chat_messages WHERE chat_id = ?1 ORDER BY id DESC LIMIT ?2") else { return vec![] };
        let mut v: Vec<(String, String)> = stmt
            .query_map(params![chat_id, n as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        v.reverse();
        v
    }

    pub fn summary(&self, chat_id: i64) -> (String, i64) {
        let conn = self.db.lock();
        conn.query_row("SELECT summary, upto_id FROM chat_summaries WHERE chat_id = ?1", params![chat_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or_default()
    }

    fn set_summary(&self, chat_id: i64, summary: &str, upto_id: i64) {
        let conn = self.db.lock();
        let _ = conn.execute(
            "INSERT INTO chat_summaries (chat_id, summary, upto_id, updated) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chat_id) DO UPDATE SET summary = excluded.summary, upto_id = excluded.upto_id, updated = excluded.updated",
            params![chat_id, summary, upto_id, Utc::now().to_rfc3339()],
        );
    }

    /// Turns after the summary cut-off, oldest first: (id, role, text).
    fn unsummarized(&self, chat_id: i64, upto_id: i64) -> Vec<(i64, String, String)> {
        let conn = self.db.lock();
        let Ok(mut stmt) = conn.prepare("SELECT id, role, text FROM chat_messages WHERE chat_id = ?1 AND id > ?2 ORDER BY id ASC") else { return vec![] };
        stmt.query_map(params![chat_id, upto_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    // ---- graph ---------------------------------------------------------------

    /// Insert or refresh a node. A repeat mention bumps `mentions` and, when a
    /// non-empty summary is given, replaces the summary. Returns the node.
    pub fn upsert_node(&self, kind: &str, name: &str, summary: &str, path: &str) -> Node {
        let name = name.trim();
        let id = node_id(kind, name);
        let now = Utc::now().to_rfc3339();
        {
            let conn = self.db.lock();
            let _ = conn.execute(
                "INSERT INTO mem_nodes (id, kind, name, norm, summary, path, mentions, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)
                 ON CONFLICT(kind, norm) DO UPDATE SET
                   mentions = mem_nodes.mentions + 1,
                   summary = CASE WHEN excluded.summary != '' THEN excluded.summary ELSE mem_nodes.summary END,
                   path = CASE WHEN excluded.path != '' THEN excluded.path ELSE mem_nodes.path END,
                   updated = excluded.updated",
                params![id, norm(kind), name, norm(name), summary.trim(), path, now],
            );
        }
        self.get(&id).unwrap_or(Node { id, kind: kind.into(), name: name.into(), summary: summary.into(), path: path.into(), mentions: 1, updated: now })
    }

    pub fn get(&self, id: &str) -> Option<Node> {
        let conn = self.db.lock();
        conn.query_row(&format!("SELECT {NODE_COLS} FROM mem_nodes WHERE id = ?1"), params![id], row_node).optional().ok().flatten()
    }

    pub fn add_edge(&self, src: &str, dst: &str, rel: &str, evidence: &str) {
        if src == dst {
            return;
        }
        let conn = self.db.lock();
        let _ = conn.execute(
            "INSERT INTO mem_edges (src, dst, rel, weight, evidence, created) VALUES (?1, ?2, ?3, 1, ?4, ?5)
             ON CONFLICT(src, dst, rel) DO UPDATE SET weight = mem_edges.weight + 1, evidence = excluded.evidence",
            params![src, dst, norm(rel), evidence, Utc::now().to_rfc3339()],
        );
    }

    /// Both directions: (relation as seen from `id`, neighbour).
    pub fn neighbors(&self, id: &str, limit: usize) -> Vec<(String, Node)> {
        let conn = self.db.lock();
        let sql = format!(
            "SELECT e.rel, e.src = ?1, {} FROM mem_edges e JOIN mem_nodes n ON n.id = CASE WHEN e.src = ?1 THEN e.dst ELSE e.src END
             WHERE e.src = ?1 OR e.dst = ?1 ORDER BY e.weight DESC, n.updated DESC LIMIT ?2",
            NODE_COLS.split(", ").map(|c| format!("n.{c}")).collect::<Vec<_>>().join(", ")
        );
        let Ok(mut stmt) = conn.prepare(&sql) else { return vec![] };
        stmt.query_map(params![id, limit as i64], |r| {
            let rel: String = r.get(0)?;
            let outgoing: bool = r.get(1)?;
            let node = Node { id: r.get(2)?, kind: r.get(3)?, name: r.get(4)?, summary: r.get(5)?, path: r.get(6)?, mentions: r.get(7)?, updated: r.get(8)? };
            Ok((if outgoing { rel } else { format!("← {rel}") }, node))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Full-text search over names and summaries, then rank by mentions and
    /// recency. Falls back to LIKE if the query can't be parsed by FTS.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Node> {
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() >= 2)
            .map(|t| format!("\"{}\"*", t.to_lowercase()))
            .collect();
        if terms.is_empty() {
            return vec![];
        }
        let conn = self.db.lock();
        let fts = format!(
            "SELECT {} FROM mem_fts f JOIN mem_nodes n ON n.rowid = f.rowid WHERE mem_fts MATCH ?1
             ORDER BY bm25(mem_fts) + (-0.05 * n.mentions) LIMIT ?2",
            NODE_COLS.split(", ").map(|c| format!("n.{c}")).collect::<Vec<_>>().join(", ")
        );
        let q_or = terms.join(" OR ");
        let run = |q: &str| -> Vec<Node> {
            conn.prepare(&fts)
                .and_then(|mut st| st.query_map(params![q, limit as i64], row_node).map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>()))
                .unwrap_or_default()
        };
        let mut hits = run(&terms.join(" AND "));
        if hits.len() < 3 {
            for n in run(&q_or) {
                if !hits.iter().any(|h| h.id == n.id) {
                    hits.push(n);
                }
            }
        }
        hits.truncate(limit);
        hits
    }

    pub fn all_nodes(&self) -> Vec<Node> {
        let conn = self.db.lock();
        conn.prepare(&format!("SELECT {NODE_COLS} FROM mem_nodes ORDER BY updated DESC"))
            .and_then(|mut st| st.query_map([], row_node).map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>()))
            .unwrap_or_default()
    }

    pub fn node_count(&self) -> i64 {
        let conn = self.db.lock();
        conn.query_row("SELECT COUNT(*) FROM mem_nodes", [], |r| r.get(0)).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Recall (what the agent calls)
// ---------------------------------------------------------------------------

/// Compact answer for a `recall` tool call: top hits with one hop of
/// neighbours each, capped in size so it never floods the context.
pub fn recall(query: &str) -> String {
    let m = global();
    let hits = m.search(query, 6);
    if hits.is_empty() {
        return "Nothing in memory matches that.".into();
    }
    let mut out = String::new();
    let mut seen: HashSet<String> = HashSet::new();
    for n in hits {
        if !seen.insert(n.id.clone()) {
            continue;
        }
        out.push_str(&format!("• [{}] {}", n.kind, n.name));
        if !n.summary.is_empty() {
            out.push_str(&format!(" — {}", truncate(&n.summary, 220)));
        }
        if !n.path.is_empty() {
            out.push_str(&format!(" (vault: {})", n.path));
        }
        out.push('\n');
        for (rel, nb) in m.neighbors(&n.id, 5) {
            out.push_str(&format!("    {rel} → [{}] {}{}\n", nb.kind, nb.name, if nb.summary.is_empty() { String::new() } else { format!(": {}", truncate(&nb.summary, 90)) }));
        }
        if out.len() > 2400 {
            break;
        }
    }
    out.trim_end().to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n).collect();
    t.push('…');
    t
}

// ---------------------------------------------------------------------------
// Extraction + rolling summary (background, cheap model)
// ---------------------------------------------------------------------------

fn parse_json_loose(text: &str) -> Value {
    let t = text.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return v;
    }
    if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) {
        if b > a {
            if let Ok(v) = serde_json::from_str::<Value>(&t[a..=b]) {
                return v;
            }
        }
    }
    Value::Null
}

const KINDS: &[&str] = &["person", "organization", "project", "place", "topic", "preference", "fact", "event"];

/// Pull entities, relations and durable facts out of one exchange and merge
/// them into the graph. Runs after the reply was sent; failures only log.
pub async fn extract(chat_id: i64, user_text: &str, assistant_text: &str, evidence_id: i64) {
    let system = format!(
        "You maintain a personal knowledge graph for ONE user from their chat with an assistant. Output ONLY minified JSON — no prose, no code fences: \
         {{\"entities\":[{{\"kind\":\"…\",\"name\":\"…\",\"summary\":\"…\"}}],\"relations\":[{{\"from\":\"name\",\"to\":\"name\",\"rel\":\"…\"}}]}}. \
         kind is one of {}. Capture only what is worth remembering later: people (with role/relationship), organizations, projects, places, recurring topics, \
         the user's stated preferences (kind preference, name = short statement), durable facts about their life (kind fact, name = short statement), and dated events. \
         Summaries are one sentence, factual, in third person about the user (\"Alex is the user's dentist\"). Reuse plain canonical names (\"Sam\", not \"Sam (friend)\"). \
         Skip greetings, transient chatter, and anything the assistant merely displayed (balances, lists). If nothing is worth keeping, output {{\"entities\":[],\"relations\":[]}}.",
        KINDS.join("|")
    );
    let prompt = format!("USER:\n{}\n\nASSISTANT:\n{}", truncate(user_text, 3000), truncate(assistant_text, 1500));
    let raw = match crate::openrouter::chat(&model(), &system, &prompt).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("memory extract failed: {e}");
            return;
        }
    };
    let v = parse_json_loose(&raw);
    if !v.is_object() {
        return;
    }
    let m = global();
    let evidence = format!("chat:{chat_id}#{evidence_id}");
    let mut ids: BTreeMap<String, String> = BTreeMap::new(); // norm name → id
    for e in v["entities"].as_array().cloned().unwrap_or_default() {
        let kind = e["kind"].as_str().unwrap_or("topic").trim().to_lowercase();
        let kind = if KINDS.contains(&kind.as_str()) { kind } else { "topic".to_string() };
        let name = e["name"].as_str().unwrap_or("").trim();
        if name.is_empty() || name.chars().count() > 120 {
            continue;
        }
        let node = m.upsert_node(&kind, name, e["summary"].as_str().unwrap_or(""), "");
        ids.insert(norm(name), node.id.clone());
        let _ = crate::vault::mirror_memory_node(&node, &m.neighbors(&node.id, 12));
    }
    for r in v["relations"].as_array().cloned().unwrap_or_default() {
        let (Some(a), Some(b)) = (r["from"].as_str(), r["to"].as_str()) else { continue };
        let rel = r["rel"].as_str().unwrap_or("related to").trim();
        let (Some(src), Some(dst)) = (ids.get(&norm(a)).cloned(), ids.get(&norm(b)).cloned()) else { continue };
        m.add_edge(&src, &dst, rel, &evidence);
        for id in [&src, &dst] {
            if let Some(n) = m.get(id) {
                let _ = crate::vault::mirror_memory_node(&n, &m.neighbors(id, 12));
            }
        }
    }
}

/// Fold turns older than the verbatim window into the rolling summary once
/// enough have piled up. Keeps the agent's context bounded.
pub async fn maybe_roll_summary(chat_id: i64) {
    let m = global();
    let (old_summary, upto) = m.summary(chat_id);
    let pending = m.unsummarized(chat_id, upto);
    let keep = window();
    if pending.len() < keep * 2 {
        return;
    }
    let fold: Vec<&(i64, String, String)> = pending.iter().take(pending.len() - keep).collect();
    let Some(last) = fold.last() else { return };
    let transcript = fold
        .iter()
        .map(|(_, role, text)| format!("{}: {}", if role == "user" { "User" } else { "Assistant" }, truncate(text, 600)))
        .collect::<Vec<_>>()
        .join("\n");
    let system = "You keep a running summary of a chat between a user and their personal assistant. Merge the EXISTING SUMMARY with the NEW TURNS into one updated summary of at most 180 words: what the user is working on, decisions, open threads, preferences, and anything the assistant promised. Third person, plain prose, no headings. Drop details that are no longer relevant.";
    let prompt = format!("EXISTING SUMMARY:\n{}\n\nNEW TURNS:\n{}", if old_summary.is_empty() { "(none)" } else { &old_summary }, transcript);
    match crate::openrouter::chat(&model(), system, &prompt).await {
        Ok(s) if !s.trim().is_empty() => m.set_summary(chat_id, s.trim(), last.0),
        Ok(_) => {}
        Err(e) => tracing::warn!("memory summary failed: {e}"),
    }
}

/// Rebuild every node's vault mirror (startup, or after a wipe of `memory/`).
pub fn mirror_all() -> usize {
    let m = global();
    let mut n = 0;
    for node in m.all_nodes() {
        if crate::vault::mirror_memory_node(&node, &m.neighbors(&node.id, 12)).is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Memory {
        Memory { db: Db::memory().unwrap() }
    }

    #[test]
    fn messages_window_and_summary_roundtrip() {
        let m = fresh();
        for i in 0..5 {
            m.add_message(7, if i % 2 == 0 { "user" } else { "assistant" }, &format!("turn {i}"));
        }
        let recent = m.recent_messages(7, 3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].1, "turn 2");
        assert_eq!(recent[2].1, "turn 4");
        assert_eq!(m.summary(7), (String::new(), 0));
        m.set_summary(7, "digest", 3);
        assert_eq!(m.summary(7), ("digest".into(), 3));
        assert_eq!(m.unsummarized(7, 3).len(), 2);
    }

    #[test]
    fn graph_upsert_edges_and_fts_search() {
        let m = fresh();
        let sam = m.upsert_node("person", "Sam", "Sam is the user's climbing partner", "");
        let sam2 = m.upsert_node("person", "sam", "", "");
        assert_eq!(sam.id, sam2.id, "case-insensitive dedupe");
        assert_eq!(sam2.mentions, 2);
        assert_eq!(sam2.summary, "Sam is the user's climbing partner", "empty summary keeps the old one");
        let tri = m.upsert_node("project", "Triathlon", "Half-distance race in August", "");
        m.add_edge(&sam.id, &tri.id, "trains for", "chat:1#9");
        m.add_edge(&sam.id, &tri.id, "trains for", "chat:1#10");
        let nb = m.neighbors(&sam.id, 5);
        assert_eq!(nb.len(), 1);
        assert_eq!(nb[0].0, "trains for");
        let back = m.neighbors(&tri.id, 5);
        assert_eq!(back[0].0, "← trains for");
        let hits = m.search("climbing", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Sam");
        assert!(m.search("triath race", 5).iter().any(|n| n.name == "Triathlon"), "prefix + OR fallback");
        assert!(m.search("zzz", 5).is_empty());
    }

    #[test]
    fn recall_is_compact() {
        let _ = GLOBAL.set(fresh());
        let m = global();
        let a = m.upsert_node("person", "Dr Lee", "Dr Lee is the user's dentist on Bank Street", "");
        let b = m.upsert_node("place", "Bank Street clinic", "", "");
        m.add_edge(&a.id, &b.id, "works at", "");
        let r = recall("dentist");
        assert!(r.contains("[person] Dr Lee"), "{r}");
        assert!(r.contains("works at → [place] Bank Street clinic"), "{r}");
        assert!(r.len() < 2600);
    }
}
