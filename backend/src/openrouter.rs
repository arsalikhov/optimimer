use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Minimal OpenRouter chat-completions client.
/// Reads the API key from OPENROUTER_API_KEY. When unset, returns a clearly
/// labelled mock so the builder stays fully demoable without credentials.
pub async fn chat(model: &str, system: &str, prompt: &str) -> Result<String> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Ok(format!(
            "[mock:{}] {}",
            model,
            prompt.chars().take(280).collect::<String>()
        ));
    }

    let mut messages = Vec::new();
    if !system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": prompt }));

    let client = reqwest::Client::new();
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&key)
        .header("HTTP-Referer", "http://localhost:5173")
        .header("X-Title", "Optimimer")
        .json(&json!({ "model": model, "messages": messages }))
        .send()
        .await?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        return Err(explain("chat", model, status, &body));
    }

    body["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("unexpected OpenRouter response: {}", body))
}

/// Tool-calling completion for the agent loop. Takes the full `messages` array
/// (system / user / assistant-with-tool_calls / tool results) and a `tools`
/// array of OpenAI-style function schemas; returns the raw assistant `message`
/// object, which carries either `content` (final answer) or `tool_calls`.
/// With no API key it returns a mock assistant turn (no tool calls) so agent
/// mode degrades gracefully instead of erroring.
pub async fn chat_tools(model: &str, messages: &[Value], tools: &Value) -> Result<Value> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Ok(json!({
            "role": "assistant",
            "content": "[mock] Agent mode needs OPENROUTER_API_KEY set to call tools."
        }));
    }

    let client = reqwest::Client::new();
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&key)
        .header("HTTP-Referer", "http://localhost:5173")
        .header("X-Title", "Optimimer")
        .json(&json!({ "model": model, "messages": messages, "tools": tools, "tool_choice": "auto" }))
        .send()
        .await?;

    let status = resp.status();
    let body: Value = resp.json().await?;
    if !status.is_success() {
        return Err(explain("agent", model, status, &body));
    }
    let msg = body["choices"][0]["message"].clone();
    if msg.is_null() {
        return Err(anyhow!("unexpected OpenRouter response: {}", body));
    }
    Ok(msg)
}

/// Turn an OpenRouter error into something the user can act on. The common
/// failures of a fresh account (no key, no credits, a free model that was
/// retired, the free-tier rate limit) each get a plain-words hint.
pub fn explain(what: &str, model: &str, status: reqwest::StatusCode, body: &Value) -> anyhow::Error {
    let detail = body["error"]["message"].as_str().unwrap_or("").trim().to_string();
    let hint = match status.as_u16() {
        401 => "OpenRouter rejected the API key — check OPENROUTER_API_KEY.".to_string(),
        402 => "your OpenRouter account has no credits. Add some at https://openrouter.ai/credits, or say \"use free models\".".to_string(),
        404 => format!("the model `{model}` isn't available on OpenRouter right now (free model ids change). Say \"use paid models\", or pin another id with the matching *_MODEL variable."),
        429 => "OpenRouter's rate limit hit. Free models allow roughly 50 requests a day (1000 once the account has ever bought $10 of credits) — wait a bit, or say \"use paid models\".".to_string(),
        _ => format!("OpenRouter {status}: {}", if detail.is_empty() { body.to_string() } else { detail }),
    };
    anyhow!("{what}: {hint}")
}

/// Remaining OpenRouter balance in dollars, or `None` without a key / offline.
pub async fn credits() -> Option<f64> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        return None;
    }
    let body: Value = reqwest::Client::new()
        .get("https://openrouter.ai/api/v1/credits")
        .bearer_auth(key.trim())
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let total = body["data"]["total_credits"].as_f64()?;
    let used = body["data"]["total_usage"].as_f64().unwrap_or(0.0);
    Some(total - used)
}
