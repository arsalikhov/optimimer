use anyhow::{anyhow, Result};
use serde_json::{json, Value};

const API: &str = "https://api.notion.com/v1";
const VERSION: &str = "2022-06-28";

/// Interpolated parameters for one Notion action. Only the fields relevant to
/// `op` are read; the rest are ignored.
#[derive(Default)]
pub struct Op {
    pub op: String,
    pub query: String,
    pub database_id: String,
    pub page_id: String,
    pub block_id: String,
    pub title: String,
    pub title_prop: String,
    pub content: String,
    pub filter_json: String,
    /// A Notion `sorts` array as JSON, applied to query_database, e.g.
    /// `[{"timestamp":"created_time","direction":"descending"}]`. Empty = unsorted.
    pub sort_json: String,
    /// A Notion `properties` object as JSON, merged into create/update calls
    /// (e.g. `{"Category":{"select":{"name":"Sagemesh"}}}`).
    pub properties_json: String,
    /// Relation properties keyed by name to an array of page ids, e.g.
    /// `{"Topics":["id1","id2"],"Areas":["id3"]}` — expanded to Notion's
    /// `{"relation":[{"id":...}]}` shape. Empty arrays are skipped.
    pub relations_json: String,
}

/// Query a database and return the RAW page objects (full `properties`), paging
/// through every result. Unlike `query_database` (which slims output to
/// id/title/url for cheap LLM feeding), finance balance + CSV dedup need actual
/// property values (Amount, Direction, Category, Date, Key), so they use this.
/// Returns an empty vec when Notion is unconfigured, so callers degrade to "no
/// transactions" rather than erroring.
pub async fn query_raw(
    database_id: &str,
    filter: Option<Value>,
    sorts: Option<Value>,
) -> Result<Vec<Value>> {
    let token = std::env::var("NOTION_TOKEN").unwrap_or_default();
    if token.is_empty() || database_id.is_empty() {
        return Ok(vec![]);
    }
    let client = reqwest::Client::new();
    let url = format!("{API}/databases/{database_id}/query");
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({ "page_size": 100 });
        if let Some(f) = &filter {
            body["filter"] = f.clone();
        }
        if let Some(s) = &sorts {
            body["sorts"] = s.clone();
        }
        if let Some(c) = &cursor {
            body["start_cursor"] = json!(c);
        }
        let resp = post(&client, &token, &url, &body).await?;
        if let Some(arr) = resp["results"].as_array() {
            out.extend(arr.iter().cloned());
        }
        if resp["has_more"].as_bool().unwrap_or(false) {
            match resp["next_cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        } else {
            break;
        }
    }
    Ok(out)
}

