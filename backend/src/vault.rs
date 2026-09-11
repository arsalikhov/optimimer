//! The Markdown vault: notes, tasks and summaries as `.md` files with YAML
//! frontmatter under `VAULT_DIR` (default `vault/`). The folder is meant to be
//! an Obsidian vault synced by Obsidian Sync / Syncthing; every property we
//! write is a plain frontmatter key so Obsidian **Bases** can filter, sort and
//! group these files without plugins. The bot never talks to a sync service —
//! it just writes files.
//!
//! Layout:
//!   notes/<date>-<slug>.md       type: note     — `/note`
//!   tasks/<date>-<slug>.md       type: task     — `/todo`, `/complete`
//!   summaries/<date>-<slug>.md   type: summary  — forwarded chats, voice memos
//!   finance/<date>-<slug>-<id>.md type: transaction — mirror of the SQLite ledger (read-only)
//!   *.base + Home.md              Bases views per folder and a dashboard (written once if missing)
//!
//! Only a small YAML subset is written and read (scalars, quoted strings, inline
//! lists), which is all Obsidian's property editor produces.

use anyhow::Result;
use chrono::Utc;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const NOTES: &str = "notes";
pub const TASKS: &str = "tasks";
pub const SUMMARIES: &str = "summaries";
/// Ledger mirror: one small note per transaction so Obsidian Bases can chart them.
pub const FINANCE: &str = "finance";
/// Knowledge-graph mirror: one flat note per memory node (people, projects, categories, facts…),
/// wikilinked to its neighbours and to the task/note files it relates to.
pub const MEMORY: &str = "memory";

/// Root of the vault. `VAULT_DIR` may be absolute (`/opt/optimimer/vault`) or
/// relative to the working directory.
pub fn dir() -> PathBuf {
    PathBuf::from(std::env::var("VAULT_DIR").unwrap_or_else(|_| "vault".to_string()))
}

/// Create the vault folders. Call once at startup; harmless if they exist.
pub fn init() -> Result<PathBuf> {
    let root = dir();
    for sub in [NOTES, TASKS, SUMMARIES, FINANCE, MEMORY] {
        fs::create_dir_all(root.join(sub))?;
    }
    Ok(root)
}

/// One parsed Markdown file.
#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub path: PathBuf,
    /// Path relative to the vault root, without `.md` — how Obsidian names it.
    pub rel: String,
    /// Frontmatter in file order.
    pub props: Vec<(String, Value)>,
    pub body: String,
}

impl Doc {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.props.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn str(&self, key: &str) -> String {
        match self.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string().trim_matches('"').to_string(),
        }
    }
    pub fn title(&self) -> String {
        let t = self.str("title");
        if !t.is_empty() {
            return t;
        }
        self.rel.rsplit('/').next().unwrap_or(&self.rel).to_string()
    }
    pub fn set(&mut self, key: &str, val: Value) {
        match self.props.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = val,
            None => self.props.push((key.to_string(), val)),
        }
    }
    /// Sort key: `created` if present, else the file's mtime.
    pub fn created(&self) -> String {
        let c = self.str("created");
        if !c.is_empty() {
            return c;
        }
        fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .map(|t| chrono::DateTime::<Utc>::from(t).to_rfc3339())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// YAML subset
// ---------------------------------------------------------------------------

fn yaml_scalar(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            // Bare only when it can't be misread: dates/times, plain words.
            let plain = !s.is_empty()
                && s.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | 'T' | '.' | ' ' | '/'))
                && !s.starts_with(' ')
                && !s.ends_with(' ')
                && !matches!(s.to_lowercase().as_str(), "true" | "false" | "null" | "yes" | "no")
                && !s.contains(": ");
            if plain { s.clone() } else { Value::String(s.clone()).to_string() }
        }
        Value::Array(a) => format!("[{}]", a.iter().map(yaml_scalar).collect::<Vec<_>>().join(", ")),
        Value::Object(_) => v.to_string(),
    }
}

