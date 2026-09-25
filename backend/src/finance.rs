//! Finance feature: weekly balance and CSV statement import.
//!
//! Money math is done HERE in Rust (exact), never by an LLM — the LLM only
//! normalizes/categorizes free-form input. Transactions live in the shared
//! SQLite db (`transactions` table): name, amount, direction
//! (Expense|Income|Transfer|Refund), category, date, source (Manual|Receipt|CSV),
//! key (dedup fingerprint) and a free-text note (e.g. a likely-duplicate flag).

use crate::db::Db;
use anyhow::{anyhow, Result};
use chrono::{Datelike, Duration, NaiveDate, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

/// One ledger row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Txn {
    pub id: String,
    /// `YYYY-MM-DD`
    pub date: String,
    pub name: String,
    /// Always positive; `direction` carries the sign's meaning.
    pub amount: f64,
    pub direction: String,
    pub category: String,
    pub source: String,
    pub key: String,
    pub note: String,
    pub created: String,
}

/// SQLite-backed ledger, mirroring the other stores.
#[derive(Clone)]
pub struct Ledger {
    db: Db,
}

static GLOBAL: OnceLock<Ledger> = OnceLock::new();

pub fn init(db: Db) -> Ledger {
    let l = Ledger { db };
    let _ = GLOBAL.set(l.clone());
    l
}

pub fn global() -> Ledger {
    GLOBAL
        .get_or_init(|| Ledger { db: Db::memory().expect("in-memory ledger") })
        .clone()
}

const COLS: &str = "id, date, name, amount, direction, category, source, key, note, created";

fn row_to_txn(r: &rusqlite::Row) -> rusqlite::Result<Txn> {
    Ok(Txn {
        id: r.get(0)?,
        date: r.get(1)?,
        name: r.get(2)?,
        amount: r.get(3)?,
        direction: r.get(4)?,
        category: r.get(5)?,
        source: r.get(6)?,
        key: r.get(7)?,
        note: r.get(8)?,
        created: r.get(9)?,
    })
}

impl Ledger {
    pub fn insert(&self, mut t: Txn) -> Result<Txn> {
        if t.id.is_empty() {
            t.id = uuid::Uuid::new_v4().to_string();
        }
        if t.created.is_empty() {
            t.created = Utc::now().to_rfc3339();
        }
        let conn = self.db.lock();
        conn.execute(
            &format!("INSERT INTO transactions ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"),
            params![t.id, t.date, t.name, t.amount, t.direction, t.category, t.source, t.key, t.note, t.created],
        )?;
        drop(conn);
        // Best-effort Markdown mirror for Obsidian Bases; the row is already saved.
        if let Err(e) = crate::vault::write_transaction(&t) {
            tracing::warn!("finance mirror write failed: {e}");
        }
        Ok(t)
    }

    fn query(&self, sql: &str, p: &[&dyn rusqlite::ToSql]) -> Vec<Txn> {
        let conn = self.db.lock();
        let Ok(mut stmt) = conn.prepare(sql) else { return vec![] };
        stmt.query_map(p, row_to_txn)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn all(&self) -> Vec<Txn> {
        self.query(&format!("SELECT {COLS} FROM transactions ORDER BY date DESC, created DESC"), &[])
    }

    /// Rows with `start <= date < end`.
    pub fn in_range(&self, start: NaiveDate, end: NaiveDate) -> Vec<Txn> {
        let (a, b) = (start.to_string(), end.to_string());
        self.query(
            &format!("SELECT {COLS} FROM transactions WHERE date >= ?1 AND date < ?2 ORDER BY date DESC"),
            &[&a, &b],
        )
    }

    pub fn recent(&self, limit: usize) -> Vec<Txn> {
        let n = limit as i64;
        self.query(&format!("SELECT {COLS} FROM transactions ORDER BY date DESC, created DESC LIMIT ?1"), &[&n])
    }

    pub fn has_key(&self, key: &str) -> bool {
        !self.query(&format!("SELECT {COLS} FROM transactions WHERE key = ?1 LIMIT 1"), &[&key]).is_empty()
    }

    pub fn delete(&self, id: &str) -> bool {
        let removed = {
            let conn = self.db.lock();
            conn.execute("DELETE FROM transactions WHERE id = ?1", params![id]).map(|n| n > 0).unwrap_or(false)
        };
        if removed {
            if let Err(e) = crate::vault::remove_transaction(id) {
                tracing::warn!("finance mirror delete failed: {e}");
            }
        }
        removed
    }
}

/// Expense categories the LLM must choose from (income rows are categorized "Income").
const CATEGORY_LIST: &[&str] = &[
    "Groceries", "Dining", "Transport", "Housing", "Utilities", "Health",
    "Entertainment", "Shopping", "Subscriptions", "Travel", "Loans", "Cash", "Other",
];
const CATEGORIES: &str =
    "Groceries, Dining, Transport, Housing, Utilities, Health, Entertainment, Shopping, Subscriptions, Travel, Loans, Cash, Other";

/// Snap an LLM-returned category onto the known set (case-insensitive), falling
/// back to "Other". Stops the model from inventing junk categories
/// (e.g. "Restaurants") or guessing a category from a street name.
fn clamp_category(c: &str) -> String {
    let c = c.trim();
    CATEGORY_LIST
        .iter()
        .find(|k| k.eq_ignore_ascii_case(c))
        .map(|k| k.to_string())
        .unwrap_or_else(|| "Other".to_string())
}

/// Sub-types of a Transfer row. Card payments settle spending that is already
/// counted as expenses, so money-flow views drop them; the other three are real
/// movements of cash into or out of the accounts being tracked.
pub const TRANSFER_KINDS: &[&str] = &["Card payment", "Savings", "Transfer in", "Transfer out"];

/// Snap an LLM transfer category onto `TRANSFER_KINDS`; unknown/legacy values
/// (e.g. the old catch-all "Transfer") are treated as card payments.
fn clamp_transfer(c: &str) -> String {
    let c = c.trim();
    TRANSFER_KINDS
        .iter()
        .find(|k| k.eq_ignore_ascii_case(c))
        .map(|k| k.to_string())
        .unwrap_or_else(|| "Card payment".to_string())
}

/// A weeks-per-month divisor: 365.25 / 12 / 7 ≈ 4.348. Used to slice a monthly
/// salary into a comparable weekly figure for the balance view.
const WEEKS_PER_MONTH: f64 = 4.348;

/// Exact e-transfer amounts that mean "rent" → forced to a Housing expense (rent
/// is paid by Interac e-transfer, so it otherwise looks like a generic transfer).
/// Opt-in via RENT_AMOUNTS (comma-separated exact amounts); empty = disabled.
fn rent_amounts() -> Vec<f64> {
    std::env::var("RENT_AMOUNTS")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse::<f64>().ok()).collect::<Vec<_>>())
        .unwrap_or_default()
}

