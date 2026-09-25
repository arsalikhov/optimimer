use crate::db::Db;
use chrono::Utc;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::OnceLock;
use uuid::Uuid;

/// One stock watch. The worker re-checks `url` every SHOPPER_CHECK_SECS
/// (default hourly); when the page finally reads as purchasable it pings
/// `chat_id` on Telegram and marks the watch done.
///
/// This powers `/watch <url>` — "tell me when this is back in stock".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Watch {
    pub id: String,
    pub chat_id: i64,
    pub url: String,
    /// Product name extracted by the LLM on the first successful check.
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub created_at: String,
    /// RFC3339 of the last check; empty means never checked (due immediately).
    #[serde(default)]
    pub last_checked: String,
    /// Human-readable outcome of the last check ("out of stock", "error: …").
    #[serde(default)]
    pub last_status: String,
    #[serde(default)]
    pub done: bool,
    /// Hash of the page text at the last out-of-stock verdict: an unchanged
    /// page keeps its verdict without asking the classifier again.
    #[serde(default)]
    pub page_hash: String,
}

/// SQLite-backed watch store (table `stock_watches`), mirroring `Scheduler`.
#[derive(Clone)]
pub struct Shopper {
    db: Db,
}

static GLOBAL: OnceLock<Shopper> = OnceLock::new();

/// Initialise the process-wide shopper store. Safe to call once at startup.
pub fn init(db: Db) -> Shopper {
    let shopper = Shopper { db };
    let _ = GLOBAL.set(shopper.clone());
    shopper
}

/// The process-wide shopper store. Falls back to an in-memory instance if
/// `init` was never called (keeps the Telegram handlers safe in tests).
pub fn global() -> Shopper {
    GLOBAL
        .get_or_init(|| Shopper {
            db: Db::memory().expect("in-memory shopper db"),
        })
        .clone()
}

impl Shopper {
    /// Add a watch, assigning an id if absent. Returns the id.
    pub fn add(&self, mut watch: Watch) -> String {
        if watch.id.is_empty() {
            watch.id = Uuid::new_v4().to_string();
        }
        if watch.created_at.is_empty() {
            watch.created_at = Utc::now().to_rfc3339();
        }
        let id = watch.id.clone();
        self.save(&watch);
        id
    }

    fn save(&self, watch: &Watch) {
        if let Ok(json) = serde_json::to_string(watch) {
            let conn = self.db.lock();
            let _ = conn.execute(
                "INSERT INTO stock_watches (id, json, done) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json, done = excluded.done",
                params![watch.id, json, watch.done as i64],
            );
        }
    }

    /// Every not-yet-done watch, oldest first. `chat_id = 0` returns all chats'.
    pub fn active(&self, chat_id: i64) -> Vec<Watch> {
        let conn = self.db.lock();
        let mut stmt = match conn.prepare("SELECT json FROM stock_watches WHERE done = 0") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut watches: Vec<Watch> = rows
            .flatten()
            .filter_map(|j| serde_json::from_str(&j).ok())
            .filter(|w: &Watch| chat_id == 0 || w.chat_id == chat_id)
            .collect();
        watches.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        watches
    }

    /// Remove (mark done) a watch by exact id. Returns whether it existed.
    pub fn remove(&self, id: &str) -> bool {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE stock_watches SET done = 1 WHERE id = ?1 AND done = 0",
            params![id],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }
}

