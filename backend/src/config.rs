//! Instance settings — everything that is personal to the owner (name,
//! timezone, categories, machines, transcription vocabulary, who may talk to
//! the bot) lives in SQLite, not in the source. The binary ships with generic
//! defaults; the installer and the in-chat onboarding fill in the rest.
//!
//! Lookup order for a setting: an environment variable (so Infisical / .env
//! still work) → the `settings` table → the built-in default.
//!
//! Access control lives here too: `chats` holds every paired chat with its role.
//! A chat pairs by sending the one-time setup code (owner) or an invite code
//! (member); `TELEGRAM_ALLOWED_CHAT_IDS` is still honoured as a static extra.

use crate::db::Db;
use rusqlite::{params, OptionalExtension};
use std::sync::OnceLock;

static DB: OnceLock<Db> = OnceLock::new();

pub fn init(db: Db) {
    let _ = DB.set(db);
}

fn db() -> Option<&'static Db> {
    DB.get()
}

// ---- keys ------------------------------------------------------------------

/// How the owner wants to be addressed.
pub const OWNER_NAME: &str = "owner_name";
/// Default IANA timezone (env `DEFAULT_TZ`); per-chat overrides live in `chat_tz`.
pub const TIMEZONE: &str = "timezone";
/// Task/note categories as "Name:hint,…" (env `VAULT_CATEGORIES`).
pub const CATEGORIES: &str = "categories";
/// Wake-on-LAN targets as "name=mac@iface,…" (env `MACHINES`).
pub const MACHINES: &str = "machines";
/// Comma-separated words the transcriber should spell correctly (env `TRANSCRIBE_VOCAB`).
pub const VOCAB: &str = "transcribe_vocab";
/// One-time code that pairs the owner's chat (env `OPTIMIMER_SETUP_CODE`).
pub const SETUP_CODE: &str = "setup_code";
/// One-time code that pairs an additional chat as a member.
pub const INVITE_CODE: &str = "invite_code";

/// Settings that have a legacy environment-variable spelling.
fn env_name(key: &str) -> String {
    match key {
        TIMEZONE => "DEFAULT_TZ".into(),
        CATEGORIES => "VAULT_CATEGORIES".into(),
        VOCAB => "TRANSCRIBE_VOCAB".into(),
        SETUP_CODE => "OPTIMIMER_SETUP_CODE".into(),
        other => other.to_uppercase(),
    }
}

/// A setting's value: env var first, then the table. Empty counts as unset.
pub fn get(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_name(key)) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    stored(key)
}

/// The stored value only (ignores the environment).
pub fn stored(key: &str) -> Option<String> {
    let db = db()?;
    let conn = db.lock();
    conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty())
}

pub fn set(key: &str, value: &str) {
    if let Some(db) = db() {
        let conn = db.lock();
        let _ = conn.execute(
            "INSERT INTO settings (key, value, updated) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated = excluded.updated",
            params![key, value.trim(), chrono::Utc::now().to_rfc3339()],
        );
    }
}

pub fn unset(key: &str) {
    if let Some(db) = db() {
        let conn = db.lock();
        let _ = conn.execute("DELETE FROM settings WHERE key = ?1", params![key]);
    }
}

/// The owner's name, or a neutral fallback.
pub fn owner_name() -> String {
    get(OWNER_NAME).unwrap_or_default()
}

// ---- machines (Wake-on-LAN) --------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub name: String,
    pub mac: String,
    pub iface: String,
}

/// Parse "name=aa:bb:cc:dd:ee:ff@eth0, other=…". The interface defaults to eth0.
pub fn parse_machines(s: &str) -> Vec<Machine> {
    s.split(',')
        .filter_map(|item| {
            let (name, rest) = item.split_once('=')?;
            let (mac, iface) = rest.split_once('@').unwrap_or((rest, "eth0"));
            let mac = normalize_mac(mac)?;
            let name = name.trim();
            (!name.is_empty()).then(|| Machine {
                name: name.to_string(),
                mac,
                iface: if iface.trim().is_empty() { "eth0".into() } else { iface.trim().into() },
            })
        })
        .collect()
}