/// True if `desc` looks like an Interac e-transfer and `amount` matches a rent amount.
fn is_rent(desc: &str, amount: f64, rents: &[f64]) -> bool {
    let lc = desc.to_lowercase();
    let etransfer = lc.contains("interac") || lc.contains("etrnsfr") || lc.contains("e-transfer");
    etransfer && rents.iter().any(|a| (a - amount.abs()).abs() < 0.005)
}

/// True if `desc` is a student-loan payment (NSLSC = National Student Loans
/// Service Centre) → a Loans expense, not generic "Other".
fn is_student_loan(desc: &str) -> bool {
    let lc = desc.to_lowercase();
    lc.contains("nslsc") || lc.contains("student loan")
}

/// True if `desc` is a transfer to a savings/investment account (Wealthsimple) →
/// a Transfer tagged "Savings" so it's excluded from spending but shown as money
/// put away rather than lost.
fn is_savings(desc: &str) -> bool {
    let lc = desc.to_lowercase();
    lc.contains("ws investments") || lc.contains("wealthsimple")
}

/// True if `desc` is a credit-card bill payment (e.g. Amex) from a bank account →
/// a Transfer, NEVER an expense, so it doesn't double-count against the purchases
/// already on that card's own statement.
fn is_card_payment(desc: &str) -> bool {
    let lc = desc.to_lowercase();
    lc.contains("amex") && (lc.contains("pymt") || lc.contains("payment") || lc.contains("bill") || lc.contains("ftd"))
}

/// Stable dedup fingerprint for a transaction: date | amount(cents) | description.
/// Re-importing the same statement row yields the same Key, so we skip it.
fn fingerprint(date: &str, amount: f64, desc: &str) -> String {
    let cents = (amount * 100.0).round() as i64;
    let norm = desc.trim().to_lowercase();
    let mut h = DefaultHasher::new();
    (date, cents, norm).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// How far apart (days) a CSV row and a hand-logged entry may sit and still be
/// treated as the same transaction — absorbs the lag between a card's posting
/// date and the day you typed `/spent`.
const MANUAL_DUP_WINDOW_DAYS: i64 = 4;

/// Whole days between two `YYYY-MM-DD` dates, or `None` if either won't parse.
fn days_apart(a: &str, b: &str) -> Option<i64> {
    let pa = chrono::NaiveDate::parse_from_str(a.get(0..10)?, "%Y-%m-%d").ok()?;
    let pb = chrono::NaiveDate::parse_from_str(b.get(0..10)?, "%Y-%m-%d").ok()?;
    Some((pa - pb).num_days().abs())
}

/// Reduce existing rows to `(amount_cents, date, direction, name)` for the
/// "same amount, different fingerprint" near-duplicate check.
fn dup_index(txns: &[Txn]) -> Vec<(i64, String, String, String)> {
    txns.iter()
        .filter(|t| !t.date.is_empty())
        .map(|t| ((t.amount.abs() * 100.0).round() as i64, t.date.clone(), t.direction.clone(), t.name.clone()))
        .collect()
}

/// A fingerprint catches a re-imported statement row exactly, but it can't bridge
/// a hand-typed entry and the bank's version of it (different description, a
/// posting date a day or two off). So when a new transaction shares an existing
/// one's amount + direction within a few days but has a DIFFERENT fingerprint,
/// it is a candidate duplicate. (Date-bounded so a recurring same-amount charge
/// weeks later is ignored.)
fn likely_dup<'a>(prior: &'a [(i64, String, String, String)], cents: i64, direction: &str, date: &str) -> Option<&'a (i64, String, String, String)> {
    prior.iter().find(|(c, d, dir, _)| {
        *c == cents
            && dir.eq_ignore_ascii_case(direction)
            && days_apart(d, date).is_some_and(|n| n <= MANUAL_DUP_WINDOW_DAYS)
    })
}

fn dup_note(cents: i64, hit_date: &str) -> String {
    format!(
        "⚠️ Likely duplicate — same amount (${:.2}) and direction as an existing transaction dated {}. Review and delete if redundant.",
        cents as f64 / 100.0,
        hit_date.get(0..10).unwrap_or(hit_date),
    )
}

/// Below this Jev probability that two entries are the same payment, a
/// same-amount neighbour is a coincidence (two $4.50 coffees) and isn't flagged.
const SAME_TXN_MIN: f64 = 0.3;

/// The near-duplicate note to stamp on a new row, if any. Amount, direction and
/// date only find the candidate; with Jev on, it also has to plausibly be the
/// same payment by name ("SQ *BLUE BOTTLE 0042" vs "coffee at blue bottle").
async fn dup_note_checked(prior: &[(i64, String, String, String)], cents: i64, direction: &str, date: &str, name: &str) -> Option<String> {
    let hit = likely_dup(prior, cents, direction, date)?;
    let q = crate::jev::Q::noul(
        "Are these two ledger entries the same real-world payment recorded twice (for example once typed by hand and once from the bank statement)?",
        "The same payment: the payee descriptions refer to the same merchant or person",
        "Two separate payments that merely share an amount",
    );
    let state = json!({
        "amount": format!("{:.2}", cents as f64 / 100.0),
        "direction": direction,
        "new_entry": { "description": name, "date": date },
        "existing_entry": { "description": hit.3, "date": hit.1 },
    });
    if let Some(p) = crate::jev::noul(state, q).await {
        if p < SAME_TXN_MIN {
            tracing::info!("not flagging a same-amount neighbour as duplicate (jev {p:.2})");
            return None;
        }
    }
    Some(dup_note(cents, &hit.1))
}

fn money(x: f64) -> String {
    format!("${:.2}", x)
}

/// A category-fitting emoji to lead a /balance sub-row, so each line is glanceable.
fn row_emoji(name: &str) -> &'static str {
    let n = name.to_lowercase();
    if n.contains("salary") {
        return "💰";
    }
    if n.contains("other income") {
        return "💵";
    }
    if n.contains("refund") {
        return "↩️";
    }
    match n.as_str() {
        "groceries" => "🛒",
        "dining" => "🍽️",
        "transport" => "🚗",
        "housing" => "🏠",
        "utilities" => "💡",
        "health" => "🏥",
        "entertainment" => "🎬",
        "shopping" => "🛍️",
        "subscriptions" => "🔁",
        "travel" => "✈️",
        "loans" => "🏦",
        "cash" => "🏧",
        _ => "▫️",
    }
}

