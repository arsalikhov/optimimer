use crate::models::{NodeResult, Node, RunResponse, Workflow};
use crate::openrouter;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

/// Runtime context: the resolved output of every node that has executed,
/// keyed by node id, plus the initial trigger `input`.
struct Ctx {
    outputs: HashMap<String, Value>,
    input: Value,
}

impl Ctx {
    /// Replace `{{...}}` references in a string.
    /// Supported: `{{input}}`, `{{input.field}}`, `{{nodeId}}`, `{{nodeId.field}}`.
    /// Walks the string by `{{`/`}}` markers (both ASCII) and copies the spans
    /// between them verbatim, so multi-byte UTF-8 literals (emoji, dashes) survive.
    fn interpolate(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after.find("}}") {
                Some(end) => {
                    out.push_str(&self.resolve(after[..end].trim()));
                    rest = &after[end + 2..];
                }
                None => {
                    // Unbalanced `{{` — emit the remainder literally.
                    out.push_str(&rest[start..]);
                    return out;
                }
            }
        }
        out.push_str(rest);
        out
    }

    fn resolve(&self, expr: &str) -> String {
        // `{{html:expr}}` resolves `expr` then HTML-escapes it, so dynamic values
        // (e.g. a Notion title containing & or <) are safe inside rich-message HTML.
        if let Some(inner) = expr.strip_prefix("html:") {
            return html_escape(&self.resolve(inner.trim()));
        }
        // `{{num:expr}}` resolves `expr` as a number, falling back to 0 when it's
        // missing/blank/non-numeric — so interpolating it into a JSON `number`
        // field (e.g. an LLM that omitted an amount) can't produce invalid JSON.
        if let Some(inner) = expr.strip_prefix("num:") {
            let v = self.resolve(inner.trim());
            return v.trim().parse::<f64>().map(|n| n.to_string()).unwrap_or_else(|_| "0".to_string());
        }
        let (head, rest) = match expr.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (expr, None),
        };
        // `{{env.NAME}}` reads a process env var — lets agents reference IDs and
        // secrets (e.g. {{env.LIFEOS_DB_ID}}) without baking them into saved JSON.
        if head == "env" {
            return rest.and_then(|name| std::env::var(name).ok()).unwrap_or_default();
        }
        let root = if head == "input" {
            &self.input
        } else {
            match self.outputs.get(head) {
                Some(v) => v,
                None => return String::new(),
            }
        };
        let mut cur = root;
        if let Some(path) = rest {
            for part in path.split('.') {
                cur = &cur[part];
            }
        }
        match cur {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }
}

