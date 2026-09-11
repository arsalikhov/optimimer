use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// Shared SQLite handle — one connection behind a mutex. For this single-node,
/// single-user bot that's plenty: writes are tiny and rare, and WAL keeps the
/// brief web/worker/bot locks cheap. This replaces the former scattering of
/// write-through JSON files (workflows.json, chat_tz.json, schedules.json,
/// lists.json) with one `optimimer.db`, so writes are atomic and O(1) instead of
/// rewriting a whole file on every change.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open (creating if needed) the database at `path` and ensure the schema.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        init_schema(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory database — used only as a safety fallback for the scheduler when
    /// `scheduler::init` was never called (e.g. in tests).
    pub fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Lock the connection. Callers must not hold the guard across an `.await`.
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        // Recover from a poisoned lock rather than cascading a panic across the
        // whole bot — a half-finished write is harmless given our simple schema.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True if `table` has no rows. Used to gate one-time JSON imports.
    /// `table` is always an internal constant, never user input.
    /// On a query error we return false (assume non-empty) so a transient failure
    /// can't re-trigger a migration that overwrites live rows with stale JSON.
    pub fn is_empty(&self, table: &str) -> bool {
        let conn = self.lock();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n == 0)
        .unwrap_or(false)
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         CREATE TABLE IF NOT EXISTS workflows (
             id         TEXT PRIMARY KEY,
             json       TEXT NOT NULL,
             updated_at TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS chat_tz (
             chat_id INTEGER PRIMARY KEY,
             tz      TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS settings (
             key     TEXT PRIMARY KEY,
             value   TEXT NOT NULL,
             updated TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS chats (
             chat_id INTEGER PRIMARY KEY,
             role    TEXT NOT NULL,
             name    TEXT NOT NULL DEFAULT '',
             joined  TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS chat_lists (
             chat_id INTEGER PRIMARY KEY,
             json    TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS schedules (
             id   TEXT PRIMARY KEY,
             json TEXT NOT NULL,
             done INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS stock_watches (
             id   TEXT PRIMARY KEY,
             json TEXT NOT NULL,
             done INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS transactions (
             id        TEXT PRIMARY KEY,
             date      TEXT NOT NULL,
             name      TEXT NOT NULL,
             amount    REAL NOT NULL,
             direction TEXT NOT NULL,
             category  TEXT NOT NULL DEFAULT '',
             source    TEXT NOT NULL DEFAULT '',
             key       TEXT NOT NULL DEFAULT '',
             note      TEXT NOT NULL DEFAULT '',
             created   TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS transactions_date ON transactions(date);
         CREATE INDEX IF NOT EXISTS transactions_key ON transactions(key);
         CREATE TABLE IF NOT EXISTS chat_messages (
             id      INTEGER PRIMARY KEY AUTOINCREMENT,
             chat_id INTEGER NOT NULL,
             role    TEXT NOT NULL,
             text    TEXT NOT NULL,
             created TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS chat_messages_chat ON chat_messages(chat_id, id);
         CREATE TABLE IF NOT EXISTS chat_summaries (
             chat_id INTEGER PRIMARY KEY,
             summary TEXT NOT NULL,
             upto_id INTEGER NOT NULL,
             updated TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS mem_nodes (
             id       TEXT PRIMARY KEY,
             kind     TEXT NOT NULL,
             name     TEXT NOT NULL,
             norm     TEXT NOT NULL,
             summary  TEXT NOT NULL DEFAULT '',
             path     TEXT NOT NULL DEFAULT '',
             mentions INTEGER NOT NULL DEFAULT 1,
             created  TEXT NOT NULL,
             updated  TEXT NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS mem_nodes_norm ON mem_nodes(kind, norm);
         CREATE VIRTUAL TABLE IF NOT EXISTS mem_fts USING fts5(name, summary, content='mem_nodes', content_rowid='rowid');
         CREATE TRIGGER IF NOT EXISTS mem_nodes_ai AFTER INSERT ON mem_nodes BEGIN
             INSERT INTO mem_fts(rowid, name, summary) VALUES (new.rowid, new.name, new.summary);
         END;
         CREATE TRIGGER IF NOT EXISTS mem_nodes_ad AFTER DELETE ON mem_nodes BEGIN
             INSERT INTO mem_fts(mem_fts, rowid, name, summary) VALUES ('delete', old.rowid, old.name, old.summary);
         END;
         CREATE TRIGGER IF NOT EXISTS mem_nodes_au AFTER UPDATE ON mem_nodes BEGIN
             INSERT INTO mem_fts(mem_fts, rowid, name, summary) VALUES ('delete', old.rowid, old.name, old.summary);
             INSERT INTO mem_fts(rowid, name, summary) VALUES (new.rowid, new.name, new.summary);
         END;
         CREATE TABLE IF NOT EXISTS mem_edges (
             id       INTEGER PRIMARY KEY AUTOINCREMENT,
             src      TEXT NOT NULL,
             dst      TEXT NOT NULL,
             rel      TEXT NOT NULL,
             weight   REAL NOT NULL DEFAULT 1,
             evidence TEXT NOT NULL DEFAULT '',
             created  TEXT NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS mem_edges_uniq ON mem_edges(src, dst, rel);
         CREATE TABLE IF NOT EXISTS convo_notes (
             id      TEXT PRIMARY KEY,
             chat_id INTEGER NOT NULL,
             json    TEXT NOT NULL,
             created TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS chat_finance (
             chat_id        INTEGER PRIMARY KEY,
             monthly_income REAL NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS llm_cache (
             key     TEXT PRIMARY KEY,
             value   TEXT NOT NULL,
             created TEXT NOT NULL DEFAULT ''
         );",
    )?;
    Ok(())
}
