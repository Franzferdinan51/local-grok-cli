//! Agent-flow per-turn policy integration for `SessionActor`.
//!
//! This module is the single integration point between the pure policy
//! engine (`xai-grok-agentflow`) and the live session. The SystemOne route
//! decision is resolved once per turn into:
//!
//! - an effort behavior policy (turn/tool budgets, subagent allowance, …),
//! - a label-driven tool shortlist applied when tool definitions are
//!   prepared for the sampler (the Task tool is hidden entirely when the
//!   effort tier forbids subagents),
//! - a plan-then-execute gate (planner prompt injected as a reminder),
//! - subagent cost guidance,
//! - a per-round budget ladder (warn at 80%, escalate at the cap with one
//!   extra step granted, stop when still over cap),
//! - a normalized-fingerprint doom-loop escalation ladder,
//! - anchored-compaction helpers (transcript archive + anchor extraction).
//!
//! Every consumer is fail-open: when agent-flow is disabled, or the route
//! carried no usable signal, the turn runs exactly as it would have.
//! Nothing here loads, unloads, or switches models — the router stays
//! read-only with respect to model state.

use std::path::Path;

use xai_grok_agentflow::EnvReader;
use xai_grok_agentflow::anchored_compaction::{
    archive_transcript, build_anchored_summary_prompt, extract_transcript_anchors,
    is_anchored_compaction_disabled, resolve_archive_keep_count, rotate_transcript_archives,
};
use xai_grok_agentflow::budgets::{BudgetUsage, TurnBudgets};
use xai_grok_agentflow::config::AgentFlowConfig;
use xai_grok_agentflow::doom_loop::{
    DoomLoopTransitionKind, DoomLoopTurnState, advance_doom_loop,
    build_doom_loop_final_unattended_body, build_doom_loop_nudge_body,
    build_doom_loop_strategy_body, is_doom_loop_disabled, record_doom_loop_tool_use,
    untried_doom_loop_tools,
};
use xai_grok_agentflow::effort::{EffortBehaviorPolicy, SubagentAllowance};
use xai_grok_agentflow::enforcement::{
    BudgetEnforcementAction, BudgetMessageContext, BudgetStageState, apply_budget_action,
    build_budget_escalation_body, build_budget_exhausted_body, build_budget_warning_body,
    evaluate_budget_enforcement, is_budget_enforcement_disabled,
};
use xai_grok_agentflow::plan_execute::{
    PLAN_ARTIFACT_FILENAME, PlanExecuteGateDecision, PlanExecuteGateInput, build_planner_prompt,
    decide_plan_then_execute, is_plan_then_execute_disabled, plan_artifact_dir,
};
use xai_grok_agentflow::subagents::{
    SubagentSpawnDecision, SubagentSpawnRequest, build_subagent_cost_guidance,
    decide_subagent_spawn, is_subagents_disabled,
};
use xai_grok_agentflow::tool_packs::{
    ComputeToolShortlistInput, ToolSchemaDescriptor, compute_tool_shortlist, resolve_active_labels,
    split_mcp_tool_name,
};
use xai_grok_tools::implementations::grok_build::task::is_task_tool_id;
use xai_grok_tools::types::ToolDefinition;

/// Outcome of resolving agent-flow policy for one routed turn.
pub(crate) struct AgentFlowRouteOutcome {
    /// Turn cap from the agent-flow budget table (`None` keeps the router's own cap).
    pub max_turns_cap: Option<usize>,
    /// System reminders to inject before the turn samples.
    pub reminders: Vec<String>,
}

/// Which stage of the budget ladder fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentFlowBudgetKind {
    Warn,
    Escalate,
    Stop,
}

/// A fired budget stage, ready to be messaged by the caller.
pub(crate) struct AgentFlowBudgetFired {
    pub kind: AgentFlowBudgetKind,
    pub body: String,
    /// The steps cap that fired; used for the stop outcome.
    pub cap_steps: Option<usize>,
}

/// Per-turn agent-flow state, owned by `SessionActor` behind a mutex.
pub(crate) struct AgentFlowTurnState {
    config: AgentFlowConfig,
    tier_name: Option<String>,
    effort_name: Option<String>,
    confidence: Option<f64>,
    labels: Vec<String>,
    task_text: String,
    policy: Option<EffortBehaviorPolicy>,
    budgets: TurnBudgets,
    plan_gate: Option<PlanExecuteGateDecision>,
    doom: DoomLoopTurnState,
    budget_stage: BudgetStageState,
    last_sent_tool_names: Vec<String>,
}