/// Escape the five characters that are significant in HTML / Telegram rich
/// messages, so interpolated dynamic text can't break the markup.
pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Execute the workflow. Nodes run in topological order starting from the
/// trigger; condition nodes gate which downstream edges are followed.
pub async fn run(wf: &Workflow, input: Value) -> RunResponse {
    let nodes: HashMap<&str, &Node> = wf.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    // adjacency: source -> [(target, sourceHandle)]
    let mut adj: HashMap<&str, Vec<(&str, Option<&str>)>> = HashMap::new();
    let mut indeg: HashMap<&str, usize> = wf.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    for e in &wf.edges {
        adj.entry(e.source.as_str())
            .or_default()
            .push((e.target.as_str(), e.source_handle.as_deref()));
        *indeg.entry(e.target.as_str()).or_insert(0) += 1;
    }

    let mut ctx = Ctx {
        outputs: HashMap::new(),
        input,
    };
    let mut results = Vec::new();

    // Kahn's algorithm, but a node only "enables" its successors along the
    // handles its execution selected (used by condition nodes).
    let mut queue: VecDeque<&str> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&id, _)| id)
        .collect();
    let mut active: HashSet<&str> = queue.iter().copied().collect();
    let mut overall_ok = true;

    while let Some(id) = queue.pop_front() {
        let node = match nodes.get(id) {
            Some(n) => *n,
            None => continue,
        };

        let started = Instant::now();
        let exec = if active.contains(id) {
            execute_node(node, &ctx).await
        } else {
            Ok((json!(null), vec![])) // skipped: parents pruned this branch
        };

        let (status, output, error, allowed_handles): (&str, Value, Option<String>, Vec<String>) =
            match exec {
                Ok((out, handles)) if active.contains(id) => {
                    ctx.outputs.insert(id.to_string(), out.clone());
                    ("ok", out, None, handles)
                }
                Ok(_) => ("skipped", json!(null), None, vec![]),
                Err(e) => {
                    overall_ok = false;
                    ("error", json!(null), Some(e.to_string()), vec![])
                }
            };

        results.push(NodeResult {
            node_id: id.to_string(),
            node_type: node.node_type.clone(),
            status: status.to_string(),
            output,
            error,
            ms: started.elapsed().as_millis(),
        });

        // relax successors
        if let Some(succs) = adj.get(id) {
            for (target, handle) in succs {
                let follow = status == "ok"
                    && match (handle, allowed_handles.is_empty()) {
                        (_, true) => true,                       // node selects all handles
                        (Some(h), false) => allowed_handles.iter().any(|a| a == h),
                        (None, false) => true,
                    };
                if follow {
                    active.insert(target);
                }
                if let Some(d) = indeg.get_mut(target) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(target);
                    }
                }
            }
        }
    }

    RunResponse {
        status: if overall_ok { "ok".into() } else { "error".into() },
        results,
    }
}

