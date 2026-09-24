//! Config-driven effort behavior policies.
//!
//! Ports ZCode 3.25.0's effort→behavior policy table: each effort tier maps
//! to max steps, max tool calls, subagent spawning policy, read breadth,
//! verification passes, compaction aggressiveness, and plan-then-execute
//! eligibility. Everything is overridable in `$GROK_HOME/config.toml`
//! (`[agent_flow.effort.<tier>]`) and via `GROK_LOCAL_EFFORT_<TIER>_*`
//! environment variables — nothing in this table is compiled into
//! behavior.

use serde::Deserialize;

use crate::{parse_bool_env, EnvReader};

/// One of the five behavior policy tiers. `thinking off` / `none` map to
/// [`EffortTier::Low`] (minimal agent behavior), matching ZCode's
/// off → low policy row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffortTier {
    Low,
    Medium,
    High,
    XHigh,
    Ultra,
}

impl EffortTier {
    /// All tiers, ascending.
    pub const ALL: [EffortTier; 5] = [
        EffortTier::Low,
        EffortTier::Medium,
        EffortTier::High,
        EffortTier::XHigh,
        EffortTier::Ultra,
    ];

    /// Canonical lowercase config/table key: "low" | "medium" | "high" | "xhigh" | "ultra".
    pub fn key(self) -> &'static str {
        match self {
            EffortTier::Low => "low",
            EffortTier::Medium => "medium",
            EffortTier::High => "high",
            EffortTier::XHigh => "xhigh",
            EffortTier::Ultra => "ultra",
        }
    }

    /// Uppercase suffix used in `GROK_LOCAL_EFFORT_<TIER>_*` variable names.
    pub fn env_key(self) -> &'static str {
        match self {
            EffortTier::Low => "LOW",
            EffortTier::Medium => "MEDIUM",
            EffortTier::High => "HIGH",
            EffortTier::XHigh => "XHIGH",
            EffortTier::Ultra => "ULTRA",
        }
    }

    /// Parses an effort name case-insensitively. Accepts the grok-local
    /// `Effort` spellings (`off`, `low`, `medium`, `high`, `xhigh`,
    /// `ultra`) plus `none`/`minimal` as Low.
    pub fn parse(name: &str) -> Option<EffortTier> {
        match name.trim().to_ascii_lowercase().as_str() {
            "low" => Some(EffortTier::Low),
            "medium" => Some(EffortTier::Medium),
            "high" => Some(EffortTier::High),
            "xhigh" | "x-high" => Some(EffortTier::XHigh),
            "ultra" => Some(EffortTier::Ultra),
            "off" | "none" | "minimal" => Some(EffortTier::Low),
            _ => None,
        }
    }
}

/// Subagent spawning allowance for an effort tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentAllowance {
    /// Never spawn subagents at this tier (low effort).
    Never,
    /// Spawn conservatively (one at a time, only when clearly parallelizable).
    Conservative,
    /// Spawn in parallel when the task decomposes.
    Parallel,
}

impl SubagentAllowance {
    pub fn key(self) -> &'static str {
        match self {
            SubagentAllowance::Never => "never",
            SubagentAllowance::Conservative => "conservative",
            SubagentAllowance::Parallel => "parallel",
        }
    }

    pub fn parse(name: &str) -> Option<SubagentAllowance> {
        match name.trim().to_ascii_lowercase().as_str() {
            "never" | "off" | "none" | "no" => Some(SubagentAllowance::Never),
            "conservative" => Some(SubagentAllowance::Conservative),
            "parallel" | "aggressive" => Some(SubagentAllowance::Parallel),
            _ => None,
        }
    }
}

/// The full behavior policy for one effort tier.
#[derive(Debug, Clone)]
pub struct EffortBehaviorPolicy {
    pub tier: EffortTier,
    /// Maximum model steps for the turn.
    pub max_steps: u32,
    /// Maximum tool calls for the turn.
    pub max_tool_calls: u32,
    /// Whether subagents may be spawned at this tier.
    pub subagents: SubagentAllowance,
    /// Maximum turns for a spawned subagent.
    pub subagent_max_turns: u32,
    /// How many files may be read per batch / exploration round.
    pub read_breadth: u32,
    /// Verification passes (test/build review) after writing code.
    pub verification_passes: u32,
    /// Compaction threshold multiplier (< 1.0 compacts earlier).
    pub compaction_aggressiveness: f64,
    /// Whether this tier is eligible for plan-then-execute gating.
    pub plan_then_execute_eligible: bool,
}

