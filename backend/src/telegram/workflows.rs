//! Running saved `cmd-*` workflow agents (notify, email) as agent tools, and rendering their output.

use super::*;

/// An agent Output beginning with this marker is sent as a rich (HTML) message
/// via sendRichMessage instead of plain Markdown. The marker is stripped first.
pub(super) const RICH_SENTINEL: &str = "<!rich>";

/// Run a `cmd-<name>` agent with structured input (`category` is passed through
/// to the agent's input; empty for the built-in ones).
pub(super) async fn run_command(
    state: &BotState,
    chat_id: i64,
    cmd: &str,
    text: &str,
    category: &str,
) -> Reply {
    let wf = match state.store.get(&format!("cmd-{cmd}")) {
        Some(w) => w,
        None => return Reply::text(format!("The `/{cmd}` agent isn't installed (expected agent id `cmd-{cmd}`). Restart the backend to seed it, or build it in the web UI.")),
    };

    let tz = tz_for(state, chat_id);
    let now = now_in_tz(&tz);
    let input = json!({
        "text": text,
        "now": now,
        "tz": tz,
        "category": category,
        "chat_id": chat_id.to_string(),
        "command": cmd,
    });

    let result = engine::run(&wf, input).await;
    let body = output_text(&result).unwrap_or_default();

    // An agent can opt into a rich (HTML) reply by prefixing its Output with the
    // sentinel; it then owns the full formatting (tables, collapsible blocks, …).
    if let Some(html) = body.trim_start().strip_prefix(RICH_SENTINEL) {
        let html = html.trim_start().to_string();
        let fallback = strip_tags(&html);
        return Reply::rich(html, fallback);
    }

    Reply::text(format_result(&wf.name, &result))
}

/// The raw Output-node value (or last successful node output) as a string.
pub(super) fn output_text(result: &RunResponse) -> Option<String> {
    let v = result
        .results
        .iter()
        .rev()
        .find(|r| r.node_type == "output" && r.status == "ok")
        .map(|r| r.output["value"].clone())
        .or_else(|| {
            result
                .results
                .iter()
                .rev()
                .find(|r| r.status == "ok")
                .map(|r| r.output.clone())
        })?;
    Some(match v {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(&other).unwrap_or_default(),
    })
}

/// Human reply from a run: the Output node's value, or the last successful node.
pub(super) fn format_result(name: &str, result: &RunResponse) -> String {
    if result.status == "error" {
        let err = result
            .results
            .iter()
            .find_map(|r| r.error.clone())
            .unwrap_or_default();
        return format!("*{name}* finished with an error:\n{err}");
    }
    let body = output_text(result)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no output)".to_string());
    format!("*{name}*\n\n{body}")
}