/// Run a single node. Returns its output value plus the list of source handles
/// that downstream edges may follow (empty = follow all).
async fn execute_node(node: &Node, ctx: &Ctx) -> anyhow::Result<(Value, Vec<String>)> {
    let d = &node.data;
    let field = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();

    match node.node_type.as_str() {
        "trigger" => Ok((ctx.input.clone(), vec![])),

        "llm" => {
            let model = {
                let m = field("model");
                if m.is_empty() {
                    "nvidia/nemotron-3-super-120b-a12b".to_string()
                } else {
                    m
                }
            };
            let system = ctx.interpolate(&field("system"));
            let prompt = ctx.interpolate(&field("prompt"));
            let text = openrouter::chat(&model, &system, &prompt).await?;
            // If the model returned JSON (optionally fenced), expose it as
            // `{{id.json.field}}` so downstream nodes can dig into structured output.
            let parsed = parse_json_loose(&text);
            Ok((json!({ "text": text, "json": parsed }), vec![]))
        }

        "http" => {
            let method = {
                let m = field("method").to_uppercase();
                if m.is_empty() { "GET".to_string() } else { m }
            };
            let url = ctx.interpolate(&field("url"));
            let body = ctx.interpolate(&field("body"));
            let client = reqwest::Client::new();
            let mut req = client.request(method.parse().unwrap_or(reqwest::Method::GET), &url);
            if !body.trim().is_empty() {
                req = req
                    .header("content-type", "application/json")
                    .body(body);
            }
            // Optional `headers_json`: a JSON object of extra request headers
            // (e.g. {"Authorization": "Bearer {{env.RESEND_API_KEY}}"}).
            let headers_raw = ctx.interpolate(&field("headers_json"));
            if !headers_raw.trim().is_empty() {
                if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&headers_raw) {
                    for (k, v) in map {
                        if let Some(s) = v.as_str() {
                            req = req.header(k, s);
                        }
                    }
                }
            }
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
            Ok((json!({ "status": status, "body": parsed }), vec![]))
        }

        "condition" => {
            // Compare interpolated `left` against `right` using `op`.
            let left = ctx.interpolate(&field("left"));
            let right = ctx.interpolate(&field("right"));
            let op = field("op");
            let pass = match op.as_str() {
                "ne" => left != right,
                "contains" => left.contains(&right),
                "gt" => left.parse::<f64>().ok().zip(right.parse::<f64>().ok()).map_or(false, |(a, b)| a > b),
                "lt" => left.parse::<f64>().ok().zip(right.parse::<f64>().ok()).map_or(false, |(a, b)| a < b),
                _ => left == right, // "eq" / default
            };
            let handle = if pass { "true" } else { "false" };
            Ok((json!({ "pass": pass }), vec![handle.to_string()]))
        }

        "notion" => {
            // `create_pages` fans out one Notion page per item in an LLM-produced
            // array, scheduling a clear for any item that carries a `clear_at`.
            if field("op") == "create_pages" {
                let cfg = CreatePagesCfg {
                    items_raw: ctx.interpolate(&field("items_json")),
                    db: ctx.interpolate(&field("database_id")),
                    chat_id: ctx.interpolate(&field("chat_id")),
                    tz: ctx.interpolate(&field("tz")),
                    title_prop: { let t = field("title_prop"); if t.is_empty() { "Name".into() } else { t } },
                    date_prop: { let p = field("date_prop"); if p.is_empty() { "Date".into() } else { p } },
                    default_hour: ctx.interpolate(&field("default_hour")).parse().unwrap_or(9),
                };
                return Ok((create_pages(cfg).await?, vec![]));
            }
            let op = crate::notion::Op {
                op: field("op"),
                query: ctx.interpolate(&field("query")),
                database_id: ctx.interpolate(&field("database_id")),
                page_id: ctx.interpolate(&field("page_id")),
                block_id: ctx.interpolate(&field("block_id")),
                title: ctx.interpolate(&field("title")),
                title_prop: field("title_prop"),
                content: ctx.interpolate(&field("content")),
                filter_json: ctx.interpolate(&field("filter_json")),
                sort_json: ctx.interpolate(&field("sort_json")),
                properties_json: ctx.interpolate(&field("properties_json")),
                relations_json: ctx.interpolate(&field("relations_json")),
            };
            let out = crate::notion::run(op).await?;
            Ok((out, vec![]))
        }

        "datetime" => {
            // Resolve an LLM "when" token object into RFC3339 — all date math in code.
            let spec: Value = serde_json::from_str(&ctx.interpolate(&field("spec"))).unwrap_or(json!({}));
            let tz = ctx.interpolate(&field("tz"));
            let default_hour = ctx.interpolate(&field("default_hour")).parse().unwrap_or(9);
            let duration = ctx.interpolate(&field("duration_minutes")).parse().unwrap_or(60);
            match crate::datetime::resolve(&spec, &tz, default_hour, duration) {
                Some(r) => Ok((json!({ "rfc3339": r.start, "rfc3339_end": r.end, "clear": r.clear, "far": r.far, "human": r.human, "human_end": r.human_end }), vec![])),
                None => Ok((json!({ "rfc3339": "", "rfc3339_end": "", "clear": "", "far": false, "human": "", "human_end": "" }), vec![])),
            }
        }

        "schedule" => {
            // Register a future action (Telegram ping and/or Notion update) with
            // the global scheduler. An empty `fire_at` is a no-op, so the node can
            // sit on a path that only sometimes needs scheduling (e.g. meetings).
            let fire_at = ctx.interpolate(&field("fire_at"));
            if fire_at.trim().is_empty() {
                return Ok((json!({ "scheduled": false }), vec![]));
            }
            let entry = crate::scheduler::Schedule {
                id: String::new(), // assigned on add
                fire_at: fire_at.trim().to_string(),
                chat_id: ctx.interpolate(&field("chat_id")).parse().unwrap_or(0),
                message: ctx.interpolate(&field("message")),
                page_id: ctx.interpolate(&field("page_id")),
                properties_json: ctx.interpolate(&field("properties_json")),
                done: false,
            };
            let id = crate::scheduler::global().add(entry);
            Ok((json!({ "scheduled": true, "id": id }), vec![]))
        }

        "output" => {
            // Echoes its interpolated value; the visible "result" of a run.
            let value = ctx.interpolate(&field("value"));
            Ok((json!({ "value": value }), vec![]))
        }

        other => Err(anyhow::anyhow!("unknown node type: {}", other)),
    }
}