/// Ryan's raised defaults (the ZCode 3.25.0 policy table), kept only as
/// fallbacks — every field is overridable via config or env.
pub fn default_policy(tier: EffortTier) -> EffortBehaviorPolicy {
    match tier {
        EffortTier::Low => EffortBehaviorPolicy {
            tier,
            max_steps: 25,
            max_tool_calls: 60,
            subagents: SubagentAllowance::Never,
            subagent_max_turns: 2,
            read_breadth: 3,
            verification_passes: 0,
            compaction_aggressiveness: 1.0,
            plan_then_execute_eligible: false,
        },
        EffortTier::Medium => EffortBehaviorPolicy {
            tier,
            max_steps: 40,
            max_tool_calls: 100,
            subagents: SubagentAllowance::Conservative,
            subagent_max_turns: 4,
            read_breadth: 6,
            verification_passes: 0,
            compaction_aggressiveness: 1.0,
            plan_then_execute_eligible: false,
        },
        EffortTier::High => EffortBehaviorPolicy {
            tier,
            max_steps: 60,
            max_tool_calls: 150,
            subagents: SubagentAllowance::Conservative,
            subagent_max_turns: 6,
            read_breadth: 10,
            verification_passes: 0,
            compaction_aggressiveness: 1.0,
            plan_then_execute_eligible: false,
        },
        EffortTier::XHigh => EffortBehaviorPolicy {
            tier,
            max_steps: 90,
            max_tool_calls: 250,
            subagents: SubagentAllowance::Parallel,
            subagent_max_turns: 8,
            read_breadth: 15,
            verification_passes: 1,
            compaction_aggressiveness: 0.9,
            plan_then_execute_eligible: true,
        },
        EffortTier::Ultra => EffortBehaviorPolicy {
            tier,
            max_steps: 120,
            max_tool_calls: 400,
            subagents: SubagentAllowance::Parallel,
            subagent_max_turns: 12,
            read_breadth: 25,
            verification_passes: 1,
            compaction_aggressiveness: 0.85,
            plan_then_execute_eligible: true,
        },
    }
}

/// TOML overrides for one tier (`[agent_flow.effort.<tier>]`). Every field
/// is optional; only set fields override the defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EffortTierOverrides {
    pub max_steps: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub subagents: Option<String>,
    pub subagent_max_turns: Option<u32>,
    pub read_breadth: Option<u32>,
    pub verification_passes: Option<u32>,
    pub compaction_aggressiveness: Option<f64>,
    pub plan_then_execute_eligible: Option<bool>,
}

fn parse_positive_u32(raw: Option<String>) -> Option<u32> {
    let value: u32 = raw?.trim().parse().ok()?;
    (value > 0).then_some(value)
}