const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

// ---- Balance views -----------------------------------------------------------

/// Weekly balance. `weeks_ago`: 0 = current week, 1 = last week, N = N weeks back.
/// The salary budget is the monthly income sliced to one week.
pub async fn weekly_balance(
    tz_name: &str,
    monthly_income: f64,
    weeks_ago: i64,
) -> Result<(String, String)> {
    let tz: chrono_tz::Tz = tz_name.parse().unwrap_or(chrono_tz::UTC);
    let today = Utc::now().with_timezone(&tz).date_naive();
    let this_monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let monday = this_monday - Duration::days(7 * weeks_ago.max(0));
    let next_monday = monday + Duration::days(7);

    let title = match weeks_ago {
        0 => "This week".to_string(),
        1 => "Last week".to_string(),
        n => format!("{n} weeks ago"),
    };
    let range = if weeks_ago > 0 {
        format!("{} – {}", monday, next_monday - Duration::days(1))
    } else {
        format!("since {monday}")
    };
    // Always bound both ends so a future-dated row (clock skew, post-dated txn)
    // can't leak into the current week.
    period_balance(monday, next_monday, title, range, monthly_income / WEEKS_PER_MONTH, monthly_income).await
}

/// Monthly balance for a specific calendar month. The salary budget is the full
/// monthly income (not sliced).
pub async fn monthly_balance(
    _tz_name: &str,
    monthly_income: f64,
    month: u32,
    year: i32,
) -> Result<(String, String)> {
    let start = chrono::NaiveDate::from_ymd_opt(year, month, 1)
        .ok_or_else(|| anyhow!("invalid month {month}/{year}"))?;
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let end = chrono::NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();

    let title = format!("{} {year}", MONTH_NAMES[(month - 1) as usize]);
    let range = format!("{} – {}", start, end - Duration::days(1));
    period_balance(start, end, title, range, monthly_income, monthly_income).await
}

/// Shared balance core: query the period, tally income/expenses/refunds/transfers,
/// render rich HTML + a plain fallback. `salary_budget` is the budgeted salary for
/// the period (a weekly slice or a whole month).
async fn period_balance(
    start: NaiveDate,
    end: NaiveDate,
    title: String,
    range: String,
    salary_budget: f64,
    monthly_income: f64,
) -> Result<(String, String)> {
    let txns = global().in_range(start, end);

    let mut salary_logged = 0.0;
    let mut other_income = 0.0;
    let mut expense_total = 0.0;
    let mut refund_total = 0.0;
    let mut savings_total = 0.0;
    let mut by_cat: Vec<(String, f64)> = Vec::new();
    for t in &txns {
        let amount = t.amount.abs();
        match t.direction.as_str() {
            "Income" => {
                // Salary is tracked separately so it's never double-counted against
                // the monthly-income slice (see below). Everything else is "other".
                if t.category.eq_ignore_ascii_case("Salary") {
                    salary_logged += amount;
                } else {
                    other_income += amount;
                }
            }
            // Card payments / inter-account moves: not income, not spending. A
            // savings transfer is still excluded from net but tracked as a memo.
            "Transfer" => {
                if t.category.eq_ignore_ascii_case("Savings") {
                    savings_total += amount;
                }
            }
            // Merchant refund: money back, nets against spending (not income).
            "Refund" => refund_total += amount,
            _ => {
                expense_total += amount;
                let cat = if t.category.is_empty() { "Other".to_string() } else { t.category.clone() };
                match by_cat.iter_mut().find(|(k, _)| *k == cat) {
                    Some(e) => e.1 += amount,
                    None => by_cat.push((cat, amount)),
                }
            }
        }
    }
    by_cat.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Salary is counted from exactly ONE source, never both:
    //  - monthly income set  → use its smoothed weekly slice; salary DEPOSITS that
    //    landed this week (Category=Salary, e.g. from a CSV import) are shown but
    //    NOT added, so the paycheck isn't double-counted.
    //  - no monthly income   → count actual logged salary deposits instead.
    let use_slice = monthly_income > 0.0;
    let salary_component = if use_slice { salary_budget } else { salary_logged };
    let total_income = salary_component + other_income;
    let net_expenses = expense_total - refund_total;
    let net = total_income - net_expenses;
    let net_dot = if net >= 0.0 { "🟢" } else { "🔴" };
    let suppressed = use_slice && salary_logged > 0.0;

    // Native rich table. The renderer draws a proper bordered grid (column +
    // row lines) when the table has a header row — without one it falls back to
    // a borderless layout. Section totals (Income/Expenses/Net) are bold with
    // indented "· " sub-rows; the net 🟢/🔴 lives in its cell (native cells
    // handle emoji fine — no monospace alignment to break).
    let pct = |amt: f64| -> String {
        if total_income > 0.0 { format!("{:.0}%", amt / total_income * 100.0) } else { String::new() }
    };
    let esc = crate::engine::html_escape;
    let row = |label: &str, amount: &str, percent: &str| -> String {
        format!("<tr><td>{label}</td><td>{amount}</td><td>{percent}</td></tr>")
    };
    // Indent sub-rows under their category with non-breaking spaces (plain
    // leading spaces collapse in the renderer) and lead with a category-fitting
    // emoji, which reads more clearly than a small bullet.
    let sub = |name: &str| format!("\u{00A0}\u{00A0}{} {}", row_emoji(name), esc(name));
    // The renderer draws no border between body rows, so divide the sections
    // with a rule row. A colspan cell's text sets the table's MINIMUM width: if
    // it exceeds the content width the slack inflates the last column (% gets
    // pushed off-screen on mobile). Keep it SHORTER than the content so columns
    // size to their data and nothing is forced wider.
    let sep = format!("<tr><td colspan=\"3\" align=\"center\">{}</td></tr>", "─".repeat(16));

    let mut rows = row("<b>Income</b>", &money(total_income), "");
    if salary_component > 0.0 {
        rows.push_str(&row(&sub("salary"), &money(salary_component), ""));
    }
    if other_income > 0.0 {
        rows.push_str(&row(&sub("other income"), &money(other_income), ""));
    }
    if suppressed {
        rows.push_str(&row(&sub("salary deposits (not counted)"), &money(salary_logged), ""));
    }
    rows.push_str(&sep);
    rows.push_str(&row("<b>Expenses</b>", &money(net_expenses), &pct(net_expenses)));
    for (cat, amt) in &by_cat {
        rows.push_str(&row(&sub(cat), &money(*amt), &pct(*amt)));
    }
    if refund_total > 0.0 {
        rows.push_str(&row(&sub("refunds"), &format!("-{}", money(refund_total)), ""));
    }
    rows.push_str(&sep);
    rows.push_str(&row(&format!("<b>Net {net_dot}</b>"), &format!("<b>{}</b>", money(net)), ""));

    let mut html = format!(
        "<b>{title}</b>  <i>{range}</i>\n<table><thead><tr><th>Item</th><th>Amount</th><th>%</th></tr></thead><tbody>{rows}</tbody></table>"
    );
    if total_income > 0.0 {
        html.push_str("\n<i>% = share of income.</i>");
    }
    if savings_total > 0.0 {
        html.push_str(&format!("\n<i>💰 Saved {} to savings (not counted in net).</i>", money(savings_total)));
    }
    if suppressed {
        html.push_str("\n<i>Salary deposits from imports aren't added because a monthly income is set — clear with /set_income 0 to count actuals.</i>");
    }

    // Plain-text fallback (older clients / rich-send failure).
    let mut fb = format!("{title} ({range})\nIncome: {}\nExpenses: {}\n", money(total_income), money(net_expenses));
    for (cat, amt) in &by_cat {
        fb.push_str(&format!("  {cat}: {}\n", money(*amt)));
    }
    fb.push_str(&format!("Net: {}", money(net)));
    if savings_total > 0.0 {
        fb.push_str(&format!("\nSaved: {}", money(savings_total)));
    }

    Ok((html, fb))
}

