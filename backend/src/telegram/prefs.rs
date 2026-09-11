//! Per-chat preferences: timezone (location pins, `set_timezone`) and the one-time JSON → SQLite migration.

use super::*;

/// Map a shared location to an IANA timezone (offline, no API key) and store it.
pub(super) fn handle_location(state: &BotState, chat_id: i64, loc: &Value) -> Reply {
    let lat = loc["latitude"].as_f64();
    let lng = loc["longitude"].as_f64();
    match (lat, lng) {
        (Some(lat), Some(lng)) => {
            let tz = finder().get_tz_name(lng, lat).to_string();
            if tz.is_empty() {
                return Reply::text("Couldn't resolve a timezone from that location.");
            }
            set_tz(state, chat_id, &tz);
            Reply::text(format!(
                "Timezone set to *{tz}* — reminders will use this. Local time is now {}.",
                now_in_tz(&tz)
            ))
        }
        _ => Reply::text("That location didn't include coordinates."),
    }
}

/// tzf-rs finder is moderately expensive to build (embedded boundary data); reuse one.
pub(super) fn finder() -> &'static tzf_rs::DefaultFinder {
    static F: OnceLock<tzf_rs::DefaultFinder> = OnceLock::new();
    F.get_or_init(tzf_rs::DefaultFinder::new)
}

pub(super) fn default_tz() -> String {
    crate::config::get(crate::config::TIMEZONE).unwrap_or_else(|| "UTC".to_string())
}

pub(super) fn tz_for(state: &BotState, chat_id: i64) -> String {
    let conn = state.db.lock();
    conn.query_row("SELECT tz FROM chat_tz WHERE chat_id = ?1", params![chat_id], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .unwrap_or_else(default_tz)
}

pub(super) fn set_tz(state: &BotState, chat_id: i64, tz: &str) {
    let conn = state.db.lock();
    let _ = conn.execute(
        "INSERT INTO chat_tz (chat_id, tz) VALUES (?1, ?2)
         ON CONFLICT(chat_id) DO UPDATE SET tz = excluded.tz",
        params![chat_id, tz],
    );
}

/// One-time import of legacy `chat_tz.json` / `lists.json` when those tables are
/// still empty. Files are left in place as a backup.
pub fn migrate_json(db: &Db, tz_path: &Path, lists_path: &Path) {
    if db.is_empty("chat_tz") && tz_path.exists() {
        if let Some(map) = std::fs::read_to_string(tz_path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<i64, String>>(&s).ok())
        {
            let conn = db.lock();
            for (chat_id, tz) in &map {
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO chat_tz (chat_id, tz) VALUES (?1, ?2)",
                    params![chat_id, tz],
                );
            }
            tracing::info!("migrated {} timezone(s) into sqlite", map.len());
        }
    }
    if db.is_empty("chat_lists") && lists_path.exists() {
        if let Some(map) = std::fs::read_to_string(lists_path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<i64, ChatLists>>(&s).ok())
        {
            let conn = db.lock();
            for (chat_id, lists) in &map {
                if let Ok(json) = serde_json::to_string(lists) {
                    let _ = conn.execute(
                        "INSERT OR REPLACE INTO chat_lists (chat_id, json) VALUES (?1, ?2)",
                        params![chat_id, json],
                    );
                }
            }
            tracing::info!("migrated {} shopping list(s) into sqlite", map.len());
        }
    }
}

/// Human + machine friendly "now" string for the LLM to resolve relative dates.
/// e.g. "2026-06-22 15:30 -04:00 (Sunday)". Falls back to UTC on a bad tz.
pub(super) fn now_in_tz(tz_name: &str) -> String {
    match tz_name.parse::<chrono_tz::Tz>() {
        Ok(tz) => Utc::now()
            .with_timezone(&tz)
            .format("%Y-%m-%d %H:%M %:z (%A)")
            .to_string(),
        Err(_) => Utc::now().format("%Y-%m-%d %H:%M +00:00 (%A)").to_string(),
    }
}