fn render(doc: &Doc) -> String {
    let mut out = String::from("---\n");
    for (k, v) in &doc.props {
        out.push_str(k);
        out.push(':');
        let s = yaml_scalar(v);
        if !s.is_empty() {
            out.push(' ');
            out.push_str(&s);
        }
        out.push('\n');
    }
    out.push_str("---\n");
    if !doc.body.is_empty() {
        out.push('\n');
        out.push_str(doc.body.trim_end());
        out.push('\n');
    }
    out
}

fn parse_scalar(raw: &str) -> Value {
    let s = raw.trim();
    if s.is_empty() {
        return Value::Null;
    }
    if s.starts_with('"') {
        return serde_json::from_str::<String>(s).map(Value::String).unwrap_or_else(|_| Value::String(s.trim_matches('"').into()));
    }
    if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
        return Value::String(s[1..s.len() - 1].replace("''", "'"));
    }
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        let items = inner
            .split(',')
            .map(parse_scalar)
            .filter(|v| !v.is_null())
            .collect::<Vec<_>>();
        return Value::Array(items);
    }
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" | "~" => Value::Null,
        _ => {
            if let Ok(n) = s.parse::<i64>() {
                return Value::from(n);
            }
            Value::String(s.to_string())
        }
    }
}

/// Parse a Markdown file with optional frontmatter. Also accepts Obsidian's
/// block-style lists (`tags:` followed by `  - x` lines).
pub fn parse(text: &str) -> (Vec<(String, Value)>, String) {
    let mut props = Vec::new();
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (props, text.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (props, text.to_string());
    };
    let fm = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']).trim_end().to_string();
    let mut lines = fm.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let key = k.trim().to_string();
        if v.trim().is_empty() {
            // Block list?
            let mut items = Vec::new();
            while let Some(next) = lines.peek() {
                let t = next.trim_start();
                if let Some(item) = t.strip_prefix("- ") {
                    items.push(parse_scalar(item));
                    lines.next();
                } else {
                    break;
                }
            }
            props.push((key, if items.is_empty() { Value::Null } else { Value::Array(items) }));
        } else {
            props.push((key, parse_scalar(v)));
        }
    }
    (props, body)
}

pub fn read(path: &Path) -> Option<Doc> {
    let text = fs::read_to_string(path).ok()?;
    let (props, body) = parse(&text);
    let rel = path
        .strip_prefix(dir())
        .unwrap_or(path)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    Some(Doc { path: path.to_path_buf(), rel, props, body })
}

pub fn save(doc: &Doc) -> Result<()> {
    if let Some(p) = doc.path.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(&doc.path, render(doc))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 60 {
            break;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() { "untitled".into() } else { s }
}

/// A file name Obsidian is happy with, kept human-readable: the graph view,
/// search results and backlinks all label notes by file name, so "Make a spec
/// sheet.md" beats "2026-09-11-make-a-spec-sheet.md". Strips the characters
/// Obsidian and common filesystems reject, collapses whitespace, caps length.
pub fn filename(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']') {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut t: String = collapsed.trim_matches(|c: char| c == '.' || c == ' ').chars().take(80).collect();
    t = t.trim_end().to_string();
    if t.is_empty() { "Untitled".into() } else { t }
}

/// `<sub>/<Title>.md`, suffixed " 2", " 3"… if taken. The date lives in
/// frontmatter (`created`), not the name.
fn fresh_path(sub: &str, title: &str, _date: &str) -> PathBuf {
    let base = filename(title);
    let d = dir().join(sub);
    let mut p = d.join(format!("{base}.md"));
    let mut n = 2;
    while p.exists() {
        p = d.join(format!("{base} {n}.md"));
        n += 1;
    }
    p
}

/// All docs under a subfolder, newest first.
pub fn list(sub: &str) -> Vec<Doc> {
    let mut docs: Vec<Doc> = fs::read_dir(dir().join(sub))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
                .filter_map(|p| read(&p))
                .collect()
        })
        .unwrap_or_default();
    docs.sort_by_key(|d| std::cmp::Reverse(d.created()));
    docs
}

// ---------------------------------------------------------------------------
// Categories (top level) and projects (nested under a category)
// ---------------------------------------------------------------------------