/// How often each watch is re-checked. Overridable for testing.
fn check_interval_secs() -> i64 {
    std::env::var("SHOPPER_CHECK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600)
}

/// Background loop: every 60s, re-check watches whose last check is older than
/// the interval. Spawned from `main`. A new watch (empty `last_checked`) is
/// picked up on the next tick, so the first check lands within a minute.
pub async fn run_worker(shopper: Shopper) {
    let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let client = reqwest::Client::new();
    tracing::info!("shopper worker started");

    loop {
        let interval = check_interval_secs();
        for mut watch in shopper.active(0) {
            let due = watch
                .last_checked
                .parse::<chrono::DateTime<Utc>>()
                .map(|t| (Utc::now() - t).num_seconds() >= interval)
                // Never checked (or unparseable timestamp) → check now.
                .unwrap_or(true);
            if !due {
                continue;
            }
            check_watch(&client, &token, &shopper, &mut watch).await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

/// One check: fetch the page, judge whether it's purchasable, persist the
/// outcome, and notify + finish the watch when it's back in stock.
async fn check_watch(client: &reqwest::Client, token: &str, shopper: &Shopper, watch: &mut Watch) {
    watch.last_checked = Utc::now().to_rfc3339();
    match check_stock(client, &watch.url, &watch.page_hash).await {
        Ok(check) => {
            if !check.product.is_empty() {
                watch.product = check.product;
            }
            if check.in_stock {
                watch.last_status = "in stock".into();
                watch.done = true;
                let name = if watch.product.is_empty() { watch.url.clone() } else { watch.product.clone() };
                notify(client, token, watch.chat_id, &format!("🛍️ Back in stock: {name}\n{}", watch.url)).await;
                tracing::info!("stock watch {} hit: {}", watch.id, watch.url);
            } else {
                watch.last_status = check.status;
                watch.page_hash = check.page_hash;
            }
        }
        Err(e) => {
            // Transient fetch/LLM failures just record the error and retry next
            // interval — a flaky product page shouldn't kill the watch.
            watch.last_status = format!("error: {e}");
            tracing::warn!("stock check failed for {}: {e}", watch.url);
        }
    }
    shopper.save(watch);
}

struct StockCheck {
    in_stock: bool,
    product: String,
    /// Human-readable verdict when not in stock ("out of stock", "pre-order", …).
    status: String,
    page_hash: String,
}

/// Jev must be this sure a page is purchasable before we ping — a false "back
/// in stock" is worse than checking again in an hour.
const IN_STOCK_MIN_CONFIDENCE: f64 = 0.8;

/// Fetch a product page and judge availability from its visible text (Jev when
/// configured, else the shopper LLM). Uncertainty counts as out of stock so the
/// watch keeps running. A page whose text hashes the same as at the last
/// out-of-stock verdict is not re-judged.
async fn check_stock(client: &reqwest::Client, url: &str, last_hash: &str) -> anyhow::Result<StockCheck> {
    let html = client
        .get(url)
        // A browser-ish UA: plenty of shops serve bot UAs an empty shell.
        .header(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0",
        )
        .header("Accept-Language", "en")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let text = page_text(&html, 12_000);
    let page_hash = text_hash(&text);
    if !last_hash.is_empty() && page_hash == last_hash {
        return Ok(StockCheck { in_stock: false, product: String::new(), status: "out of stock (page unchanged)".into(), page_hash });
    }

    if crate::jev::enabled() {
        let q = crate::jev::Q::choice(
            "Judging only from this product page's visible text, can the product be bought right now?",
            [
                ("in_stock", "Purchasable now: an enabled add-to-cart/buy button, 'in stock', or a delivery date"),
                ("out_of_stock", "'Out of stock', 'sold out', 'unavailable', 'notify me when available', or a disabled buy button"),
                ("preorder", "Only available to pre-order or backorder, not shipping now"),
                ("not_a_product_page", "Not a single product's page (a search, a category, an error, a login or captcha wall)"),
                ("unclear", "The text doesn't say either way"),
            ],
        );
        let state = json!({ "url": url, "page_text": text });
        if let Some(pick) = crate::jev::choice(state, q).await {
            tracing::info!("stock check {url}: {}", pick.top(2));
            let in_stock = pick.confident(IN_STOCK_MIN_CONFIDENCE) == Some("in_stock");
            let status = match pick.choice.as_str() {
                "in_stock" => "possibly in stock (not sure enough to ping)",
                "preorder" => "pre-order only",
                "not_a_product_page" => "not a product page — check the link",
                "unclear" => "unclear",
                _ => "out of stock",
            };
            // Jev doesn't write text: the product name comes from the page's own title.
            return Ok(StockCheck { in_stock, product: page_title(&html), status: status.into(), page_hash });
        }
        // Jev failed: fall through to the LLM.
    }

    let model = crate::llm::shopper();
    let system = "You judge whether a product page shows the product as purchasable RIGHT NOW. \
                  Signals for in stock: an enabled add-to-cart/buy button, 'in stock', a deliverable date. \
                  Signals against: 'out of stock', 'sold out', 'unavailable', 'notify me when available', \
                  'pre-order', a disabled buy button. If the text is ambiguous or not a product page, \
                  say in_stock false. Output ONLY minified JSON \
                  {\"in_stock\": true|false, \"product\": \"<short product name or empty>\"} — no prose.";
    let prompt = format!("URL: {url}\n\nVisible page text (truncated):\n{text}");
    let answer = crate::openrouter::chat(&model, system, &prompt).await?;

    let parsed = parse_json_loose(&answer);
    Ok(StockCheck {
        in_stock: parsed["in_stock"].as_bool().unwrap_or(false),
        product: parsed["product"].as_str().unwrap_or("").trim().to_string(),
        status: "out of stock".into(),
        page_hash,
    })
}

fn text_hash(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The product name a shop gives its own page: `og:title`, else `<title>`, with
/// a trailing " | Shop name" / " - Shop name" dropped.
pub(crate) fn page_title(html: &str) -> String {
    let lower = html.to_lowercase();
    let og = lower.find("property=\"og:title\"").or_else(|| lower.find("property='og:title'")).and_then(|at| {
        let tag_start = lower[..at].rfind('<')?;
        let tag_end = at + lower[at..].find('>')?;
        let tag = &html[tag_start..tag_end];
        let c = tag.to_lowercase().find("content=")? + "content=".len();
        let quote = tag[c..].chars().next()?;
        let rest = &tag[c + 1..];
        Some(rest[..rest.find(quote)?].to_string())
    });
    let title = og.or_else(|| {
        let s = lower.find("<title")?;
        let s = s + lower[s..].find('>')? + 1;
        let e = s + lower[s..].find("</title>")?;
        Some(html[s..e].to_string())
    });
    let t = title.unwrap_or_default();
    let t = t.replace("&amp;", "&").replace("&#39;", "'").replace("&quot;", "\"");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = [" | ", " – ", " - ", " — "].iter().fold(t.clone(), |acc, sep| match acc.rsplit_once(sep) { Some((head, _)) if !head.is_empty() => head.to_string(), _ => acc });
    t.chars().take(120).collect()
}

/// Crude HTML → visible text: drop script/style bodies, strip tags, collapse
/// whitespace, cap at `max` chars. Good enough for an LLM to read a shop page
/// without shipping half a megabyte of markup.
pub(crate) fn page_text(html: &str, max: usize) -> String {
    let mut out = String::with_capacity(html.len().min(max));
    // Remove <script>…</script> and <style>…</style> wholesale (case-insensitive).
    let lower = html.to_lowercase();
    let mut cut: Vec<(usize, usize)> = Vec::new();
    for tag in ["script", "style", "noscript", "svg"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut from = 0;
        while let Some(s) = lower[from..].find(&open).map(|i| i + from) {
            match lower[s..].find(&close).map(|i| i + s + close.len()) {
                Some(e) => {
                    cut.push((s, e));
                    from = e;
                }
                None => break,
            }
        }
    }
    cut.sort_unstable();
    let mut pos = 0;
    let mut kept = String::with_capacity(html.len());
    for (s, e) in cut {
        if s > pos {
            kept.push_str(&html[pos..s]);
        }
        pos = pos.max(e);
    }
    kept.push_str(&html[pos..]);

    // Strip remaining tags; a '>'-less trailing tag just ends the document.
    let mut in_tag = false;
    let mut last_space = true;
    for c in kept.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                // Tag boundaries separate words ("</td><td>").
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            _ if in_tag => {}
            c if c.is_whitespace() => {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            c => {
                out.push(c);
                last_space = false;
            }
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Best-effort JSON extraction from an LLM answer (tolerates code fences/prose).
fn parse_json_loose(text: &str) -> Value {
    if let Ok(v) = serde_json::from_str(text.trim()) {
        return v;
    }
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}')) {
        if e > s {
            if let Ok(v) = serde_json::from_str(&text[s..=e]) {
                return v;
            }
        }
    }
    Value::Null
}

/// Plain-text Telegram ping (no parse_mode — product names and URLs routinely
/// contain Markdown-hostile characters).
async fn notify(client: &reqwest::Client, token: &str, chat_id: i64, text: &str) {
    if token.is_empty() || chat_id == 0 {
        return;
    }
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let body = json!({ "chat_id": chat_id, "text": text });
    if let Err(e) = client.post(&url).json(&body).send().await {
        tracing::warn!("shopper sendMessage failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_text_strips_markup_and_scripts() {
        let html = "<html><head><style>.a{color:red}</style><script>var x='<b>hi</b>';</script></head>\
                    <body><h1>Widget</h1><p>Out of  stock</p></body></html>";
        let text = page_text(html, 1000);
        assert!(text.contains("Widget"));
        assert!(text.contains("Out of stock"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains("var x"));
    }

    #[test]
    fn parse_json_loose_handles_fences() {
        let v = parse_json_loose("```json\n{\"in_stock\": true, \"product\": \"Widget\"}\n```");
        assert_eq!(v["in_stock"], Value::Bool(true));
        assert_eq!(parse_json_loose("[mock:model] whatever"), Value::Null);
    }

    #[test]
    fn page_title_prefers_og_and_drops_shop_suffix() {
        let html = r#"<head><title>Ignored | Shop</title><meta property="og:title" content="Widget Pro 2 &amp; Case"></head>"#;
        assert_eq!(page_title(html), "Widget Pro 2 & Case");
        assert_eq!(page_title("<title>\n Widget Pro - Big Shop </title>"), "Widget Pro");
        assert_eq!(page_title("<p>no title</p>"), "");
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let shopper = Shopper { db: Db::memory().unwrap() };
        let id = shopper.add(Watch {
            id: String::new(),
            chat_id: 7,
            url: "https://example.com/widget".into(),
            product: String::new(),
            created_at: String::new(),
            last_checked: String::new(),
            last_status: String::new(),
            done: false,
            ..Default::default()
        });
        assert_eq!(shopper.active(7).len(), 1);
        assert_eq!(shopper.active(8).len(), 0);
        assert!(shopper.remove(&id));
        assert!(!shopper.remove(&id));
        assert!(shopper.active(7).is_empty());
    }
}
