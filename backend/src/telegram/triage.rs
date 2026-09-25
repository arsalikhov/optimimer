//! Jev-backed triage of a plain text message before the agent sees it:
//!
//! * **control** — "forget this conversation", "switch to Opus", "back to
//!   normal", however it's phrased. The keyword matchers in `mod.rs` stay as
//!   the fast path; this catches what they miss. Only short messages are
//!   asked, and only a confident answer acts.
//! * **effort** — how much reasoning the message needs, so a hard request
//!   starts on the strong/max rung instead of relying on the cheap model to
//!   notice and call `escalate` itself.
//! * **forward comments** — whether a message sent while a forward batch waits
//!   for its comment is that comment or a new, unrelated request.
//!
//! Everything is a no-op when Jev is off (or `JEV_TRIAGE=off`).

use super::*;
use crate::jev::{self, Q};

/// Control commands are short; longer messages are never asked about them.
const CONTROL_MAX_WORDS: usize = 12;
/// Clearing context or rerouting the chat must be near-certain.
const CONTROL_MIN: f64 = 0.9;
/// Start on a higher rung only when Jev is fairly sure it's needed.
const EFFORT_MIN: f64 = 0.7;
/// Below this probability a message is not the forward batch's comment.
const FORWARD_COMMENT_MIN: f64 = 0.25;

/// A control request recognised by Jev.
#[derive(Debug, PartialEq)]
pub(super) enum Control {
    Clear,
    /// `Some(rung)` pins the chat to a rung, `None` returns it to normal.
    Route(Option<&'static str>),
}

#[derive(Debug, Default)]
pub(super) struct Triage {
    pub control: Option<Control>,
    /// A rung to start this turn on ("strong" / "max"), if the message needs it.
    pub rung: Option<&'static str>,
}

fn triage_on() -> bool {
    jev::enabled() && !matches!(std::env::var("JEV_TRIAGE").unwrap_or_default().trim().to_lowercase().as_str(), "off" | "0" | "false" | "no")
}

/// Ask about one message. Empty when Jev is off, unsure, or failed.
pub(super) async fn triage(text: &str) -> Triage {
    if !triage_on() {
        return Triage::default();
    }
    let mut questions = vec![(
        "effort".to_string(),
        Q::choice(
            "How much reasoning does a personal assistant need to answer this message well?",
            [
                ("everyday", "Everyday assistant work: capturing tasks, notes, reminders or expenses, quick lookups, short answers, chit-chat"),
                ("strong", "Real reasoning: multi-step planning, careful analysis or comparison, writing or debugging code, nuanced advice"),
                ("max", "Genuinely hard: long rigorous problems or deep research-style reasoning where a mistake is costly"),
            ],
        ),
    )];
    if text.split_whitespace().count() <= CONTROL_MAX_WORDS {
        questions.push((
            "control".to_string(),
            Q::choice(
                "Is this message a command about the chat itself (its context or which AI model answers), and if so which?",
                [
                    ("none", "An ordinary message for the assistant — anything that is not one of the commands below"),
                    ("clear_context", "Asks to wipe or forget the current conversation and start a fresh chat"),
                    ("route_strong", "Asks to switch this chat to the stronger model (Sonnet) from now on"),
                    ("route_max", "Asks to switch this chat to the strongest model (Opus) from now on"),
                    ("route_unsafe", "Asks to switch this chat to the uncensored, low-guardrail model (Grok) from now on"),
                    ("back_to_normal", "Asks to return this chat to the normal, default model"),
                ],
            ),
        ));
    }
    let answers = match jev::ask(json!({ "message": text }), questions).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("jev triage failed: {e}");
            return Triage::default();
        }
    };
    let control = answers.choice("control").and_then(|p| match p.confident(CONTROL_MIN)? {
        "clear_context" => Some(Control::Clear),
        "route_strong" => Some(Control::Route(Some("strong"))),
        "route_max" => Some(Control::Route(Some("max"))),
        "route_unsafe" => Some(Control::Route(Some("unsafe"))),
        "back_to_normal" => Some(Control::Route(None)),
        _ => None,
    });
    let rung = answers.choice("effort").and_then(|p| match p.confident(EFFORT_MIN)? {
        "strong" => Some("strong"),
        "max" => Some("max"),
        _ => None,
    });
    if control.is_some() || rung.is_some() {
        tracing::info!("triage: control {control:?}, start rung {rung:?}");
    }
    Triage { control, rung }
}

/// The ladder position of a route, as in `llm::model_for_route`.
fn rung_of(route: Option<&str>) -> u8 {
    crate::llm::model_for_route(route).1
}

/// The rung this turn should start on: Jev's pick, but only if it's higher
/// than where the chat is already routed (never a step down).
pub(super) fn start_rung(jev_rung: Option<&'static str>, stored: Option<&str>) -> Option<&'static str> {
    jev_rung.filter(|r| rung_of(Some(r)) > rung_of(stored))
}

/// With a forward batch waiting for its comment: is `text` plainly NOT about
/// those forwards (a new request that should go to the agent instead)?
pub(super) async fn unrelated_to_forward(preview: &str, text: &str) -> bool {
    if !jev::enabled() {
        return false;
    }
    let q = Q::noul(
        "The user forwarded some messages and was asked what they want done with them. Is their next message a comment or instruction about those forwarded messages?",
        "About the forwards: what to focus on, context about them, what to do with them, or a short reply like 'summarize' or 'just save it'",
        "A new, unrelated request or question for the assistant",
    );
    // This runs on the polling loop (ordering with the forwards matters), so it
    // gets a tight budget: on a slow answer the message is taken as the comment.
    let ask = jev::noul(json!({ "forwarded": preview, "next_message": text }), q);
    match tokio::time::timeout(std::time::Duration::from_secs(3), ask).await.ok().flatten() {
        Some(p) => {
            tracing::info!("forward comment check: {p:.2}");
            p < FORWARD_COMMENT_MIN
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_rung_never_steps_down() {
        assert_eq!(start_rung(Some("strong"), None), Some("strong"));
        assert_eq!(start_rung(Some("strong"), Some("max")), None);
        assert_eq!(start_rung(Some("max"), Some("strong")), Some("max"));
        assert_eq!(start_rung(Some("max"), Some("unsafe")), None);
        assert_eq!(start_rung(None, None), None);
    }
}
