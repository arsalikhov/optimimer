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
        return Err(anyhow!("OpenRouter {}: {}", status, body));
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
        return Err(anyhow!("OpenRouter {}: {}", status, body));
    }
    let msg = body["choices"][0]["message"].clone();
    if msg.is_null() {
        return Err(anyhow!("unexpected OpenRouter response: {}", body));
    }
    Ok(msg)
}
