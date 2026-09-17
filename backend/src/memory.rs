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
//! * Every node except tasks/notes (their vault file is the record) is mirrored
//!   to `memory/<slug>.md` with wikilinks for its edges, so the graph is visible
//!   in Obsidian's graph view. Names are unique across kinds: one node per name.
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
    m.seed_categories();
    m
}

/// Kinds the extractor may create. `category` is deliberately absent: the
/// category list is fixed (see `vault::categories`) and seeded at startup.
pub const KINDS: &[&str] = &["person", "organization", "project", "place", "topic", "preference", "fact", "event"];

/// A graph over `db` that isn't the process-wide one — tests only, so each one
/// gets a clean database instead of racing over `GLOBAL`.
#[cfg(test)]
pub fn scoped(db: Db) -> Memory {
    Memory { db }
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
    crate::llm::memory()
}

/// Message id below which this chat's turns are hidden from the agent (see
/// `clear_context`); 0 when the thread was never cleared.
fn context_cutoff(chat_id: i64) -> i64 {
    crate::config::stored(&format!("context_cutoff:{chat_id}")).and_then(|v| v.parse().ok()).unwrap_or(0)
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

/// Outcome of `Memory::edit_node`.
// `Updated` carries two whole nodes and the other variants carry almost
// nothing; one of these is built per edit, so the size gap costs nothing.
#[allow(clippy::large_enum_variant)]
pub enum Edit {
    /// Nothing in the graph goes by that name.
    Missing,
    /// The new name is already taken by a different node (holds that node's name).
    Conflict(String),
    Updated { before: Node, after: Node },
}

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
        let cutoff = context_cutoff(chat_id);
        let conn = self.db.lock();
        let Ok(mut stmt) = conn.prepare("SELECT role, text FROM chat_messages WHERE chat_id = ?1 AND id > ?3 ORDER BY id DESC LIMIT ?2") else { return vec![] };
        let mut v: Vec<(String, String)> = stmt
            .query_map(params![chat_id, n as i64, cutoff], |r| Ok((r.get(0)?, r.get(1)?)))
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

    /// Start a fresh thread: everything said so far drops out of the agent's
    /// context (window and rolling summary), while the messages stay stored and
    /// the knowledge graph, tasks and notes are untouched. Returns how many
    /// turns were hidden.
    pub fn clear_context(&self, chat_id: i64) -> usize {
        // Read the setting BEFORE taking the connection: the settings table
        // lives behind the same (non-reentrant) lock.
        let cutoff = context_cutoff(chat_id);
        let (max_id, hidden): (i64, i64) = {
            let conn = self.db.lock();
            conn.query_row(
                "SELECT COALESCE(MAX(id), 0), COUNT(*) FROM chat_messages WHERE chat_id = ?1 AND id > ?2",
                params![chat_id, cutoff],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 0))
        };
        crate::config::set(&format!("context_cutoff:{chat_id}"), &max_id.to_string());
        self.set_summary(chat_id, "", max_id);
        hidden as usize
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

    /// Make sure every configured category exists as a node (with its hint as
    /// the summary), so extraction resolves "Work" to the category rather
    /// than inventing a project of the same name.
    pub fn seed_categories(&self) {
        for (name, hint) in crate::vault::categories() {
            if self.by_name(&name).is_none() {
                self.upsert_node("category", &name, &hint, "");
            }
        }
    }

    /// The node mirroring a vault file, if any.
    pub fn by_path(&self, path: &str) -> Option<Node> {
        if path.is_empty() {
            return None;
        }
        let conn = self.db.lock();
        conn.query_row(&format!("SELECT {NODE_COLS} FROM mem_nodes WHERE path = ?1 LIMIT 1"), params![path], row_node)
            .optional()
            .ok()
            .flatten()
    }

    /// Any node with this name, whatever its kind.
    pub fn by_name(&self, name: &str) -> Option<Node> {
        let conn = self.db.lock();
        conn.query_row(&format!("SELECT {NODE_COLS} FROM mem_nodes WHERE norm = ?1 ORDER BY mentions DESC LIMIT 1"), params![norm(name)], row_node)
            .optional()
            .ok()
            .flatten()
    }

    /// Insert or refresh a node. Names are unique across kinds: if a node with
    /// this name already exists (under any kind) it is reused — its kind wins —
    /// with `mentions` bumped and a non-empty summary/path replacing the old.
    /// A kind of `category` can only come from `seed_categories`.
    pub fn upsert_node(&self, kind: &str, name: &str, summary: &str, path: &str) -> Node {
        let name = name.trim();
        let now = Utc::now().to_rfc3339();
        if let Some(existing) = self.by_name(name) {
            {
                let conn = self.db.lock();
                let _ = conn.execute(
                    "UPDATE mem_nodes SET mentions = mentions + 1,
                       summary = CASE WHEN ?2 != '' THEN ?2 ELSE summary END,
                       path = CASE WHEN ?3 != '' THEN ?3 ELSE path END,
                       updated = ?4 WHERE id = ?1",
                    params![existing.id, summary.trim(), path, now],
                );
            }
            return self.get(&existing.id).unwrap_or(existing);
        }
        let id = node_id(kind, name);
        {
            let conn = self.db.lock();
            let _ = conn.execute(
                "INSERT INTO mem_nodes (id, kind, name, norm, summary, path, mentions, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)
                 ON CONFLICT(id) DO UPDATE SET mentions = mem_nodes.mentions + 1, updated = excluded.updated",
                params![id, norm(kind), name, norm(name), summary.trim(), path, now],
            );
        }
        self.get(&id).unwrap_or(Node { id, kind: kind.into(), name: name.into(), summary: summary.into(), path: path.into(), mentions: 1, updated: now })
    }

    /// Repoint the graph at a vault file that moved: a note or task whose title
    /// (and therefore file name) was edited. No-op when nothing referred to it.
    pub fn repoint(&self, old_rel: &str, new_rel: &str, new_title: &str) {
        if old_rel.is_empty() || old_rel == new_rel {
            return;
        }
        let now = Utc::now().to_rfc3339();
        let conn = self.db.lock();
        let _ = conn.execute(
            "UPDATE mem_nodes SET path = ?2, name = ?3, norm = ?4, updated = ?5 WHERE path = ?1",
            params![old_rel, new_rel, new_title, norm(new_title), now],
        );
    }

    /// Edit a node that already exists: rename it, replace its summary, or move
    /// it to another kind. Blank arguments leave that field alone.
    pub fn edit_node(&self, name: &str, new_name: &str, summary: &str, kind: &str) -> Edit {
        let Some(before) = self.by_name(name) else {
            return Edit::Missing;
        };
        let new_name = new_name.trim();
        let summary = summary.trim();
        // Only the extractor's kinds; `category` is seeded, not edited.
        let kind = { let k = norm(kind); if KINDS.contains(&k.as_str()) { k } else { String::new() } };
        if !new_name.is_empty() && !new_name.eq_ignore_ascii_case(&before.name) {
            if let Some(other) = self.by_name(new_name) {
                if other.id != before.id {
                    return Edit::Conflict(other.name);
                }
            }
        }
        let now = Utc::now().to_rfc3339();
        {
            let conn = self.db.lock();
            let _ = conn.execute(
                "UPDATE mem_nodes SET
                   name = CASE WHEN ?2 != '' THEN ?2 ELSE name END,
                   norm = CASE WHEN ?3 != '' THEN ?3 ELSE norm END,
                   summary = CASE WHEN ?4 != '' THEN ?4 ELSE summary END,
                   kind = CASE WHEN ?5 != '' THEN ?5 ELSE kind END,
                   updated = ?6 WHERE id = ?1",
                params![before.id, new_name, norm(new_name), summary, kind, now],
            );
        }
        match self.get(&before.id) {
            Some(after) => Edit::Updated { before, after },
            None => Edit::Missing,
        }
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
    /// The ids currently linked to `id`. Capture these *before* removing or
    /// renaming a node: afterwards the edges are gone and there is no way to
    /// tell whose mirror needs rewriting.
    pub fn neighbor_ids(&self, id: &str) -> Vec<String> {
        self.neighbors(id, 50).into_iter().map(|(_, n)| n.id).collect()
    }

    /// Rewrite the vault mirrors of these nodes, skipping any that are gone.
    /// Call it with the `neighbor_ids` captured before a node was deleted,
    /// merged away or renamed: their "Related" lists still name the old file,
    /// and Obsidian draws a wikilink to a missing file as a node in the graph —
    /// so a dead link looks exactly like the memory is still there.
    pub fn remirror(&self, ids: &[String]) -> usize {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut n = 0;
        for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
            if let Some(node) = self.get(id) {
                if crate::vault::mirror_memory_node(&node, &self.neighbors(id, 12)).is_ok() {
                    n += 1;
                }
            }
        }
        n
    }

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

    /// Nodes that own a mirror note of their own: everything except categories
    /// (seeded, not learned) and the task/note entries, which point at their
    /// own vault file instead of getting a duplicate.
    pub fn mirrored_nodes(&self) -> Vec<Node> {
        self.all_nodes().into_iter().filter(|n| n.path.is_empty() && n.kind != "category").collect()
    }

    pub fn all_nodes(&self) -> Vec<Node> {
        let conn = self.db.lock();
        conn.prepare(&format!("SELECT {NODE_COLS} FROM mem_nodes ORDER BY updated DESC"))
            .and_then(|mut st| st.query_map([], row_node).map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>()))
            .unwrap_or_default()
    }

    // ---- weekly sweep (see `crate::sweep`) --------------------------------

    /// Nodes the sweep has never looked at, oldest first. Categories are seeded,
    /// not learned, so they are never up for consolidation.
    pub fn unswept(&self, limit: usize) -> Vec<Node> {
        let conn = self.db.lock();
        conn.prepare(&format!("SELECT {NODE_COLS} FROM mem_nodes WHERE swept = '' AND kind != 'category' ORDER BY updated ASC LIMIT ?1"))
            .and_then(|mut st| st.query_map(params![limit as i64], row_node).map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>()))
            .unwrap_or_default()
    }

    /// How many nodes the sweep has yet to look at.
    pub fn unswept_count(&self) -> i64 {
        let conn = self.db.lock();
        conn.query_row("SELECT COUNT(*) FROM mem_nodes WHERE swept = '' AND kind != 'category'", [], |r| r.get(0)).unwrap_or(0)
    }

    /// Clear every sweep stamp so the next runs walk the whole graph again, a
    /// batch at a time. Backs the one-off "tidy up everything" pass — the
    /// weekly run stays incremental, and the batch cap still bounds each call.
    pub fn reset_swept(&self) -> usize {
        let conn = self.db.lock();
        conn.execute("UPDATE mem_nodes SET swept = '' WHERE swept != ''", []).unwrap_or(0)
    }

    /// Record that the sweep has considered these nodes, so the next run starts
    /// from whatever has been learned since.
    pub fn mark_swept(&self, ids: &[String]) {
        let now = Utc::now().to_rfc3339();
        let conn = self.db.lock();
        for id in ids {
            let _ = conn.execute("UPDATE mem_nodes SET swept = ?2 WHERE id = ?1", params![id, now]);
        }
    }

    /// Overwrite a node's fields by id. Unlike `edit_node` this takes no view on
    /// what is sensible — the sweep has already decided.
    pub fn set_node(&self, id: &str, name: &str, kind: &str, summary: &str) {
        let now = Utc::now().to_rfc3339();
        let conn = self.db.lock();
        let _ = conn.execute(
            "UPDATE mem_nodes SET
               name = CASE WHEN ?2 != '' THEN ?2 ELSE name END,
               norm = CASE WHEN ?3 != '' THEN ?3 ELSE norm END,
               kind = CASE WHEN ?4 != '' THEN ?4 ELSE kind END,
               summary = CASE WHEN ?5 != '' THEN ?5 ELSE summary END,
               updated = ?6 WHERE id = ?1",
            params![id, name.trim(), norm(name), norm(kind), summary.trim(), now],
        );
    }

    /// Fold `absorb` into `keep`: every edge is repointed at the survivor, the
    /// mention counts add up, and the absorbed rows go. Edges that would become
    /// self-loops or collide with one `keep` already has are dropped, since
    /// `mem_edges` is unique on (src, dst, rel). Returns how many nodes went.
    pub fn merge_nodes(&self, keep: &str, absorb: &[String]) -> usize {
        let now = Utc::now().to_rfc3339();
        let conn = self.db.lock();
        let mut gone = 0;
        for id in absorb.iter().filter(|id| id.as_str() != keep) {
            let Some(mentions) = conn
                .query_row("SELECT mentions FROM mem_nodes WHERE id = ?1", params![id], |r| r.get::<_, i64>(0))
                .optional()
                .ok()
                .flatten()
            else {
                continue;
            };
            let _ = conn.execute("UPDATE OR IGNORE mem_edges SET src = ?2 WHERE src = ?1", params![id, keep]);
            let _ = conn.execute("UPDATE OR IGNORE mem_edges SET dst = ?2 WHERE dst = ?1", params![id, keep]);
            let _ = conn.execute("DELETE FROM mem_edges WHERE src = ?1 OR dst = ?1 OR src = dst", params![id]);
            let _ = conn.execute("DELETE FROM mem_nodes WHERE id = ?1", params![id]);
            let _ = conn.execute("UPDATE mem_nodes SET mentions = mentions + ?2, updated = ?3 WHERE id = ?1", params![keep, mentions, now]);
            gone += 1;
        }
        gone
    }

    /// Drop a node and everything hanging off it.
    pub fn delete_node(&self, id: &str) -> bool {
        let conn = self.db.lock();
        let _ = conn.execute("DELETE FROM mem_edges WHERE src = ?1 OR dst = ?1", params![id]);
        conn.execute("DELETE FROM mem_nodes WHERE id = ?1", params![id]).unwrap_or(0) > 0
    }

    pub fn node_count(&self) -> i64 {
        let conn = self.db.lock();
        conn.query_row("SELECT COUNT(*) FROM mem_nodes", [], |r| r.get(0)).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Vault docs in the graph
// ---------------------------------------------------------------------------

/// Register a vault doc (task or note) in the graph: a node carrying its path,
/// linked to its category and project nodes, so `recall` can surface it and the
/// Obsidian graph shows the connection. Idempotent per path.
pub fn link_vault_doc(kind: &str, doc: &crate::vault::Doc) {
    let m = global();
    if m.by_path(&doc.rel).is_some() {
        return;
    }
    let node = m.upsert_node(kind, &doc.title(), "", &doc.rel);
    let cat = doc.str("category");
    let cat_node = if cat.is_empty() { None } else { Some(m.upsert_node("category", &cat, "", "")) };
    if let Some(c) = &cat_node {
        m.add_edge(&node.id, &c.id, "in category", &doc.rel);
    }
    let project = doc.str("project");
    if !project.is_empty() {
        let pn = m.upsert_node("project", &project, "", "");
        m.add_edge(&node.id, &pn.id, "part of", &doc.rel);
        if let Some(c) = &cat_node {
            m.add_edge(&pn.id, &c.id, "in category", "");
        }
        let _ = crate::vault::mirror_memory_node(&pn, &m.neighbors(&pn.id, 12));
    }
    if let Some(c) = &cat_node {
        let _ = crate::vault::mirror_memory_node(c, &m.neighbors(&c.id, 12));
    }
}

/// Startup pass: make sure every task/note file in the vault has a graph node
/// (covers files created before the graph existed, or after a wipe).
pub fn index_vault() -> usize {
    let before = global().node_count();
    for d in crate::vault::list(crate::vault::TASKS) {
        link_vault_doc("task", &d);
    }
    for d in crate::vault::list(crate::vault::NOTES) {
        link_vault_doc("note", &d);
    }
    (global().node_count() - before).max(0) as usize
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

/// Pull entities, relations and durable facts out of one exchange and merge
/// them into the graph. `recorded` lists what the assistant already saved this
/// turn (tasks, notes, expenses…) so the extractor doesn't restate them as
/// facts. Runs after the reply was sent; failures only log.
/// Tools whose turns are worth a pass by the extractor. Everything else the
/// agent can do is a **command** — ticking a shopping list, logging an expense,
/// waking a machine, flipping a setting — and the graph is not a log of those:
/// "Buy shampoo" and "shopping mode enabled" are not things to remember about
/// someone's life. A turn that called no tool at all is plain conversation,
/// which is where "my dentist is Alex" actually shows up, so that is read too.
pub const WORTH_LEARNING: &[&str] = &["save_note", "save_memo", "create_task", "complete_task", "edit_note", "edit_summary"];

/// Whether the extractor should even look at a turn, given the tools it called.
pub fn worth_learning(called: &[String]) -> bool {
    called.is_empty() || called.iter().any(|n| WORTH_LEARNING.contains(&n.as_str()))
}

pub async fn extract(chat_id: i64, user_text: &str, assistant_text: &str, recorded: &[String], evidence_id: i64) {
    let categories = crate::vault::categories().iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", ");
    let system = format!(
        "You maintain a personal knowledge graph for ONE user from their chat with an assistant. Output ONLY minified JSON — no prose, no code fences: \
         {{\"entities\":[{{\"kind\":\"…\",\"name\":\"…\",\"summary\":\"…\"}}],\"relations\":[{{\"from\":\"name\",\"to\":\"name\",\"rel\":\"…\"}}]}}. \
         kind is one of {}. Capture only what is worth remembering later: people (with role/relationship), organizations, projects, places, recurring topics, \
         the user's stated preferences (kind preference, name = short statement), durable facts about their life (kind fact, name = short statement), and dated events. \
         Summaries are one sentence, factual, in third person about the user (\"Alex is the user's dentist\"). Reuse plain canonical names (\"Sam\", not \"Sam (friend)\"). \
         FIXED CATEGORIES already exist and must never be emitted as entities: {categories}. Refer to them by name in relations only (e.g. project X \"belongs to\" Work). \
         A project is narrower than a category (\"Website redesign\" is a project under the Work category; \"Work\" itself is not a project). \
         Do NOT emit facts or events that merely restate something the assistant already recorded this turn (listed under RECORDED): the task/note/expense file is the record. \
         NEVER store the mechanics of using the assistant: items on a shopping list or the list itself (\"Buy shampoo\"), the assistant's own modes, \
         settings or state (\"shopping mode enabled\", \"shopping mode triggers the grocery checklist\"), confirmations of what it just did, single \
         purchases, or anything only useful for the next few minutes. Ask of each entity: would this still matter in a year? If not, drop it. \
         Skip greetings, transient chatter, and anything the assistant merely displayed (balances, lists). If nothing is worth keeping, output {{\"entities\":[],\"relations\":[]}}.",
        KINDS.join("|")
    );
    let recorded_block = if recorded.is_empty() { "(nothing)".to_string() } else { recorded.iter().map(|r| format!("- {}", truncate(r, 160))).collect::<Vec<_>>().join("\n") };
    let prompt = format!("USER:\n{}\n\nASSISTANT:\n{}\n\nRECORDED THIS TURN:\n{}", truncate(user_text, 3000), truncate(assistant_text, 1500), recorded_block);
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
        let name = e["name"].as_str().unwrap_or("").trim();
        if name.is_empty() || name.chars().count() > 120 || kind == "category" {
            continue;
        }
        // Categories are fixed: an entity carrying a category's name (whatever
        // kind the model chose) resolves to the existing category node.
        if let Some(existing) = m.by_name(name).filter(|n| n.kind == "category") {
            ids.insert(norm(name), existing.id.clone());
            continue;
        }
        let kind = if KINDS.contains(&kind.as_str()) { kind } else { "topic".to_string() };
        let summary = e["summary"].as_str().unwrap_or("");
        // A name the graph doesn't know yet may still be someone it does
        // ("Sam" vs "Sam Smith"): Jev decides, and double-checks the kind.
        let (canonical, kind) = if m.by_name(name).is_none() { resolve_entity(&m, name, &kind, summary, user_text).await } else { (name.to_string(), kind) };
        let node = m.upsert_node(&kind, &canonical, summary, "");
        ids.insert(norm(name), node.id.clone());
        let _ = crate::vault::mirror_memory_node(&node, &m.neighbors(&node.id, 12));
    }
    for r in v["relations"].as_array().cloned().unwrap_or_default() {
        let (Some(a), Some(b)) = (r["from"].as_str(), r["to"].as_str()) else { continue };
        let rel = r["rel"].as_str().unwrap_or("related to").trim();
        // Endpoints may be entities from this batch or anything already known
        // (categories, earlier people/projects, tasks the bot filed).
        let resolve = |name: &str| ids.get(&norm(name)).cloned().or_else(|| m.by_name(name).map(|n| n.id));
        let (Some(src), Some(dst)) = (resolve(a), resolve(b)) else { continue };
        m.add_edge(&src, &dst, rel, &evidence);
        for id in [&src, &dst] {
            if let Some(n) = m.get(id) {
                let _ = crate::vault::mirror_memory_node(&n, &m.neighbors(id, 12));
            }
        }
    }
}

