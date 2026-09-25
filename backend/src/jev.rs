//! Minimal client for TypeSafe AI's **Jev** model (`POST /v1/systemone`).
//!
//! Jev never writes text: a request carries a `state` (a string or JSON) and a
//! map of typed questions — `noul` (yes/no probability), `choice` (one of up to
//! 255 named options) and `score` (a level on a 2–10 step rubric) — and the
//! answers come back as calibrated probabilities. That makes it the right tool
//! for the decisions this app used to ask an LLM to spell out as JSON (which
//! task did you mean, is this in stock, which category, memo or command) and
//! for the keyword heuristics that guessed them.
//!
//! Optional, like the paid tier: with `JEV_API_KEY` unset `enabled()` is false,
//! every helper returns `None`, and each caller keeps its previous behaviour.
//! Failures only log — a decision Jev can't make falls back the same way.
//!
//! Env: `JEV_API_KEY`, `JEV_MODEL` (default `jev-latest`; pin a versioned id
//! such as `jev-1.13.0` once thresholds matter), `JEV_BASE_URL` (default
//! `https://api.typesafe.ai`).
//!
//! OpenRouter also serves the model, which saves a second account: set
//! `JEV_BASE_URL=https://openrouter.ai/api`, `JEV_MODEL=typesafe/jev-1.13` and
//! the OpenRouter key. `/v1/systemone` is the same route there and takes this
//! body unchanged; note the model is unlisted in OpenRouter's `/models`
//! catalogue, so look it up by id rather than expecting to find it there.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

const DEFAULT_BASE: &str = "https://api.typesafe.ai";
const DEFAULT_MODEL: &str = "jev-latest";
/// Jev answers in well under a second; anything slower is a stuck request.
const TIMEOUT: Duration = Duration::from_secs(15);
/// The API allows at most this many options in one choice.
pub const MAX_OPTIONS: usize = 255;

fn key() -> String {
    std::env::var("JEV_API_KEY").unwrap_or_default().trim().to_string()
}

fn model() -> String {
    std::env::var("JEV_MODEL").ok().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).unwrap_or_else(|| DEFAULT_MODEL.into())
}

fn base_url() -> String {
    std::env::var("JEV_BASE_URL").ok().map(|b| b.trim().trim_end_matches('/').to_string()).filter(|b| !b.is_empty()).unwrap_or_else(|| DEFAULT_BASE.into())
}

/// Is Jev configured? Callers skip building a request when it isn't.
pub fn enabled() -> bool {
    !key().is_empty()
}

/// One typed question. The question id is never shown to the model, so the
/// instructions must carry the whole meaning.
#[derive(Clone, Debug)]
pub enum Q {
    /// Yes/no. `when_true` / `when_false` optionally spell out what each side means.
    Noul { instructions: String, when_true: String, when_false: String },
    /// One of the named options; a description may be empty when the name says enough.
    /// Include a way out ("none", "unclear") — without one the model must pick
    /// something even when nothing fits.
    Choice { instructions: String, options: Vec<(String, String)> },
    /// A level on an ordered rubric, lowest first (2 to 10 levels).
    Score { instructions: String, levels: Vec<String> },
}

impl Q {
    pub fn noul(instructions: impl Into<String>, when_true: impl Into<String>, when_false: impl Into<String>) -> Self {
        Q::Noul { instructions: instructions.into(), when_true: when_true.into(), when_false: when_false.into() }
    }

    pub fn choice<N: Into<String>, D: Into<String>>(instructions: impl Into<String>, options: impl IntoIterator<Item = (N, D)>) -> Self {
        Q::Choice { instructions: instructions.into(), options: options.into_iter().map(|(n, d)| (n.into(), d.into())).collect() }
    }

    fn to_json(&self) -> Value {
        match self {
            Q::Noul { instructions, when_true, when_false } => {
                let mut q = json!({ "type": "noul", "instructions": instructions });
                let mut criteria = Map::new();
                if !when_true.trim().is_empty() {
                    criteria.insert("true".into(), json!(when_true));
                }
                if !when_false.trim().is_empty() {
                    criteria.insert("false".into(), json!(when_false));
                }
                if !criteria.is_empty() {
                    q["criteria"] = Value::Object(criteria);
                }
                q
            }
            Q::Choice { instructions, options } => {
                let criteria: Map<String, Value> = options
                    .iter()
                    .take(MAX_OPTIONS)
                    .map(|(n, d)| (n.clone(), if d.trim().is_empty() { Value::Null } else { json!(d) }))
                    .collect();
                json!({ "type": "choice", "instructions": instructions, "criteria": criteria })
            }
            Q::Score { instructions, levels } => json!({ "type": "score", "instructions": instructions, "criteria": levels }),
        }
    }
}

/// A choice answer: the winning option and how sure Jev is of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub choice: String,
    pub confidence: f64,
    /// Every option with its probability, most likely first.
    pub probabilities: Vec<(String, f64)>,
}

impl Pick {
    /// The winning option, only if Jev is at least `min` sure of it.
    pub fn confident(&self, min: f64) -> Option<&str> {
        (self.confidence >= min).then_some(self.choice.as_str())
    }

