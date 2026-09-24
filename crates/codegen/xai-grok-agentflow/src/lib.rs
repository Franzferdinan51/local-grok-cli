//! Agent-flow policy engine: the grok-local parity port of ZCode 3.25.0's
//! speedstack agent-flow enhancements.
//!
//! Every module in this crate is pure, deterministic, and **fail-open**:
//! when configuration, routing input, or confidence is missing, callers get
//! the full (unpruned, unbudgeted) behavior rather than a broken turn.
//!
//! The crate never touches inference, never loads or switches models, and
//! never performs I/O except through the explicit artifact functions in
//! [`plan_execute`] and [`anchored_compaction`]. No model IDs are hard-coded
//! here: model choice always flows from the router decision, the registry,
//! or explicit user config.
//!
//! ## Modules
//!
//! - [`effort`]: config-driven effort behavior policies (the table behind
//!   `Effort::canonical_caps()`).
//! - [`budgets`]: per-turn max-step / max-tool-call budget resolution and
//!   usage checks.
//! - [`enforcement`]: the 80% warn → 100% escalate → +1 step → stop state
//!   machine.
//! - [`tool_packs`]: label-driven tool schema pruning, schema token
//!   estimation, and missed-call recovery detection.
//! - [`plan_execute`]: plan-then-execute gating, model resolution (pinned
//!   model reused, never switched), planner/executor prompts, and the
//!   `PLAN.md` artifact.
//! - [`doom_loop`]: normalized near-duplicate tool-call fingerprinting and
//!   the nudge → strategy-change → final escalation ladder.
//! - [`anchored_compaction`]: anchor extraction, transcript archiving with
//!   rotation, boundary-compact events, and the anchored summary prompt.
//! - [`config`]: `$GROK_HOME/config.toml` `[agent_flow]` section plus
//!   `GROK_LOCAL_*` environment overrides.

pub mod anchored_compaction;
pub mod budgets;
pub mod config;
pub mod doom_loop;
pub mod effort;
pub mod enforcement;
pub mod plan_execute;
pub mod subagents;
pub mod tool_packs;

/// Reads one environment variable. Production code passes
/// `&|key| std::env::var(key).ok()`; tests pass a closure over a map so no
/// real environment is needed.
pub type EnvReader<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Case-insensitive boolean env spelling: 1/true/yes/on → Some(true),
/// 0/false/no/off → Some(false), anything else → None.
pub fn parse_bool_env(raw: Option<&str>) -> Option<bool> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Kill-switch semantics: a value that parses to boolean false disables the
/// feature; missing or any other value leaves it enabled.
pub fn is_env_disabled(raw: Option<&str>) -> bool {
    matches!(parse_bool_env(raw), Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_env_spellings() {
        assert_eq!(parse_bool_env(Some("1")), Some(true));
        assert_eq!(parse_bool_env(Some("TRUE")), Some(true));
        assert_eq!(parse_bool_env(Some(" yes ")), Some(true));
        assert_eq!(parse_bool_env(Some("0")), Some(false));
        assert_eq!(parse_bool_env(Some("off")), Some(false));
        assert_eq!(parse_bool_env(None), None);
        assert_eq!(parse_bool_env(Some("banana")), None);
    }

    #[test]
    fn env_disabled_semantics() {
        assert!(is_env_disabled(Some("0")));
        assert!(is_env_disabled(Some("false")));
        assert!(!is_env_disabled(None));
        assert!(!is_env_disabled(Some("")));
        assert!(!is_env_disabled(Some("1")));
    }
}
