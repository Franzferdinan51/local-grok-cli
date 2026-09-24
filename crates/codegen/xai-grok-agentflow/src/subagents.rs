//! Cost-aware subagent policy (ZCode 3.25.0 Rank 10 parity).
//!
//! Every spawned subagent forks the full fixed session-prompt overhead, so
//! spawning is gated by the effort behavior policy instead of being free:
//! low effort never spawns, conservative tiers spawn only for clearly
//! parallelizable work, and parallel tiers may fan out when the task
//! decomposes. The default posture is in-loop parallel tool calls; subagents
//! are reserved for genuinely broad, independent investigations.
//!
//! Everything here is pure and fail-open: when the policy is missing, the
//! caller gets the permissive historical behavior (allowed) plus the
//! cost-caution guidance, never a broken turn. This module never spawns
//! anything and never touches inference.

use crate::effort::{EffortBehaviorPolicy, SubagentAllowance};
use crate::{EnvReader, is_env_disabled};

/// Kill switch: a falsy value disables agent-flow subagent guidance entirely.
pub const SUBAGENTS_ENV: &str = "GROK_LOCAL_SUBAGENTS";

/// Fallback child turn budget when no effort policy is available (fail-open).
pub const SUBAGENT_CHILD_TURNS_FALLBACK: u32 = 4;

/// True when a falsy env spelling disables subagent guidance.
pub fn is_subagents_disabled(get_env: EnvReader<'_>) -> bool {
    is_env_disabled(get_env(SUBAGENTS_ENV).as_deref())
}

/// Why a spawn was allowed or denied. The string form is log/telemetry-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentSpawnReason {
    /// Parallel tier: the task decomposes, fan out.
    AllowedParallel,
    /// Conservative tier with enough independent units to justify the fork cost.
    AllowedConservative,
    /// Fail-open: no policy available, historical behavior preserved.
    AllowedFailOpen,
    /// Effort tier forbids spawning (e.g. low effort).
    DeniedTierPolicy,
    /// Spawn depth cap reached.
    DeniedDepthCap,
    /// Conservative tier without enough independent work to justify the cost.
    DeniedNoDecomposition,
    /// Kill-switched via [`SUBAGENTS_ENV`].
    DeniedDisabled,
}

impl SubagentSpawnReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SubagentSpawnReason::AllowedParallel => "allowed_parallel",
            SubagentSpawnReason::AllowedConservative => "allowed_conservative",
            SubagentSpawnReason::AllowedFailOpen => "allowed_fail_open",
            SubagentSpawnReason::DeniedTierPolicy => "denied_tier_policy",
            SubagentSpawnReason::DeniedDepthCap => "denied_depth_cap",
            SubagentSpawnReason::DeniedNoDecomposition => "denied_no_decomposition",
            SubagentSpawnReason::DeniedDisabled => "denied_disabled",
        }
    }

    pub fn allowed(self) -> bool {
        matches!(
            self,
            SubagentSpawnReason::AllowedParallel
                | SubagentSpawnReason::AllowedConservative
                | SubagentSpawnReason::AllowedFailOpen
        )
    }
}

/// What the caller knows about a prospective spawn.
pub struct SubagentSpawnRequest<'a> {
    /// Short task summary (drives the child brief, never sent anywhere).
    pub task_summary: &'a str,
    /// How many independent units the task decomposes into (0 = unknown).
    pub parallel_units: u32,
    /// Depth of the requesting session (0 = top-level session).
    pub parent_depth: u32,
    /// Maximum allowed spawn depth.
    pub max_depth: u32,
}

/// The spawn decision plus the guidance text the caller should surface.
pub struct SubagentSpawnDecision {
    pub allowed: bool,
    pub reason: SubagentSpawnReason,
    /// Turn budget for the child session (from the effort policy).
    pub child_max_turns: u32,
    /// Reminder text for the *parent* (cost discipline / disabled notice).
    pub parent_guidance: Option<String>,
    /// First-message brief for the *child* (budget + reporting contract).
    pub child_brief: Option<String>,
}