    /// "a 62%, b 30%" — the top `n` options, for logs and review notes.
    pub fn top(&self, n: usize) -> String {
        self.probabilities.iter().take(n).map(|(o, p)| format!("{o} {:.0}%", p * 100.0)).collect::<Vec<_>>().join(", ")
    }
}

/// The answers of one request, keyed by question id.
#[derive(Clone, Debug, Default)]
pub struct Answers(Map<String, Value>);

impl Answers {
    /// Probability that a noul is true.
    pub fn noul(&self, id: &str) -> Option<f64> {
        self.0.get(id)?.get("noul")?.as_f64()
    }

    pub fn choice(&self, id: &str) -> Option<Pick> {
        let a = self.0.get(id)?;
        let choice = a.get("choice")?.as_str()?.to_string();
        let mut probabilities: Vec<(String, f64)> = a
            .get("probabilities")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| v.as_f64().map(|p| (k.clone(), p))).collect())
            .unwrap_or_default();
        probabilities.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
        let confidence = a
            .get("confidence")
            .and_then(Value::as_f64)
            .or_else(|| probabilities.iter().find(|(o, _)| *o == choice).map(|(_, p)| *p))
            .unwrap_or(0.0);
        Some(Pick { choice, confidence, probabilities })
    }

    /// A score's (expected level, confidence).
    pub fn score(&self, id: &str) -> Option<(f64, f64)> {
        let a = self.0.get(id)?;
        Some((a.get("score")?.as_f64()?, a.get("confidence").and_then(Value::as_f64).unwrap_or(0.0)))
    }
}

/// Build the request body. `state` must be a string, object or array — a bare
/// number/bool/null is wrapped as a string, since the API rejects scalars.
fn body(model: &str, state: Value, questions: &[(String, Q)]) -> Value {
    let state = match state {
        Value::String(_) | Value::Object(_) | Value::Array(_) => state,
        Value::Null => json!(""),
        other => json!(other.to_string()),
    };
    let qs: Map<String, Value> = questions.iter().map(|(id, q)| (id.clone(), q.to_json())).collect();
    json!({ "state": state, "model": model, "questions": qs })
}

/// A readable reason from an error body. The API nests it under `detail`, as a
/// string, an `{error_type, message}` object, or a list of validation errors.
fn error_detail(body: &Value) -> String {
    match &body["detail"] {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
        Value::Array(a) => a.iter().filter_map(|e| e["msg"].as_str()).collect::<Vec<_>>().join("; "),
        _ => body.to_string(),
    }
}

/// Ask every question about one `state` in a single request. Errors when Jev
/// isn't configured, so callers normally go through `enabled()` first or use
/// the `noul` / `choice` helpers, which fold failures into `None`.
pub async fn ask(state: Value, questions: Vec<(String, Q)>) -> Result<Answers> {
    let key = key();
    if key.is_empty() {
        return Err(anyhow!("JEV_API_KEY unset"));
    }
    ask_at(&base_url(), &key, &model(), state, &questions).await
}

async fn ask_at(base: &str, key: &str, model: &str, state: Value, questions: &[(String, Q)]) -> Result<Answers> {
    if questions.is_empty() {
        return Ok(Answers::default());
    }
    let url = format!("{base}/v1/systemone");
    let payload = body(model, state, questions);
    let client = reqwest::Client::new();
    // One retry on a rate limit, server error or timeout; a 4xx is our bug.
    let mut attempt = 0;
    loop {
        attempt += 1;
        let sent = client.post(&url).bearer_auth(key).header("Accept", "application/json").json(&payload).timeout(TIMEOUT).send().await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) if attempt < 2 && (e.is_timeout() || e.is_connect()) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            Err(e) => return Err(anyhow!("couldn't reach Jev ({e})")),
        };
        let status = resp.status();
        let json: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            tracing::debug!(
                "jev {} question(s), {} input tokens",
                questions.len(),
                json["usage"]["input_tokens"].as_u64().unwrap_or(0)
            );
            let answers = json["answers"].as_object().cloned().ok_or_else(|| anyhow!("unexpected Jev response: {json}"))?;
            return Ok(Answers(answers));
        }
        if attempt < 2 && (status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()) {
            tokio::time::sleep(Duration::from_millis(800)).await;
            continue;
        }
        let hint = match status.as_u16() {
            401 | 403 => "Jev rejected the API key — check JEV_API_KEY.".to_string(),
            _ => format!("Jev {status}: {}", error_detail(&json)),
        };
        return Err(anyhow!(hint));
    }
}

/// One yes/no question. `None` when Jev is off or the call failed (logged).
pub async fn noul(state: Value, q: Q) -> Option<f64> {
    if !enabled() {
        return None;
    }
    match ask(state, vec![("q".into(), q)]).await {
        Ok(a) => a.noul("q"),
        Err(e) => {
            tracing::warn!("jev noul failed: {e}");
            None
        }
    }
}

