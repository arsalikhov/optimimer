//! The weekly memory sweep: a strong model re-reads what the graph has learned
//! recently and folds duplicates together.
//!
//! The extractor that runs after every turn is deliberately cheap and eager, so
//! the graph drifts: "Sam", "Sam Rivera" and "my climbing partner Sam" become
//! three nodes, each with a fragment of the story. Once a week the top model
//! (`MEMORY_SWEEP_MODEL`, Opus on the paid tier) is handed those nodes and
//! merges them into one node with the fuller summary.
//!
//! What keeps this affordable as the graph grows is that a node is only ever
//! read **once**: `mem_nodes.swept` is stamped for everything a run considered,
//! and the next run starts from what has been learned since. Look-alikes of a
//! new node are pulled in by full-text search even if they were swept before,
//! so an old "Sam" can still absorb a new one.
//!
//! Guard rails, because this deletes things: category nodes are never touched
//! (they are seeded, not learned), nodes that mirror a task or note file are
//! never deleted or absorbed (the file is the record), and every id the model
//! names must have been in the batch it was shown.

use crate::memory::{self, Node};
use chrono::Utc;
use serde_json::Value;

/// Days between runs.
fn every_days() -> i64 {
    std::env::var("MEMORY_SWEEP_DAYS").ok().and_then(|s| s.parse().ok()).filter(|d| *d > 0).unwrap_or(7)
}

/// Most nodes read in one run — the ceiling on what a sweep can cost.
fn batch_size() -> usize {
    std::env::var("MEMORY_SWEEP_MAX").ok().and_then(|s| s.parse().ok()).filter(|n| *n > 0).unwrap_or(60)
}

fn model() -> String {
    match std::env::var("MEMORY_SWEEP_MODEL") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => crate::llm::max(),
    }
}

/// `off` / `0` / `false` disables the worker; the owner can still ask for a
/// sweep by hand.
fn enabled() -> bool {
    !matches!(
        std::env::var("MEMORY_SWEEP").unwrap_or_default().trim().to_lowercase().as_str(),
        "off" | "0" | "false" | "no"
    )
}

/// Setting holding the last run's timestamp.
const LAST_RUN: &str = "memory_swept_at";

/// What one run did, in the order a person would want to hear it.
#[derive(Default)]
pub struct Report {
    pub considered: usize,
    pub merged: usize,
    /// Entries that were about using the assistant, not about the user.
    pub noise: usize,
    /// Entries that were simply empty or unintelligible.
    pub deleted: usize,
    /// Entries dropped because their note was deleted in the vault.
    pub from_vault: usize,
    /// Entries still waiting for a later run.
    pub remaining: i64,
    /// One line per change, e.g. "Sam Rivera ← Sam, climbing partner Sam".
    pub lines: Vec<String>,
}

impl Report {
    pub fn changed(&self) -> bool {
        self.merged > 0 || self.noise > 0 || self.deleted > 0 || self.from_vault > 0
    }

    /// A message for the owner. Empty when nothing happened.
    pub fn message(&self) -> String {
        if !self.changed() {
            return String::new();
        }
        let mut m = format!("Tidied up memory: {} merged", self.merged);
        if self.noise > 0 {
            m.push_str(&format!(", {} command leftover{} dropped", self.noise, if self.noise == 1 { "" } else { "s" }));
        }
        if self.deleted > 0 {
            m.push_str(&format!(", {} empty dropped", self.deleted));
        }
        m.push_str(&format!(", out of {} entries.", self.considered));
        if self.from_vault > 0 {
            m.push_str(&format!(
                "\nAlso forgot {} entr{} you deleted in the vault.",
                self.from_vault,
                if self.from_vault == 1 { "y" } else { "ies" }
            ));
        }
        for l in self.lines.iter().take(10) {
            m.push_str(&format!("\n• {l}"));
        }
        if self.lines.len() > 10 {
            m.push_str(&format!("\n…and {} more.", self.lines.len() - 10));
        }
        if self.remaining > 0 {
            m.push_str(&format!("\n{} entries still to go — ask again to carry on.", self.remaining));
        }
        m
    }
}