impl Default for AgentFlowTurnState {
    fn default() -> Self {
        Self {
            config: AgentFlowConfig::default(),
            tier_name: None,
            effort_name: None,
            confidence: None,
            labels: Vec::new(),
            task_text: String::new(),
            policy: None,
            budgets: TurnBudgets::default(),
            plan_gate: None,
            doom: DoomLoopTurnState::default(),
            budget_stage: BudgetStageState::default(),
            last_sent_tool_names: Vec::new(),
        }
    }
}

fn read_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

impl AgentFlowTurnState {
    fn reset_turn_state(&mut self) {
        self.tier_name = None;
        self.effort_name = None;
        self.confidence = None;
        self.labels.clear();
        self.task_text.clear();
        self.policy = None;
        self.budgets = TurnBudgets::default();
        self.plan_gate = None;
        self.doom = DoomLoopTurnState::default();
        self.budget_stage = BudgetStageState::default();
        self.last_sent_tool_names.clear();
    }

    /// Resolve the route decision into per-turn policy. Resets per-turn
    /// ladder state. Returns the turn cap plus reminders for the caller to
    /// inject. Never touches model state.
    pub(crate) fn apply_route_decision(
        &mut self,
        decision: &xai_grok_systemone::RouteDecision,
        prompt_text: &str,
        session_id: &str,
    ) -> AgentFlowRouteOutcome {
        self.reset_turn_state();
        let empty = AgentFlowRouteOutcome {
            max_turns_cap: None,
            reminders: Vec::new(),
        };
        let get_env: EnvReader<'_> = &read_env;
        let config = AgentFlowConfig::load();
        self.config = config.clone();
        if !config.enabled {
            return empty;
        }
        let effort_name = decision.effort.as_str().to_owned();
        let tier_name = decision.tier.map(|t| t.as_str().to_owned());
        let confidence = decision.confidence;
        let policy = config.policy_for_effort(&effort_name, get_env);
        let budgets = config.resolve_budgets(tier_name.as_deref(), Some(&effort_name), get_env);
        let labels = resolve_active_labels(&decision.task_labels, prompt_text);
        let gate = if config.plan_execute && !is_plan_then_execute_disabled(get_env) {
            Some(decide_plan_then_execute(PlanExecuteGateInput {
                policy: Some(&policy),
                route_tier: tier_name.as_deref(),
                task_labels: &labels,
                task_text: prompt_text,
                route_confidence: confidence,
                explicit_config: None,
                get_env,
            }))
        } else {
            None
        };

        let mut reminders = Vec::new();
        if gate.as_ref().is_some_and(|g| g.plan) {
            let plan_path =
                plan_artifact_dir(&xai_dirs::grok_home(), session_id).join(PLAN_ARTIFACT_FILENAME);
            reminders.push(build_planner_prompt(
                prompt_text,
                &plan_path.to_string_lossy(),
            ));
        }
        if !is_subagents_disabled(get_env) {
            match policy.subagents {
                SubagentAllowance::Never | SubagentAllowance::Conservative => {
                    reminders.push(build_subagent_cost_guidance(policy.subagents));
                }
                SubagentAllowance::Parallel => {}
            }
        }

        tracing::info!(
            effort = %effort_name,
            tier = tier_name.as_deref().unwrap_or("none"),
            labels = %labels.join(","),
            confidence = ?confidence,
            max_steps = ?budgets.max_steps,
            max_tool_calls = ?budgets.max_tool_calls,
            plan = gate.as_ref().is_some_and(|g| g.plan),
            subagents = ?policy.subagents,
            "agentflow: route policy resolved"
        );

        self.tier_name = tier_name;
        self.effort_name = Some(effort_name);
        self.confidence = confidence;
        self.labels = labels;
        self.task_text = prompt_text.to_owned();
        self.policy = Some(policy);
        self.budgets = budgets;
        self.plan_gate = gate;

        AgentFlowRouteOutcome {
            max_turns_cap: self.budgets.max_steps.map(|s| s as usize),
            reminders,
        }
    }

