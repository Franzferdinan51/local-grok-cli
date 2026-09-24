//! Budget enforcement state machine.
//!
//! Ports ZCode 3.25.0's soft/hard turn-budget ladder:
//!
//! 1. At ceil(80%) of any cap: inject a one-time warning reminder.
//! 2. At 100% of any cap: inject a forced wrap-up / strategy-change warning
//!    and grant exactly one more model step.
//! 3. On the next model step after the escalation: stop the turn with an
//!    explicit resumable-state message and a resume offer.
//!
//! The explicit user's `max_turns` always wins over the routed cap; that
//! precedence is enforced by the caller when it resolves the effective
//! limit. The whole ladder is disabled by `GROK_LOCAL_BUDGET_ENFORCE=0`.

use crate::EnvReader;
use crate::budgets::{BudgetHit, BudgetUsage, TurnBudgets, check_budget_hit, warn_threshold};

/// Warning is injected once at ceil(80%) of the cap.
pub const BUDGET_WARN_FRACTION: f64 = 0.8;

/// Kill switch: set to `0` to disable budget enforcement entirely.
pub const BUDGET_ENFORCE_KILL_SWITCH_ENV: &str = "GROK_LOCAL_BUDGET_ENFORCE";

/// Per-turn ladder progress. Starts zeroed; survives across model steps of
/// one turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetStageState {
    /// The 80% warning was already injected.
    pub warned: bool,
    /// The 100% escalation was already injected (one more step granted).
    pub escalated: bool,
}

/// What the enforcement step decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetEnforcementAction {
    /// Budget fine (or no budget configured) — keep going.
    Ok,
    /// Reached ceil(80%) — inject the warning reminder once.
    Warn,
    /// Reached 100% — inject the forced wrap-up and grant one more step.
    Escalate,
    /// Already escalated and still over cap — stop the turn.
    Stop,
}

/// Context for building human-readable budget messages.
#[derive(Debug, Clone)]
pub struct BudgetMessageContext {
    pub steps: u64,
    pub tool_calls: u64,
    pub max_steps: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub tier: Option<String>,
}

impl BudgetMessageContext {
    pub fn from_usage_tier(usage: BudgetUsage, budgets: TurnBudgets, tier: Option<&str>) -> Self {
        BudgetMessageContext {
            steps: usage.steps,
            tool_calls: usage.tool_calls,
            max_steps: budgets.max_steps,
            max_tool_calls: budgets.max_tool_calls,
            tier: tier.map(str::to_string),
        }
    }
}

/// Evaluates the ladder for one model step. Check order matters: a jump
/// straight past the warn line lands on escalate, and an already-escalated
/// turn over cap stops.
pub fn evaluate_budget_enforcement(
    usage: BudgetUsage,
    budgets: TurnBudgets,
    stage: &BudgetStageState,
) -> BudgetEnforcementAction {
    let hit: BudgetHit = check_budget_hit(usage, budgets);
    if hit.any_hit() {
        return if stage.escalated {
            BudgetEnforcementAction::Stop
        } else {
            BudgetEnforcementAction::Escalate
        };
    }
    if !stage.warned {
        let warn_steps = budgets
            .max_steps
            .is_some_and(|cap| usage.steps >= warn_threshold(cap));
        let warn_calls = budgets
            .max_tool_calls
            .is_some_and(|cap| usage.tool_calls >= warn_threshold(cap));
        if warn_steps || warn_calls {
            return BudgetEnforcementAction::Warn;
        }
    }
    BudgetEnforcementAction::Ok
}

/// Advances the stage state after an action was handled.
pub fn apply_budget_action(stage: &mut BudgetStageState, action: BudgetEnforcementAction) {
    match action {
        BudgetEnforcementAction::Warn => stage.warned = true,
        BudgetEnforcementAction::Escalate => {
            stage.warned = true;
            stage.escalated = true;
        }
        BudgetEnforcementAction::Ok | BudgetEnforcementAction::Stop => {}
    }
}

/// `GROK_LOCAL_BUDGET_ENFORCE=0` disables the whole ladder.
pub fn is_budget_enforcement_disabled(get_env: EnvReader<'_>) -> bool {
    get_env(BUDGET_ENFORCE_KILL_SWITCH_ENV).as_deref() == Some("0")
}

fn cap_line(max_steps: Option<u32>, max_tool_calls: Option<u32>) -> String {
    format!(
        "({} steps, {} tool calls)",
        max_steps
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unbounded".to_string()),
        max_tool_calls
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unbounded".to_string())
    )
}

/// Reminder body for the 80% warning.
pub fn build_budget_warning_body(ctx: &BudgetMessageContext) -> String {
    format!(
        "Turn budget 80% used: {}/{} steps, {}/{} tool calls {} ({} effort). \
         Wrap up efficiently: avoid starting new exploration threads, reuse results you already have, \
         and steer toward the final answer.",
        ctx.steps,
        ctx.max_steps.map(|n| n.to_string()).unwrap_or_default(),
        ctx.tool_calls,
        ctx.max_tool_calls
            .map(|n| n.to_string())
            .unwrap_or_default(),
        cap_line(ctx.max_steps, ctx.max_tool_calls),
        ctx.tier.as_deref().unwrap_or("routed")
    )
}

/// Reminder body for the 100% escalation (one more model step granted).
pub fn build_budget_escalation_body(ctx: &BudgetMessageContext) -> String {
    format!(
        "Turn budget exhausted: {}/{} steps, {}/{} tool calls {}. \
         CHANGE STRATEGY: you have exactly one more model step before this turn is stopped. \
         Do not start new tool exploration. Synthesize what you already have into the final answer now, \
         and note anything left undone.",
        ctx.steps,
        ctx.max_steps.map(|n| n.to_string()).unwrap_or_default(),
        ctx.tool_calls,
        ctx.max_tool_calls
            .map(|n| n.to_string())
            .unwrap_or_default(),
        cap_line(ctx.max_steps, ctx.max_tool_calls)
    )
}