// ---- CSV import (dedup-aware) ------------------------------------------------

/// One normalized transaction the LLM extracts from a CSV row.
#[derive(serde::Deserialize)]
struct Row {
    #[serde(default)]
    date: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    amount: f64,
    #[serde(default)]
    direction: String,
    #[serde(default)]
    category: String,
}

/// Decide whether a CSV is a credit-card or chequing statement. Priority:
/// 1. an explicit caption ("credit"/"chequing"/…),
/// 2. Amex files (name contains "activity") → credit,
/// 3. BMO files (name contains "statement") told apart by header — chequing has a
///    "Transaction Type" column, credit cards have "Card #"/"Posting Date",
/// 4. otherwise "" → let the model infer.
pub fn detect_account(file_name: &str, caption: &str, csv: &str) -> &'static str {
    let cap = caption.to_lowercase();
    if cap.contains("cheq") || cap.contains("check") || cap.contains("debit") || cap.contains("bank") {
        return "chequing";
    }
    if cap.contains("credit") || cap.contains("card") || cap.contains("visa") || cap.contains("mastercard") || cap.contains("amex") {
        return "credit";
    }
    let name = file_name.to_lowercase();
    if name.contains("activity") {
        return "credit"; // Amex export
    }
    // Inspect the header rows (BMO 'statement' files don't say which type by name).
    let head: String = csv.lines().take(8).collect::<Vec<_>>().join("\n").to_lowercase();
    if head.contains("transaction type") || head.contains("first bank card") {
        return "chequing";
    }
    if head.contains("posting date") || head.contains("card #") || head.contains("card#") {
        return "credit";
    }
    ""
}

/// Outcome of a CSV import.
pub struct ImportSummary {
    pub created: usize,
    pub skipped: usize,
    /// Card payments / inter-account transfers — created but excluded from /balance.
    pub transfers: usize,
    /// Created but tagged "likely duplicate" in their Note for you to review.
    pub flagged: usize,
    /// Created, but the category classifier wasn't sure — tagged in their Note.
    pub uncertain: usize,
    pub parsed: usize,
}

