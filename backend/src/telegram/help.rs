//! There is no "/" command menu — the bot is conversational. On start we
//! delete any menu an older version registered so Telegram stops suggesting
//! commands that no longer exist.

use super::*;

pub(super) async fn clear_commands(client: &reqwest::Client, api: &str) {
    match client.post(format!("{api}/deleteMyCommands")).json(&json!({})).send().await {
        Ok(resp) if resp.status().is_success() => tracing::info!("cleared bot command menu"),
        Ok(resp) => tracing::warn!("deleteMyCommands rejected: {}", resp.status()),
        Err(e) => tracing::warn!("deleteMyCommands failed: {e}"),
    }
}