/// Default taxonomy. `category` is the coarse bucket every task and note gets;
/// `project` is an optional finer label that lives under one category
/// (Fitness → Triathlon, SageMesh → Launch). Override the names with
/// `VAULT_CATEGORIES="Admin:chores and errands, Work:…"` (name:hint pairs).
const DEFAULT_CATEGORIES: &[(&str, &str)] = &[
    ("Admin", "chores, errands, appointments (dentist, doctor, mechanic), bills, paperwork, calls to make"),
    ("SageMesh", "anything about the SageMesh company or product"),
    ("Aqusense", "anything about the Aqusense venture"),
    ("Fitness", "training, workouts, races, gear (projects: Triathlon, Ironman)"),
    ("Home", "house, repairs, furniture, moving, groceries logistics"),
    ("Finance", "money admin, taxes, investments, subscriptions"),
    ("Learning", "courses, reading, study, skills"),
    ("Social", "friends, family, events, gifts"),
    ("Travel", "trips, bookings, packing"),
    ("Personal", "hobbies and anything that fits nowhere else"),
];

/// (name, hint) pairs, from `VAULT_CATEGORIES` or the built-in list.
pub fn categories() -> Vec<(String, String)> {
    if let Ok(v) = std::env::var("VAULT_CATEGORIES") {
        let parsed: Vec<(String, String)> = v
            .split(',')
            .filter_map(|item| {
                let (name, hint) = item.split_once(':').unwrap_or((item, ""));
                let name = name.trim();
                (!name.is_empty()).then(|| (name.to_string(), hint.trim().to_string()))
            })
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    DEFAULT_CATEGORIES.iter().map(|(n, h)| (n.to_string(), h.to_string())).collect()
}

/// Snap a model-chosen category onto the configured list (case-insensitive,
/// also accepts a prefix match like "sage" → "SageMesh"); falls back to the
/// last entry, which is the catch-all.
pub fn clamp_category(c: &str) -> String {
    let cats = categories();
    let c = c.trim().to_lowercase();
    if let Some((n, _)) = cats.iter().find(|(n, _)| n.to_lowercase() == c) {
        return n.clone();
    }
    if !c.is_empty() {
        if let Some((n, _)) = cats.iter().find(|(n, _)| n.to_lowercase().starts_with(&c) || c.starts_with(&n.to_lowercase())) {
            return n.clone();
        }
    }
    cats.last().map(|(n, _)| n.clone()).unwrap_or_else(|| "Personal".into())
}

/// "Name — hint" lines for a parser prompt.
pub fn categories_prompt() -> String {
    categories()
        .iter()
        .map(|(n, h)| if h.is_empty() { format!("- {n}") } else { format!("- {n}: {h}") })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

pub struct NewNote {
    pub title: String,
    pub category: String,
    pub tags: Vec<String>,
    /// inbox | draft | final
    pub status: String,
    pub body: String,
    pub source: String,
}

pub fn write_note(n: NewNote) -> Result<Doc> {
    let now = Utc::now().to_rfc3339();
    let mut doc = Doc { path: fresh_path(NOTES, &n.title, &now), ..Default::default() };
    doc.set("title", Value::String(n.title));
    doc.set("type", Value::String("note".into()));
    doc.set("category", Value::String(clamp_category(&n.category)));
    doc.set("status", Value::String(if n.status.is_empty() { "inbox".into() } else { n.status.to_lowercase() }));
    doc.set("tags", Value::Array(n.tags.into_iter().map(|t| Value::String(slug(&t))).collect()));
    doc.set("source", Value::String(n.source));
    doc.set("created", Value::String(now));
    doc.body = n.body;
    save(&doc)?;
    Ok(read(&doc.path).unwrap_or(doc))
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

pub struct NewTask {
    pub title: String,
    /// One of `categories()`; snapped by `clamp_category`.
    pub category: String,
    /// high | medium | low
    pub priority: String,
    /// todo | doing
    pub status: String,
    /// Local time, `YYYY-MM-DDTHH:MM` or `YYYY-MM-DD` (empty = undated).
    pub due: String,
    pub due_end: String,
    pub project: String,
    pub body: String,
}

/// Obsidian's date/datetime properties want local ISO without an offset.
pub fn local_iso(rfc3339: &str) -> String {
    let s = rfc3339.trim();
    if s.len() >= 16 && s.as_bytes()[10] == b'T' {
        s[..16].to_string()
    } else {
        s.get(..10).unwrap_or(s).to_string()
    }
}

pub fn write_task(t: NewTask) -> Result<Doc> {
    let now = Utc::now().to_rfc3339();
    let mut doc = Doc { path: fresh_path(TASKS, &t.title, &now), ..Default::default() };
    doc.set("title", Value::String(t.title));
    doc.set("type", Value::String("task".into()));
    doc.set("category", Value::String(clamp_category(&t.category)));
    doc.set("status", Value::String(if t.status.is_empty() { "todo".into() } else { t.status.to_lowercase() }));
    doc.set("priority", Value::String(if t.priority.is_empty() { "medium".into() } else { t.priority.to_lowercase() }));
    doc.set("due", if t.due.is_empty() { Value::Null } else { Value::String(t.due) });
    doc.set("due_end", if t.due_end.is_empty() { Value::Null } else { Value::String(t.due_end) });
    doc.set("project", if t.project.is_empty() { Value::Null } else { Value::String(t.project) });
    doc.set("completed", Value::Null);
    doc.set("created", Value::String(now));
    doc.body = t.body;
    save(&doc)?;
    Ok(read(&doc.path).unwrap_or(doc))
}

pub fn is_open(d: &Doc) -> bool {
    !matches!(d.str("status").to_lowercase().as_str(), "done" | "cancelled" | "canceled")
}

pub fn open_tasks() -> Vec<Doc> {
    list(TASKS).into_iter().filter(is_open).collect()
}

/// "Category: project, project" lines — existing projects grouped under
/// their category, so the parser reuses names instead of inventing them.
pub fn projects_prompt() -> String {
    let mut by_cat: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for d in list(TASKS).iter().chain(list(NOTES).iter()) {
        let p = d.str("project");
        if p.is_empty() {
            continue;
        }
        let c = { let c = d.str("category"); if c.is_empty() { "Personal".to_string() } else { c } };
        let v = by_cat.entry(c).or_default();
        if !v.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
            v.push(p);
        }
    }
    if by_cat.is_empty() {
        return "(none yet)".into();
    }
    by_cat.into_iter().map(|(c, ps)| format!("- {c}: {}", ps.join(", "))).collect::<Vec<_>>().join("\n")
}

pub fn complete(doc: &mut Doc) -> Result<()> {
    doc.set("status", Value::String("done".into()));
    doc.set("completed", Value::String(local_iso(&Utc::now().to_rfc3339())));
    save(doc)
}

// ---------------------------------------------------------------------------
// Summaries
// ---------------------------------------------------------------------------

pub struct NewSummary {
    pub title: String,
    /// forward | voice
    pub kind: String,
    pub source: String,
    pub note: String,
    pub summary: Vec<String>,
    pub actions: Vec<String>,
    pub transcript: String,
}

pub fn write_summary(s: NewSummary) -> Result<Doc> {
    let now = Utc::now().to_rfc3339();
    let mut doc = Doc { path: fresh_path(SUMMARIES, &s.title, &now), ..Default::default() };
    doc.set("title", Value::String(s.title));
    doc.set("type", Value::String("summary".into()));
    doc.set("kind", Value::String(s.kind));
    doc.set("source", Value::String(s.source));
    doc.set("tags", Value::Array(vec![Value::String("summary".into())]));
    doc.set("created", Value::String(now));
    let mut body = String::new();
    if !s.note.trim().is_empty() {
        body.push_str(&format!("> {}\n\n", s.note.trim()));
    }
    body.push_str("## Summary\n\n");
    for b in &s.summary {
        body.push_str(&format!("- {b}\n"));
    }
    if !s.actions.is_empty() {
        body.push_str("\n## Action items\n\n");
        for a in &s.actions {
            body.push_str(&format!("- [ ] {a}\n"));
        }
    }
    body.push_str("\n## Transcript\n\n");
    for line in s.transcript.lines() {
        if !line.trim().is_empty() {
            body.push_str(line.trim());
            body.push_str("  \n");
        }
    }
    doc.body = body;
    save(&doc)?;
    Ok(read(&doc.path).unwrap_or(doc))
}

// ---------------------------------------------------------------------------
// Finance mirror
// ---------------------------------------------------------------------------

fn txn_suffix(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect()
}

/// Write (or overwrite) the note mirroring one ledger row. The file name ends
/// in the first 8 chars of the row id so delete/backfill can find it without
/// parsing every file. SQLite stays the source of truth: edits made to these
/// files in Obsidian do not flow back.
pub fn write_transaction(t: &crate::finance::Txn) -> Result<Doc> {
    let d = dir().join(FINANCE);
    fs::create_dir_all(&d)?;
    let path = d.join(format!("{}-{}-{}.md", t.date, slug(&t.name), txn_suffix(&t.id)));
    let mut doc = Doc { path, ..Default::default() };
    doc.set("title", Value::String(t.name.clone()));
    doc.set("type", Value::String("transaction".into()));
    doc.set("date", Value::String(t.date.clone()));
    doc.set("amount", serde_json::json!((t.amount * 100.0).round() / 100.0));
    doc.set("direction", Value::String(t.direction.clone()));
    doc.set("category", Value::String(t.category.clone()));
    doc.set("source", Value::String(t.source.clone()));
    doc.set("flagged", Value::Bool(!t.note.is_empty()));
    doc.set("txn_id", Value::String(t.id.clone()));
    doc.set("created", Value::String(t.created.clone()));
    doc.body = t.note.clone();
    save(&doc)?;
    Ok(doc)
}

/// Remove the mirror note for a ledger row id (no-op if absent).
pub fn remove_transaction(id: &str) -> Result<()> {
    let suffix = format!("-{}.md", txn_suffix(id));
    if let Ok(rd) = fs::read_dir(dir().join(FINANCE)) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().ends_with(&suffix) {
                fs::remove_file(e.path())?;
            }
        }
    }
    Ok(())
}

