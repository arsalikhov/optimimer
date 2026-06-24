use crate::db::Db;
use crate::models::Workflow;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;

/// SQLite-backed workflow store. Each row is one workflow serialized to JSON,
/// keyed by id, with `updated_at` mirrored into a column so `list()` can sort
/// without parsing every row. Same public API as the old JSON-file store.
#[derive(Clone)]
pub struct Store {
    db: Db,
}

impl Store {
    pub fn new(db: Db) -> Self {
        Store { db }
    }

    pub fn list(&self) -> Vec<Workflow> {
        let conn = self.db.lock();
        let mut stmt = match conn.prepare("SELECT json FROM workflows ORDER BY updated_at DESC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.flatten()
            .filter_map(|j| serde_json::from_str::<Workflow>(&j).ok())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Workflow> {
        let conn = self.db.lock();
        conn.query_row("SELECT json FROM workflows WHERE id = ?1", params![id], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
    }

    pub fn upsert(&self, wf: Workflow) -> Workflow {
        if let Ok(json) = serde_json::to_string(&wf) {
            let conn = self.db.lock();
            let _ = conn.execute(
                "INSERT INTO workflows (id, json, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
                params![wf.id, json, wf.updated_at],
            );
        }
        wf
    }

    pub fn delete(&self, id: &str) -> bool {
        let conn = self.db.lock();
        conn.execute("DELETE FROM workflows WHERE id = ?1", params![id])
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// One-time import of a legacy `workflows.json` (a `{id: Workflow}` map) when
    /// the table is still empty. Leaves the file untouched as a backup.
    pub fn migrate_json(&self, path: &Path) {
        if !self.db.is_empty("workflows") || !path.exists() {
            return;
        }
        let map: HashMap<String, Workflow> = match std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(m) => m,
            None => return,
        };
        let n = map.len();
        for wf in map.into_values() {
            self.upsert(wf);
        }
        tracing::info!("migrated {n} workflow(s) from {} into sqlite", path.display());
    }
}
