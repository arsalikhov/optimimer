//! Finance feature: weekly balance and CSV statement import.
//!
//! Money math is done HERE in Rust (exact), never by an LLM — the LLM only
//! normalizes/categorizes free-form input. Transactions live in the Notion
//! "Finances" database (`FINANCES_DB_ID`) with properties: Name(title),
//! Amount(number), Direction(select Expense|Income), Category(select), Date(date),
//! Source(select Manual|Receipt|CSV), Key(rich_text dedup fingerprint), Note.

use anyhow::{anyhow, Result};
use chrono::{Datelike, Duration, Utc};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

/// Expense categories the LLM must choose from (income rows are categorized "Income").
const CATEGORY_LIST: &[&str] = &[
    "Groceries", "Dining", "Transport", "Housing", "Utilities", "Health",
    "Entertainment", "Shopping", "Subscriptions", "Travel", "Loans", "Cash", "Other",
];
const CATEGORIES: &str =
    "Groceries, Dining, Transport, Housing, Utilities, Health, Entertainment, Shopping, Subscriptions, Travel, Loans, Cash, Other";

/// Snap an LLM-returned category onto the known set (case-insensitive), falling
/// back to "Other". Stops the model from inventing junk Notion select options
/// (e.g. "Restaurants") or guessing a category from a street name.
fn clamp_category(c: &str) -> String {
    let c = c.trim();
    CATEGORY_LIST
        .iter()
        .find(|k| k.eq_ignore_ascii_case(c))
        .map(|k| k.to_string())
        .unwrap_or_else(|| "Other".to_string())
}

/// A weeks-per-month divisor: 365.25 / 12 / 7 ≈ 4.348. Used to slice a monthly
/// salary into a comparable weekly figure for the balance view.
const WEEKS_PER_MONTH: f64 = 4.348;

fn db_id() -> String {
    std::env::var("FINANCES_DB_ID").unwrap_or_default()
}

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

// ---- Notion property extractors ---------------------------------------------

fn prop_number(props: &Value, key: &str) -> f64 {
    props[key]["number"].as_f64().unwrap_or(0.0)
}
fn prop_select(props: &Value, key: &str) -> String {
    props[key]["select"]["name"].as_str().unwrap_or("").to_string()
}
fn prop_rich(props: &Value, key: &str) -> String {
    props[key]["rich_text"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["plain_text"].as_str()).collect::<String>())
        .unwrap_or_default()
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

/// Reduce existing Finances pages to `(amount_cents, date, direction)` for the
/// "same amount, different fingerprint" near-duplicate check.
fn dup_index(pages: &[Value]) -> Vec<(i64, String, String)> {
    pages
        .iter()
        .filter_map(|p| {
            let props = &p["properties"];
            let date = props["Date"]["date"]["start"].as_str().unwrap_or("").to_string();
            if date.is_empty() {
                return None;
            }
            let cents = (prop_number(props, "Amount").abs() * 100.0).round() as i64;
            Some((cents, date, prop_select(props, "Direction")))
        })
        .collect()
}

/// A fingerprint catches a re-imported statement row exactly, but it can't bridge
/// a hand-typed entry and the bank's version of it (different description, a
/// posting date a day or two off). So when a new transaction shares an existing
/// one's amount + direction within a few days but has a DIFFERENT fingerprint,
/// flag it for manual review instead of guessing — return the Note text to stamp
/// on it. (Date-bounded so a recurring same-amount charge weeks later is ignored.)
fn likely_dup_note(prior: &[(i64, String, String)], cents: i64, direction: &str, date: &str) -> Option<String> {
    let hit = prior.iter().find(|(c, d, dir)| {
        *c == cents
            && dir.eq_ignore_ascii_case(direction)
            && days_apart(d, date).is_some_and(|n| n <= MANUAL_DUP_WINDOW_DAYS)
    })?;
    Some(format!(
        "⚠️ Likely duplicate — same amount (${:.2}) and direction as an existing transaction dated {}. Review and delete if redundant.",
        cents as f64 / 100.0,
        hit.1.get(0..10).unwrap_or(&hit.1),
    ))
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
    let filter = json!({ "and": [
        { "property": "Date", "date": { "on_or_after": monday.to_string() } },
        { "property": "Date", "date": { "before": next_monday.to_string() } }
    ]});
    period_balance(filter, title, range, monthly_income / WEEKS_PER_MONTH, monthly_income).await
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
    let filter = json!({ "and": [
        { "property": "Date", "date": { "on_or_after": start.to_string() } },
        { "property": "Date", "date": { "before": end.to_string() } }
    ]});
    period_balance(filter, title, range, monthly_income, monthly_income).await
}