fn render_machines(list: &[Machine]) -> String {
    list.iter().map(|m| format!("{}={}@{}", m.name, m.mac, m.iface)).collect::<Vec<_>>().join(",")
}

/// Accepts `aa:bb:cc:dd:ee:ff`, `aa-bb-…` or `aabb.ccdd.eeff`; returns the colon form.
pub fn normalize_mac(s: &str) -> Option<String> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_lowercase();
    if hex.len() != 12 {
        return None;
    }
    Some(hex.as_bytes().chunks(2).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(":"))
}

pub fn machines() -> Vec<Machine> {
    get(MACHINES).map(|s| parse_machines(&s)).unwrap_or_default()
}

pub fn find_machine(name: &str) -> Option<Machine> {
    let want = name.trim().to_lowercase();
    let all = machines();
    if want.is_empty() && all.len() == 1 {
        return all.into_iter().next();
    }
    all.into_iter().find(|m| m.name.to_lowercase() == want)
}

/// Add or replace a machine by name. Returns the normalized entry, or None for a bad MAC.
pub fn add_machine(name: &str, mac: &str, iface: &str) -> Option<Machine> {
    let mac = normalize_mac(mac)?;
    let name = name.trim().replace([',', '=', '@'], "");
    if name.is_empty() {
        return None;
    }
    let m = Machine { name, mac, iface: if iface.trim().is_empty() { "eth0".into() } else { iface.trim().into() } };
    let mut list: Vec<Machine> = machines().into_iter().filter(|x| x.name.to_lowercase() != m.name.to_lowercase()).collect();
    list.push(m.clone());
    set(MACHINES, &render_machines(&list));
    Some(m)
}

pub fn remove_machine(name: &str) -> bool {
    let before = machines();
    let after: Vec<Machine> = before.iter().filter(|x| !x.name.eq_ignore_ascii_case(name.trim())).cloned().collect();
    if after.len() == before.len() {
        return false;
    }
    set(MACHINES, &render_machines(&after));
    true
}

// ---- transcription vocabulary ------------------------------------------------

