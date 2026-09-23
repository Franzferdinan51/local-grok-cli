//! Native SystemOne per-turn routing for interactive/ACP sessions.
//!
//! Every human prompt is routed through the local SystemOne router (fail-open):
//! the routed reasoning effort auto-tunes the turn (unless the user explicitly
//! set one), and the routed turn budget caps runaway tool loops (unless the
//! user passed an explicit `--max-turns`).
//!
//! The router's `model_id` is advisory only: this module never switches models
//! and never unloads anything. Kill switches: `GROK_LOCAL_SYSTEMONE=0` or
//! `[systemone] enabled = false`.

use std::sync::Arc;

use agent_client_protocol as acp;
use parking_lot::Mutex;
use xai_grok_sampling_types::ReasoningEffort;

/// Per-session SystemOne routing state for interactive turns.
#[derive(Debug, Default)]
pub(crate) struct SystemOneTurnState {
    /// Reasoning effort the router applied on a previous turn (`None` if the
    /// router has never applied one). Lets the router update its own choice
    /// per task without clobbering a user's explicit setting.
    pub router_effort: Option<ReasoningEffort>,
    /// Turn budget the router applied (`None` if never). The user's explicit
    /// `max_turns` always wins over this.
    pub router_max_turns: Option<usize>,
    /// Set once the user explicitly chooses an effort (slash command or
    /// config): the router stands down on effort from then on.
    pub effort_user_locked: bool,
}

impl SystemOneTurnState {
    /// Effective turn cap: the user's explicit limit wins, else the router's.
    pub(crate) fn effective_max_turns(&self, user_max_turns: Option<usize>) -> Option<usize> {
        user_max_turns.or(self.router_max_turns)
    }
}

fn systemone_effort_to_reasoning(effort: xai_grok_systemone::Effort) -> ReasoningEffort {
    match effort {
        xai_grok_systemone::Effort::Low => ReasoningEffort::Low,
        xai_grok_systemone::Effort::Medium => ReasoningEffort::Medium,
        xai_grok_systemone::Effort::High => ReasoningEffort::High,
    }
}

/// Extract plain text from ACP content blocks for routing.
fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    let mut text = String::new();
    for block in blocks {
        if let acp::ContentBlock::Text(t) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
        }
    }
    text
}

impl super::SessionActor {
    /// Route one human turn through SystemOne and apply the decision.
    /// Fail-open: any error, timeout, or disabled router leaves the turn
    /// exactly as it would have been.
    pub(super) async fn maybe_route_systemone_turn(
        self: &Arc<Self>,
        prompt_blocks: &[acp::ContentBlock],
    ) {
        let text = prompt_text(prompt_blocks);
        let trimmed = text.trim();
        // Skip slash commands (handled by their own dispatch) and empty input.
        if trimmed.is_empty() || trimmed.starts_with('/') {
            return;
        }
        let cfg = xai_grok_systemone::SystemOneConfig::load();
        if !cfg.routing_active() {
            return;
        }
        let decision = xai_grok_systemone::route_for_task(&text, "turn", &cfg).await;
        tracing::info!(
            "systemone: tier={} effort={} max_turns={} source={} confidence={:.2} model_advisory={}",
            decision.tier.map(|t| t.as_str()).unwrap_or("fail-open"),
            decision.effort.as_str(),
            decision.max_turns,
            decision.source.as_str(),
            decision.confidence.unwrap_or(0.0),
            decision.model_id.as_deref().unwrap_or("-"),
        );
        self.apply_systemone_decision(&decision).await;
    }

    /// Apply a routing decision to this turn. Never switches models.
    async fn apply_systemone_decision(
        self: &Arc<Self>,
        decision: &xai_grok_systemone::RouteDecision,
    ) {
        // --- Reasoning effort: only when the user hasn't locked one in. ---
        let want_effort = systemone_effort_to_reasoning(decision.effort);
        let (locked, router_owned) = {
            let state = self.systemone_turn.lock();
            (state.effort_user_locked, state.router_effort.is_some())
        };

        if !locked && let Some(mut sampling) = self.chat_state_handle.get_sampling_config().await {
            let user_set = sampling.reasoning_effort.is_some() && !router_owned;
            if user_set {
                // The user (or their config) chose an effort: stand down
                // from now on.
                self.systemone_turn.lock().effort_user_locked = true;
            } else if self
                .models_manager
                .model_supports_reasoning_effort(&sampling.model)
            {
                // Router-owned (or unset): safe to (re)apply. Never touch
                // `sampling.model` — the router is advisory on models.
                sampling.reasoning_effort = Some(want_effort);
                self.chat_state_handle.update_sampling_config(sampling);
                self.systemone_turn.lock().router_effort = Some(want_effort);
                tracing::debug!(
                    "systemone: applied reasoning effort {:?} for this turn",
                    want_effort,
                );
            }
        }

        // --- Turn budget: the user's explicit max_turns always wins. ---
        if self.max_turns.is_none() {
            self.systemone_turn.lock().router_max_turns = Some(decision.max_turns as usize);
        }

        // --- Evidence for the session log. ---
        xai_grok_telemetry::unified_log::info(
            "shell.systemone.route",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "tier": decision.tier.map(|t| t.as_str()).unwrap_or("fail-open"),
                "effort": decision.effort.as_str(),
                "max_turns": decision.max_turns,
                "source": decision.source.as_str(),
                "confidence": decision.confidence.unwrap_or(0.0),
            })),
        );
    }

    /// Called when the user explicitly sets a reasoning effort: the router
    /// must not override it afterwards.
    pub(super) fn note_systemone_effort_user_locked(self: &Arc<Self>) {
        let mut state = self.systemone_turn.lock();
        state.effort_user_locked = true;
        // Forget what the router applied: the user's choice is authoritative.
        state.router_effort = None;
    }
}
