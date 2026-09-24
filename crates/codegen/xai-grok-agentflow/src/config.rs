//! `$GROK_HOME/config.toml` `[agent_flow]` section plus
//! `GROK_LOCAL_*` environment overrides.
//!
//! Precedence (low → high): compiled defaults → `[agent_flow]` TOML →
//! `GROK_LOCAL_*` environment. Nothing here calls inference; this is pure
//! configuration resolution.
//!
//! Example TOML:
//!
//! ```toml
//! [agent_flow]
//! enabled = true
//! prune = true
//! prune_confidence = 0.6
//! boundary_watermark = 0.55
//! archive_keep = 5
//! budget_enforce = true
//! plan_execute = true
//! anchored_compact = true
//! doom_loop = true
//!
//! [agent_flow.effort.xhigh]
//! max_steps = 90
//! max_tool_calls = 250
//! subagents = "parallel"
//!
//! [agent_flow.budgets.heavy]
//! max_steps = 200
//! max_tool_calls = 300
//! ```

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::budgets::{TurnBudgets, resolve_env_turn_budgets};
use crate::effort::{
    EffortBehaviorPolicy, EffortTier, EffortTierOverrides, default_policy, policy_for_tier,
};
use crate::{EnvReader, is_env_disabled};

fn default_true() -> bool {
    true
}
fn default_prune_confidence() -> f64 {
    crate::tool_packs::TOOL_PACK_PRUNE_CONFIDENCE_THRESHOLD
}
fn default_boundary_watermark() -> f64 {
    crate::anchored_compaction::BOUNDARY_COMPACT_WATERMARK_DEFAULT
}
fn default_archive_keep() -> usize {
    crate::anchored_compaction::ARCHIVE_KEEP_COUNT_DEFAULT
}

/// One tier/dimension pair of turn budgets from TOML.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BudgetPairConfig {
    pub max_steps: Option<u32>,
    pub max_tool_calls: Option<u32>,
}

/// TOML budget table (`[agent_flow.budgets.<tier>]` plus `default`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BudgetTableConfig {
    pub economy: BudgetPairConfig,
    pub balanced: BudgetPairConfig,
    pub heavy: BudgetPairConfig,
    pub default: BudgetPairConfig,
}

/// The `[agent_flow]` section. Every field has a default, so a missing or
/// partial section still resolves.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentFlowConfig {
    /// Master switch for the agent-flow features.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tool-pack pruning switch.
    #[serde(default = "default_true")]
    pub prune: bool,
    /// Confidence floor for tool-pack pruning.
    #[serde(default = "default_prune_confidence")]
    pub prune_confidence: f64,
    /// Budget enforcement ladder switch.
    #[serde(default = "default_true")]
    pub budget_enforce: bool,
    /// Plan-then-execute gate switch.
    #[serde(default = "default_true")]
    pub plan_execute: bool,
    /// Anchored compaction switch.
    #[serde(default = "default_true")]
    pub anchored_compact: bool,
    /// Doom-loop escalation switch.
    #[serde(default = "default_true")]
    pub doom_loop: bool,
    /// Token-pressure watermark for boundary-triggered compaction.
    #[serde(default = "default_boundary_watermark")]
    pub boundary_watermark: f64,
    /// Transcript archives kept per session.
    #[serde(default = "default_archive_keep")]
    pub archive_keep: usize,
    /// Per-tier effort behavior overrides (`[agent_flow.effort.<tier>]`).
    #[serde(default)]
    pub effort: HashMap<String, EffortTierOverrides>,
    /// Turn budget table (`[agent_flow.budgets.<tier>]`).
    #[serde(default)]
    pub budgets: BudgetTableConfig,
}

impl Default for AgentFlowConfig {
    fn default() -> Self {
        AgentFlowConfig {
            enabled: true,
            prune: true,
            prune_confidence: default_prune_confidence(),
            budget_enforce: true,
            plan_execute: true,
            anchored_compact: true,
            doom_loop: true,
            boundary_watermark: default_boundary_watermark(),
            archive_keep: default_archive_keep(),
            effort: HashMap::new(),
            budgets: BudgetTableConfig::default(),
        }
    }
}

impl AgentFlowConfig {
    /// Loads from `$GROK_HOME/config.toml` (`[agent_flow]` section), then
    /// applies `GROK_LOCAL_*` env overrides. Never fails: missing or
    /// invalid config yields defaults.
    pub fn load() -> Self {
        Self::load_from_env(&xai_dirs::grok_home(), &|key| std::env::var(key).ok())
    }