/// One choice question. `None` when Jev is off or the call failed (logged).
pub async fn choice(state: Value, q: Q) -> Option<Pick> {
    if !enabled() {
        return None;
    }
    match ask(state, vec![("q".into(), q)]).await {
        Ok(a) => a.choice("q"),
        Err(e) => {
            tracing::warn!("jev choice failed: {e}");
            None
        }
    }
}

/// Parse an option list typed by a person or interpolated from a prompt block:
/// one option per line (`- Name: description` or `Name`), or a single line of
/// comma-separated `Name` / `Name: description` items. Used by the workflow
/// node, where options often come from `{{input.categories}}`.
pub fn parse_options(text: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let items: Vec<&str> = if lines.len() == 1 { lines[0].split(',').map(str::trim).filter(|s| !s.is_empty()).collect() } else { lines };
    let mut out: Vec<(String, String)> = Vec::new();
    for item in items {
        let item = item.trim_start_matches(['-', '*', '•']).trim();
        let (name, desc) = match item.split_once(':') {
            Some((n, d)) => (n.trim(), d.trim()),
            None => (item, ""),
        };
        if !name.is_empty() && !out.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            out.push((name.to_string(), desc.to_string()));
        }
    }
    out.truncate(MAX_OPTIONS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_has_the_documented_wire_shape() {
        let qs = vec![
            ("urgent".to_string(), Q::noul("Does this convey urgency?", "time-sensitive", "")),
            ("team".to_string(), Q::choice("Which team?", [("billing", "Payments"), ("other", "")])),
            ("mood".to_string(), Q::Score { instructions: "How upset?".into(), levels: vec!["calm".into(), "angry".into()] }),
        ];
        let b = body("jev-latest", json!("help!"), &qs);
        assert_eq!(b["state"], "help!");
        assert_eq!(b["model"], "jev-latest");
        assert_eq!(b["questions"]["urgent"], json!({ "type": "noul", "instructions": "Does this convey urgency?", "criteria": { "true": "time-sensitive" } }));
        assert_eq!(b["questions"]["team"]["criteria"], json!({ "billing": "Payments", "other": null }));
        assert_eq!(b["questions"]["mood"]["criteria"], json!(["calm", "angry"]));
        assert_eq!(body("m", json!(3), &qs)["state"], "3", "scalars are wrapped");
    }

    #[test]
    fn answers_parse_tolerantly() {
        let a = Answers(
            json!({
                "u": { "type": "noul", "noul": 0.92 },
                "t": { "type": "choice", "choice": "billing", "probabilities": { "other": 0.2, "billing": 0.8 }, "confidence": 0.8 },
                "s": { "type": "score", "score": 1.4, "legend": {}, "probabilities": {}, "confidence": 0.6 },
                "x": { "type": "future" }
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        assert_eq!(a.noul("u"), Some(0.92));
        let p = a.choice("t").unwrap();
        assert_eq!(p.choice, "billing");
        assert_eq!(p.probabilities[0].0, "billing", "sorted most likely first");
        assert_eq!(p.confident(0.7), Some("billing"));
        assert_eq!(p.confident(0.9), None);
        assert_eq!(p.top(1), "billing 80%");
        assert_eq!(a.score("s"), Some((1.4, 0.6)));
        assert_eq!(a.choice("x"), None);
        assert_eq!(a.noul("missing"), None);
    }

    #[test]
    fn option_lists_parse_both_ways() {
        let lines = parse_options("- Admin: chores and appointments\n- Work\n- work: dup\n");
        assert_eq!(lines, vec![("Admin".into(), "chores and appointments".into()), ("Work".into(), String::new())]);
        let inline = parse_options("Salary: payroll, Income");
        assert_eq!(inline, vec![("Salary".into(), "payroll".into()), ("Income".into(), String::new())]);
    }

    #[test]
    fn error_bodies_read_in_every_shape() {
        assert_eq!(error_detail(&json!({ "detail": "Invalid request." })), "Invalid request.");
        assert_eq!(error_detail(&json!({ "detail": { "error_type": "x", "message": "too big" } })), "too big");
        assert_eq!(error_detail(&json!({ "detail": [{ "msg": "a" }, { "msg": "b" }] })), "a; b");
    }

    /// End to end against a loopback stand-in for the API: headers, path, body
    /// and one retry after a 503.
    #[tokio::test]
    async fn talks_to_the_api_and_retries_once() {
        use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
        use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/v1/systemone",
                post(|State(hits): State<Arc<AtomicUsize>>, headers: HeaderMap, Json(b): Json<Value>| async move {
                    if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "detail": "busy" })));
                    }
                    assert_eq!(headers["authorization"], "Bearer k");
                    assert_eq!(b["model"], "m");
                    assert_eq!(b["questions"]["q"]["type"], "noul");
                    (axum::http::StatusCode::OK, Json(json!({ "model": "m", "answers": { "q": { "type": "noul", "noul": 0.25 } }, "usage": { "input_tokens": 300, "output_tokens": 1 } })))
                }),
            )
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let a = ask_at(&format!("http://{addr}"), "k", "m", json!({ "text": "hi" }), &[("q".into(), Q::noul("?", "", ""))]).await.unwrap();
        assert_eq!(a.noul("q"), Some(0.25));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}