/// User-visible stop message with an explicit resume offer.
pub fn build_budget_exhausted_body(ctx: &BudgetMessageContext) -> String {
    format!(
        "Stopped: turn budget exhausted ({}/{} steps, {}/{} tool calls {}). \
         The session state is saved and this turn is resumable — send another message \
         (for example \"continue\") to pick up where it left off. To allow longer turns, \
         raise the caps with GROK_LOCAL_BUDGET_<TIER>_MAX_STEPS / GROK_LOCAL_BUDGET_<TIER>_MAX_TOOL_CALLS \
         (TIER = ECONOMY, BALANCED, or HEAVY), GROK_LOCAL_BUDGET_DEFAULT_*, the [agent_flow] config, \
         or your explicit max_turns setting, which always wins over the routed budget.",
        ctx.steps,
        ctx.max_steps.map(|n| n.to_string()).unwrap_or_default(),
        ctx.tool_calls,
        ctx.max_tool_calls
            .map(|n| n.to_string())
            .unwrap_or_default(),
        cap_line(ctx.max_steps, ctx.max_tool_calls)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGETS: TurnBudgets = TurnBudgets {
        max_steps: Some(25),
        max_tool_calls: Some(60),
    };

    fn ctx(usage: BudgetUsage) -> BudgetMessageContext {
        BudgetMessageContext::from_usage_tier(usage, BUDGETS, Some("balanced"))
    }

    #[test]
    fn ladder_warn_once_then_escalate_then_stop() {
        let mut stage = BudgetStageState::default();
        // 80% of 25 = 20 → warn.
        let mut action = evaluate_budget_enforcement(
            BudgetUsage {
                steps: 20,
                tool_calls: 0,
            },
            BUDGETS,
            &stage,
        );
        assert_eq!(action, BudgetEnforcementAction::Warn);
        apply_budget_action(&mut stage, action);
        // Still at warn line: no second warning.
        action = evaluate_budget_enforcement(
            BudgetUsage {
                steps: 21,
                tool_calls: 0,
            },
            BUDGETS,
            &stage,
        );
        assert_eq!(action, BudgetEnforcementAction::Ok);
        // Cap hit → escalate.
        action = evaluate_budget_enforcement(
            BudgetUsage {
                steps: 25,
                tool_calls: 0,
            },
            BUDGETS,
            &stage,
        );
        assert_eq!(action, BudgetEnforcementAction::Escalate);
        apply_budget_action(&mut stage, action);
        // One more step granted, still over cap → stop.
        action = evaluate_budget_enforcement(
            BudgetUsage {
                steps: 26,
                tool_calls: 0,
            },
            BUDGETS,
            &stage,
        );
        assert_eq!(action, BudgetEnforcementAction::Stop);
    }

    #[test]
    fn jump_past_warn_goes_straight_to_escalate() {
        let stage = BudgetStageState::default();
        let action = evaluate_budget_enforcement(
            BudgetUsage {
                steps: 25,
                tool_calls: 60,
            },
            BUDGETS,
            &stage,
        );
        assert_eq!(action, BudgetEnforcementAction::Escalate);
    }

    #[test]
    fn tool_call_dimension_drives_the_ladder_too() {
        let stage = BudgetStageState::default();
        // 80% of 60 = 48 → warn on the tool-call dimension alone.
        let action = evaluate_budget_enforcement(
            BudgetUsage {
                steps: 0,
                tool_calls: 48,
            },
            BUDGETS,
            &stage,
        );
        assert_eq!(action, BudgetEnforcementAction::Warn);
    }

    #[test]
    fn empty_budget_never_fires() {
        let stage = BudgetStageState::default();
        for usage in [
            BudgetUsage {
                steps: 0,
                tool_calls: 0,
            },
            BudgetUsage {
                steps: 10_000,
                tool_calls: 10_000,
            },
        ] {
            assert_eq!(
                evaluate_budget_enforcement(usage, TurnBudgets::default(), &stage),
                BudgetEnforcementAction::Ok
            );
        }
    }

    #[test]
    fn stop_requires_prior_escalation() {
        // Over cap without an escalation stage → escalate, not stop.
        let stage = BudgetStageState::default();
        let action = evaluate_budget_enforcement(
            BudgetUsage {
                steps: 99,
                tool_calls: 0,
            },
            BUDGETS,
            &stage,
        );
        assert_eq!(action, BudgetEnforcementAction::Escalate);
    }

    #[test]
    fn kill_switch_spelling() {
        assert!(is_budget_enforcement_disabled(&|k| (k
            == BUDGET_ENFORCE_KILL_SWITCH_ENV)
            .then(|| "0".to_string())));
        assert!(!is_budget_enforcement_disabled(&|_| None));
        assert!(!is_budget_enforcement_disabled(&|k| (k
            == BUDGET_ENFORCE_KILL_SWITCH_ENV)
            .then(|| "1".to_string())));
    }

    #[test]
    fn message_bodies_mention_budget_and_resume() {
        let usage = BudgetUsage {
            steps: 25,
            tool_calls: 60,
        };
        let warn = build_budget_warning_body(&ctx(usage));
        assert!(warn.contains("80%") && warn.contains("25/25"));
        let esc = build_budget_escalation_body(&ctx(usage));
        assert!(esc.contains("exactly one more model step"));
        let stop = build_budget_exhausted_body(&ctx(usage));
        assert!(stop.contains("resumable") && stop.contains("GROK_LOCAL_BUDGET_"));
    }
}