    /// Apply the agent-flow tool shortlist to the prepared definitions.
    /// Fail-open: returns the input unchanged when disabled, when pruning is
    /// off, or when the route carried no usable signal. Records the sent
    /// tool names for doom-loop untried-tool suggestions.
    pub(crate) fn filter_tool_definitions(
        &mut self,
        defs: Vec<ToolDefinition>,
    ) -> Vec<ToolDefinition> {
        let get_env: EnvReader<'_> = &read_env;
        self.last_sent_tool_names = defs.iter().map(|d| d.function.name.clone()).collect();
        if !self.config.enabled {
            return defs;
        }
        let descriptors: Vec<ToolSchemaDescriptor> = defs
            .iter()
            .map(|d| {
                let (is_mcp, server) = split_mcp_tool_name(&d.function.name);
                ToolSchemaDescriptor {
                    name: d.function.name.clone(),
                    description: d.function.description.clone().unwrap_or_default(),
                    params_json: serde_json::to_string(&d.function.parameters).unwrap_or_default(),
                    server,
                    is_mcp,
                }
            })
            .collect();
        let shortlist = compute_tool_shortlist(ComputeToolShortlistInput {
            tools: &descriptors,
            task_labels: &self.labels,
            task_text: &self.task_text,
            tier: self.tier_name.as_deref(),
            confidence: self.confidence,
            confidence_threshold: Some(self.config.prune_confidence),
            prune_config_enabled: Some(self.config.prune),
            force_full_reason: None,
            get_env,
        });
        tracing::info!(
            pruned = shortlist.pruned,
            reason = %shortlist.reason,
            tokens_before = shortlist.schema_tokens_before,
            tokens_after = shortlist.schema_tokens_after,
            labels = %shortlist.labels.join(","),
            "agentflow: tool shortlist computed"
        );
        if !shortlist.pruned {
            return defs;
        }
        let mut filtered: Vec<ToolDefinition> = defs
            .into_iter()
            .filter(|d| shortlist.keep_names.iter().any(|k| k == &d.function.name))
            .collect();
        // Effort tiers that forbid subagents lose the Task tool entirely:
        // the model cannot spawn what it cannot see.
        if self
            .policy
            .as_ref()
            .is_some_and(|p| p.subagents == SubagentAllowance::Never)
            && !is_subagents_disabled(get_env)
        {
            let before = filtered.len();
            filtered.retain(|d| !is_task_tool_id(&d.function.name));
            if filtered.len() != before {
                tracing::info!(
                    removed = before - filtered.len(),
                    "agentflow: Task tool hidden (subagents disabled at this effort tier)"
                );
            }
        }
        filtered
    }

    /// Evaluate the per-turn budget ladder. Returns the fired stage (with its
    /// reminder body); `None` means keep going or fail-open. Each stage fires
    /// at most once per turn.
    pub(crate) fn check_budget(
        &mut self,
        steps_used: u64,
        tool_calls: u64,
    ) -> Option<AgentFlowBudgetFired> {
        let get_env: EnvReader<'_> = &read_env;
        if !self.config.enabled
            || !self.config.budget_enforce
            || is_budget_enforcement_disabled(get_env)
            || self.budgets.is_empty()
        {
            return None;
        }
        let usage = BudgetUsage {
            steps: steps_used,
            tool_calls,
        };
        let action = evaluate_budget_enforcement(usage, self.budgets, &self.budget_stage);
        apply_budget_action(&mut self.budget_stage, action);
        if matches!(action, BudgetEnforcementAction::Ok) {
            return None;
        }
        let ctx =
            BudgetMessageContext::from_usage_tier(usage, self.budgets, self.tier_name.as_deref());
        let (kind, body) = match action {
            BudgetEnforcementAction::Ok => return None,
            BudgetEnforcementAction::Warn => {
                (AgentFlowBudgetKind::Warn, build_budget_warning_body(&ctx))
            }
            BudgetEnforcementAction::Escalate => (
                AgentFlowBudgetKind::Escalate,
                build_budget_escalation_body(&ctx),
            ),
            BudgetEnforcementAction::Stop => {
                (AgentFlowBudgetKind::Stop, build_budget_exhausted_body(&ctx))
            }
        };
        Some(AgentFlowBudgetFired {
            kind,
            body,
            cap_steps: self.budgets.max_steps.map(|s| s as usize),
        })
    }

    /// Advance the doom-loop ladder for one observed tool call. Returns the
    /// escalation reminder body when a stage fires, `None` otherwise. Each
    /// stage fires at most once per streak; the existing stationarity system
    /// stays on as the final safety net.
    pub(crate) fn observe_tool_call(
        &mut self,
        tool_name: &str,
        input_json: &str,
    ) -> Option<String> {
        let get_env: EnvReader<'_> = &read_env;
        if !self.config.enabled || !self.config.doom_loop || is_doom_loop_disabled(get_env) {
            return None;
        }
        record_doom_loop_tool_use(&mut self.doom, tool_name);
        let transition = advance_doom_loop(&mut self.doom, tool_name, input_json);
        let looping = transition.tool_name.as_deref().unwrap_or(tool_name);
        match transition.kind {
            DoomLoopTransitionKind::None => None,
            DoomLoopTransitionKind::Nudge => {
                Some(build_doom_loop_nudge_body(looping, transition.streak))
            }
            DoomLoopTransitionKind::StrategyChange => {
                let pack_keep: Option<&[String]> = if self.last_sent_tool_names.is_empty() {
                    None
                } else {
                    Some(&self.last_sent_tool_names)
                };
                let untried = untried_doom_loop_tools(
                    pack_keep,
                    &self.last_sent_tool_names,
                    &self.doom.used_tools,
                    looping,
                    5,
                );
                Some(build_doom_loop_strategy_body(
                    looping,
                    transition.streak,
                    &untried,
                ))
            }
            DoomLoopTransitionKind::Final => Some(build_doom_loop_final_unattended_body(
                looping,
                transition.streak,
            )),
        }
    }

