use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

/// Transcribe audio bytes via OpenRouter. Defaults to `mistralai/voxtral-small-24b-2507`
/// — the only Voxtral on OpenRouter (the `voxtral-mini-transcribe` slug is Mistral-API
/// only and 500s here). Telegram voice notes are OGG/Opus, so `format` is usually "ogg".
/// Reuses OPENROUTER_API_KEY; without it, returns a labelled mock so the voice flow still
/// wires up offline. Override the model with TRANSCRIBE_MODEL.
pub async fn transcribe(audio: Vec<u8>, format: &str) -> Result<String> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Ok(format!("[mock transcript — set OPENROUTER_API_KEY] ({} bytes)", audio.len()));
    }
    let model = crate::llm::transcribe();

    // Bias the model toward the user's domain vocabulary (names, brands, jargon)
    // so an acronym or product name isn't misheard. The list lives in settings
    // (`config::VOCAB`, grown via the add_vocab tool) or TRANSCRIBE_VOCAB.
    let mut instruction =
        "Transcribe this audio verbatim. Output only the transcript text, nothing else.".to_string();
    let vocab = crate::config::vocab().join(", ");
    if !vocab.trim().is_empty() {
        instruction.push_str(&format!(
            " The speaker may use these specific names/brands/terms — prefer these exact spellings when a word matches them phonetically: {}.",
            vocab.trim()
        ));
    }

    let body = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": instruction },
                { "type": "input_audio", "input_audio": { "data": STANDARD.encode(&audio), "format": format } }
            ]
        }]
    });

    let resp = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&key)
        .header("HTTP-Referer", "http://localhost:5173")
        .header("X-Title", "Optimimer")
        .json(&body)
        .timeout(crate::openrouter::LLM_TIMEOUT)
        .send()
        .await
        .map_err(|e| crate::openrouter::net_err("transcription", &model, e))?;
    let status = resp.status();
    let j: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(crate::openrouter::explain("transcription", &model, status, &j));
    }
    j["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("unexpected transcription response: {}", j))
}
