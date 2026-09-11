use crate::db::Db;
use chrono::Utc;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use uuid::Uuid;

/// One scheduled action. When `fire_at` (RFC3339, any offset) is reached the
/// worker sends `message` to `chat_id` via Telegram.
///
/// This powers `/remind` and `/notify` (Telegram ping at a time). Rows saved by
/// older versions may carry extra fields; serde ignores them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub fire_at: String,
    pub chat_id: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub done: bool,
}

/// SQLite-backed scheduler store (table `schedules`), mirroring `store::Store`.
#[derive(Clone)]
pub struct Scheduler {
    db: Db,
}

static GLOBAL: OnceLock<Scheduler> = OnceLock::new();

/// Initialise the process-wide scheduler. Safe to call once at startup.
pub fn init(db: Db) -> Scheduler {
    let sched = Scheduler { db };
    let _ = GLOBAL.set(sched.clone());
    sched
}

/// The process-wide scheduler. Falls back to an in-memory instance if `init`
/// was never called (keeps the `schedule` node from panicking in tests).
pub fn global() -> Scheduler {
    GLOBAL
        .get_or_init(|| Scheduler {
            db: Db::memory().expect("in-memory scheduler db"),
        })
        .clone()
}

impl Scheduler {
    /// Add an entry, assigning an id if absent. Returns the id.
    pub fn add(&self, mut entry: Schedule) -> String {
        if entry.id.is_empty() {
            entry.id = Uuid::new_v4().to_string();
        }
        let id = entry.id.clone();
        if let Ok(json) = serde_json::to_string(&entry) {
            let conn = self.db.lock();
            let _ = conn.execute(
                "INSERT INTO schedules (id, json, done) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json, done = excluded.done",
                params![id, json, entry.done as i64],
            );
        }
        id
    }

    /// Pull every entry that is due and not yet done, marking them done first so
    /// the firing below never double-sends. Returns the claimed entries.
    fn claim_due(&self) -> Vec<Schedule> {
        let now = Utc::now();
        let conn = self.db.lock();
        let pending: Vec<Schedule> = {
            let mut stmt = match conn.prepare("SELECT json FROM schedules WHERE done = 0") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            rows.flatten()
                .filter_map(|j| serde_json::from_str::<Schedule>(&j).ok())
                .collect()
        };
        let mut due = Vec::new();
        for mut entry in pending {
            let is_due = chrono::DateTime::parse_from_rfc3339(&entry.fire_at)
                .map(|t| t.with_timezone(&Utc) <= now)
                // Unparseable times fire once immediately rather than getting stuck.
                .unwrap_or(true);
            if is_due {
                entry.done = true;
                if let Ok(json) = serde_json::to_string(&entry) {
                    let _ = conn.execute(
                        "UPDATE schedules SET json = ?2, done = 1 WHERE id = ?1",
                        params![entry.id, json],
                    );
                }
                due.push(entry);
            }
        }
        due
    }

    /// One-time import of a legacy `schedules.json` when the table is empty.
    pub fn migrate_json(&self, path: &Path) {
        if !self.db.is_empty("schedules") || !path.exists() {
            return;
        }
        let map: HashMap<String, Schedule> = match std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(m) => m,
            None => return,
        };
        let n = map.len();
        for entry in map.into_values() {
            self.add(entry);
        }
        tracing::info!("migrated {n} schedule(s) from {} into sqlite", path.display());
    }
}

/// Background loop: every 30s, fire all due entries. Spawned from `main`.
pub async fn run_worker(sched: Scheduler) {
    let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let client = reqwest::Client::new();
    tracing::info!("scheduler worker started");

    loop {
        for entry in sched.claim_due() {
            fire(&client, &token, &entry).await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

async fn fire(client: &reqwest::Client, token: &str, entry: &Schedule) {
    if !entry.message.trim().is_empty() && !token.is_empty() && entry.chat_id != 0 {
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let body = json!({ "chat_id": entry.chat_id, "text": entry.message, "parse_mode": "Markdown" });
        if let Err(e) = client.post(&url).json(&body).send().await {
            tracing::warn!("scheduler sendMessage failed: {e}");
        }
    }
}
