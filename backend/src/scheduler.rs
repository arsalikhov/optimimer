use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

/// One scheduled action. When `fire_at` (RFC3339, any offset) is reached the
/// worker sends `message` to `chat_id` via Telegram (if non-empty) and/or patches
/// the Notion page `page_id` with `properties_json` (if both non-empty).
///
/// This powers `/remind` and `/notify` (Telegram ping at a time) and the
/// automatic "clear a meeting an hour after it starts" rule (a Notion update).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub fire_at: String,
    pub chat_id: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub page_id: String,
    #[serde(default)]
    pub properties_json: String,
    #[serde(default)]
    pub done: bool,
}

/// JSON-file backed scheduler store, mirroring `store::Store`.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Mutex<HashMap<String, Schedule>>>,
    path: PathBuf,
}

static GLOBAL: OnceLock<Scheduler> = OnceLock::new();

/// Initialise the process-wide scheduler. Safe to call once at startup.
pub fn init(path: PathBuf) -> Scheduler {
    let sched = Scheduler::load(path);
    let _ = GLOBAL.set(sched.clone());
    sched
}

/// The process-wide scheduler. Falls back to an in-memory instance if `init`
/// was never called (keeps the `schedule` node from panicking in tests).
pub fn global() -> Scheduler {
    GLOBAL
        .get()
        .cloned()
        .unwrap_or_else(|| Scheduler::load("schedules.json".into()))
}

impl Scheduler {
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, Schedule>>(&s).ok())
            .unwrap_or_default();
        Scheduler {
            inner: Arc::new(Mutex::new(map)),
            path,
        }
    }

    fn persist(&self, map: &HashMap<String, Schedule>) {
        if let Ok(json) = serde_json::to_string_pretty(map) {
            let _ = std::fs::write(&self.path, json);
        }
    }

    /// Add an entry, assigning an id if absent. Returns the id.
    pub fn add(&self, mut entry: Schedule) -> String {
        if entry.id.is_empty() {
            entry.id = Uuid::new_v4().to_string();
        }
        let id = entry.id.clone();
        let mut map = self.inner.lock().unwrap();
        map.insert(id.clone(), entry);
        self.persist(&map);
        id
    }

    /// Pull every entry that is due and not yet done, marking them done first so
    /// the firing below never double-sends. Returns the claimed entries.
    fn claim_due(&self) -> Vec<Schedule> {
        let now = Utc::now();
        let mut map = self.inner.lock().unwrap();
        let mut due = Vec::new();
        for entry in map.values_mut() {
            if entry.done {
                continue;
            }
            let is_due = chrono::DateTime::parse_from_rfc3339(&entry.fire_at)
                .map(|t| t.with_timezone(&Utc) <= now)
                // Unparseable times fire once immediately rather than getting stuck.
                .unwrap_or(true);
            if is_due {
                entry.done = true;
                due.push(entry.clone());
            }
        }
        if !due.is_empty() {
            self.persist(&map);
        }
        due
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

    if !entry.page_id.trim().is_empty() && !entry.properties_json.trim().is_empty() {
        let op = crate::notion::Op {
            op: "update_page".into(),
            page_id: entry.page_id.clone(),
            properties_json: entry.properties_json.clone(),
            ..Default::default()
        };
        if let Err(e) = crate::notion::run(op).await {
            tracing::warn!("scheduler Notion update failed: {e}");
        }
    }
}