/// Shared balance core: query the period, tally income/expenses/refunds/transfers,
/// render rich HTML + a plain fallback. `salary_budget` is the budgeted salary for
/// the period (a weekly slice or a whole month).
async fn period_balance(
    filter: Value,
    title: String,
    range: String,
    salary_budget: f64,
    monthly_income: f64,
) -> Result<(String, String)> {
    let pages = crate::notion::query_raw(&db_id(), Some(filter), None).await?;

    let mut salary_logged = 0.0;
    let mut other_income = 0.0;
    let mut expense_total = 0.0;
    let mut refund_total = 0.0;
    let mut savings_total = 0.0;
    let mut by_cat: Vec<(String, f64)> = Vec::new();
    for p in &pages {
        let props = &p["properties"];
        let amount = prop_number(props, "Amount").abs();
        match prop_select(props, "Direction").as_str() {
            "Income" => {
                // Salary is tracked separately so it's never double-counted against
                // the monthly-income slice (see below). Everything else is "other".
                if prop_select(props, "Category").eq_ignore_ascii_case("Salary") {
                    salary_logged += amount;
                } else {
                    other_income += amount;
                }
            }
            // Card payments / inter-account moves: not income, not spending. A
            // savings transfer is still excluded from net but tracked as a memo.
            "Transfer" => {
                if prop_select(props, "Category").eq_ignore_ascii_case("Savings") {
                    savings_total += amount;
                }
            }
            // Merchant refund: money back, nets against spending (not income).
            "Refund" => refund_total += amount,
            _ => {
                expense_total += amount;
                let cat = {
                    let c = prop_select(props, "Category");
                    if c.is_empty() { "Other".to_string() } else { c }
                };
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
    pub parsed: usize,
}

/// Import a bank/credit-card CSV: LLM normalizes + categorizes every row, then we
/// create one Finances page per row — SKIPPING any whose fingerprint already
/// exists in the DB, so re-importing the same (or overlapping) statement adds
/// nothing. `account_hint` ("credit" / "chequing" / "") tells the normalizer how
/// to read a credit: a card payment (Transfer) vs real income.
pub async fn import_csv(csv: &str, today: &str, account_hint: &str) -> Result<ImportSummary> {
    let rows = normalize_csv(csv, today, account_hint).await?;
    let parsed = rows.len();
    if parsed == 0 {
        return Ok(ImportSummary { created: 0, skipped: 0, transfers: 0, flagged: 0, parsed: 0 });
    }

    // Existing fingerprints (exact re-import dedup) and an amount/date/direction
    // index (cross-source "likely duplicate" flagging) — both from one fetch.
    let existing_pages = crate::notion::query_raw(&db_id(), None, None).await?;
    let mut seen: HashSet<String> = existing_pages
        .iter()
        .map(|p| prop_rich(&p["properties"], "Key"))
        .filter(|k| !k.is_empty())
        .collect();
    let prior = dup_index(&existing_pages);

    let mut created = 0;
    let mut skipped = 0;
    let mut transfers = 0;
    let mut flagged = 0;
    let rents = rent_amounts();
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
            "Transfer" => "Transfer".to_string(),
            // Refund/Expense: snap to a known category so the model can't spawn
            // junk Notion options or guess a category from an ATM's street name.
            _ => clamp_category(&r.category),
        };
        // Deterministic overrides for known recurring payees the model otherwise
        // mislabels: rent e-transfers → Housing, NSLSC → Loans.
        let (direction, category) = if is_rent(&r.description, r.amount, &rents) {
            ("Expense", "Housing".to_string())
        } else if is_student_loan(&r.description) {
            ("Expense", "Loans".to_string())
        } else if is_savings(&r.description) {
            ("Transfer", "Savings".to_string())
        } else if is_card_payment(&r.description) {
            ("Transfer", "Transfer".to_string())
        } else {
            (direction, category)
        };

        // Cross-source near-duplicate: this row's fingerprint is new, but its
        // amount + direction matches an existing transaction within a few days
        // (e.g. the bank's version of something you already logged by hand, or an
        // overlapping statement with a reworded description). Import it anyway,
        // but stamp a Note so you can eyeball it in Notion and delete if redundant.
        let row_cents = (r.amount.abs() * 100.0).round() as i64;
        let note = likely_dup_note(&prior, row_cents, direction, &date);

        let mut props = json!({
            "Amount": { "number": r.amount.abs() },
            "Direction": { "select": { "name": direction } },
            "Category": { "select": { "name": category } },
            "Date": { "date": { "start": date } },
            "Source": { "select": { "name": "CSV" } },
            "Key": { "rich_text": [{ "text": { "content": key } }] },
        });
        if let Some(n) = &note {
            props["Note"] = json!({ "rich_text": [{ "text": { "content": n } }] });
        }
        let op = crate::notion::Op {
            op: "create_page".to_string(),
            database_id: db_id(),
            title: if r.description.trim().is_empty() { "Transaction".to_string() } else { r.description.trim().to_string() },
            title_prop: "Name".to_string(),
            properties_json: props.to_string(),
            ..Default::default()
        };
        match crate::notion::run(op).await {
            Ok(_) => {
                created += 1;
                if direction == "Transfer" {
                    transfers += 1;
                }
                if note.is_some() {
                    flagged += 1;
                }
            }
            Err(e) => tracing::warn!("finance import: failed to create row: {e}"),
        }
    }
    Ok(ImportSummary { created, skipped, transfers, flagged, parsed })
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

/// Create a hand-logged transaction from the parse agent's JSON
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
    let existing = crate::notion::query_raw(&db_id(), None, None).await?;

    // Exact fingerprint already present → don't create a second copy.
    if existing.iter().any(|p| prop_rich(&p["properties"], "Key") == key) {
        return Ok(Logged {
            html: None,
            text: format!("Already logged — {title} {} on {date} is already in your Finances DB (same name, amount and date). Not added again.", money(amount)),
        });
    }

    let cents = (amount * 100.0).round() as i64;
    let note = likely_dup_note(&dup_index(&existing), cents, direction, &date);

    let mut props = json!({
        "Amount": { "number": amount },
        "Direction": { "select": { "name": direction } },
        "Category": { "select": { "name": category } },
        "Date": { "date": { "start": date } },
        "Source": { "select": { "name": "Manual" } },
        "Key": { "rich_text": [{ "text": { "content": key } }] },
    });
    if let Some(n) = &note {
        props["Note"] = json!({ "rich_text": [{ "text": { "content": n } }] });
    }
    let op = crate::notion::Op {
        op: "create_page".to_string(),
        database_id: db_id(),
        title: title.clone(),
        title_prop: "Name".to_string(),
        properties_json: props.to_string(),
        ..Default::default()
    };
    let page = crate::notion::run(op).await?;
    let url = page["url"].as_str().unwrap_or("");

    let esc = crate::engine::html_escape;
    let mut html = format!(
        "<!rich><b>Logged {noun}</b>\n<table><tr><td>Amount</td><td>{}</td></tr><tr><td>{who}</td><td>{}</td></tr><tr><td>Category</td><td>{}</td></tr><tr><td>Date</td><td>{date}</td></tr></table>",
        money(amount),
        esc(&title),
        esc(&category),
    );
    if !url.is_empty() {
        html.push_str(&format!("\n<a href=\"{}\">Link to Notion page</a>", esc(url)));
    }
    if note.is_some() {
        html.push_str("\n<blockquote>⚠️ Possible duplicate of an existing transaction — flagged in its Note so you can review and delete if redundant.</blockquote>");
    }
    let text = format!(
        "Logged {noun}: {} — {title} ({category}){}",
        money(amount),
        if note.is_some() { " ⚠️ possible duplicate" } else { "" },
    );
    Ok(Logged { html: Some(html), text })
}

/// Ask an LLM to turn raw CSV text into a clean JSON array of transactions.
/// `account_hint` ("credit" / "chequing" / "") disambiguates what a credit means.
async fn normalize_csv(csv: &str, today: &str, account_hint: &str) -> Result<Vec<Row>> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(anyhow!("OPENROUTER_API_KEY unset — cannot parse CSV"));
    }
    let model = std::env::var("FINANCE_MODEL").unwrap_or_else(|_| "anthropic/claude-sonnet-4.6".to_string());
    // Cap the payload so a huge statement can't blow the context window.
    let csv = if csv.len() > 16000 { &csv[..16000] } else { csv };

    // Cache the parse: re-importing the same statement (same model/account/text)
    // reuses the prior result instead of paying for the LLM again. `today` is
    // deliberately NOT in the key — it only fills missing dates, and keying on it
    // would defeat the cache across days.
    let ck = crate::cache::key("csv", &[&model, account_hint, csv]);
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
         For \"Expense\" choose the best category; use \"Salary\" ONLY for payroll. One element per transaction row; ignore header rows, opening/closing balance lines, and blank rows. If a date is missing use {today}; normalize all dates to YYYY-MM-DD."
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

