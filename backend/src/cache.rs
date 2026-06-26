//! Content-addressed cache for expensive, deterministic LLM calls — CSV
//! statement parsing and receipt OCR. Keyed by a hash of the inputs (model +
//! payload), so re-sending the same statement or photo never pays the model
//! twice. Backed by the shared SQLite db (`llm_cache` table) via a process-wide
//! handle set once at startup; if `init` was never called, every op is a no-op
//! so callers transparently fall back to calling the model.

use crate::db::Db;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

static GLOBAL: OnceLock<Db> = OnceLock::new();

/// Point the cache at the shared database. Call once at startup.
pub fn init(db: Db) {
    let _ = GLOBAL.set(db);
}

/// Build a cache key: a namespace plus the inputs, hashed to a short hex string.
/// The namespace is kept as a readable prefix so rows are easy to eyeball.
pub fn key(namespace: &str, parts: &[&str]) -> String {
    let mut h = DefaultHasher::new();
    namespace.hash(&mut h);
    for p in parts {
        p.hash(&mut h);
    }
    format!("{namespace}:{:016x}", h.finish())
}

/// Look up a cached value, or `None` on a miss / when the cache is uninitialised.
pub fn get(key: &str) -> Option<String> {
    let db = GLOBAL.get()?;
    let conn = db.lock();
    conn.query_row("SELECT value FROM llm_cache WHERE key = ?1", [key], |r| r.get::<_, String>(0))
        .ok()
}

/// Store a value (overwriting any prior value for this key). No-op if uninitialised.
pub fn put(key: &str, value: &str) {
    let Some(db) = GLOBAL.get() else { return };
    let now = chrono::Utc::now().to_rfc3339();
    let conn = db.lock();
    let _ = conn.execute(
        "INSERT INTO llm_cache (key, value, created) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, created = excluded.created",
        rusqlite::params![key, value, now],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_namespaced() {
        let a = key("csv", &["model", "credit", "row1,row2"]);
        let b = key("csv", &["model", "credit", "row1,row2"]);
        let c = key("csv", &["model", "credit", "row1,row3"]);
        assert_eq!(a, b, "same inputs must hash equal");
        assert_ne!(a, c, "different inputs must differ");
        assert!(a.starts_with("csv:"), "namespace prefix preserved");
    }

    #[test]
    fn put_then_get_round_trips() {
        init(Db::memory().unwrap());
        let k = key("test", &["x"]);
        assert_eq!(get(&k), None);
        put(&k, "hello");
        assert_eq!(get(&k), Some("hello".to_string()));
        put(&k, "updated");
        assert_eq!(get(&k), Some("updated".to_string()));
    }
}