struct CreatePagesCfg {
    items_raw: String,
    db: String,
    chat_id: String,
    tz: String,
    title_prop: String,
    date_prop: String,
    default_hour: u32,
}

/// Create one Notion page per item in `items_json` (a JSON array of
/// `{title, properties, notes, when, duration_minutes}`). The item's `when` token
/// object is resolved in code into the `date_prop` date (start, plus end when the
/// item's Type is "Meeting"); meetings also schedule a clear (check ` Complete` +
/// Status=Done) one hour after they start. Returns `{count, results, summary}`.
async fn create_pages(cfg: CreatePagesCfg) -> anyhow::Result<Value> {
    const CLEAR_PROPS: &str = "{\" Complete\":{\"checkbox\":true},\"Status\":{\"status\":{\"name\":\"Done\"}}}";
    let items: Vec<Value> = serde_json::from_str(&cfg.items_raw).unwrap_or_default();
    let mut results = Vec::new();
    for it in &items {
        let mut props = it.get("properties").cloned().unwrap_or_else(|| json!({}));
        let is_meeting = props.pointer("/Type/select/name").and_then(|v| v.as_str()) == Some("Meeting");
        let duration = it.get("duration_minutes").and_then(|v| v.as_i64()).unwrap_or(if is_meeting { 60 } else { 0 });
        let resolved = it
            .get("when")
            .filter(|w| w.is_object())
            .and_then(|w| crate::datetime::resolve(w, &cfg.tz, cfg.default_hour, duration));

        if let Some(r) = &resolved {
            if !r.start.is_empty() {
                props[&cfg.date_prop] = if is_meeting {
                    json!({ "date": { "start": r.start, "end": r.end } })
                } else {
                    json!({ "date": { "start": r.start } })
                };
            }
        }

        let op = crate::notion::Op {
            op: "create_page".into(),
            database_id: cfg.db.clone(),
            title: it.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            title_prop: cfg.title_prop.clone(),
            content: it.get("notes").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            properties_json: props.to_string(),
            ..Default::default()
        };
        let created = crate::notion::run(op).await?;
        if let (true, Some(r), Some(id)) = (is_meeting, &resolved, created.get("id").and_then(|v| v.as_str())) {
            crate::scheduler::global().add(crate::scheduler::Schedule {
                id: String::new(),
                fire_at: r.clear.clone(),
                chat_id: cfg.chat_id.parse().unwrap_or(0),
                message: String::new(),
                page_id: id.to_string(),
                properties_json: CLEAR_PROPS.to_string(),
                done: false,
            });
        }
        results.push(created);
    }
    let summary = results
        .iter()
        .map(|r| format!("• {} {}", r.get("title").and_then(|v| v.as_str()).unwrap_or("item"), r.get("url").and_then(|v| v.as_str()).unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({ "count": results.len(), "results": results, "summary": summary.trim_end() }))
}

/// Best-effort parse of LLM output into JSON. Strips a leading/trailing Markdown
/// code fence (```json … ```) and falls back to the first `{…}`/`[…]` span.
/// Returns `Value::Null` when nothing parses.
fn parse_json_loose(text: &str) -> Value {
    let mut s = text.trim();
    if let Some(rest) = s.strip_prefix("```") {
        // drop an optional language tag on the first line, then the closing fence
        let rest = rest.splitn(2, '\n').nth(1).unwrap_or(rest);
        s = rest.strip_suffix("```").unwrap_or(rest).trim();
    }
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return v;
    }
    // Fallback: grab the outermost JSON object/array span.
    if let (Some(start), Some(end)) = (s.find(['{', '[']), s.rfind(['}', ']'])) {
        if end > start {
            if let Ok(v) = serde_json::from_str::<Value>(&s[start..=end]) {
                return v;
            }
        }
    }
    Value::Null
}