/// Import a bank/credit-card CSV: LLM normalizes + categorizes every row, then we
/// insert one ledger row per CSV row — SKIPPING any whose fingerprint already
/// exists in the DB, so re-importing the same (or overlapping) statement adds
/// nothing. `account_hint` ("credit" / "chequing" / "") tells the normalizer how
/// to read a credit: a card payment (Transfer) vs real income.
pub async fn import_csv(csv: &str, today: &str, account_hint: &str) -> Result<ImportSummary> {
    let rows = normalize_csv(csv, today, account_hint).await?;
    if !rows.is_empty() {
        purge_placeholders();
    }
    let parsed = rows.len();
    if parsed == 0 {
        return Ok(ImportSummary { created: 0, skipped: 0, transfers: 0, flagged: 0, uncertain: 0, parsed: 0 });
    }

    // Existing fingerprints (exact re-import dedup) and an amount/date/direction
    // index (cross-source "likely duplicate" flagging) — both from one fetch.
    let existing = global().all();
    let mut seen: HashSet<String> = existing.iter().map(|t| t.key.clone()).filter(|k| !k.is_empty()).collect();
    let prior = dup_index(&existing);

    let mut created = 0;
    let mut skipped = 0;
    let mut transfers = 0;
    let mut flagged = 0;
    let rents = rent_amounts();

    // Pass 1: dedup and a first classification from the normalizer's labels.
    struct Pending { row: Row, date: String, key: String, direction: &'static str, category: String, fixed: bool, unsure: Option<String> }
    let mut pending: Vec<Pending> = Vec::new();
    for r in rows {
        if r.amount == 0.0 && r.description.trim().is_empty() {
            continue;
        }
        let date = if r.date.trim().is_empty() { today.to_string() } else { r.date.trim().to_string() };
        let key = fingerprint(&date, r.amount, &r.description);
        // Dedup against the DB and against earlier rows in this same batch.
        if !seen.insert(key.clone()) {
            skipped += 1;
            continue;
        }
        let direction = match r.direction.to_lowercase().as_str() {
            "income" => "Income",
            "transfer" => "Transfer",
            "refund" => "Refund",
            _ => "Expense",
        };
        let category = match direction {
            // Keep "Salary" distinct so /balance won't double-count it against a
            // configured monthly income; other income collapses to "Income".
            "Income" => {
                if r.category.eq_ignore_ascii_case("Salary") { "Salary".to_string() } else { "Income".to_string() }
            }
            "Transfer" => clamp_transfer(&r.category),
            // Refund/Expense: snap to a known category so the model can't spawn
            // junk categories or guess a category from an ATM's street name.
            _ => clamp_category(&r.category),
        };
        // Deterministic overrides for known recurring payees the model otherwise
        // mislabels: rent e-transfers → Housing, NSLSC → Loans.
        let fixed = if is_rent(&r.description, r.amount, &rents) {
            Some(("Expense", "Housing".to_string()))
        } else if is_student_loan(&r.description) {
            Some(("Expense", "Loans".to_string()))
        } else if is_savings(&r.description) {
            Some(("Transfer", "Savings".to_string()))
        } else if is_card_payment(&r.description) {
            Some(("Transfer", "Card payment".to_string()))
        } else {
            None
        };
        let (direction, category, fixed) = match fixed {
            Some((d, c)) => (d, c, true),
            None => (direction, category, false),
        };
        pending.push(Pending { row: r, date, key, direction, category, fixed, unsure: None });
    }

    // Pass 2: Jev re-picks each category within its direction; a row it can't
    // place confidently keeps the normalizer's label but is flagged for review
    // instead of quietly landing in "Other".
    if crate::jev::enabled() {
        let asks: Vec<(usize, TxnAsk)> = pending
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.fixed)
            .map(|(i, p)| (i, TxnAsk { description: p.row.description.clone(), amount: p.row.amount.abs(), direction: p.direction }))
            .collect();
        let picks = refine_categories(account_hint, &asks.iter().map(|(_, a)| a.clone()).collect::<Vec<_>>()).await;
        for ((i, _), pick) in asks.iter().zip(picks) {
            let Some(pick) = pick else { continue };
            let p = &mut pending[*i];
            if pick.confidence >= CATEGORY_MIN_CONFIDENCE {
                p.category = pick.choice.clone();
            } else {
                p.unsure = Some(format!("❓ Category unsure ({}) — review.", pick.top(2)));
            }
        }
    }

    // Pass 3: near-duplicate check and insert.
    let mut uncertain = 0;
    for p in pending {
        // Cross-source near-duplicate: this row's fingerprint is new, but its
        // amount + direction matches an existing transaction within a few days
        // (e.g. the bank's version of something you already logged by hand, or an
        // overlapping statement with a reworded description). Import it anyway,
        // but stamp a note so you can eyeball it in /transactions and remove it.
        let row_cents = (p.row.amount.abs() * 100.0).round() as i64;
        let dup = dup_note_checked(&prior, row_cents, p.direction, &p.date, &p.row.description).await;
        let note = [dup.clone(), p.unsure.clone()].into_iter().flatten().collect::<Vec<_>>().join(" ");

        let txn = Txn {
            date: p.date,
            name: if p.row.description.trim().is_empty() { "Transaction".to_string() } else { p.row.description.trim().to_string() },
            amount: p.row.amount.abs(),
            direction: p.direction.to_string(),
            category: p.category,
            source: "CSV".into(),
            key: p.key,
            note,
            ..Default::default()
        };
        match global().insert(txn) {
            Ok(_) => {
                created += 1;
                if p.direction == "Transfer" {
                    transfers += 1;
                }
                if dup.is_some() {
                    flagged += 1;
                }
                if p.unsure.is_some() {
                    uncertain += 1;
                }
            }
            Err(e) => tracing::warn!("finance import: failed to create row: {e}"),
        }
    }
    if created > 0 {
        crate::charts::refresh();
    }
    Ok(ImportSummary { created, skipped, transfers, flagged, uncertain, parsed })
}

/// Jev must be this sure of a category to replace the normalizer's label;
/// below it the row is flagged "category unsure" instead.
const CATEGORY_MIN_CONFIDENCE: f64 = 0.5;

/// Questions per Jev request: every question is judged independently, but one
/// request has a token budget, so a long statement goes in chunks.
const ROWS_PER_REQUEST: usize = 40;

#[derive(Clone)]
struct TxnAsk {
    description: String,
    amount: f64,
    direction: &'static str,
}

/// What each category means, for the classifier. Same order as `CATEGORY_LIST`.
const CATEGORY_HINTS: &[&str] = &[
    "Supermarkets, grocers, food and household supplies for home",
    "Restaurants, cafés, bars, takeout and food delivery",
    "Transit, taxis, ride-hailing, fuel, parking, tolls, car costs",
    "Rent, mortgage, home insurance, repairs, furnishings",
    "Electricity, gas, water, internet, phone bills",
    "Pharmacy, doctors, dentists, therapy, medical and health insurance",
    "Movies, concerts, games, events, hobbies",
    "Retail purchases: clothing, electronics, general merchandise, online stores",
    "Recurring digital or membership subscriptions (streaming, software, gym)",
    "Flights, hotels, travel bookings, foreign transactions on a trip",
    "Loan repayments, including student loans",
    "ATM / bank-machine cash withdrawals (often just an address or ATM code)",
    "Anything that fits none of the above",
];

/// The categories a row of this direction may take, with their meanings.
fn category_options(direction: &str) -> Vec<(String, String)> {
    match direction {
        "Income" => vec![
            ("Salary".into(), "Payroll, wages or direct deposit from an employer".into()),
            ("Income".into(), "Any other money in: government or tax deposit, interest, an e-transfer received".into()),
        ],
        "Transfer" => vec![
            ("Card payment".into(), "Paying a credit-card bill, seen from either the card or the bank account".into()),
            ("Savings".into(), "A contribution to savings or investments".into()),
            ("Transfer in".into(), "Money arriving from another of the user's own accounts".into()),
            ("Transfer out".into(), "Money leaving to another of the user's own accounts, not a card payment or savings".into()),
        ],
        _ => CATEGORY_LIST.iter().zip(CATEGORY_HINTS).map(|(n, h)| (n.to_string(), h.to_string())).collect(),
    }
}

/// One Jev choice per row (batched), picking its category within its direction.
/// Returns one entry per ask; `None` where Jev didn't answer.
async fn refine_categories(account_hint: &str, asks: &[TxnAsk]) -> Vec<Option<crate::jev::Pick>> {
    let statement = match account_hint {
        "credit" => "credit-card statement",
        "chequing" => "chequing / bank-account statement",
        _ => "bank or credit-card statement (type unknown)",
    };
    let mut out = Vec::with_capacity(asks.len());
    for chunk in asks.chunks(ROWS_PER_REQUEST) {
        let questions: Vec<(String, crate::jev::Q)> = chunk
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let instructions = format!(
                    "Which category fits this {} transaction from the statement: \"{}\", ${:.2}? Judge by the payee; never infer a category from a street name or address.",
                    a.direction.to_lowercase(),
                    a.description.trim(),
                    a.amount
                );
                (format!("row_{i}"), crate::jev::Q::Choice { instructions, options: category_options(a.direction) })
            })
            .collect();
        match crate::jev::ask(json!({ "statement": statement }), questions).await {
            Ok(answers) => out.extend((0..chunk.len()).map(|i| answers.choice(&format!("row_{i}")))),
            Err(e) => {
                tracing::warn!("jev category pass failed: {e}");
                out.extend(chunk.iter().map(|_| None));
            }
        }
    }
    out
}