/// What each entity kind means, for Jev. Same order as `KINDS`.
const KIND_HINTS: &[&str] = &[
    "A person: a friend, relative, colleague, professional",
    "A company, institution, team or other organization",
    "A project: a bounded piece of work or goal the user is pursuing",
    "A place: a city, venue, address, region",
    "A recurring subject or interest",
    "Something the user likes, dislikes or wants done a certain way",
    "A durable fact about the user's life",
    "Something that happens at a particular time",
];

/// Jev must be this sure two names are one entity before they're merged.
const SAME_ENTITY_MIN: f64 = 0.85;
/// …and this sure of a different kind than the extractor chose.
const KIND_MIN: f64 = 0.8;

/// For an entity name not yet in the graph: the existing node's name if Jev
/// judges it the same entity under another name, else the name as given; and
/// the kind, re-picked by Jev when it is confident. Unchanged when Jev is off.
async fn resolve_entity(m: &Memory, name: &str, kind: &str, summary: &str, context: &str) -> (String, String) {
    let fallback = (name.to_string(), kind.to_string());
    if !crate::jev::enabled() {
        return fallback;
    }
    let candidates: Vec<Node> = m.search(name, 8).into_iter().filter(|n| n.kind != "category" && norm(&n.name) != norm(name)).take(5).collect();
    let mut questions = vec![(
        "kind".to_string(),
        crate::jev::Q::choice(
            format!("What kind of thing is \"{name}\" in the user's personal knowledge graph?"),
            KINDS.iter().zip(KIND_HINTS).map(|(k, h)| (k.to_string(), h.to_string())),
        ),
    )];
    for (i, c) in candidates.iter().enumerate() {
        questions.push((
            format!("same_{i}"),
            crate::jev::Q::noul(
                format!(
                    "Is \"{name}\" (described as: {summary}) the same {kind} as the already-known \"{}\" ({}: {})?",
                    c.name, c.kind, c.summary
                ),
                "The same real-world entity, just named differently (a first name, a nickname, an abbreviation)",
                "Different entities that merely share a word",
            ),
        ));
    }
    let answers = match crate::jev::ask(serde_json::json!({ "conversation": truncate(context, 1500) }), questions).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("jev entity resolution failed: {e}");
            return fallback;
        }
    };
    let best = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| answers.noul(&format!("same_{i}")).map(|p| (p, c)))
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((p, c)) = best.filter(|(p, _)| *p >= SAME_ENTITY_MIN) {
        tracing::info!("memory: '{name}' is '{}' (jev {p:.2})", c.name);
        return (c.name.clone(), c.kind.clone());
    }
    let kind = answers.choice("kind").and_then(|p| p.confident(KIND_MIN).map(str::to_string)).unwrap_or(fallback.1);
    (fallback.0, kind)
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
        let sam2 = m.upsert_node("project", "sam", "", "");
        assert_eq!(sam.id, sam2.id, "case-insensitive dedupe across kinds");
        assert_eq!(sam2.kind, "person", "first kind wins");
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
    fn commands_are_not_learned_from() {
        let t = |v: &[&str]| worth_learning(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(t(&[]), "plain conversation is where facts turn up");
        assert!(t(&["save_note"]));
        assert!(t(&["create_task"]));
        assert!(t(&["edit_summary"]));
        assert!(!t(&["add_shopping_item"]), "a shopping list is not a memory");
        assert!(!t(&["show_shopping_list", "clear_shopping_list"]));
        assert!(!t(&["log_expense"]));
        assert!(!t(&["get_balance", "list_transactions"]));
        assert!(!t(&["wake_machine", "set_timezone", "web_search"]));
        // One worthwhile tool among commands still earns a pass.
        assert!(t(&["add_shopping_item", "save_note"]));
    }

    #[test]
    fn editing_a_node_corrects_renames_and_refuses_collisions() {
        let m = fresh();
        let sam = m.upsert_node("person", "Sam", "Sam lives in Berlin", "");
        m.upsert_node("person", "Alex", "", "");

        // A blank field leaves that one alone.
        let Edit::Updated { after, .. } = m.edit_node("sam", "", "Sam lives in Lisbon", "") else {
            panic!("expected an update");
        };
        assert_eq!(after.id, sam.id, "edits keep the id, so edges still point at it");
        assert_eq!(after.name, "Sam");
        assert_eq!(after.summary, "Sam lives in Lisbon");
        assert_eq!(after.kind, "person");

        // Renaming moves the lookup with it.
        let Edit::Updated { before, after } = m.edit_node("Sam", "Sam Rivera", "", "organization") else {
            panic!("expected an update");
        };
        assert_eq!(before.name, "Sam");
        assert_eq!(after.name, "Sam Rivera");
        assert_eq!(after.kind, "organization");
        assert_eq!(after.summary, "Sam lives in Lisbon", "a blank summary is not a wipe");
        assert!(m.by_name("Sam").is_none());
        assert_eq!(m.by_name("sam rivera").unwrap().id, sam.id);

        assert!(matches!(m.edit_node("Sam Rivera", "Alex", "", ""), Edit::Conflict(n) if n == "Alex"));
        assert!(matches!(m.edit_node("nobody", "", "x", ""), Edit::Missing));
        // `category` is seeded, never something an edit can conjure.
        let Edit::Updated { after, .. } = m.edit_node("Sam Rivera", "", "", "category") else {
            panic!("expected an update");
        };
        assert_eq!(after.kind, "organization");
    }

    #[test]
    fn categories_are_seeded_and_win() {
        let m = fresh();
        m.seed_categories();
        let cat = m.by_name("Work").expect("seeded");
        assert_eq!(cat.kind, "category");
        let again = m.upsert_node("project", "Work", "", "");
        assert_eq!(again.id, cat.id, "a 'project' called Work is the category");
        assert_eq!(again.kind, "category");
        let proto = m.upsert_node("project", "Work prototype", "", "");
        assert_ne!(proto.id, cat.id);
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

#[cfg(test)]
mod clear_ctx_tests {
    use super::*;

    #[test]
    fn clear_context_hides_old_turns_and_does_not_deadlock() {
        let db = crate::db::Db::memory().unwrap();
        crate::config::init(db.clone());
        let m = Memory { db };
        m.add_message(5, "user", "one");
        m.add_message(5, "assistant", "two");
        let (tx, rx) = std::sync::mpsc::channel();
        let m2 = m.clone();
        std::thread::spawn(move || { let n = m2.clear_context(5); tx.send(n).unwrap(); });
        let n = rx.recv_timeout(std::time::Duration::from_secs(3)).expect("clear_context deadlocked");
        assert_eq!(n, 2);
        assert!(m.recent_messages(5, 10).is_empty());
        m.add_message(5, "user", "three");
        assert_eq!(m.recent_messages(5, 10).len(), 1);
    }
}
