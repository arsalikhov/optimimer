//! Web access for the agent: `search` (DuckDuckGo's HTML endpoint by default —
//! no key, no cost — or Brave Search when BRAVE_API_KEY is set) and
//! `read_page` (fetch a URL and reduce it to readable text). Results are kept
//! small on purpose: the model sees a handful of hits, then reads one page.

use anyhow::{anyhow, Result};
use serde_json::Value;

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36 Optimimer/0.1";

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

/// Top `n` results for `query`.
pub async fn search(query: &str, n: usize) -> Result<Vec<Hit>> {
    let query = query.trim();
    if query.is_empty() {
        return Err(anyhow!("empty query"));
    }
    match std::env::var("BRAVE_API_KEY") {
        Ok(k) if !k.trim().is_empty() => brave(query, n, k.trim()).await,
        _ => duckduckgo(query, n).await,
    }
}

async fn brave(query: &str, n: usize, key: &str) -> Result<Vec<Hit>> {
    let body: Value = client()
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", &n.to_string())])
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let hits = body["web"]["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(Hit {
                        title: r["title"].as_str()?.to_string(),
                        url: r["url"].as_str()?.to_string(),
                        snippet: strip_tags(r["description"].as_str().unwrap_or("")),
                    })
                })
                .take(n)
                .collect()
        })
        .unwrap_or_default();
    Ok(hits)
}

async fn duckduckgo(query: &str, n: usize) -> Result<Vec<Hit>> {
    let resp = client()
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .header("Accept", "text/html")
        .send()
        .await?;
    let status = resp.status();
    let html = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("search returned HTTP {status}"));
    }
    let hits = parse_duckduckgo(&html, n);
    if hits.is_empty() && html.to_lowercase().contains("anomaly") {
        return Err(anyhow!("search is rate-limiting us right now — try again in a minute (or set BRAVE_API_KEY)"));
    }
    Ok(hits)
}

/// Pull (title, url, snippet) triples out of DuckDuckGo's HTML. Links are
/// redirect URLs carrying the real target in the `uddg` parameter.
pub(crate) fn parse_duckduckgo(html: &str, n: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut rest = html;
    while hits.len() < n {
        let Some(a) = rest.find("class=\"result__a\"") else { break };
        // href sits just before the class attribute in DDG's markup; search backwards to the tag start.
        let tag_start = rest[..a].rfind("<a ").unwrap_or(0);
        let tag = &rest[tag_start..];
        let Some(tag_end) = tag.find('>') else { break };
        let open = &tag[..tag_end];
        let href = attr(open, "href").unwrap_or_default();
        let after = &tag[tag_end + 1..];
        let Some(close) = after.find("</a>") else { break };
        let title = strip_tags(&after[..close]);
        let mut snippet = String::new();
        if let Some(s) = after.find("result__snippet") {
            if let Some(gt) = after[s..].find('>') {
                let body = &after[s + gt + 1..];
                if let Some(end) = body.find("</a>").or_else(|| body.find("</td>")).or_else(|| body.find("</div>")) {
                    snippet = strip_tags(&body[..end]);
                }
            }
        }
        rest = &after[close + 4..];
        let url = real_url(&href);
        if url.is_empty() || url.contains("duckduckgo.com/y.js") {
            continue;
        }
        hits.push(Hit { title, url, snippet });
    }
    hits
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=\"");
    let i = tag.find(&pat)? + pat.len();
    let j = tag[i..].find('"')?;
    Some(html_unescape(&tag[i..i + j]))
}

/// `//duckduckgo.com/l/?uddg=https%3A%2F%2F…&rut=…` → the decoded target.
fn real_url(href: &str) -> String {
    let full = if href.starts_with("//") { format!("https:{href}") } else { href.to_string() };
    if let Ok(u) = url::Url::parse(&full) {
        if u.host_str().map(|h| h.ends_with("duckduckgo.com")).unwrap_or(false) {
            if let Some((_, v)) = u.query_pairs().find(|(k, _)| k == "uddg") {
                return v.into_owned();
            }
        }
        return u.to_string();
    }
    String::new()
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#x27;", "'").replace("&#39;", "'").replace("&nbsp;", " ")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    html_unescape(&out).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Fetch a page and return its readable text, capped at `max` characters.
pub async fn read_page(url: &str, max: usize) -> Result<String> {
    let url = url.trim();
    let parsed = url::Url::parse(url).map_err(|_| anyhow!("that doesn't look like a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow!("only http(s) URLs can be read"));
    }
    let resp = client().get(parsed).header("Accept", "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5").send().await?;
    let status = resp.status();
    let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("page returned HTTP {status}"));
    }
    let text = if ctype.contains("html") || body.trim_start().starts_with('<') {
        crate::shopper::page_text(&body, max)
    } else {
        body.chars().take(max).collect()
    };
    if text.trim().is_empty() {
        return Err(anyhow!("the page had no readable text (maybe it needs JavaScript)"));
    }
    Ok(text)
}

/// Compact rendering for the model.
pub fn render(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "No results.".into();
    }
    hits.iter()
        .enumerate()
        .map(|(i, h)| {
            let snip = if h.snippet.is_empty() { String::new() } else { format!("\n   {}", truncate(&h.snippet, 220)) };
            format!("{}. {} — {}{}", i + 1, h.title, h.url, snip)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duckduckgo_markup() {
        let html = r#"<div class="result"><h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc">Example <b>Page</b></a></h2>
        <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc">A <b>snippet</b> here.</a></div>
        <div class="result"><a class="result__a" href="https://other.org/x">Other</a></div>"#;
        let hits = parse_duckduckgo(html, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], Hit { title: "Example Page".into(), url: "https://example.com/page".into(), snippet: "A snippet here.".into() });
        assert_eq!(hits[1].url, "https://other.org/x");
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Network test — run with `cargo test web::live -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn search_and_read_live() {
        let hits = search("Raspberry Pi 5 release date", 5).await.expect("search");
        assert!(!hits.is_empty(), "no hits");
        println!("{}", render(&hits));
        let text = read_page(&hits[0].url, 800).await.expect("read");
        println!("--- page ---\n{text}");
        assert!(text.len() > 100);
    }
}