/// When the file name, caption and header don't say what kind of statement a
/// CSV is (`detect_account` returned ""), ask Jev. "" when it can't tell.
pub async fn infer_account(csv: &str) -> &'static str {
    let head: String = csv.lines().take(12).collect::<Vec<_>>().join("\n");
    let q = crate::jev::Q::choice(
        "What kind of account is this CSV statement export from?",
        [
            ("credit", "A credit card: purchases as charges, plus 'payment received' / 'thank you' credits"),
            ("chequing", "A chequing or bank account: payroll deposits, e-transfers, bill payments, debit/credit columns"),
            ("unclear", "Can't tell from these lines"),
        ],
    );
    match crate::jev::choice(json!(head), q).await {
        Some(p) if p.confidence >= 0.75 => match p.choice.as_str() {
            "credit" => "credit",
            "chequing" => "chequing",
            _ => "",
        },
        _ => "",
    }
}

/// A hand-logged transaction: an expense (`/spent`, receipt photo) or income (`/earned`).
#[derive(Clone, Copy)]
pub enum ManualKind {
    Spent,
    Earned,
}

/// A formatted confirmation to send back to Telegram. `html` is the rich body
/// (with `text` as its fallback); when `html` is `None`, send `text` plain.
pub struct Logged {
    pub html: Option<String>,
    pub text: String,
}

/// Insert a hand-logged transaction from the parse agent's JSON
/// (`{amount, merchant|source, category, date}`), applying the SAME dedup as CSV
/// import so manual and imported rows can't double up:
///   • identical fingerprint already in the DB → NOT added again (you double-tapped
///     or re-sent it); the reply says so.
///   • a different fingerprint but the same amount + direction within a few days →
///     created, but flagged "likely duplicate" in its Note for you to review.
/// Manual rows now carry a Key too, so a later CSV import can dedup against them.
pub async fn log_manual(parsed_json: &str, kind: ManualKind, today: &str) -> Result<Logged> {
    // The model is told to emit bare JSON, but tolerate stray code fences.
    let cleaned = parsed_json
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let v: Value = serde_json::from_str(cleaned).map_err(|e| anyhow!("couldn't parse entry JSON: {e}"))?;

    let amount = v["amount"]
        .as_f64()
        .or_else(|| v["amount"].as_str().and_then(|s| s.replace(['$', ','], "").trim().parse().ok()))
        .unwrap_or(0.0)
        .abs();
    if amount == 0.0 {
        return Ok(Logged {
            html: None,
            text: "I couldn't read an amount from that — try e.g. `/spent 24.50 on lunch`.".to_string(),
        });
    }
    let date = {
        let d = v["date"].as_str().unwrap_or("").trim();
        if d.len() >= 10 { d[..10].to_string() } else { today.to_string() }
    };
    let (direction, title, category, noun, who) = match kind {
        ManualKind::Spent => {
            let title = v["merchant"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Expense").to_string();
            ("Expense", title, clamp_category(v["category"].as_str().unwrap_or("Other")), "expense", "Merchant")
        }
        ManualKind::Earned => {
            let title = v["source"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Income").to_string();
            let cat = if v["category"].as_str().unwrap_or("").eq_ignore_ascii_case("Salary") { "Salary" } else { "Income" };
            ("Income", title, cat.to_string(), "income", "Source")
        }
    };

    let key = fingerprint(&date, amount, &title);
    purge_placeholders();
    let ledger = global();

    // Exact fingerprint already present → don't create a second copy.
    if ledger.has_key(&key) {
        return Ok(Logged {
            html: None,
            text: format!("Already logged — {title} {} on {date} is already in the ledger (same name, amount and date). Not added again.", money(amount)),
        });
    }

    let cents = (amount * 100.0).round() as i64;
    let note = dup_note_checked(&dup_index(&ledger.all()), cents, direction, &date, &title).await;

    ledger.insert(Txn {
        date: date.clone(),
        name: title.clone(),
        amount,
        direction: direction.to_string(),
        category: category.clone(),
        source: "Manual".into(),
        key,
        note: note.clone().unwrap_or_default(),
        ..Default::default()
    })?;
    crate::charts::refresh();

    let esc = crate::engine::html_escape;
    let mut html = format!(
        "<!rich><b>Logged {noun}</b>\n<table><tr><td>Amount</td><td>{}</td></tr><tr><td>{who}</td><td>{}</td></tr><tr><td>Category</td><td>{}</td></tr><tr><td>Date</td><td>{date}</td></tr></table>",
        money(amount),
        esc(&title),
        esc(&category),
    );
    if note.is_some() {
        html.push_str("\n<blockquote>⚠️ Possible duplicate of an existing transaction — flagged in its note; see /transactions to review and remove.</blockquote>");
    }
    let text = format!(
        "Logged {noun}: {} — {title} ({category}){}",
        money(amount),
        if note.is_some() { " ⚠️ possible duplicate" } else { "" },
    );
    Ok(Logged { html: Some(html), text })
}

// ---- Placeholder data --------------------------------------------------------

/// Source tag for demo rows seeded into an empty ledger so the dashboard has
/// something to show. Purged as soon as a real transaction arrives.
pub const PLACEHOLDER_SOURCE: &str = "Placeholder";

/// Delete every placeholder row (and its vault mirror). Returns the count.
pub fn purge_placeholders() -> usize {
    let rows: Vec<Txn> = global().all().into_iter().filter(|t| t.source == PLACEHOLDER_SOURCE).collect();
    let n = rows.len();
    for t in rows {
        global().delete(&t.id);
    }
    if n > 0 {
        tracing::info!("purged {n} placeholder transaction(s)");
        crate::charts::refresh();
    }
    n
}

/// Seed a deterministic three-month sample ledger (this month so far plus the
/// two before it). Only meant for an empty ledger; the caller checks that.
pub fn seed_placeholders(today: NaiveDate) -> usize {
    let month_start = |d: NaiveDate| NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d);
    let prev = |d: NaiveDate| {
        let (y, m) = if d.month() == 1 { (d.year() - 1, 12) } else { (d.year(), d.month() - 1) };
        NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(d)
    };
    let this = month_start(today);
    let months = [prev(prev(this)), prev(this), this];
    // (day, name, amount, direction, category)
    let template: &[(u32, &str, f64, &str, &str)] = &[
        (1, "Sample · Payroll", 2850.0, "Income", "Salary"),
        (15, "Sample · Payroll", 2850.0, "Income", "Salary"),
        (1, "Sample · Rent e-transfer", 1860.0, "Expense", "Housing"),
        (2, "Sample · Hydro", 96.4, "Expense", "Utilities"),
        (3, "Sample · Phone plan", 58.0, "Expense", "Subscriptions"),
        (4, "Sample · Grocery run", 112.35, "Expense", "Groceries"),
        (6, "Sample · Coffee", 6.25, "Expense", "Dining"),
        (7, "Sample · Transit pass", 128.15, "Expense", "Transport"),
        (9, "Sample · Restaurant", 74.9, "Expense", "Dining"),
        (11, "Sample · Grocery run", 98.7, "Expense", "Groceries"),
        (12, "Sample · Streaming", 18.99, "Expense", "Subscriptions"),
        (13, "Sample · Pharmacy", 32.4, "Expense", "Health"),
        (14, "Sample · ATM withdrawal", 100.0, "Expense", "Cash"),
        (16, "Sample · Savings contribution", 600.0, "Transfer", "Savings"),
        (17, "Sample · Credit card payment", 1450.0, "Transfer", "Card payment"),
        (18, "Sample · Grocery run", 121.6, "Expense", "Groceries"),
        (19, "Sample · Online order", 89.99, "Expense", "Shopping"),
        (20, "Sample · Return refund", 45.0, "Refund", "Shopping"),
        (21, "Sample · Student loan", 193.13, "Expense", "Loans"),
        (22, "Sample · Concert tickets", 140.0, "Expense", "Entertainment"),
        (23, "Sample · From savings", 200.0, "Transfer", "Transfer in"),
        (25, "Sample · Grocery run", 104.2, "Expense", "Groceries"),
        (26, "Sample · Dinner out", 62.5, "Expense", "Dining"),
        (27, "Sample · Freelance invoice", 400.0, "Income", "Income"),
        (28, "Sample · Gas", 65.0, "Expense", "Transport"),
    ];
    let mut n = 0;
    for (i, m) in months.iter().enumerate() {
        for (day, name, amount, dir, cat) in template {
            let Some(d) = NaiveDate::from_ymd_opt(m.year(), m.month(), *day) else { continue };
            if d > today {
                continue;
            }
            // Vary amounts a little per month so the charts aren't flat lines.
            let wobble = 1.0 + (i as f64 - 1.0) * 0.06;
            let amt = if *dir == "Income" && *cat == "Salary" { *amount } else { ((amount * wobble) * 100.0).round() / 100.0 };
            let key = format!("placeholder:{}:{}:{}", d, name, amt);
            if global().has_key(&key) {
                continue;
            }
            let ok = global()
                .insert(Txn {
                    date: d.to_string(),
                    name: name.to_string(),
                    amount: amt,
                    direction: dir.to_string(),
                    category: cat.to_string(),
                    source: PLACEHOLDER_SOURCE.into(),
                    key,
                    ..Default::default()
                })
                .is_ok();
            if ok {
                n += 1;
            }
        }
    }
    if n > 0 {
        crate::charts::refresh();
    }
    n
}

/// `/transactions [n]` — the newest rows, numbered so `/remove_transaction N`
/// can target one. Returns (rich html, plain fallback).
pub fn recent_listing(limit: usize) -> (String, String) {
    let rows = global().recent(limit);
    if rows.is_empty() {
        return ("<b>No transactions yet.</b>".into(), "No transactions yet.".into());
    }
    let esc = crate::engine::html_escape;
    let mut html = String::from("<b>Recent transactions</b><table><thead><tr><th>#</th><th>Date</th><th>Name</th><th>Amount</th></tr></thead><tbody>");
    let mut text = String::from("Recent transactions\n");
    for (i, t) in rows.iter().enumerate() {
        let sign = match t.direction.as_str() { "Income" | "Refund" => "+", "Transfer" => "↔", _ => "-" };
        let flag = if !t.note.is_empty() { " ⚠️" } else if t.source == PLACEHOLDER_SOURCE { " (sample)" } else { "" };
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}{flag}<br><i>{}</i></td><td>{sign}{}</td></tr>",
            i + 1, esc(&t.date), esc(&t.name), esc(&t.category), money(t.amount)
        ));
        text.push_str(&format!("{}. {} {} {sign}{} ({}){flag}\n", i + 1, t.date, t.name, money(t.amount), t.category));
    }
    html.push_str("</tbody></table><i>⚠️ = flagged as a likely duplicate. Remove one with /remove_transaction N.</i>");
    text.push_str("⚠️ = likely duplicate. Remove one with /remove_transaction N.");
    (html, text)
}