    /// Pure spawn decision for a prospective subagent, using this turn's
    /// resolved effort policy. Intended for the Task-tool spawn path.
    #[allow(dead_code)]
    pub(crate) fn subagent_spawn_decision(
        &self,
        task_summary: &str,
        parallel_units: u32,
        parent_depth: u32,
        max_depth: u32,
    ) -> SubagentSpawnDecision {
        let get_env: EnvReader<'_> = &read_env;
        decide_subagent_spawn(
            self.policy.as_ref(),
            &SubagentSpawnRequest {
                task_summary,
                parallel_units,
                parent_depth,
                max_depth,
            },
            get_env,
        )
    }

    /// Archive the pre-compaction transcript and build the anchored summary
    /// prompt. Returns `None` when disabled or on any I/O failure (fail-open:
    /// compaction proceeds without anchors).
    pub(crate) fn anchored_compaction_prep(
        &self,
        grok_home: &Path,
        session_id: &str,
        messages: &[(String, String)],
    ) -> Option<String> {
        let get_env: EnvReader<'_> = &read_env;
        if !self.config.enabled
            || !self.config.anchored_compact
            || is_anchored_compaction_disabled(get_env)
        {
            return None;
        }
        let transcript_text: String = messages
            .iter()
            .map(|(role, text)| format!("{role}: {text}"))
            .collect::<Vec<_>>()
            .join("\n");
        let anchors = extract_transcript_anchors(&transcript_text);
        if anchors.is_empty() {
            return None;
        }
        if archive_transcript(grok_home, session_id, messages, &anchors).is_err() {
            return None;
        }
        let keep = resolve_archive_keep_count(get_env);
        rotate_transcript_archives(grok_home, session_id, keep);
        Some(build_anchored_summary_prompt(&anchors))
    }
}

/// Select the best plan from multiple candidate plans via SystemOne's plan
/// ranking (`POST /v1/systemone/rank-plans`).
///
/// Returns `(winner_index, rankings)`. The winner is the input plan whose
/// ranking carries the highest score; fail-open: on any error (or no scores)
/// the winner is index 0 and every ranking has `score: None` (input order
/// preserved — the original first plan runs).
///
/// Single-plan flows should skip this entirely — ranking one plan is a
/// no-op by construction. The native agent-flow generates one plan per
/// turn, so there is no honest native caller today; the production consumer
/// is the ACP adapter's `grok_local_plan_then_execute` (`candidate_plans`).
pub(crate) async fn select_best_plan(
    task: &str,
    plans: &[xai_grok_systemone::PlanInput],
    cfg: &xai_grok_systemone::SystemOneConfig,
) -> (usize, Vec<xai_grok_systemone::PlanRanking>) {
    let rankings = xai_grok_systemone::rank_plans(task, plans, cfg).await;
    let winner = rankings
        .iter()
        .filter_map(|r| {
            let score = r.score?;
            let idx = plans.iter().position(|p| p.id == r.id)?;
            Some((idx, score))
        })
        .max_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    (winner, rankings)
}

#[cfg(test)]
mod select_best_plan_tests {
    use super::select_best_plan;
    use xai_grok_systemone::{PlanInput, SystemOneConfig};

    fn disabled_cfg() -> SystemOneConfig {
        SystemOneConfig {
            urls: Vec::new(),
            ..SystemOneConfig::default()
        }
    }

    fn two_plans() -> Vec<PlanInput> {
        vec![
            PlanInput {
                id: "a".to_string(),
                text: "first plan".to_string(),
            },
            PlanInput {
                id: "b".to_string(),
                text: "second plan".to_string(),
            },
        ]
    }

    #[tokio::test]
    async fn fail_open_preserves_input_order() {
        // Routing disabled: no ranking, original-first wins.
        let plans = two_plans();
        let (winner, rankings) = select_best_plan("do a thing", &plans, &disabled_cfg()).await;
        assert_eq!(winner, 0);
        assert_eq!(rankings.len(), 2);
        assert!(rankings.iter().all(|r| r.score.is_none()));
        assert_eq!(rankings[0].id, "a");
        assert_eq!(rankings[1].id, "b");
    }

    #[tokio::test]
    async fn empty_plans_yield_empty_ranking() {
        let (winner, rankings) = select_best_plan("do a thing", &[], &disabled_cfg()).await;
        assert_eq!(winner, 0);
        assert!(rankings.is_empty());
    }
}
