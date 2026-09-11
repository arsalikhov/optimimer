//! Stock watches (`/watch`, `/watches`, `/unwatch`) — thin handlers over `crate::shopper`.

use super::*;

/// `/watch <url>` — start an hourly stock watch on a product page. Bare
/// `/watch` shows the current list instead.
pub(super) fn handle_watch(chat_id: i64, body: &str) -> Reply {
    let url = body
        .split_whitespace()
        .find(|w| w.starts_with("http://") || w.starts_with("https://"))
        // Trailing sentence punctuation from "watch this: <url>." isn't part of the link.
        .map(|u| u.trim_end_matches(['.', ',', ')', '>']));
    let url = match url {
        Some(u) => u,
        None if body.trim().is_empty() => return watches_reply(chat_id),
        None => {
            return Reply::text(
                "Usage: `/watch <product url>` — I'll re-check it hourly and ping you when it's back in stock.",
            )
        }
    };
    let shopper = crate::shopper::global();
    if shopper.active(chat_id).iter().any(|w| w.url == url) {
        return Reply::text("Already watching that link. Use /watches to see the list.");
    }
    shopper.add(crate::shopper::Watch {
        chat_id,
        url: url.to_string(),
        ..Default::default()
    });
    Reply::text(format!(
        "👀 Watching {url}\nFirst check lands within a minute, then hourly — I'll ping you the moment it's back in stock.\nUse /watches to list, `/unwatch <number>` to stop."
    ))
}

/// `/watches` — the chat's active stock watches, numbered for `/unwatch`.
pub(super) fn watches_reply(chat_id: i64) -> Reply {
    let watches = crate::shopper::global().active(chat_id);
    if watches.is_empty() {
        return Reply::text(
            "No active stock watches. Start one with `/watch <product url>`.",
        );
    }
    let mut lines = vec!["Active stock watches:".to_string()];
    for (i, w) in watches.iter().enumerate() {
        let name = if w.product.is_empty() { w.url.clone() } else { format!("{} — {}", w.product, w.url) };
        let status = if w.last_status.is_empty() { "not checked yet" } else { &w.last_status };
        lines.push(format!("{}. {name} ({status})", i + 1));
    }
    lines.push("Stop one with `/unwatch <number>`, or `/unwatch all`.".into());
    Reply::text(lines.join("\n"))
}

/// `/unwatch <number|url fragment|all>` — stop stock watches.
pub(super) fn handle_unwatch(chat_id: i64, body: &str) -> Reply {
    let shopper = crate::shopper::global();
    let watches = shopper.active(chat_id);
    if watches.is_empty() {
        return Reply::text("No active stock watches to stop.");
    }
    let q = body.trim();
    if q.eq_ignore_ascii_case("all") {
        let n = watches.len();
        for w in &watches {
            shopper.remove(&w.id);
        }
        return Reply::text(format!("Stopped {n} stock watch(es)."));
    }
    let target = match q.parse::<usize>() {
        Ok(n) if (1..=watches.len()).contains(&n) => Some(&watches[n - 1]),
        Ok(_) => None,
        // Not a number → match on the URL or product name.
        Err(_) if !q.is_empty() => {
            let ql = q.to_lowercase();
            watches
                .iter()
                .find(|w| w.url.to_lowercase().contains(&ql) || w.product.to_lowercase().contains(&ql))
        }
        Err(_) => None,
    };
    match target {
        Some(w) => {
            shopper.remove(&w.id);
            Reply::text(format!("Stopped watching {}", w.url))
        }
        None => watches_reply(chat_id),
    }
}