/// Write mirror notes for any ledger rows that don't have one yet (startup).
/// Returns how many were written.
pub fn backfill_transactions(txns: &[crate::finance::Txn]) -> usize {
    let present: std::collections::HashSet<String> = fs::read_dir(dir().join(FINANCE))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".md").map(str::to_string))
                .filter_map(|n| n.rsplit('-').next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut n = 0;
    for t in txns {
        if present.contains(&txn_suffix(&t.id)) {
            continue;
        }
        match write_transaction(t) {
            Ok(_) => n += 1,
            Err(e) => tracing::warn!("finance mirror write failed for {}: {e}", t.id),
        }
    }
    n
}

/// Starter files dropped into the vault root if absent: one `.base` per
/// folder plus a `Home.md` dashboard that embeds a view from each. Never
/// overwritten — the user is expected to customise them. Returns the names
/// written this time.
pub fn ensure_starter_files() -> Result<Vec<&'static str>> {
    const FILES: &[(&str, &str)] = &[
        ("Finances.base", include_str!("../assets/Finances.base")),
        ("Tasks.base", include_str!("../assets/Tasks.base")),
        ("Notes.base", include_str!("../assets/Notes.base")),
        ("Summaries.base", include_str!("../assets/Summaries.base")),
        ("Memory.base", include_str!("../assets/Memory.base")),
        ("Home.md", include_str!("../assets/Home.md")),
    ];
    let mut written = Vec::new();
    for (name, body) in FILES {
        let p = dir().join(name);
        if p.exists() {
            continue;
        }
        fs::write(&p, body)?;
        written.push(*name);
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Memory graph mirror
// ---------------------------------------------------------------------------

/// Where a node's note lives. Tasks and notes have no memory note: their own
/// vault file is the record, so links point straight at it.
fn memory_link(node: &crate::memory::Node) -> String {
    if !node.path.is_empty() {
        return node.path.clone();
    }
    format!("{MEMORY}/{}", filename(&node.name))
}

/// Write (or rewrite) the vault note for one memory node: frontmatter with its
/// kind and stats, the summary, and a "Related" list of wikilinks — to other
/// memory notes or, for task/note neighbours, to the vault file itself. Nodes
/// that ARE vault files (tasks, notes) get no note of their own.
pub fn mirror_memory_node(node: &crate::memory::Node, neighbors: &[(String, crate::memory::Node)]) -> Result<Doc> {
    if !node.path.is_empty() {
        return Ok(Doc::default());
    }
    let rel = memory_link(node);
    let path = dir().join(format!("{rel}.md"));
    let mut doc = Doc { path, rel: rel.clone(), ..Default::default() };
    doc.set("title", Value::String(node.name.clone()));
    doc.set("type", Value::String("memory".into()));
    doc.set("kind", Value::String(node.kind.clone()));
    doc.set("mentions", Value::from(node.mentions));
    doc.set("updated", Value::String(local_iso(&node.updated)));
    let mut body = String::new();
    if !node.summary.is_empty() {
        body.push_str(&node.summary);
        body.push('\n');
    }
    if !neighbors.is_empty() {
        body.push_str("\n## Related\n\n");
        for (r, n) in neighbors {
            body.push_str(&format!("- {r}: [[{}|{}]]\n", memory_link(n), n.name));
        }
    }
    doc.body = body;
    save(&doc)?;
    Ok(doc)
}

/// How many memory notes exist on disk (to decide whether to re-mirror at start).
pub fn memory_mirror_count() -> usize {
    fs::read_dir(dir().join(MEMORY))
        .map(|rd| rd.flatten().filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md")).count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

fn terms(q: &str) -> Vec<String> {
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(str::to_string)
        .collect()
}

/// Rank docs in `sub` by term overlap: title hits weigh 3, body hits 1 each
/// (capped), tags 2. Returns up to `limit` scored docs, best first.
pub fn search(sub: &str, query: &str, limit: usize) -> Vec<Doc> {
    let ts = terms(query);
    if ts.is_empty() {
        return vec![];
    }
    let mut scored: Vec<(i64, Doc)> = list(sub)
        .into_iter()
        .filter_map(|d| {
            let title = d.title().to_lowercase();
            let body = d.body.to_lowercase();
            let tags = d.get("tags").map(|t| t.to_string().to_lowercase()).unwrap_or_default();
            let mut score = 0i64;
            for t in &ts {
                if title.contains(t.as_str()) {
                    score += 3;
                }
                if tags.contains(t.as_str()) {
                    score += 2;
                }
                score += (body.matches(t.as_str()).count() as i64).min(3);
            }
            (score > 0).then_some((score, d))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.created().cmp(&a.1.created())));
    scored.into_iter().take(limit).map(|(_, d)| d).collect()
}

/// Open tasks whose title contains every query term (or, failing that, any).
pub fn find_open_tasks(query: &str) -> Vec<Doc> {
    let ts = terms(query);
    let open = open_tasks();
    if ts.is_empty() {
        return open;
    }
    let all: Vec<Doc> = open
        .iter()
        .filter(|d| {
            let t = d.title().to_lowercase();
            ts.iter().all(|q| t.contains(q.as_str()))
        })
        .cloned()
        .collect();
    if !all.is_empty() {
        return all;
    }
    open.into_iter()
        .filter(|d| {
            let t = d.title().to_lowercase();
            ts.iter().any(|q| t.contains(q.as_str()))
        })
        .collect()
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Guard against an unset VAULT_DIR pointing somewhere unexpected in prod.
pub fn describe() -> String {
    dir().canonicalize().map(|p| p.display().to_string()).unwrap_or_else(|_| dir().display().to_string())
}

pub fn err_hint(e: &anyhow::Error) -> String {
    format!("{e} (VAULT_DIR = {})", describe())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests share the process env, so serialise the ones that set VAULT_DIR.
    static ENV: Mutex<()> = Mutex::new(());

    fn temp_vault() -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        let g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        // Under target/ (gitignored) rather than the system temp dir, which some
        // sandboxes point at the working directory.
        let p = std::env::current_dir().unwrap().join("target").join("test-vaults").join(uuid::Uuid::new_v4().to_string());
        std::env::set_var("VAULT_DIR", &p);
        init().unwrap();
        (g, p)
    }

    #[test]
    fn frontmatter_round_trips() {
        let mut d = Doc::default();
        d.set("title", Value::String("A: b & \"c\"".into()));
        d.set("tags", Value::Array(vec![Value::String("x".into()), Value::String("y-z".into())]));
        d.set("due", Value::String("2026-09-12T14:00".into()));
        d.set("done", Value::Bool(false));
        d.set("project", Value::Null);
        d.body = "hello\nworld".into();
        let text = render(&d);
        assert!(text.contains("due: 2026-09-12T14:00\n"), "{text}");
        assert!(text.contains("project:\n"));
        let (props, body) = parse(&text);
        let back = Doc { props, body, ..Default::default() };
        assert_eq!(back.str("title"), "A: b & \"c\"");
        assert_eq!(back.get("tags").unwrap().as_array().unwrap().len(), 2);
        assert_eq!(back.get("done"), Some(&Value::Bool(false)));
        assert_eq!(back.body, "hello\nworld");
    }

    #[test]
    fn parses_obsidian_block_lists() {
        let (props, _) = parse("---\ntags:\n  - a\n  - b\nstatus: todo\n---\nbody");
        let d = Doc { props, ..Default::default() };
        assert_eq!(d.get("tags").unwrap().as_array().unwrap().len(), 2);
        assert_eq!(d.str("status"), "todo");
    }

    #[test]
    fn task_lifecycle_and_search() {
        let (_g, root) = temp_vault();
        let t = write_task(NewTask {
            title: "Call Sam about the venue".into(),
            category: "social".into(),
            priority: "High".into(),
            status: String::new(),
            due: "2026-09-12T14:00".into(),
            due_end: String::new(),
            project: "Wedding".into(),
            body: "call sam".into(),
        })
        .unwrap();
        assert!(t.path.starts_with(root.join("tasks")));
        assert_eq!(t.str("priority"), "high");
        assert_eq!(t.str("category"), "Social", "category snapped to the list");
        assert_eq!(open_tasks().len(), 1);
        assert_eq!(projects_prompt(), "- Social: Wedding");
        assert_eq!(find_open_tasks("sam venue").len(), 1);
        assert_eq!(find_open_tasks("dentist").len(), 0);
        let mut found = find_open_tasks("call sam").remove(0);
        complete(&mut found).unwrap();
        assert!(open_tasks().is_empty());
        assert_eq!(read(&found.path).unwrap().str("status"), "done");
        // Same title again gets a -2 suffix rather than clobbering.
        let t2 = write_task(NewTask { title: "Call Sam about the venue".into(), category: String::new(), priority: String::new(), status: String::new(), due: String::new(), due_end: String::new(), project: String::new(), body: String::new() }).unwrap();
        assert!(t2.rel.ends_with("Call Sam about the venue 2"), "{}", t2.rel);
        assert!(t.rel.ends_with("tasks/Call Sam about the venue"), "{}", t.rel);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn notes_write_and_search() {
        let (_g, root) = temp_vault();
        write_note(NewNote { title: "VPN setup".into(), category: "Learning".into(), tags: vec!["Home Lab".into()], status: "Inbox".into(), body: "wireguard on the pi".into(), source: "telegram".into() }).unwrap();
        write_note(NewNote { title: "Taxes 2026".into(), category: "finance".into(), tags: vec![], status: String::new(), body: "file by april".into(), source: "telegram".into() }).unwrap();
        let hits = search(NOTES, "wireguard", 8);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title(), "VPN setup");
        assert_eq!(hits[0].get("tags").unwrap()[0], Value::String("home-lab".into()));
        assert_eq!(list(NOTES).len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn category_clamping() {
        assert_eq!(clamp_category("admin"), "Admin");
        assert_eq!(clamp_category("Sage"), "SageMesh");
        assert_eq!(clamp_category("chores"), "Personal", "unknown → catch-all");
        assert_eq!(clamp_category(""), "Personal");
    }

    #[test]
    fn filenames_are_readable_and_safe() {
        assert_eq!(filename("Make a spec sheet"), "Make a spec sheet");
        assert_eq!(filename("Call Sam: venue / budget?"), "Call Sam venue budget");
        assert_eq!(filename("  ...  "), "Untitled");
        assert_eq!(filename("#tag [x] a|b"), "tag x a b");
    }

    #[test]
    fn local_iso_strips_offset() {
        assert_eq!(local_iso("2026-09-12T14:00:00-04:00"), "2026-09-12T14:00");
        assert_eq!(local_iso("2026-09-12"), "2026-09-12");
        assert_eq!(local_iso(""), "");
    }
}