/// Decide whether a subagent spawn is allowed. Pure; never spawns.
///
/// Precedence: kill switch → depth cap → effort-tier allowance. A missing
/// policy fails open to the historical behavior (allowed) with cost-caution
/// guidance attached.
pub fn decide_subagent_spawn(
    policy: Option<&EffortBehaviorPolicy>,
    request: &SubagentSpawnRequest<'_>,
    get_env: EnvReader<'_>,
) -> SubagentSpawnDecision {
    if is_subagents_disabled(get_env) {
        return SubagentSpawnDecision {
            allowed: false,
            reason: SubagentSpawnReason::DeniedDisabled,
            child_max_turns: 0,
            parent_guidance: None,
            child_brief: None,
        };
    }
    if request.parent_depth >= request.max_depth {
        return SubagentSpawnDecision {
            allowed: false,
            reason: SubagentSpawnReason::DeniedDepthCap,
            child_max_turns: 0,
            parent_guidance: Some(format!(
                "Subagent depth cap reached ({}/{}). Finish the remaining work in this session instead of delegating further.",
                request.parent_depth, request.max_depth
            )),
            child_brief: None,
        };
    }
    let Some(policy) = policy else {
        // Fail-open: no policy (routing skipped, config missing) — keep the
        // historical behavior but attach the cost-caution guidance.
        let child_max_turns = SUBAGENT_CHILD_TURNS_FALLBACK;
        return SubagentSpawnDecision {
            allowed: true,
            reason: SubagentSpawnReason::AllowedFailOpen,
            child_max_turns,
            parent_guidance: Some(build_subagent_cost_guidance(
                SubagentAllowance::Conservative,
            )),
            child_brief: Some(build_subagent_child_brief(
                request.task_summary,
                child_max_turns,
            )),
        };
    };
    let child_max_turns = policy.subagent_max_turns;
    match policy.subagents {
        SubagentAllowance::Never => SubagentSpawnDecision {
            allowed: false,
            reason: SubagentSpawnReason::DeniedTierPolicy,
            child_max_turns: 0,
            parent_guidance: Some(build_subagent_cost_guidance(SubagentAllowance::Never)),
            child_brief: None,
        },
        SubagentAllowance::Conservative => {
            if request.parallel_units >= 2 {
                SubagentSpawnDecision {
                    allowed: true,
                    reason: SubagentSpawnReason::AllowedConservative,
                    child_max_turns,
                    parent_guidance: Some(build_subagent_cost_guidance(
                        SubagentAllowance::Conservative,
                    )),
                    child_brief: Some(build_subagent_child_brief(
                        request.task_summary,
                        child_max_turns,
                    )),
                }
            } else {
                SubagentSpawnDecision {
                    allowed: false,
                    reason: SubagentSpawnReason::DeniedNoDecomposition,
                    child_max_turns: 0,
                    parent_guidance: Some(
                        "This work does not decompose into enough independent units to justify \
                         a subagent fork at the current effort tier. Do it in this session with \
                         parallel tool calls instead."
                            .to_string(),
                    ),
                    child_brief: None,
                }
            }
        }
        SubagentAllowance::Parallel => SubagentSpawnDecision {
            allowed: true,
            reason: SubagentSpawnReason::AllowedParallel,
            child_max_turns,
            parent_guidance: None,
            child_brief: Some(build_subagent_child_brief(
                request.task_summary,
                child_max_turns,
            )),
        },
    }
}

/// Parent-facing reminder text for the current allowance tier.
///
/// - `Never` → spawning is disabled; do the work in-session.
/// - `Conservative`/`Parallel` → cost discipline: prefer in-loop parallel
///   tool calls, reserve subagents for broad independent investigations.
pub fn build_subagent_cost_guidance(allowance: SubagentAllowance) -> String {
    match allowance {
        SubagentAllowance::Never => "Subagent spawning is disabled at the current effort tier. \
            Do the work in this session with parallel tool calls instead of delegating."
            .to_string(),
        SubagentAllowance::Conservative => "Subagent cost discipline: each spawned subagent forks \
            the full session prompt overhead, so prefer in-loop parallel tool calls for work you \
            can do directly. Spawn subagents only for genuinely broad, independent investigations \
            that decompose into parallel units — and keep each delegation narrow with a concrete \
            deliverable."
            .to_string(),
        SubagentAllowance::Parallel => "Subagent fan-out is allowed at the current effort tier. \
            Still prefer in-loop parallel tool calls for anything you can do directly; reserve \
            subagents for independent investigations that genuinely run in parallel."
            .to_string(),
    }
}

