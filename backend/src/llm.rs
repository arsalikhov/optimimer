//! Which LLMs to use. Two tiers:
//!
//! * `free` (the default) — OpenRouter's free models. No credits needed, so a
//!   fresh account works out of the box; slower and less accurate, and the
//!   audio/vision ones are best-effort.
//! * `paid` — cheap-but-good models for the everyday work (Claude Haiku 4.5
//!   for the agent, Gemini Flash Lite for parsing), with an escalation ladder
//!   the agent climbs only when a request needs it: `strong` (Claude Sonnet 5)
//!   and `max` (Claude Opus 5). Billed to the account's OpenRouter credits.
//!
//! The tier is a stored setting (`model_tier`, env `MODEL_TIER`) that the owner
//! picks during onboarding or by saying "use paid models". Each role can still
//! be pinned to any OpenRouter id with its own env var (AGENT_MODEL, …).
//! Free model ids rotate on OpenRouter; when one disappears the error the user
//! sees says so, and the matching env var is the escape hatch.

use crate::config;

pub const TIER: &str = "model_tier";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Free,
    Paid,
}

pub fn tier() -> Tier {
    match config::get(TIER).as_deref().map(|s| s.trim().to_lowercase()).as_deref() {
        Some("paid") => Tier::Paid,
        _ => Tier::Free,
    }
}

pub fn set_tier(t: Tier) {
    config::set(TIER, if t == Tier::Paid { "paid" } else { "free" });
}

pub fn is_paid() -> bool {
    tier() == Tier::Paid
}

struct Set {
    /// The conversational agent (must support tool calling).
    agent: &'static str,
    /// What the agent escalates to for hard requests (`escalate("strong")`).
    strong: &'static str,
    /// The top of the ladder, for genuinely hard problems (`escalate("max")`).
    max: &'static str,
    /// A model with minimal content guardrails (`escalate("unsafe")`, owner only,
    /// only when the user explicitly asks). Must support tool calling.
    lax: &'static str,
    /// Strict JSON parsers (tasks, notes, money, CSV rows, summaries).
    parser: &'static str,
    /// Cheap background work (memory extraction, reminders, email drafts).
    cheap: &'static str,
    /// Reading photos, receipts included (needs image input).
    ocr: &'static str,
    /// Voice notes (needs audio input).
    transcribe: &'static str,
    /// Reading product pages for stock watches.
    shopper: &'static str,
}

const FREE: Set = Set {
    agent: "nvidia/nemotron-3-super-120b-a12b:free",
    strong: "nvidia/nemotron-3-ultra-550b-a55b:free",
    max: "nvidia/nemotron-3-ultra-550b-a55b:free",
    // No free unmoderated model with tool calling exists; stays on the agent model.
    lax: "nvidia/nemotron-3-super-120b-a12b:free",
    parser: "nvidia/nemotron-3-super-120b-a12b:free",
    cheap: "nvidia/nemotron-3.5-lightning:free",
    ocr: "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
    transcribe: "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
    shopper: "nvidia/nemotron-3.5-lightning:free",
};

// Prices (per million tokens, in/out, OpenRouter 2026-09): Haiku 4.5 $1/$5,
// Gemini 3.1 Flash Lite $0.25/$1.5, Sonnet 5 $2/$10, Opus 5 $5/$25 — versus
// the old flat Sonnet 4.6 at $3/$15 for everything.
const PAID: Set = Set {
    agent: "anthropic/claude-haiku-4.5",
    strong: "anthropic/claude-sonnet-5",
    max: "anthropic/claude-opus-5",
    // Unmoderated on OpenRouter and supports tools ($2/$6); the others in that
    // category (Dolphin, Hermes, …) can't call tools, so the turn would die.
    lax: "x-ai/grok-4.6",
    parser: "google/gemini-3.1-flash-lite",
    cheap: "google/gemini-3.1-flash-lite",
    ocr: "anthropic/claude-haiku-4.5",
    transcribe: "mistralai/voxtral-small-24b-2507",
    shopper: "google/gemini-3.1-flash-lite",
};