    /// Testable variant with an explicit home directory and env reader.
    pub fn load_from_env(grok_home: &Path, get_env: EnvReader<'_>) -> Self {
        let mut config = Self::default();
        if let Ok(text) = std::fs::read_to_string(grok_home.join("config.toml"))
            && let Ok(table) = text.parse::<toml::Table>()
            && let Some(section) = table.get("agent_flow")
            && let Ok(parsed) = AgentFlowConfig::deserialize(section.clone())
        {
            config = parsed;
        }
        config.apply_env(get_env);
        config
    }

    /// Applies `GROK_LOCAL_*` environment overrides (env wins over TOML).
    pub fn apply_env(&mut self, get_env: EnvReader<'_>) {
        if get_env("GROK_LOCAL_AGENTFLOW").as_deref() == Some("0") {
            self.enabled = false;
        }
        if get_env("GROK_LOCAL_AGENTFLOW_PRUNE").as_deref() == Some("0") {
            self.prune = false;
        }
        if is_env_disabled(get_env("GROK_LOCAL_BUDGET_ENFORCE").as_deref()) {
            self.budget_enforce = false;
        }
        if is_env_disabled(get_env("GROK_LOCAL_PLAN_EXECUTE").as_deref()) {
            self.plan_execute = false;
        }
        if is_env_disabled(get_env("GROK_LOCAL_ANCHORED_COMPACT").as_deref()) {
            self.anchored_compact = false;
        }
        if is_env_disabled(get_env("GROK_LOCAL_DOOM_LOOP").as_deref()) {
            self.doom_loop = false;
        }
        if let Some(raw) = get_env("GROK_LOCAL_PRUNE_CONFIDENCE")
            && let Ok(value) = raw.trim().parse::<f64>()
            && value.is_finite()
            && value > 0.0
            && value <= 1.0
        {
            self.prune_confidence = value;
        }
    }

    /// Resolves the effort behavior policy for an effort name (accepts the
    /// grok-local `Effort` spellings; unknown → Medium policy).
    pub fn policy_for_effort(
        &self,
        effort_name: &str,
        get_env: EnvReader<'_>,
    ) -> EffortBehaviorPolicy {
        let tier = EffortTier::parse(effort_name).unwrap_or(EffortTier::Medium);
        policy_for_tier(tier, self.effort.get(tier.key()), get_env)
    }

    /// The config-driven replacement for `Effort::canonical_caps()`:
    /// `(max_steps, max_tool_calls)` for an effort name.
    pub fn effort_caps(&self, effort_name: &str, get_env: EnvReader<'_>) -> (u32, u32) {
        let policy = self.policy_for_effort(effort_name, get_env);
        (policy.max_steps, policy.max_tool_calls)
    }

    fn toml_budgets_for_tier(&self, route_tier: Option<&str>) -> TurnBudgets {
        let pair = match route_tier.map(|t| t.trim().to_ascii_lowercase()).as_deref() {
            Some("economy") | Some("edge") => &self.budgets.economy,
            Some("balanced") => &self.budgets.balanced,
            Some("heavy") => &self.budgets.heavy,
            _ => &self.budgets.default,
        };
        TurnBudgets {
            max_steps: pair.max_steps.or(self.budgets.default.max_steps),
            max_tool_calls: pair.max_tool_calls.or(self.budgets.default.max_tool_calls),
        }
    }

    /// Resolves the effective turn budgets: env → TOML table → effort
    /// policy (via `effort_name`). A dimension stays unbounded when no
    /// layer provides it.
    pub fn resolve_budgets(
        &self,
        route_tier: Option<&str>,
        effort_name: Option<&str>,
        get_env: EnvReader<'_>,
    ) -> TurnBudgets {
        let env_budgets = resolve_env_turn_budgets(route_tier, get_env);
        let toml_budgets = self.toml_budgets_for_tier(route_tier);
        let policy = effort_name.map(|name| self.policy_for_effort(name, get_env));
        TurnBudgets {
            max_steps: env_budgets
                .max_steps
                .or(toml_budgets.max_steps)
                .or_else(|| policy.as_ref().map(|p| p.max_steps)),
            max_tool_calls: env_budgets
                .max_tool_calls
                .or(toml_budgets.max_tool_calls)
                .or_else(|| policy.as_ref().map(|p| p.max_tool_calls)),
        }
    }