/// First-message brief for a spawned child: hard turn budget plus the
/// reporting contract. Keeps children cheap and their results actionable.
pub fn build_subagent_child_brief(task_summary: &str, child_max_turns: u32) -> String {
    let task = task_summary.trim();
    let task_line = if task.is_empty() {
        "the delegated investigation".to_string()
    } else {
        let short: String = task.chars().take(400).collect();
        format!("{short}")
    };
    format!(
        "You are a subagent with a hard budget of {child_max_turns} tool-use turns for: {task_line}\n\n\
         Work autonomously within that budget. Report back concisely: what you did, the key \
         findings, and the file paths involved. Do not spawn further subagents; if the work does \
         not fit the budget, report partial results and what remains."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effort::{EffortTier, default_policy};
    use std::collections::HashMap;

    fn reader(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn req(parallel_units: u32) -> SubagentSpawnRequest<'static> {
        SubagentSpawnRequest {
            task_summary: "investigate the auth flow",
            parallel_units,
            parent_depth: 0,
            max_depth: 3,
        }
    }

    #[test]
    fn never_tier_denies_with_guidance() {
        let policy = default_policy(EffortTier::Low);
        assert_eq!(policy.subagents, SubagentAllowance::Never);
        let r = reader(&[]);
        let d = decide_subagent_spawn(Some(&policy), &req(4), &r);
        assert!(!d.allowed);
        assert_eq!(d.reason, SubagentSpawnReason::DeniedTierPolicy);
        assert!(d.parent_guidance.is_some());
        assert!(d.child_brief.is_none());
    }

    #[test]
    fn conservative_needs_decomposition() {
        let policy = default_policy(EffortTier::Medium);
        let r = reader(&[]);
        let denied = decide_subagent_spawn(Some(&policy), &req(1), &r);
        assert!(!denied.allowed);
        assert_eq!(denied.reason, SubagentSpawnReason::DeniedNoDecomposition);
        let allowed = decide_subagent_spawn(Some(&policy), &req(3), &r);
        assert!(allowed.allowed);
        assert_eq!(allowed.reason, SubagentSpawnReason::AllowedConservative);
        assert_eq!(allowed.child_max_turns, policy.subagent_max_turns);
        assert!(allowed.child_brief.unwrap().contains("hard budget"));
    }

    #[test]
    fn parallel_tier_allows_freely() {
        let policy = default_policy(EffortTier::Ultra);
        let r = reader(&[]);
        let d = decide_subagent_spawn(Some(&policy), &req(0), &r);
        assert!(d.allowed);
        assert_eq!(d.reason, SubagentSpawnReason::AllowedParallel);
        assert!(d.parent_guidance.is_none());
    }

    #[test]
    fn depth_cap_denies() {
        let policy = default_policy(EffortTier::Ultra);
        let r = reader(&[]);
        let request = SubagentSpawnRequest {
            parent_depth: 3,
            max_depth: 3,
            ..req(5)
        };
        let d = decide_subagent_spawn(Some(&policy), &request, &r);
        assert!(!d.allowed);
        assert_eq!(d.reason, SubagentSpawnReason::DeniedDepthCap);
    }

    #[test]
    fn kill_switch_denies() {
        let policy = default_policy(EffortTier::Ultra);
        let r = reader(&[(SUBAGENTS_ENV, "0")]);
        let d = decide_subagent_spawn(Some(&policy), &req(5), &r);
        assert!(!d.allowed);
        assert_eq!(d.reason, SubagentSpawnReason::DeniedDisabled);
    }

    #[test]
    fn missing_policy_fails_open() {
        let r = reader(&[]);
        let d = decide_subagent_spawn(None, &req(1), &r);
        assert!(d.allowed);
        assert_eq!(d.reason, SubagentSpawnReason::AllowedFailOpen);
        assert_eq!(d.child_max_turns, SUBAGENT_CHILD_TURNS_FALLBACK);
        assert!(d.parent_guidance.is_some());
    }

    #[test]
    fn reason_strings_round_trip() {
        assert!(SubagentSpawnReason::AllowedParallel.allowed());
        assert!(!SubagentSpawnReason::DeniedTierPolicy.allowed());
        assert_eq!(
            SubagentSpawnReason::DeniedNoDecomposition.as_str(),
            "denied_no_decomposition"
        );
    }

    #[test]
    fn child_brief_handles_empty_task() {
        let brief = build_subagent_child_brief("   ", 4);
        assert!(brief.contains("hard budget of 4"));
        assert!(brief.contains("delegated investigation"));
    }
}