fn parse_positive_finite_f64(raw: Option<String>) -> Option<f64> {
    let value: f64 = raw?.trim().parse().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Resolves the effective policy for a tier: compiled defaults → TOML
/// overrides → `GROK_LOCAL_EFFORT_<TIER>_*` environment (env wins).
pub fn policy_for_tier(
    tier: EffortTier,
    toml: Option<&EffortTierOverrides>,
    get_env: EnvReader<'_>,
) -> EffortBehaviorPolicy {
    let mut policy = default_policy(tier);
    let prefix = format!("GROK_LOCAL_EFFORT_{}_", tier.env_key());
    let env = |name: &str| get_env(&format!("{prefix}{name}"));

    if let Some(t) = toml {
        if let Some(v) = t.max_steps {
            policy.max_steps = v;
        }
        if let Some(v) = t.max_tool_calls {
            policy.max_tool_calls = v;
        }
        if let Some(v) = t.subagents.as_deref().and_then(SubagentAllowance::parse) {
            policy.subagents = v;
        }
        if let Some(v) = t.subagent_max_turns {
            policy.subagent_max_turns = v;
        }
        if let Some(v) = t.read_breadth {
            policy.read_breadth = v;
        }
        if let Some(v) = t.verification_passes {
            policy.verification_passes = v;
        }
        if let Some(v) = t.compaction_aggressiveness {
            policy.compaction_aggressiveness = v;
        }
        if let Some(v) = t.plan_then_execute_eligible {
            policy.plan_then_execute_eligible = v;
        }
    }

    if let Some(v) = parse_positive_u32(env("MAX_STEPS")) {
        policy.max_steps = v;
    }
    if let Some(v) = parse_positive_u32(env("MAX_TOOL_CALLS")) {
        policy.max_tool_calls = v;
    }
    if let Some(v) = env("SUBAGENTS").and_then(|s| SubagentAllowance::parse(&s)) {
        policy.subagents = v;
    }
    if let Some(v) = parse_positive_u32(env("SUBAGENT_MAX_TURNS")) {
        policy.subagent_max_turns = v;
    }
    if let Some(v) = parse_positive_u32(env("READ_BREADTH")) {
        policy.read_breadth = v;
    }
    if let Some(v) = parse_positive_u32(env("VERIFICATION_PASSES")) {
        policy.verification_passes = v;
    }
    if let Some(v) = parse_positive_finite_f64(env("COMPACTION_AGGRESSIVENESS")) {
        policy.compaction_aggressiveness = v;
    }
    if let Some(v) = parse_bool_env(env("PLAN_ELIGIBLE").as_deref()) {
        policy.plan_then_execute_eligible = v;
    }

    policy
}

/// Whether subagents may be spawned. A missing policy fails open to
/// allowed (current behavior preserved).
pub fn is_subagent_spawning_allowed(policy: Option<&EffortBehaviorPolicy>) -> bool {
    policy
        .map(|p| p.subagents != SubagentAllowance::Never)
        .unwrap_or(true)
}

/// Plan-then-execute eligibility: true when the resolved policy allows it,
/// or the route tier is `heavy` (the "effort xhigh/ultra" policy row maps to
/// heavy routing).
pub fn is_plan_then_execute_eligible(
    policy: Option<&EffortBehaviorPolicy>,
    route_tier: Option<&str>,
) -> bool {
    if policy.map(|p| p.plan_then_execute_eligible).unwrap_or(false) {
        return true;
    }
    matches!(
        route_tier.map(|t| t.trim().to_ascii_lowercase()).as_deref(),
        Some("heavy")
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
    fn effort_tier_parse_spellings() {
        assert_eq!(EffortTier::parse("off"), Some(EffortTier::Low));
        assert_eq!(EffortTier::parse("LOW"), Some(EffortTier::Low));
        assert_eq!(EffortTier::parse("xhigh"), Some(EffortTier::XHigh));
        assert_eq!(EffortTier::parse("x-high"), Some(EffortTier::XHigh));
        assert_eq!(EffortTier::parse("ultra"), Some(EffortTier::Ultra));
        assert_eq!(EffortTier::parse("medium"), Some(EffortTier::Medium));
        assert_eq!(EffortTier::parse("high"), Some(EffortTier::High));
        assert_eq!(EffortTier::parse("turbo"), None);
    }

    #[test]
    fn default_table_matches_zcode_325_values() {
        let low = default_policy(EffortTier::Low);
        assert_eq!((low.max_steps, low.max_tool_calls), (25, 60));
        assert_eq!(low.subagents, SubagentAllowance::Never);
        assert!(!low.plan_then_execute_eligible);

        let medium = default_policy(EffortTier::Medium);
        assert_eq!((medium.max_steps, medium.max_tool_calls), (40, 100));
        assert_eq!(medium.subagents, SubagentAllowance::Conservative);

        let high = default_policy(EffortTier::High);
        assert_eq!((high.max_steps, high.max_tool_calls), (60, 150));

        let xhigh = default_policy(EffortTier::XHigh);
        assert_eq!((xhigh.max_steps, xhigh.max_tool_calls), (90, 250));
        assert_eq!(xhigh.subagents, SubagentAllowance::Parallel);
        assert_eq!(xhigh.verification_passes, 1);
        assert!((xhigh.compaction_aggressiveness - 0.9).abs() < f64::EPSILON);
        assert!(xhigh.plan_then_execute_eligible);

        let ultra = default_policy(EffortTier::Ultra);
        assert_eq!((ultra.max_steps, ultra.max_tool_calls), (120, 400));
        assert_eq!(ultra.subagent_max_turns, 12);
        assert_eq!(ultra.read_breadth, 25);
        assert!((ultra.compaction_aggressiveness - 0.85).abs() < f64::EPSILON);
        assert!(ultra.plan_then_execute_eligible);
    }

    #[test]
    fn env_overrides_beat_toml_overrides_beat_defaults() {
        let vars = env_of(&[
            ("GROK_LOCAL_EFFORT_HIGH_MAX_STEPS", "77"),
            ("GROK_LOCAL_EFFORT_HIGH_SUBAGENTS", "parallel"),
        ]);
        let get_env = |k: &str| vars.get(k).cloned();
        let toml = EffortTierOverrides {
            max_steps: Some(42),
            max_tool_calls: Some(4242),
            ..Default::default()
        };
        let policy = policy_for_tier(EffortTier::High, Some(&toml), &get_env);
        assert_eq!(policy.max_steps, 77); // env wins over TOML
        assert_eq!(policy.max_tool_calls, 4242); // TOML wins over default
        assert_eq!(policy.subagents, SubagentAllowance::Parallel); // env
        assert_eq!(policy.subagent_max_turns, 6); // default untouched
    }

    #[test]
    fn invalid_env_values_are_ignored() {
        let vars = env_of(&[
            ("GROK_LOCAL_EFFORT_LOW_MAX_STEPS", "0"),
            ("GROK_LOCAL_EFFORT_LOW_MAX_TOOL_CALLS", "-5"),
            ("GROK_LOCAL_EFFORT_LOW_SUBAGENTS", "sometimes"),
        ]);
        let get_env = |k: &str| vars.get(k).cloned();
        let policy = policy_for_tier(EffortTier::Low, None, &get_env);
        let def = default_policy(EffortTier::Low);
        assert_eq!(policy.max_steps, def.max_steps);
        assert_eq!(policy.max_tool_calls, def.max_tool_calls);
        assert_eq!(policy.subagents, def.subagents);
    }

    #[test]
    fn toml_off_does_not_clobber_defaults() {
        let get_env = |_: &str| None;
        let toml = EffortTierOverrides::default();
        let policy = policy_for_tier(EffortTier::Ultra, Some(&toml), &get_env);
        let def = default_policy(EffortTier::Ultra);
        assert_eq!(policy.max_steps, def.max_steps);
        assert_eq!(policy.max_tool_calls, def.max_tool_calls);
    }

    #[test]
    fn subagent_allowance_parse() {
        assert_eq!(SubagentAllowance::parse("never"), Some(SubagentAllowance::Never));
        assert_eq!(
            SubagentAllowance::parse("Conservative"),
            Some(SubagentAllowance::Conservative)
        );
        assert_eq!(
            SubagentAllowance::parse("parallel"),
            Some(SubagentAllowance::Parallel)
        );
        assert_eq!(SubagentAllowance::parse("maybe"), None);
    }

    #[test]
    fn subagent_spawning_allowed_matrix() {
        assert!(!is_subagent_spawning_allowed(Some(&default_policy(
            EffortTier::Low
        ))));
        assert!(is_subagent_spawning_allowed(Some(&default_policy(
            EffortTier::Medium
        ))));
        assert!(is_subagent_spawning_allowed(Some(&default_policy(
            EffortTier::Ultra
        ))));
        // Missing policy fails open.
        assert!(is_subagent_spawning_allowed(None));
    }

    #[test]
    fn plan_execute_eligibility_matrix() {
        assert!(!is_plan_then_execute_eligible(None, Some("balanced")));
        assert!(is_plan_then_execute_eligible(None, Some("heavy")));
        assert!(is_plan_then_execute_eligible(
            Some(&default_policy(EffortTier::XHigh)),
            Some("economy")
        ));
        assert!(!is_plan_then_execute_eligible(
            Some(&default_policy(EffortTier::Low)),
            Some("economy")
        ));
    }
}