/// Delete the N-th newest row (1-based, as listed by `/transactions`).
pub fn remove_nth(n: usize) -> Option<Txn> {
    let rows = global().recent(n.max(1));
    let t = rows.into_iter().nth(n.checked_sub(1)?)?;
    let removed = global().delete(&t.id).then_some(t);
    if removed.is_some() {
        crate::charts::refresh();
    }
    removed
}

/// Ask an LLM to turn raw CSV text into a clean JSON array of transactions.
/// `account_hint` ("credit" / "chequing" / "") disambiguates what a credit means.
async fn normalize_csv(csv: &str, today: &str, account_hint: &str) -> Result<Vec<Row>> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow!("OPENROUTER_API_KEY unset — cannot parse CSV"));
    }
    let model = crate::llm::finance();
    // Cap the payload so a huge statement can't blow the context window.
    let csv = if csv.len() > 16000 { &csv[..16000] } else { csv };

    // Cache the parse: re-importing the same statement (same model/account/text)
    // reuses the prior result instead of paying for the LLM again. `today` is
    // deliberately NOT in the key — it only fills missing dates, and keying on it
    // would defeat the cache across days.
    let ck = crate::cache::key("csv-v2", &[&model, account_hint, csv]);
    if let Some(cached) = crate::cache::get(&ck) {
        if let Ok(rows) = serde_json::from_str::<Vec<Row>>(&cached) {
            return Ok(rows);
        }
    }

    let account_line = match account_hint.trim().to_lowercase().as_str() {
        "credit" => "This is a CREDIT-CARD statement.",
        "chequing" => "This is a CHEQUING / bank-account statement.",
        _ => "First infer the type: a CREDIT-CARD statement (columns like 'Card #'/'Posting Date'; purchases plus 'payment received' lines) or a CHEQUING/bank statement (a 'Transaction Type' CREDIT/DEBIT column; payroll, e-transfers, bill payments).",
    };
    let system = format!(
        "You convert a CSV bank/credit-card statement into JSON. {account_line} Output ONLY a minified JSON array — no prose, no code fences. Each element: {{\"date\":\"YYYY-MM-DD\",\"description\":\"merchant/payee\",\"amount\":<positive number>,\"direction\":\"Expense\"|\"Income\"|\"Transfer\"|\"Refund\",\"category\":\"<one of: {CATEGORIES}>\"}}. The input amount may be SIGNED and there may be a Transaction Type (CREDIT/DEBIT) column — use the sign, the type, and the description to choose direction, but OUTPUT amount as a POSITIVE number.\n\
         Direction meanings: \"Expense\" = a real purchase/charge/fee/bill or money spent; \"Refund\" = a merchant credit reversing a purchase (a return) or a fee rebate (money back, reduces spending); \"Transfer\" = a payment toward a credit card, a move between your OWN accounts, or an investment contribution (NOT income, NOT spending); \"Income\" = real money in (payroll/wages, government/tax deposit, interest, or an Interac e-transfer RECEIVED).\n\
         CREDIT-CARD statement: a POSITIVE amount is a purchase → \"Expense\"; a NEGATIVE amount is a credit → \"Refund\" if it's a merchant return, or \"Transfer\" if it's a card payment ('PAYMENT RECEIVED', 'THANK YOU', a 'TF'/transfer from a bank account). A credit card has NO \"Income\".\n\
         CHEQUING statement: payroll ('PAY/PAY', wages, direct deposit) → \"Income\" category \"Salary\"; government/tax deposit, interest, or Interac e-transfer RECEIVED → \"Income\"; a fee rebate → \"Refund\"; a transfer to a credit card ('TF <long card number>', 'AMEX … PYMT/FTD/BILL'), a move between your own accounts, or an investment ('WS INVESTMENTS','INV/PLA') → \"Transfer\"; a bill, purchase, insurance, fee, or Interac e-transfer SENT → \"Expense\".\n\
         An ABM/ATM cash withdrawal — a bank-machine withdrawal, often shown only as a street address or location with a code like '[IB]', 'ABM', 'ATM', or 'WITHDRAWAL' and no merchant — is an \"Expense\" with category \"Cash\"; NEVER infer a category from the street name/address of such a row. A student-loan payment ('NSLSC' / 'student loan') is an \"Expense\" with category \"Loans\".\n\
         For \"Expense\" choose the best category; use \"Salary\" ONLY for payroll. For \"Transfer\" the category MUST be one of: \"Card payment\" (paying a credit-card bill, seen from either account), \"Savings\" (a contribution to savings or investments), \"Transfer in\" (money arriving from another of your OWN accounts, e.g. a withdrawal from savings), \"Transfer out\" (money leaving to another of your own accounts that is not a card payment or savings). One element per transaction row; ignore header rows, opening/closing balance lines, and blank rows. If a date is missing use {today}; normalize all dates to YYYY-MM-DD."
    );
    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": format!("CSV:\n{csv}") }
        ]
    });
    let resp = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&key)
        .header("HTTP-Referer", "http://localhost:5173")
        .header("X-Title", "Optimimer")
        .json(&body)
        .timeout(std::time::Duration::from_secs(240))
        .send()
        .await?;
    let status = resp.status();
    let j: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(anyhow!("CSV parse {}: {}", status, j));
    }
    let content = j["choices"][0]["message"]["content"].as_str().unwrap_or("");
    let cleaned = strip_fences(content);
    let rows: Vec<Row> = serde_json::from_str(&cleaned)
        .map_err(|e| anyhow!("model did not return a JSON array of transactions: {e}"))?;
    crate::cache::put(&ck, &cleaned);
    Ok(rows)
}

