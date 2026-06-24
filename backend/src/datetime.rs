use chrono::{Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde_json::Value;

/// A resolved instant, plus the meeting end (start + duration), the auto-clear
/// time (start + 1h), and whether it's more than 12h out. All RFC3339 with the
/// chat timezone's offset.
pub struct Resolved {
    pub start: String,
    pub end: String,
    pub clear: String,
    pub far: bool,
    /// Human-readable local start, e.g. "Tue Jun 23, 4:00 PM".
    pub human: String,
    /// Human-readable local end clock time, e.g. "5:00 PM".
    pub human_end: String,
}

/// Resolve an LLM-extracted "when" token object into concrete timestamps. The
/// LLM does no date arithmetic — it only fills whichever of these it sees:
///   in_minutes / in_hours / in_days   — relative offsets ("in 2 mins", "tomorrow"=1 day)
///   month / day / year                — an explicit calendar date ("June 23")
///   weekday / weekday_next            — a named weekday ("next tuesday")
///   hour (0-23) / minute (0-59)       — clock time ("2pm"=14)
/// `default_hour` is used when a date is given without a clock time; `duration_minutes`
/// sets the meeting end. An empty/absent spec resolves to "now".
pub fn resolve(spec: &Value, tz_name: &str, default_hour: u32, duration_minutes: i64) -> Option<Resolved> {
    let tz: Tz = tz_name.parse().unwrap_or(chrono_tz::UTC);
    let now = Utc::now().with_timezone(&tz);

    let in_min = spec.get("in_minutes").and_then(Value::as_i64);
    let in_hours = spec.get("in_hours").and_then(Value::as_i64);
    let in_days = spec.get("in_days").and_then(Value::as_i64);
    let month = spec.get("month").and_then(Value::as_u64).map(|v| v as u32);
    let day = spec.get("day").and_then(Value::as_u64).map(|v| v as u32);
    let year = spec.get("year").and_then(Value::as_i64).map(|v| v as i32);
    let weekday = spec.get("weekday").and_then(Value::as_str).and_then(parse_weekday);
    let weekday_next = spec.get("weekday_next").and_then(Value::as_bool).unwrap_or(false);
    let hour = spec.get("hour").and_then(Value::as_u64).map(|v| v as u32);
    let minute = spec.get("minute").and_then(Value::as_u64).map(|v| v as u32);

    let nothing = in_min.is_none() && in_hours.is_none() && in_days.is_none()
        && month.is_none() && weekday.is_none() && hour.is_none() && minute.is_none();

    let start = if nothing {
        now
    } else if (in_min.is_some() || in_hours.is_some()) && hour.is_none() && month.is_none() && weekday.is_none() {
        // Pure duration offset: "in 2 minutes", "in 3 hours".
        now + Duration::minutes(in_min.unwrap_or(0) + in_hours.unwrap_or(0) * 60)
    } else {
        let date = if let (Some(m), Some(d)) = (month, day) {
            resolve_md(now.date_naive(), m, d, year)?
        } else if let Some(wd) = weekday {
            next_weekday(now.date_naive(), wd, weekday_next)
        } else {
            now.date_naive() + Duration::days(in_days.unwrap_or(0))
        };
        let nt = NaiveTime::from_hms_opt(hour.unwrap_or(default_hour), minute.unwrap_or(0), 0)?;
        let ndt = date.and_time(nt);
        // Localize, tolerating DST spring-forward gaps.
        tz.from_local_datetime(&ndt).earliest().or_else(|| tz.from_local_datetime(&ndt).latest())?
    };

    let end = start + Duration::minutes(duration_minutes.max(0));
    Some(Resolved {
        start: start.to_rfc3339(),
        end: end.to_rfc3339(),
        clear: (start + Duration::hours(1)).to_rfc3339(),
        far: (start - now) > Duration::hours(12),
        human: start.format("%a %b %d, %-I:%M %p").to_string(),
        human_end: end.format("%-I:%M %p").to_string(),
    })
}

/// Build a calendar date from month/day, inferring the year as the next future
/// occurrence when not given (so "June 23" in July means next year).
fn resolve_md(today: NaiveDate, m: u32, d: u32, year: Option<i32>) -> Option<NaiveDate> {
    if let Some(y) = year {
        return NaiveDate::from_ymd_opt(y, m, d);
    }
    let this = NaiveDate::from_ymd_opt(today.year(), m, d)?;
    if this < today {
        NaiveDate::from_ymd_opt(today.year() + 1, m, d)
    } else {
        Some(this)
    }
}

/// Next date matching a weekday. A named weekday means the upcoming one (never
/// today); `next_week` pushes it a further 7 days ("next Tuesday").
fn next_weekday(today: NaiveDate, wd: Weekday, next_week: bool) -> NaiveDate {
    let cur = today.weekday().num_days_from_monday() as i64;
    let tgt = wd.num_days_from_monday() as i64;
    let mut delta = (tgt - cur).rem_euclid(7);
    if delta == 0 {
        delta = 7;
    }
    if next_week {
        delta += 7;
    }
    today + Duration::days(delta)
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    match s.to_lowercase().get(..3)? {
        "mon" => Some(Weekday::Mon),
        "tue" => Some(Weekday::Tue),
        "wed" => Some(Weekday::Wed),
        "thu" => Some(Weekday::Thu),
        "fri" => Some(Weekday::Fri),
        "sat" => Some(Weekday::Sat),
        "sun" => Some(Weekday::Sun),
        _ => None,
    }
}
