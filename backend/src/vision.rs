use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

/// OCR a receipt/invoice image into a single plain-text expense sentence, e.g.
/// `Spent 45.20 at Loblaws on 2026-06-22.`. That sentence is then fed to the
/// `/spent` agent, which parses + categorizes it like any typed expense — so the
/// image path reuses all the normal capture logic.
///
/// Uses a vision-capable model via OpenRouter (default `anthropic/claude-sonnet-4.6`;
/// override with `OCR_MODEL`). Without `OPENROUTER_API_KEY` it returns a labelled
/// mock so the flow still wires up offline.
pub async fn read_receipt(image: Vec<u8>, mime: &str) -> Result<String> {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Ok("[mock receipt — set OPENROUTER_API_KEY] Spent 0 at Unknown".to_string());
    }
    let model = std::env::var("OCR_MODEL").unwrap_or_else(|_| "anthropic/claude-sonnet-4.6".to_string());
    let mime = if mime.is_empty() { "image/jpeg" } else { mime };

    // Cache by a hash of the exact image bytes (+ model): re-sending the same
    // photo returns the prior OCR sentence instead of re-running the vision call.
    let img_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        image.hash(&mut h);
        format!("{:016x}", h.finish())
    };
    let ck = crate::cache::key("ocr", &[model.as_str(), &img_hash]);
    if let Some(cached) = crate::cache::get(&ck) {
        return Ok(cached);
    }

    let data_url = format!("data:{};base64,{}", mime, STANDARD.encode(&image));

    let body = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "This image is a receipt or invoice. Extract the FINAL total amount paid, the merchant/vendor name, and the date. Reply with ONE line only and nothing else, in EXACTLY this form: 'Spent <amount> at <merchant> on <YYYY-MM-DD>'. Use digits only for the amount — no currency symbol, no thousands separators. If the date is unreadable, omit ' on <YYYY-MM-DD>'. If the total is unreadable, use 0." },
                { "type": "image_url", "image_url": { "url": data_url } }
            ]
        }]
    });

    let resp = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&key)
        .header("HTTP-Referer", "http://localhost:5173")
        .header("X-Title", "Optimimer")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let j: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(anyhow!("receipt OCR {}: {}", status, j));
    }
    let out = j["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("unexpected OCR response: {}", j))?;
    crate::cache::put(&ck, &out);
    Ok(out)
}
