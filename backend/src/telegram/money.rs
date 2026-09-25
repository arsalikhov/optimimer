//! Money commands (`/spent`, `/earned`, `/set_income`, `/balance`) — parsing via the cmd-* agents, math in Rust.

use super::*;

/// Capture a hand-logged transaction (`/spent`, `/earned`, or a receipt photo).
/// The `cmd-<name>` agent only PARSES the text into JSON; the ledger insert plus
/// dedup (reject an identical fingerprint, flag a same-amount near-duplicate) is
/// done in `finance::log_manual` so manual and CSV-imported rows stay consistent.
pub(super) async fn handle_money(state: &BotState, chat_id: i64, cmd: &str, body: &str) -> Reply {
    if body.trim().is_empty() {
        return Reply::text(format!("Usage: `/{cmd} <amount and what it was for>`"));
    }
    let wf = match state.store.get(&format!("cmd-{cmd}")) {
        Some(w) => w,
        None => return Reply::text(format!("The `/{cmd}` parser isn't installed (expected agent id `cmd-{cmd}`). Restart the backend to re-seed it.")),
    };
    let tz = tz_for(state, chat_id);
    let now = now_in_tz(&tz);
    let input = json!({
        "text": body,
        "now": now,
        "tz": tz,
        "chat_id": chat_id.to_string(),
        "command": cmd,
    });
    let result = engine::run(&wf, input).await;
    let mut parsed = output_text(&result).unwrap_or_default();
    // A confident Jev category pick overrules the parser's.
    if let Some(category) = jev_category(&result) {
        let cleaned = parsed.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
        if let Ok(mut v) = serde_json::from_str::<Value>(cleaned) {
            if v.is_object() {
                v["category"] = json!(category);
                parsed = v.to_string();
            }
        }
    }
    let kind = if cmd == "earned" {
        crate::finance::ManualKind::Earned
    } else {
        crate::finance::ManualKind::Spent
    };
    let today = now.get(..10).unwrap_or(&now);
    match crate::finance::log_manual(&parsed, kind, today).await {
        Ok(logged) => match logged.html {
            Some(html) => Reply::rich(html, logged.text),
            None => Reply::text(logged.text),
        },
        Err(e) => Reply::text(format!("Couldn't log that: {e}")),
    }
}

/// A chat's configured monthly income (0 if unset).
pub(super) fn monthly_income(state: &BotState, chat_id: i64) -> f64 {
    let conn = state.db.lock();
    conn.query_row(
        "SELECT monthly_income FROM chat_finance WHERE chat_id = ?1",
        params![chat_id],
        |r| r.get::<_, f64>(0),
    )
    .ok()
    .unwrap_or(0.0)
}

pub(super) fn set_monthly_income(state: &BotState, chat_id: i64, amount: f64) {
    let conn = state.db.lock();
    let _ = conn.execute(
        "INSERT INTO chat_finance (chat_id, monthly_income) VALUES (?1, ?2)
         ON CONFLICT(chat_id) DO UPDATE SET monthly_income = excluded.monthly_income",
        params![chat_id, amount],
    );
}

/// `/set_income 5000` (or `/income` to show the current value). Drives the salary
/// slice in `/balance`.
pub(super) fn handle_set_income(state: &BotState, chat_id: i64, body: &str) -> Reply {
    let raw = body.trim().trim_start_matches('$').replace(',', "");
    if raw.is_empty() {
        let cur = monthly_income(state, chat_id);
        if cur <= 0.0 {
            return Reply::text("No monthly income set yet. Set it with `/set_income 5000`.");
        }
        return Reply::text(format!(
            "Monthly income is *${cur:.2}* (≈ ${:.2}/week).\nChange it with `/set_income <amount>`.",
            cur / 4.348
        ));
    }
    match raw.parse::<f64>() {
        Ok(amount) if amount >= 0.0 => {
            set_monthly_income(state, chat_id, amount);
            Reply::text(format!(
                "Monthly income set to *${amount:.2}* (≈ ${:.2}/week). Used in /balance.\nSalary deposits in imported CSVs won't be double-counted while this is set. Use `/set_income 0` to count actual paychecks instead.",
                amount / 4.348
            ))
        }
        _ => Reply::text("That isn't a number. Try `/set_income 5000`."),
    }
}

/// `/balance` — income vs expenses for a week or a month (exact math in
/// `finance.rs`). Bare `/balance` = this week; `/balance last` / `/balance 2` =
/// past weeks; `/balance june` / `/balance may 2025` / `/balance this month` /
/// `/balance last month` = a calendar month.
pub(super) async fn handle_balance(state: &BotState, chat_id: i64, body: &str) -> Reply {
    let tz = tz_for(state, chat_id);
    let income = monthly_income(state, chat_id);
    let result = if let Some((month, year)) = parse_month(body, &tz) {
        crate::finance::monthly_balance(&tz, income, month, year).await
    } else {
        crate::finance::weekly_balance(&tz, income, parse_weeks_ago(body)).await
    };
    match result {
        Ok((html, fallback)) => Reply::rich(html, fallback),
        Err(e) => Reply::text(format!("Couldn't compute the balance: {e}")),
    }
}

/// Parse a `/balance` argument naming a month → (month 1-12, year). Handles
/// "this month", "last month", a month name ("june"/"jun"), and an optional
/// explicit 4-digit year; with no year, a month after the current one is assumed
/// to mean last year (e.g. asking for "december" in June → last December).
pub(super) fn parse_month(body: &str, tz_name: &str) -> Option<(u32, i32)> {
    use chrono::Datelike;
    let b = body.trim().to_lowercase();
    if b.is_empty() {
        return None;
    }
    let tz: chrono_tz::Tz = tz_name.parse().unwrap_or(chrono_tz::UTC);
    let now = Utc::now().with_timezone(&tz);
    let (cur_y, cur_m) = (now.year(), now.month());

    if b.contains("this month") {
        return Some((cur_m, cur_y));
    }
    if b.contains("last month") || b.contains("previous month") {
        return Some(if cur_m == 1 { (12, cur_y - 1) } else { (cur_m - 1, cur_y) });
    }
    let abbr = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let month = abbr.iter().position(|a| b.contains(a)).map(|i| i as u32 + 1)?;
    let year = b
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<i32>().ok())
        .find(|y| (2000..3000).contains(y))
        .unwrap_or(if month <= cur_m { cur_y } else { cur_y - 1 });
    Some((month, year))
}

/// Parse a `/balance` argument into a week offset: "" / "this" → 0; a number → that
/// many weeks back; "last"/"previous" → 1.
pub(super) fn parse_weeks_ago(body: &str) -> i64 {
    let b = body.trim().to_lowercase();
    if b.is_empty() || b.starts_with("this") {
        return 0;
    }
    if let Some(n) = b
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
    {
        return n.max(0);
    }
    if b.contains("last") || b.contains("prev") {
        return 1;
    }
    0
}