    /// Default policy for a tier with no overrides (useful for tests).
    pub fn default_tier_policy(tier: EffortTier) -> EffortBehaviorPolicy {
        default_policy(tier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_load_from_missing_config() {
        let home = tempfile::tempdir().unwrap();
        let config = AgentFlowConfig::load_from_env(home.path(), &|_| None);
        assert!(config.enabled);
        assert!(config.prune);
        assert!(config.budget_enforce);
        assert!(config.plan_execute);
        assert!(config.anchored_compact);
        assert!(config.doom_loop);
        assert!((config.prune_confidence - 0.6).abs() < f64::EPSILON);
        assert!((config.boundary_watermark - 0.55).abs() < f64::EPSILON);
        assert_eq!(config.archive_keep, 5);
    }

    #[test]
    fn toml_section_parses_and_env_wins() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[agent_flow]\nprune = false\nprune_confidence = 0.8\n\n\
             [agent_flow.effort.xhigh]\nmax_steps = 42\n\n\
             [agent_flow.budgets.heavy]\nmax_steps = 200\n",
        )
        .unwrap();
        let config = AgentFlowConfig::load_from_env(home.path(), &|_| None);
        assert!(!config.prune);
        assert!((config.prune_confidence - 0.8).abs() < f64::EPSILON);
        assert!(config.budget_enforce); // untouched default

        let get_env = |k: &str| {
            if k == "GROK_LOCAL_AGENTFLOW_PRUNE" {
                Some("1".to_string())
            } else {
                None
            }
        };
        // prune=false in TOML, but env only disables (never re-enables) — stays false.
        let config = AgentFlowConfig::load_from_env(home.path(), &get_env);
        assert!(!config.prune);

        let get_env = |k: &str| {
            if k == "GROK_LOCAL_BUDGET_ENFORCE" {
                Some("0".to_string())
            } else {
                None
            }
        };
        let config = AgentFlowConfig::load_from_env(home.path(), &get_env);
        assert!(!config.budget_enforce);
    }

    #[test]
    fn effort_policy_reads_toml_overrides() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[agent_flow.effort.xhigh]\nmax_steps = 42\nsubagents = \"never\"\n",
        )
        .unwrap();
        let config = AgentFlowConfig::load_from_env(home.path(), &|_| None);
        let policy = config.policy_for_effort("xhigh", &|_| None);
        assert_eq!(policy.max_steps, 42);
        assert_eq!(policy.subagents, crate::effort::SubagentAllowance::Never);
        // Untouched fields keep compiled defaults.
        assert_eq!(policy.max_tool_calls, 250);
    }

    #[test]
    fn budget_resolution_layers_env_toml_policy() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[agent_flow.budgets.heavy]\nmax_steps = 200\n",
        )
        .unwrap();
        let config = AgentFlowConfig::load_from_env(home.path(), &|_| None);
        // TOML provides steps; policy provides tool calls.
        let budgets = config.resolve_budgets(Some("heavy"), Some("xhigh"), &|_| None);
        assert_eq!(budgets.max_steps, Some(200));
        assert_eq!(budgets.max_tool_calls, Some(250));

        // Env beats TOML.
        let get_env =
            |k: &str| (k == "GROK_LOCAL_BUDGET_HEAVY_MAX_STEPS").then(|| "11".to_string());
        let budgets = config.resolve_budgets(Some("heavy"), Some("xhigh"), &get_env);
        assert_eq!(budgets.max_steps, Some(11));
    }

    #[test]
    fn effort_caps_follow_config() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[agent_flow.effort.medium]\nmax_steps = 7\nmax_tool_calls = 8\n",
        )
        .unwrap();
        let config = AgentFlowConfig::load_from_env(home.path(), &|_| None);
        assert_eq!(config.effort_caps("medium", &|_| None), (7, 8));
        // Unknown effort names fall back to the medium policy.
        assert_eq!(config.effort_caps("turbo", &|_| None).0, 7);
    }

    #[test]
    fn invalid_toml_falls_back_to_defaults() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "not valid toml [[[").unwrap();
        let config = AgentFlowConfig::load_from_env(home.path(), &|_| None);
        assert!(config.enabled);
        assert!((config.prune_confidence - 0.6).abs() < f64::EPSILON);
    }
}