pub fn vocab() -> Vec<String> {
    get(VOCAB)
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Merge new terms in (case-insensitive dedupe). Returns the full list.
pub fn add_vocab(terms: &[String]) -> Vec<String> {
    let mut list = vocab();
    for t in terms {
        let t = t.trim();
        if !t.is_empty() && !list.iter().any(|x| x.eq_ignore_ascii_case(t)) {
            list.push(t.to_string());
        }
    }
    set(VOCAB, &list.join(", "));
    list
}

// ---- chats / pairing -----------------------------------------------------------

pub const ROLE_OWNER: &str = "owner";
pub const ROLE_MEMBER: &str = "member";

#[derive(Clone, Debug)]
pub struct Chat {
    pub chat_id: i64,
    pub role: String,
    pub name: String,
    pub joined: String,
}

pub fn chat_role(chat_id: i64) -> Option<String> {
    let db = db()?;
    let conn = db.lock();
    conn.query_row("SELECT role FROM chats WHERE chat_id = ?1", params![chat_id], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
}

pub fn is_owner(chat_id: i64) -> bool {
    chat_role(chat_id).as_deref() == Some(ROLE_OWNER)
}

pub fn chats() -> Vec<Chat> {
    let Some(db) = db() else { return vec![] };
    let conn = db.lock();
    let Ok(mut stmt) = conn.prepare("SELECT chat_id, role, name, joined FROM chats ORDER BY joined") else { return vec![] };
    stmt.query_map([], |r| Ok(Chat { chat_id: r.get(0)?, role: r.get(1)?, name: r.get(2)?, joined: r.get(3)? }))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

pub fn owner_chat() -> Option<i64> {
    chats().into_iter().find(|c| c.role == ROLE_OWNER).map(|c| c.chat_id)
}

pub fn register_chat(chat_id: i64, role: &str, name: &str) {
    if let Some(db) = db() {
        let conn = db.lock();
        let _ = conn.execute(
            "INSERT INTO chats (chat_id, role, name, joined) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chat_id) DO UPDATE SET role = excluded.role, name = CASE WHEN excluded.name = '' THEN chats.name ELSE excluded.name END",
            params![chat_id, role, name.trim(), chrono::Utc::now().to_rfc3339()],
        );
    }
}

pub fn set_chat_name(chat_id: i64, name: &str) {
    if let Some(db) = db() {
        let conn = db.lock();
        let _ = conn.execute("UPDATE chats SET name = ?2 WHERE chat_id = ?1", params![chat_id, name.trim()]);
    }
}

pub fn chat_name(chat_id: i64) -> String {
    chats().into_iter().find(|c| c.chat_id == chat_id).map(|c| c.name).unwrap_or_default()
}

pub fn remove_chat(chat_id: i64) -> bool {
    let Some(db) = db() else { return false };
    let conn = db.lock();
    conn.execute("DELETE FROM chats WHERE chat_id = ?1 AND role != ?2", params![chat_id, ROLE_OWNER])
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// Short, unambiguous code like `K7QD-3MXP` (no 0/O/1/I).
pub fn new_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let raw = uuid::Uuid::new_v4();
    let bytes = raw.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().take(8).enumerate() {
        if i == 4 {
            out.push('-');
        }
        out.push(ALPHABET[(*b as usize) % ALPHABET.len()] as char);
    }
    out
}

fn same_code(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// The setup code that pairs the owner. Comes from the installer/env when set;
/// otherwise generated once and kept in the table. `None` once an owner exists.
pub fn setup_code() -> Option<String> {
    if owner_chat().is_some() {
        return None;
    }
    if let Some(c) = get(SETUP_CODE) {
        return Some(c);
    }
    let c = new_code();
    set(SETUP_CODE, &c);
    Some(c)
}

/// Try to pair `chat_id` with `text`. Returns the role granted, if any.
pub fn try_pair(chat_id: i64, text: &str, name: &str) -> Option<&'static str> {
    if let Some(code) = setup_code() {
        if same_code(text, &code) {
            register_chat(chat_id, ROLE_OWNER, name);
            unset(SETUP_CODE);
            return Some(ROLE_OWNER);
        }
    }
    if let Some(code) = stored(INVITE_CODE) {
        if same_code(text, &code) {
            register_chat(chat_id, ROLE_MEMBER, name);
            unset(INVITE_CODE);
            return Some(ROLE_MEMBER);
        }
    }
    None
}

/// Mint a fresh one-time invite code (replaces any unused one).
pub fn new_invite() -> String {
    let c = new_code();
    set(INVITE_CODE, &c);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_and_machine_parsing() {
        assert_eq!(normalize_mac("AA-BB-CC-DD-EE-FF").as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(normalize_mac("aabb.ccdd.eeff").as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert!(normalize_mac("nope").is_none());
        let ms = parse_machines("desk=aa:bb:cc:dd:ee:ff@wlan0, nas=00-11-22-33-44-55, bad=zz");
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].iface, "wlan0");
        assert_eq!(ms[1].iface, "eth0");
        assert_eq!(render_machines(&ms), "desk=aa:bb:cc:dd:ee:ff@wlan0,nas=00:11:22:33:44:55@eth0");
    }

    #[test]
    fn codes_compare_loosely() {
        let c = new_code();
        assert_eq!(c.len(), 9);
        assert!(same_code(&c.to_lowercase().replace('-', " "), &c));
        assert!(!same_code("", ""));
    }
}