/// Strip ```json fences / stray prose around a JSON array.
fn strip_fences(s: &str) -> String {
    let t = s.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    match (t.find('['), t.rfind(']')) {
        (Some(a), Some(b)) if b > a => t[a..=b].to_string(),
        _ => t.to_string(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_insert_range_and_dedup() {
        let l = Ledger { db: Db::memory().unwrap() };
        l.insert(Txn { date: "2026-09-01".into(), name: "Coffee".into(), amount: 4.5, direction: "Expense".into(), category: "Dining".into(), key: "k1".into(), ..Default::default() }).unwrap();
        l.insert(Txn { date: "2026-09-15".into(), name: "Pay".into(), amount: 3000.0, direction: "Income".into(), category: "Salary".into(), key: "k2".into(), ..Default::default() }).unwrap();
        assert!(l.has_key("k1"));
        assert!(!l.has_key("nope"));
        let sept = l.in_range(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 9, 10).unwrap());
        assert_eq!(sept.len(), 1);
        assert_eq!(sept[0].name, "Coffee");
        assert_eq!(l.recent(1)[0].name, "Pay");
        let id = l.recent(1)[0].id.clone();
        assert!(l.delete(&id));
        assert_eq!(l.all().len(), 1);
    }

    #[test]
    fn placeholders_seed_and_purge() {
        init(Db::memory().unwrap());
        // Mirror notes land in whatever VAULT_DIR is current (a gitignored test
        // vault or backend/vault); don't touch the process-wide env from here.
        let today = NaiveDate::from_ymd_opt(2026, 9, 11).unwrap();
        let n = seed_placeholders(today);
        assert!(n > 40, "seeded {n}");
        assert_eq!(seed_placeholders(today), 0, "idempotent");
        assert!(global().all().iter().all(|t| t.source == PLACEHOLDER_SOURCE));
        assert!(global().all().iter().all(|t| t.date <= "2026-09-11".to_string()), "nothing in the future");
        assert_eq!(purge_placeholders(), n);
        assert!(global().all().is_empty());
    }

    #[test]
    fn near_duplicate_is_flagged_within_window() {
        let prior = vec![(2450, "2026-09-01".to_string(), "Expense".to_string(), "Coffee".to_string())];
        assert!(likely_dup(&prior, 2450, "Expense", "2026-09-03").is_some());
        assert!(likely_dup(&prior, 2450, "Expense", "2026-09-20").is_none());
        assert!(likely_dup(&prior, 2450, "Income", "2026-09-01").is_none());
    }

    #[test]
    fn category_options_follow_direction() {
        assert!(category_options("Expense").iter().any(|(n, _)| n == "Cash"));
        assert_eq!(category_options("Income").len(), 2);
        assert_eq!(category_options("Transfer").iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), TRANSFER_KINDS.to_vec());
    }
}
