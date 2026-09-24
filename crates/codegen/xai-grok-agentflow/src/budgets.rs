//! Per-turn max-step / max-tool-call budget resolution and usage checks.
//!
//! Resolution order per dimension: tiered environment override
//! (`GROK_LOCAL_BUDGET_<TIER>_MAX_<DIM>`), then
//! `GROK_LOCAL_BUDGET_DEFAULT_MAX_<DIM>`, then the TOML budget table, then
//! the effort behavior policy. A turn only has a budget when some layer
//! provides one; otherwise enforcement stays off (fail-open).
//!
//! Tool calls are counted as actual calls, not tool turns.

use crate::EnvReader;

/// Per-turn budgets. `None` means unbounded on that dimension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnBudgets {
    pub max_steps: Option<u32>,
    pub max_tool_calls: Option<u32>,
}

impl TurnBudgets {
    pub fn is_empty(self) -> bool {
        self.max_steps.is_none() && self.max_tool_calls.is_none()
    }
}

/// Cumulative usage for the current turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetUsage {
    /// Model steps used so far (including the current one).
    pub steps: u64,
    /// Actual tool calls made so far (not tool turns).
    pub tool_calls: u64,
}

/// Which dimensions have hit their cap (usage >= cap).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetHit {
    pub steps_hit: bool,
    pub tool_calls_hit: bool,
}

impl BudgetHit {
    pub fn any_hit(self) -> bool {
        self.steps_hit || self.tool_calls_hit
    }
}

/// Parses a budget env value: digits only, > 0. Anything else → None.
pub fn parse_budget_value(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u32>().ok().filter(|&n| n > 0)
}

fn env_tier_key(route_tier: Option<&str>) -> Option<&'static str> {
    match route_tier.map(|t| t.trim().to_ascii_lowercase()).as_deref() {
        Some("economy") | Some("edge") => Some("ECONOMY"),
        Some("balanced") => Some("BALANCED"),
        Some("heavy") => Some("HEAVY"),
        _ => None,
    }
}

/// Resolves the env-driven portion of the turn budgets: per-tier override
/// first, then the default. TOML and policy layers are applied by the caller
/// ([`crate::config::AgentFlowConfig::resolve_budgets`]).
pub fn resolve_env_turn_budgets(route_tier: Option<&str>, get_env: EnvReader<'_>) -> TurnBudgets {
    let tier_key = env_tier_key(route_tier);
    let pick = |dimension: &str| -> Option<u32> {
        if let Some(tier) = tier_key {
            let raw = get_env(&format!("GROK_LOCAL_BUDGET_{tier}_MAX_{dimension}"));
            if let Some(value) = raw.as_deref().and_then(parse_budget_value) {
                return Some(value);
            }
        }
        get_env(&format!("GROK_LOCAL_BUDGET_DEFAULT_MAX_{dimension}"))
            .as_deref()
            .and_then(parse_budget_value)
    };
    TurnBudgets {
        max_steps: pick("STEPS"),
        max_tool_calls: pick("TOOL_CALLS"),
    }
}

/// Checks usage against caps. A cap is hit when usage >= cap.
pub fn check_budget_hit(usage: BudgetUsage, budgets: TurnBudgets) -> BudgetHit {
    BudgetHit {
        steps_hit: budgets
            .max_steps
            .is_some_and(|cap| usage.steps >= cap as u64),
        tool_calls_hit: budgets
            .max_tool_calls
            .is_some_and(|cap| usage.tool_calls >= cap as u64),
    }
}

/// Warn threshold for a cap: ceil(80%).
pub fn warn_threshold(cap: u32) -> u64 {
    (f64::from(cap) * 0.8).ceil() as u64
}

/// One-line human-readable budget description for logs and telemetry.
pub fn describe_turn_budgets(tier: Option<&str>, budgets: TurnBudgets) -> String {
    let steps = budgets
        .max_steps
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unbounded".to_string());
    let calls = budgets
        .max_tool_calls
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unbounded".to_string());
    format!(
        "tier={} maxSteps={} maxToolCalls={}",
        tier.unwrap_or("none"),
        steps,
        calls
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn tiered_env_beats_default_env() {
        let vars = env_of(&[
            ("GROK_LOCAL_BUDGET_HEAVY_MAX_STEPS", "200"),
            ("GROK_LOCAL_BUDGET_DEFAULT_MAX_STEPS", "50"),
            ("GROK_LOCAL_BUDGET_DEFAULT_MAX_TOOL_CALLS", "150"),
        ]);
        let get_env = |k: &str| vars.get(k).cloned();
        let b = resolve_env_turn_budgets(Some("heavy"), &get_env);
        assert_eq!(b.max_steps, Some(200));
        assert_eq!(b.max_tool_calls, Some(150));

        let b2 = resolve_env_turn_budgets(Some("economy"), &get_env);
        assert_eq!(b2.max_steps, Some(50));
    }

    #[test]
    fn unknown_tier_falls_back_to_default() {
        let vars = env_of(&[("GROK_LOCAL_BUDGET_DEFAULT_MAX_STEPS", "42")]);
        let get_env = |k: &str| vars.get(k).cloned();
        let b = resolve_env_turn_budgets(Some("turbo"), &get_env);
        assert_eq!(b.max_steps, Some(42));
        assert_eq!(b.max_tool_calls, None);
    }

    #[test]
    fn invalid_env_values_are_unset() {
        let vars = env_of(&[
            ("GROK_LOCAL_BUDGET_BALANCED_MAX_STEPS", "0"),
            ("GROK_LOCAL_BUDGET_BALANCED_MAX_TOOL_CALLS", "abc"),
            ("GROK_LOCAL_BUDGET_BALANCED_MAX_FROBNICATOR", "10"),
        ]);
        let get_env = |k: &str| vars.get(k).cloned();
        let b = resolve_env_turn_budgets(Some("balanced"), &get_env);
        assert_eq!(b, TurnBudgets::default());
        assert!(b.is_empty());
    }

    #[test]
    fn parse_budget_value_digits_only() {
        assert_eq!(parse_budget_value("60"), Some(60));
        assert_eq!(parse_budget_value("  60 "), Some(60));
        assert_eq!(parse_budget_value("0"), None);
        assert_eq!(parse_budget_value("-5"), None);
        assert_eq!(parse_budget_value("1.5"), None);
        assert_eq!(parse_budget_value(""), None);
    }

    #[test]
    fn hit_check_uses_greater_or_equal() {
        let budgets = TurnBudgets {
            max_steps: Some(25),
            max_tool_calls: Some(60),
        };
        assert_eq!(
            check_budget_hit(
                BudgetUsage {
                    steps: 24,
                    tool_calls: 59
                },
                budgets
            ),
            BudgetHit {
                steps_hit: false,
                tool_calls_hit: false
            }
        );
        assert!(
            check_budget_hit(
                BudgetUsage {
                    steps: 25,
                    tool_calls: 60
                },
                budgets
            )
            .any_hit()
        );
    }

    #[test]
    fn warn_threshold_is_ceil_80_percent() {
        assert_eq!(warn_threshold(25), 20);
        assert_eq!(warn_threshold(26), 21); // ceil(20.8)
        assert_eq!(warn_threshold(60), 48);
        assert_eq!(warn_threshold(1), 1);
    }

    #[test]
    fn describe_format() {
        assert_eq!(
            describe_turn_budgets(
                Some("heavy"),
                TurnBudgets {
                    max_steps: Some(90),
                    max_tool_calls: None
                }
            ),
            "tier=heavy maxSteps=90 maxToolCalls=unbounded"
        );
    }
}