fn set() -> &'static Set {
    if is_paid() { &PAID } else { &FREE }
}

/// An explicit env override wins; otherwise the tier's default for the role.
fn pick(env: &str, role: fn(&Set) -> &'static str) -> String {
    match std::env::var(env) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => role(set()).to_string(),
    }
}

pub fn agent() -> String { pick("AGENT_MODEL", |s| s.agent) }
pub fn strong() -> String { pick("STRONG_MODEL", |s| s.strong) }
pub fn max() -> String { pick("MAX_MODEL", |s| s.max) }
pub fn lax() -> String { pick("UNSAFE_MODEL", |s| s.lax) }
pub fn parser() -> String { pick("PARSER_MODEL", |s| s.parser) }
pub fn memory() -> String { pick("MEMORY_MODEL", |s| s.cheap) }
/// Summaries and CSV classification read like the agent's work: agent-grade model.
pub fn convo() -> String { pick("CONVO_MODEL", |s| s.agent) }
pub fn finance() -> String { pick("FINANCE_MODEL", |s| s.agent) }
pub fn ocr() -> String { pick("OCR_MODEL", |s| s.ocr) }
pub fn transcribe() -> String { pick("TRANSCRIBE_MODEL", |s| s.transcribe) }
pub fn shopper() -> String { pick("SHOPPER_MODEL", |s| s.shopper) }

// ---- per-chat route --------------------------------------------------------
// The owner can pin a chat to a rung ("escalate to unsafe", "use opus") until
// they say "back to normal". Stored as settings key `route:<chat_id>`.

/// Where a chat is routed: None = the everyday model.
pub fn route(chat_id: i64) -> Option<String> {
    config::stored(&format!("route:{chat_id}")).filter(|r| matches!(r.as_str(), "strong" | "max" | "unsafe"))
}

pub fn set_route(chat_id: i64, route: Option<&str>) {
    let key = format!("route:{chat_id}");
    match route {
        Some(r) => config::set(&key, r),
        None => config::unset(&key),
    }
}

/// The model a chat's turn starts on, and the ladder rung that corresponds to
/// it (0 everyday, 1 strong, 2 max, 3 unsafe) so `escalate` never steps down.
pub fn model_for_route(route: Option<&str>) -> (String, u8) {
    match route {
        Some("strong") => (strong(), 1),
        Some("max") => (max(), 2),
        Some("unsafe") => (lax(), 3),
        _ => (agent(), 0),
    }
}

/// A workflow node's `model` field: a placeholder follows the tier, anything
/// else is a literal OpenRouter id.
pub fn resolve(spec: &str) -> String {
    match spec.trim() {
        "" | "auto" | "$agent" => agent(),
        "$parser" => parser(),
        "$cheap" => memory(),
        other => other.to_string(),
    }
}

/// One line for `show_settings` and the onboarding prompt.
pub fn describe() -> String {
    let base = match tier() {
        Tier::Free => "free (OpenRouter's free Nemotron models — no credits needed, weaker)".to_string(),
        Tier::Paid => format!("paid ({} everyday, {} when escalated, {} at most, {} on request as 'unsafe'; {} for parsing)", agent(), strong(), max(), lax(), parser()),
    };
    let pinned: Vec<String> = ["AGENT_MODEL", "STRONG_MODEL", "MAX_MODEL", "UNSAFE_MODEL", "PARSER_MODEL", "MEMORY_MODEL", "CONVO_MODEL", "FINANCE_MODEL", "OCR_MODEL", "TRANSCRIBE_MODEL", "SHOPPER_MODEL"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()).map(|v| format!("{k}={v}")))
        .collect();
    if pinned.is_empty() { base } else { format!("{base}; pinned: {}", pinned.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_follow_the_tier() {
        // No settings db in tests → free tier; no env → tier defaults.
        assert_eq!(resolve("$parser"), FREE.parser);
        assert_eq!(resolve(""), FREE.agent);
        assert_eq!(resolve("some/model"), "some/model");
        assert!(FREE.agent.ends_with(":free") && FREE.ocr.ends_with(":free"));
    }
}