/// Execute a Notion action. Returns a slimmed-down JSON value (ids/titles/urls)
/// rather than Notion's verbose payloads, so it stays cheap to feed into an LLM.
/// Without NOTION_TOKEN, Notion is treated as optional: write actions are saved
/// to a local JSON file (see `save_offline`) so the bot is fully usable Notion-free.
pub async fn run(o: Op) -> Result<Value> {
    let token = std::env::var("NOTION_TOKEN").unwrap_or_default();
    if token.is_empty() {
        return save_offline(&o);
    }
    let client = reqwest::Client::new();

    match o.op.as_str() {
        "search" => {
            let body = json!({ "query": o.query, "page_size": 10 });
            let resp = post(&client, &token, &format!("{API}/search"), &body).await?;
            Ok(simplify(&resp["results"]))
        }

        "query_database" => {
            if o.database_id.is_empty() {
                // Notion has a token but no database configured for this action —
                // treat it like the offline path rather than failing the command.
                return save_offline(&o);
            }
            let mut body = json!({ "page_size": 100 });
            if !o.filter_json.trim().is_empty() {
                let filter: Value = serde_json::from_str(&o.filter_json)
                    .map_err(|e| anyhow!("filter_json is not valid JSON: {e}"))?;
                body["filter"] = filter;
            }
            if !o.sort_json.trim().is_empty() {
                let sorts: Value = serde_json::from_str(&o.sort_json)
                    .map_err(|e| anyhow!("sort_json is not valid JSON: {e}"))?;
                body["sorts"] = sorts;
            }
            let url = format!("{API}/databases/{}/query", o.database_id);
            let resp = post(&client, &token, &url, &body).await?;
            Ok(simplify(&resp["results"]))
        }

        "create_page" => {
            if o.database_id.is_empty() {
                // No target database configured — save the page to disk instead
                // of erroring, so capture still works without Notion setup.
                return save_offline(&o);
            }
            let title_prop = if o.title_prop.is_empty() { "Name" } else { &o.title_prop };
            let children: Vec<Value> = o
                .content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(paragraph)
                .collect();
            // Start from the title property, then merge any extra properties
            // (Category/Type/Date/Status…) supplied as JSON.
            let mut properties = json!({
                title_prop: { "title": [{ "text": { "content": o.title } }] }
            });
            merge_properties(&mut properties, &o.properties_json)?;
            merge_relations(&mut properties, &o.relations_json)?;
            let body = json!({
                "parent": { "database_id": o.database_id },
                "properties": properties,
                "children": children
            });
            let resp = post(&client, &token, &format!("{API}/pages"), &body).await?;
            Ok(json!({ "id": resp["id"], "url": resp["url"], "title": o.title }))
        }

        "update_page" => {
            if o.page_id.is_empty() {
                return Err(anyhow!("update_page needs a page_id"));
            }
            let mut properties = json!({});
            merge_properties(&mut properties, &o.properties_json)?;
            if !o.title.is_empty() {
                let title_prop = if o.title_prop.is_empty() { "Name" } else { &o.title_prop };
                properties[title_prop] = json!({ "title": [{ "text": { "content": o.title } }] });
            }
            let body = json!({ "properties": properties });
            let resp = patch(&client, &token, &format!("{API}/pages/{}", o.page_id), &body).await?;
            Ok(json!({ "ok": true, "id": resp["id"], "url": resp["url"] }))
        }

        "append" => {
            let target = if !o.block_id.is_empty() { &o.block_id } else { &o.page_id };
            if target.is_empty() {
                return Err(anyhow!("append needs a block_id or page_id"));
            }
            let children: Vec<Value> = o
                .content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(paragraph)
                .collect();
            let body = json!({ "children": children });
            let url = format!("{API}/blocks/{target}/children");
            let resp = patch(&client, &token, &url, &body).await?;
            Ok(json!({ "ok": true, "appended": children_len(&resp) }))
        }

        "get_page" => {
            if o.page_id.is_empty() {
                return Err(anyhow!("get_page needs a page_id"));
            }
            let resp = get(&client, &token, &format!("{API}/pages/{}", o.page_id)).await?;
            Ok(json!({
                "id": resp["id"],
                "url": resp["url"],
                "title": extract_title(&resp),
                "properties": resp["properties"]
            }))
        }

        other => Err(anyhow!("unknown Notion action: {other}")),
    }
}

/// Notion fallback used when Notion isn't fully configured — either NOTION_TOKEN
/// is unset, or a write/read targets a database whose id env var is empty. Write
/// actions (create/update/append) are persisted as a JSON file on disk instead of
/// hitting the API, so a Raspberry Pi deployment works with no Notion setup at all.
/// Read actions have nothing to persist offline and return an empty, labelled result.
///
/// The output directory is `OPTIMIMER_NOTION_FALLBACK_DIR` (default `notion-out`),
/// resolved relative to the backend's working directory (e.g. `/opt/optimimer`).
fn save_offline(o: &Op) -> Result<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};

    if matches!(o.op.as_str(), "search" | "query_database" | "get_page") {
        return Ok(json!({
            "offline": true,
            "op": o.op,
            "count": 0,
            "results": [],
            "note": "Notion not configured (token or database id missing) — reads are unavailable; writes are saved to disk."
        }));
    }

    let dir = std::env::var("OPTIMIMER_NOTION_FALLBACK_DIR")
        .unwrap_or_else(|_| "notion-out".to_string());
    std::fs::create_dir_all(&dir).map_err(|e| anyhow!("cannot create '{dir}': {e}"))?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let properties: Value = serde_json::from_str(o.properties_json.trim()).unwrap_or(Value::Null);
    let relations: Value = serde_json::from_str(o.relations_json.trim()).unwrap_or(Value::Null);
    let record = json!({
        "op": o.op,
        "title": o.title,
        "content": o.content,
        "database_id": o.database_id,
        "page_id": o.page_id,
        "properties": properties,
        "relations": relations,
        "saved_at_ms": stamp,
    });

    let slug: String = o
        .title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').chars().take(40).collect::<String>();
    let slug = if slug.is_empty() { "item".to_string() } else { slug };
    let path = std::path::Path::new(&dir).join(format!("{stamp}-{}-{slug}.json", o.op));
    std::fs::write(&path, serde_json::to_vec_pretty(&record)?)
        .map_err(|e| anyhow!("cannot write '{}': {e}", path.display()))?;

    Ok(json!({
        "saved": true,
        "op": o.op,
        "title": o.title,
        "path": path.display().to_string(),
        "note": "Saved to disk — Notion not fully configured (token or database id missing), so this was written locally."
    }))
}