/// The batch for one run: every node learned since the last sweep, plus the
/// look-alikes full-text search turns up for each — those are the ones a new
/// node might be a duplicate of.
fn batch(m: &memory::Memory) -> Vec<Node> {
    let fresh = m.unswept(batch_size());
    let mut out: Vec<Node> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    fn push(n: Node, out: &mut Vec<Node>, seen: &mut Vec<String>) {
        if n.kind != "category" && !seen.contains(&n.id) {
            seen.push(n.id.clone());
            out.push(n);
        }
    }
    for n in fresh {
        let query = format!("{} {}", n.name, n.summary);
        push(n, &mut out, &mut seen);
        for near in m.search(&query, 4) {
            push(near, &mut out, &mut seen);
        }
    }
    out
}

fn render(nodes: &[Node]) -> String {
    nodes
        .iter()
        .map(|n| {
            let summary = if n.summary.is_empty() { "(no summary)" } else { n.summary.as_str() };
            let file = if n.path.is_empty() { String::new() } else { format!(" [file: {}]", n.path) };
            format!("{} | {} | {} | {}{}", n.id, n.kind, n.name, summary, file)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const SYSTEM: &str = "\
You are tidying the long-term memory of a personal assistant. You are given entries as `id | kind | name | summary`, \
some of which describe the same person, project, place or fact under different names or with overlapping detail.

Output ONLY minified JSON: {\"merge\":[{\"keep\":\"<id>\",\"absorb\":[\"<id>\",…],\"name\":\"…\",\"kind\":\"…\",\
\"summary\":\"…\"}],\"noise\":[\"<id>\"],\"delete\":[\"<id>\"]} — no prose, no code fences.

- merge: only entries that are genuinely the same thing. `keep` is the id that survives — prefer one marked [file:…], \
  otherwise the fullest entry. `name` is the best canonical name, `kind` its kind, and `summary` ONE sentence in third \
  person that keeps every distinct fact from all of them. Losing detail is worse than leaving a duplicate.
- noise: entries about USING the assistant rather than about the user's life. This memory is a picture of a person, \
  not a log of what they asked a bot to do. Anything in these classes is noise:
  * items on a shopping or to-buy list, and the lists themselves — \"Buy shampoo\", \"Milk and eggs\"
  * the assistant's own modes, settings, features or behaviour — \"shopping mode enabled\", \"shopping mode triggers \
    the grocery checklist\", \"the bot wakes the desktop by Wake-on-LAN\"
  * a single purchase, expense or ledger row — \"spent $12.50 on lunch\"
  * a confirmation or restatement of something the assistant did — \"task saved\", \"reminder set for 9am\"
  * one-off requests and their answers — a stock watch, a machine that was woken, a page that was searched
  * anything that stopped mattering the same day it was said
  Real memories look different: who someone is, what a project is for, a standing preference, where the user lives, \
  something true about their life. Ask of each entry: would this still matter in a year, to someone who never used \
  this bot? If not, it is noise.
- delete: the leftovers — entries that are empty, unintelligible, or say nothing at all.
- Never put an entry marked [file:…] in `noise` or `delete`; a task or note file is the record and outlives any \
  tidy-up. When unsure, leave an entry alone — a stale memory is cheaper than a lost one.
- Two people who merely share a first name are different people. A project and the person running it are different \
  entries — link them in a summary, don't merge them.
- Nothing to do is a normal answer: {\"merge\":[],\"noise\":[],\"delete\":[]}.";

/// Run one sweep now. Safe to call with an empty graph; returns an empty report
/// when there is nothing new to look at.
pub async fn run() -> Report {
    let m = memory::global();
    // Before anything else: notes deleted in Obsidian are deletions. This has
    // to come first — `heal_vault` below rewrites every mirror from the graph,
    // and would otherwise put back the file you just removed.
    let from_vault = reconcile();
    let nodes = batch(&m);
    let mut report = Report { considered: nodes.len(), from_vault, ..Default::default() };
    if nodes.len() < 2 {
        // Nothing can be a duplicate of nothing; still stamp it so a single new
        // node doesn't get re-read every week. The vault is still tidied — a
        // run with nothing to merge is exactly when leftovers get noticed.
        m.mark_swept(&nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>());
        heal_vault(&m);
        return report;
    }

    let model = model();
    let raw = match crate::openrouter::chat(&model, SYSTEM, &render(&nodes)).await {
        Ok(r) => r,
        Err(e) => {
            // Leave the nodes unswept so the next run tries again.
            tracing::warn!("memory sweep: {model} failed: {e}");
            return report;
        }
    };
    let trimmed = raw.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let Ok(plan) = serde_json::from_str::<Value>(trimmed) else {
        tracing::warn!("memory sweep: {model} returned no usable JSON");
        return report;
    };

    apply(&m, &nodes, &plan, &mut report);
    m.mark_swept(&nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>());
    report.remaining = m.unswept_count();
    heal_vault(&m);
    crate::config::set(LAST_RUN, &Utc::now().to_rfc3339());
    if report.changed() {
        tracing::info!(
            "memory sweep: merged {}, dropped {} command leftover(s) and {} empty, of {} considered, {} left",
            report.merged, report.noise, report.deleted, report.considered, report.remaining
        );
    }
    report
}

/// `off` / `0` / `false` stops a note deleted in the vault from deleting the
/// memory behind it.
fn deletes_honored() -> bool {
    !matches!(
        std::env::var("MEMORY_VAULT_DELETES").unwrap_or_default().trim().to_lowercase().as_str(),
        "off" | "0" | "false" | "no"
    )
}

/// Delete a memory note in Obsidian and the memory goes with it. The vault is
/// the copy you actually read, so a file missing from it is an instruction, not
/// drift: the node and its edges go, and whatever linked to it is rewritten.
///
/// Guards, because this cannot be undone:
/// * an **empty** `memory/` folder means it was wiped or hasn't synced yet, so
///   it is rebuilt from the graph rather than obeyed;
/// * categories and the entries mirroring a task or note file own no note of
///   their own, and are never candidates;
/// * a pass that would take out more than half of a graph of five or more is
///   read as a half-finished sync and skipped with a warning.
///
/// Renaming a note in Obsidian reads as deleting it. Ask for the rename instead
/// ("call that memory Sam Rivera") and the file follows.
pub fn reconcile() -> usize {
    reconcile_with(&memory::global())
}

fn reconcile_with(m: &memory::Memory) -> usize {
    if !deletes_honored() {
        return 0;
    }
    let on_disk = crate::vault::memory_mirror_names();
    if on_disk.is_empty() {
        return 0;
    }
    let owned = m.mirrored_nodes();
    let total = owned.len();
    let missing: Vec<Node> = owned.into_iter().filter(|n| !on_disk.contains(&crate::vault::filename(&n.name))).collect();
    if missing.is_empty() {
        return 0;
    }
    if total >= 5 && missing.len() * 2 > total {
        tracing::warn!(
            "memory: {} of {total} mirror notes are missing — that looks like a half-synced vault, not a deletion; leaving the graph alone",
            missing.len()
        );
        return 0;
    }
    let mut gone = 0;
    for node in &missing {
        // Captured first: once the node goes, so do the edges that say who was
        // pointing at it.
        let neighbors = m.neighbor_ids(&node.id);
        // Obsidian has its own trash, but the graph entry is ours to keep — the
        // relations only exist here.
        let _ = crate::vault::trash_memory(node, &m.neighbors(&node.id, 12), "deleted in the vault");
        if m.delete_node(&node.id) {
            m.remirror(&neighbors);
            tracing::info!("memory: '{}' was deleted in the vault — dropped from the graph", node.name);
            gone += 1;
        }
    }
    gone
}

/// Bring the vault's `memory/` folder back in line with the graph: rewrite
/// every mirror from the edges as they now stand, then delete the files with no
/// node left behind them. At this size it costs nothing, and it runs on every
/// sweep — including one with nothing to merge, which is exactly when leftovers
/// get noticed — so a dangling "Related" link heals whatever left it there.
fn heal_vault(m: &memory::Memory) {
    let rewritten = memory::mirror_all();
    let live: Vec<String> = m.all_nodes().into_iter().filter(|n| n.path.is_empty()).map(|n| n.name).collect();
    let pruned = crate::vault::prune_memory_mirrors(&live);
    if pruned > 0 {
        tracing::info!("memory sweep: rewrote {rewritten} mirror(s), pruned {pruned} orphan(s)");
    }
}

/// Walk the whole graph again rather than only what is new, a batch at a time.
/// For clearing a backlog — the entries an earlier, less careful extractor
/// filed. The batch cap still applies, so this is bounded like any other run;
/// `Report::remaining` says whether another pass is worth it.
pub async fn run_all() -> Report {
    let cleared = memory::global().reset_swept();
    tracing::info!("memory sweep: re-reading the whole graph ({cleared} entries un-stamped)");
    run().await
}

/// Carry out the model's plan, ignoring anything it made up.
fn apply(m: &memory::Memory, nodes: &[Node], plan: &Value, report: &mut Report) {
    let find = |id: &str| nodes.iter().find(|n| n.id == id);
    // A node mirroring a task or note is that file's entry in the graph; the
    // file outlives any tidy-up, so the entry does too.
    let removable = |n: &Node| n.path.is_empty() && n.kind != "category";
    // Everything that linked to a node we take away. Their mirrors name it in
    // their "Related" list, so they have to be rewritten once it is gone —
    // captured here because the edges don't survive the removal.
    let mut orphaned: Vec<String> = Vec::new();

    for merge in plan["merge"].as_array().unwrap_or(&vec![]) {
        let Some(keep) = merge["keep"].as_str().and_then(find) else { continue };
        if keep.kind == "category" {
            continue;
        }
        let absorb: Vec<Node> = merge["absorb"]
            .as_array()
            .map(|a| a.iter().filter_map(|id| id.as_str()).filter(|id| *id != keep.id).filter_map(find).filter(|n| removable(n)).cloned().collect())
            .unwrap_or_default();
        if absorb.is_empty() {
            continue;
        }
        let ids: Vec<String> = absorb.iter().map(|n| n.id.clone()).collect();
        for n in &absorb {
            orphaned.extend(m.neighbor_ids(&n.id));
            // Into the bin before it is folded away, in case the merge was wrong.
            let _ = crate::vault::trash_memory(n, &m.neighbors(&n.id, 12), &format!("merged into “{}”", keep.name));
        }
        let gone = m.merge_nodes(&keep.id, &ids);
        if gone == 0 {
            continue;
        }
        let name = merge["name"].as_str().unwrap_or("").trim();
        let kind = merge["kind"].as_str().unwrap_or("").trim();
        let summary = merge["summary"].as_str().unwrap_or("").trim();
        m.set_node(&keep.id, name, kind, summary);
        if let Some(after) = m.get(&keep.id) {
            if !after.name.eq_ignore_ascii_case(&keep.name) {
                let _ = crate::vault::remove_memory_mirror(&keep.name);
            }
            let _ = crate::vault::mirror_memory_node(&after, &m.neighbors(&after.id, 12));
            report.lines.push(format!("{} ← {}", after.name, absorb.iter().map(|n| n.name.clone()).collect::<Vec<_>>().join(", ")));
        }
        report.merged += gone;
    }

    // Command leftovers first, then the plain junk — same treatment, counted
    // apart so the report can say which kind of clutter it cleared.
    for (key, noise) in [("noise", true), ("delete", false)] {
        for id in plan[key].as_array().unwrap_or(&vec![]).iter().filter_map(|v| v.as_str()) {
            let Some(node) = find(id).filter(|n| removable(n)) else { continue };
            let neighbors = m.neighbor_ids(&node.id);
            let _ = crate::vault::trash_memory(node, &m.neighbors(&node.id, 12), if noise { "not a memory, a command" } else { "empty" });
            if !m.delete_node(&node.id) {
                continue;
            }
            orphaned.extend(neighbors);
            if noise {
                report.noise += 1;
                report.lines.push(format!("dropped “{}” (that's a command, not a memory)", node.name));
            } else {
                report.deleted += 1;
                report.lines.push(format!("dropped “{}”", node.name));
            }
        }
    }

    m.remirror(&orphaned);
}

/// Whether a scheduled run is due. A graph that has never been swept waits one
/// interval first, so a fresh install doesn't sweep three nodes on day one.
fn due() -> bool {
    let Some(last) = crate::config::get(LAST_RUN) else {
        crate::config::set(LAST_RUN, &Utc::now().to_rfc3339());
        return false;
    };
    chrono::DateTime::parse_from_rfc3339(&last)
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_days() >= every_days())
        .unwrap_or(true)
}

/// Background loop: wake hourly, sweep when a week has passed, and tell the
/// owner what changed — memory shouldn't rewrite itself behind their back.
pub async fn run_worker() {
    match enabled() {
        true => tracing::info!("memory sweep worker started (every {} days)", every_days()),
        // Still worth a loop: picking up vault deletions is plain sync, not
        // consolidation, and shouldn't need the model to be switched on.
        false => tracing::info!("memory sweep disabled (MEMORY_SWEEP); still following vault deletions"),
    }
    let client = reqwest::Client::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        // A note deleted in Obsidian takes effect within the hour rather than
        // waiting for the weekly run.
        if reconcile() > 0 {
            heal_vault(&memory::global());
        }
        match crate::vault::purge_trash() {
            0 => {}
            n => tracing::info!("bin: {n} memor{} past {} days, gone for good", if n == 1 { "y" } else { "ies" }, crate::vault::trash_days()),
        }
        if !enabled() || !due() {
            continue;
        }
        let report = run().await;
        if !report.changed() {
            continue;
        }
        let (Some(chat_id), Ok(token)) = (crate::config::owner_chat(), std::env::var("TELEGRAM_BOT_TOKEN")) else {
            continue;
        };
        let body = serde_json::json!({ "chat_id": chat_id, "text": report.message() });
        if let Err(e) = client.post(format!("https://api.telegram.org/bot{token}/sendMessage")).json(&body).send().await {
            tracing::warn!("memory sweep: couldn't notify the owner: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use serde_json::json;

    /// `apply` mirrors into the vault, so point it somewhere disposable.
    use crate::vault::test_vault as temp_vault;

    /// `memory::mirror_all` works on the global graph; tests use a scoped one.
    fn mirror(m: &memory::Memory) {
        for node in m.all_nodes() {
            let _ = crate::vault::mirror_memory_node(&node, &m.neighbors(&node.id, 12));
        }
    }

    #[test]
    fn merges_duplicates_and_ignores_what_it_was_not_shown() {
        let (_g, root) = temp_vault();
        let m = memory::scoped(Db::memory().unwrap());
        let long = m.upsert_node("person", "Sam Rivera", "Sam is the user's climbing partner", "");
        let short = m.upsert_node("person", "Sam R", "Sam lives in Lisbon", "");
        let tri = m.upsert_node("project", "Triathlon", "Half-distance race in August", "");
        let note = m.upsert_node("note", "VPN setup", "", "notes/VPN setup");
        let junk = m.upsert_node("fact", "hmm", "", "");
        let shampoo = m.upsert_node("fact", "Buy shampoo", "The user wants shampoo", "");
        m.add_edge(&short.id, &tri.id, "trains for", "chat:1#1");

        let nodes = vec![long.clone(), short.clone(), tri.clone(), note.clone(), junk.clone(), shampoo.clone()];
        let plan = json!({
            "merge": [
                { "keep": long.id, "absorb": [short.id, "person:nobody"], "name": "Sam Rivera", "kind": "person",
                  "summary": "Sam Rivera is the user's climbing partner and lives in Lisbon" },
                // A node that mirrors a vault file can never be absorbed.
                { "keep": tri.id, "absorb": [note.id], "name": "", "kind": "", "summary": "" }
            ],
            "noise": [shampoo.id, note.id],
            "delete": [junk.id]
        });
        let mut report = Report { considered: nodes.len(), ..Default::default() };
        apply(&m, &nodes, &plan, &mut report);

        assert_eq!(report.merged, 1, "only the real duplicate merged");
        assert_eq!(report.noise, 1, "the command leftover went; the file-backed node did not");
        assert_eq!(report.deleted, 1);
        assert!(m.by_name("Buy shampoo").is_none());
        assert!(m.by_name("Sam R").is_none());
        let sam = m.by_name("Sam Rivera").unwrap();
        assert_eq!(sam.id, long.id, "the survivor keeps its id");
        assert!(sam.summary.contains("Lisbon") && sam.summary.contains("climbing"));
        assert_eq!(sam.mentions, 2, "mentions add up");
        // The absorbed node's edge now hangs off the survivor.
        let nb = m.neighbors(&sam.id, 5);
        assert_eq!(nb.len(), 1);
        assert_eq!(nb[0].1.name, "Triathlon");
        assert!(m.get(&note.id).is_some());
        assert!(m.get(&junk.id).is_none());
        assert!(!root.join("memory").join("Sam R.md").exists(), "the absorbed mirror is gone");
        assert!(root.join("memory").join("Sam Rivera.md").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    /// A dropped entry must leave nothing behind in the vault: not its own
    /// file, and not a wikilink to it in whatever it was linked to. Obsidian
    /// draws a link to a missing file as a node, so a leftover link looks
    /// exactly like the memory never went away.
    #[test]
    fn dropping_an_entry_leaves_no_ghost_in_the_graph() {
        let (_g, root) = temp_vault();
        let m = memory::scoped(Db::memory().unwrap());
        let personal = m.upsert_node("topic", "Personal", "hobbies and the rest", "");
        let shampoo = m.upsert_node("fact", "Buy shampoo", "The user wants shampoo", "");
        m.add_edge(&shampoo.id, &personal.id, "belongs to", "chat:1#1");
        let _ = crate::vault::mirror_memory_node(&personal, &m.neighbors(&personal.id, 12));
        let _ = crate::vault::mirror_memory_node(&shampoo, &m.neighbors(&shampoo.id, 12));
        let personal_md = root.join("memory").join("Personal.md");
        assert!(std::fs::read_to_string(&personal_md).unwrap().contains("Buy shampoo"));

        let nodes = vec![personal.clone(), shampoo.clone()];
        let plan = json!({ "merge": [], "noise": [shampoo.id], "delete": [] });
        let mut report = Report::default();
        apply(&m, &nodes, &plan, &mut report);

        assert_eq!(report.noise, 1);
        assert!(!root.join("memory").join("Buy shampoo.md").exists(), "its own mirror is gone");
        let after = std::fs::read_to_string(&personal_md).unwrap();
        assert!(!after.contains("Buy shampoo"), "no dead wikilink left behind:\n{after}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Orphan files from before the fix — or from any path that missed one —
    /// get cleared out, while hand-written notes in `memory/` are left alone.
    #[test]
    fn orphan_mirrors_are_pruned_but_hand_written_notes_survive() {
        let (_g, root) = temp_vault();
        let m = memory::scoped(Db::memory().unwrap());
        let sam = m.upsert_node("person", "Sam", "climbs", "");
        let _ = crate::vault::mirror_memory_node(&sam, &[]);
        let dir = root.join("memory");
        std::fs::write(dir.join("Buy shampoo.md"), "---\ntitle: Buy shampoo\ntype: memory\n---\n\nleftover\n").unwrap();
        std::fs::write(dir.join("My own notes.md"), "---\ntitle: My own notes\n---\n\nmine\n").unwrap();

        let live: Vec<String> = m.all_nodes().into_iter().map(|n| n.name).collect();
        assert_eq!(crate::vault::prune_memory_mirrors(&live), 1);
        assert!(!dir.join("Buy shampoo.md").exists());
        assert!(dir.join("Sam.md").exists());
        assert!(dir.join("My own notes.md").exists(), "not ours, not touched");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_note_deleted_in_the_vault_deletes_the_memory() {
        let (_g, root) = temp_vault();
        let m = memory::scoped(Db::memory().unwrap());
        let sam = m.upsert_node("person", "Sam", "climbs", "");
        let tri = m.upsert_node("project", "Triathlon", "race in August", "");
        let _note = m.upsert_node("note", "VPN setup", "", "notes/VPN setup");
        m.add_edge(&sam.id, &tri.id, "trains for", "chat:1#1");
        mirror(&m);
        let dir = root.join("memory");
        assert!(dir.join("Sam.md").exists() && dir.join("Triathlon.md").exists());
        assert!(!dir.join("VPN setup.md").exists(), "file-backed entries get no note of their own");

        // Delete one in "Obsidian".
        std::fs::remove_file(dir.join("Sam.md")).unwrap();
        assert_eq!(reconcile_with(&m), 1);
        assert!(m.by_name("Sam").is_none(), "gone from the database, not just the folder");
        assert!(m.by_name("Triathlon").is_some());
        assert!(m.by_name("VPN setup").is_some(), "a note's entry is not collateral");
        // Its neighbour no longer links to a file that isn't there.
        let tri_md = std::fs::read_to_string(dir.join("Triathlon.md")).unwrap();
        assert!(!tri_md.contains("Sam"), "{tri_md}");
        // And a later re-mirror must not resurrect it.
        mirror(&m);
        assert!(!dir.join("Sam.md").exists());
        // It is recoverable, not gone.
        let binned = crate::vault::trashed("Sam").expect("Sam is in the bin");
        assert_eq!(binned.str("reason"), "deleted in the vault");
        assert_eq!(crate::vault::trashed_relations(&binned), vec![("trains for".to_string(), "Triathlon".to_string())]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_half_synced_vault_is_not_a_deletion() {
        let (_g, root) = temp_vault();
        let m = memory::scoped(Db::memory().unwrap());
        for name in ["Sam", "Triathlon", "Lisbon", "Aqusense", "Bike fit", "Dentist"] {
            m.upsert_node("topic", name, "something", "");
        }
        mirror(&m);
        let dir = root.join("memory");
        // An empty folder is a wipe, not six deletions.
        for f in std::fs::read_dir(&dir).unwrap().flatten() {
            std::fs::remove_file(f.path()).unwrap();
        }
        assert_eq!(reconcile_with(&m), 0, "an empty folder is rebuilt, never obeyed");
        assert_eq!(m.mirrored_nodes().len(), 6);

        // Most-but-not-all missing reads as a sync in progress.
        mirror(&m);
        for name in ["Sam", "Triathlon", "Lisbon", "Aqusense"] {
            std::fs::remove_file(dir.join(format!("{name}.md"))).unwrap();
        }
        assert_eq!(reconcile_with(&m), 0, "4 of 6 missing is a half-synced vault");
        assert_eq!(m.mirrored_nodes().len(), 6);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_bin_holds_a_dropped_memory_and_empties_after_a_month() {
        let (_g, root) = temp_vault();
        let m = memory::scoped(Db::memory().unwrap());
        let sam = m.upsert_node("person", "Sam", "Sam is the user's climbing partner", "");
        let tri = m.upsert_node("project", "Triathlon", "race", "");
        m.add_edge(&sam.id, &tri.id, "trains for", "chat:1#1");
        crate::vault::trash_memory(&sam, &m.neighbors(&sam.id, 12), "you asked me to forget it").unwrap();
        m.delete_node(&sam.id);

        let binned = crate::vault::trash();
        assert_eq!(binned.len(), 1);
        assert_eq!(binned[0].str("reason"), "you asked me to forget it");
        assert!(binned[0].body.contains("climbing partner"), "the summary survives");
        // A note in the bin must not draw edges in the graph view.
        assert!(!binned[0].body.contains("[["), "no wikilinks in the bin:\n{}", binned[0].body);

        // Still inside the window, so nothing is purged.
        assert_eq!(crate::vault::purge_trash(), 0);
        // Backdate it past the window and it goes for good.
        let mut old = binned[0].clone();
        old.set("deleted", serde_json::json!((Utc::now() - chrono::Duration::days(crate::vault::trash_days() + 1)).to_rfc3339()));
        crate::vault::save(&old).unwrap();
        assert_eq!(crate::vault::purge_trash(), 1);
        assert!(crate::vault::trash().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_batch_is_read_once() {
        let (_g, _root) = temp_vault();
        let m = memory::scoped(Db::memory().unwrap());
        m.upsert_node("person", "Sam", "climbs", "");
        m.upsert_node("project", "Triathlon", "race", "");
        let first = batch(&m);
        assert_eq!(first.len(), 2);
        m.mark_swept(&first.iter().map(|n| n.id.clone()).collect::<Vec<_>>());
        assert!(m.unswept(10).is_empty(), "swept nodes stay out of the next prompt");
        // …but something new pulls its look-alikes back in, swept or not.
        m.upsert_node("person", "Sam Rivera", "climbs with the user", "");
        let names: Vec<String> = batch(&m).into_iter().map(|n| n.name).collect();
        assert!(names.contains(&"Sam Rivera".to_string()) && names.contains(&"Sam".to_string()), "{names:?}");
    }
}