/// Merge a JSON object of Notion properties into `target`. Empty/blank input is
/// ignored; non-object JSON is an error so misconfigured agents fail loudly.
fn merge_properties(target: &mut Value, props_json: &str) -> Result<()> {
    if props_json.trim().is_empty() {
        return Ok(());
    }
    let extra: Value = serde_json::from_str(props_json)
        .map_err(|e| anyhow!("properties_json is not valid JSON: {e}"))?;
    match extra {
        Value::Object(map) => {
            for (k, v) in map {
                target[k] = v;
            }
            Ok(())
        }
        _ => Err(anyhow!("properties_json must be a JSON object")),
    }
}

/// Expand `{"Prop":["id1","id2"]}` into Notion relation properties on `target`.
/// Empty arrays are skipped so an unmatched relation is simply left unset.
fn merge_relations(target: &mut Value, relations_json: &str) -> Result<()> {
    if relations_json.trim().is_empty() {
        return Ok(());
    }
    let extra: Value = serde_json::from_str(relations_json)
        .map_err(|e| anyhow!("relations_json is not valid JSON: {e}"))?;
    if let Value::Object(map) = extra {
        for (prop, ids) in map {
            let rel: Vec<Value> = ids
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str())
                .map(|id| json!({ "id": id }))
                .collect();
            if !rel.is_empty() {
                target[prop] = json!({ "relation": rel });
            }
        }
    }
    Ok(())
}

fn paragraph(text: &str) -> Value {
    json!({
        "object": "block",
        "type": "paragraph",
        "paragraph": { "rich_text": [{ "type": "text", "text": { "content": text } }] }
    })
}

fn children_len(resp: &Value) -> usize {
    resp["results"].as_array().map(|a| a.len()).unwrap_or(0)
}

/// Pull the title text out of a page or database object, regardless of which
/// property holds it.
fn extract_title(obj: &Value) -> String {
    if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
        for v in props.values() {
            if v.get("type").and_then(|t| t.as_str()) == Some("title") {
                return rich_text_to_string(v.get("title"));
            }
        }
    }
    rich_text_to_string(obj.get("title"))
}

fn rich_text_to_string(v: Option<&Value>) -> String {
    v.and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("plain_text").and_then(|s| s.as_str()))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Reduce a results array to {id, title, url, object} entries.
fn simplify(results: &Value) -> Value {
    let items: Vec<Value> = results
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|o| {
                    json!({
                        "id": o.get("id"),
                        "title": extract_title(o),
                        "url": o.get("url"),
                        "created_time": o.get("created_time"),
                        "object": o.get("object")
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({ "count": items.len(), "results": items })
}

async fn post(client: &reqwest::Client, token: &str, url: &str, body: &Value) -> Result<Value> {
    check(client.post(url).bearer_auth(token).header("Notion-Version", VERSION).json(body).send().await?).await
}

async fn patch(client: &reqwest::Client, token: &str, url: &str, body: &Value) -> Result<Value> {
    check(client.patch(url).bearer_auth(token).header("Notion-Version", VERSION).json(body).send().await?).await
}

async fn get(client: &reqwest::Client, token: &str, url: &str) -> Result<Value> {
    check(client.get(url).bearer_auth(token).header("Notion-Version", VERSION).send().await?).await
}

async fn check(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body["message"].as_str().unwrap_or("").to_string();
        return Err(anyhow!("Notion {}: {}", status, if msg.is_empty() { body.to_string() } else { msg }));
    }
    Ok(body)
}
