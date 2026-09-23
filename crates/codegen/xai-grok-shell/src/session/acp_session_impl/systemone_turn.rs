//! Native SystemOne per-turn routing for interactive/ACP sessions.
//!
//! Every human prompt is routed through the local SystemOne router (fail-open).
//! Two fully independent controls shape each turn:
//!
//! - **Thinking** ([`ThinkingMode`]): `Auto` lets the router pick the reasoning
//!   effort per task (it may reach `XHigh`/`Ultra` for heavy work); a pinned
//!   level makes the router stand down on effort and the pinned level is
//!   applied instead.
//! - **Model selection** ([`ModelSelection`]): `Auto` records the router's
//!   model pick as an advisory shown in the UI; `Pinned` (the default) uses
//!   the session's active model.
//!
//! Source of truth: the `[systemone]` config file (re-read every turn, so
//! `/thinking`, `/effort` and `/model` take effect on the next prompt) with
//! `GROK_LOCAL_SYSTEMONE_THINKING` / `GROK_LOCAL_SYSTEMONE_MODEL_SELECTION`
//! env vars as session-scoped overrides. The session keeps no copy of the
//! settings — there is nothing to go stale and no lock that `/thinking auto`
//! cannot release.
//!
//! The router's `model_id` is advisory only: this module never switches models
//! and never unloads anything. Kill switches: `GROK_LOCAL_SYSTEMONE=0` or
//! `[systemone] enabled = false`.

use std::sync::Arc;

use agent_client_protocol as acp;
use xai_grok_sampling_types::ReasoningEffort;
use xai_grok_systemone::{Effort, LastRoute, ModelSelection, ThinkingMode};

/// Per-session SystemOne routing state for interactive turns.
///
/// The only session-scoped memory is which effort values the thinking system
/// itself applied: that is what lets the router update its own choice per task
/// without clobbering an explicitly configured effort (`--effort`,
/// `default_reasoning_effort`) — the conservative "explicit wins" rule.
#[derive(Debug, Default)]
pub(crate) struct SystemOneTurnState {
    /// Effort the thinking system (router-auto or a pinned level) last
    /// applied. `Some` means "we own this value, safe to overwrite";
    /// `None` with an explicit sampling effort means "hands off".
    pub thinking_owned: Option<ReasoningEffort>,
    /// Turn budget the router applied (`None` if never). The user's explicit
    /// `max_turns` always wins over this.
    pub router_max_turns: Option<usize>,
    /// Thinking level actually applied on the last routed turn (for display).
    pub last_thinking_applied: Option<Effort>,
    /// Router's model pick on the last routed turn (advisory only).
    pub last_model_advisory: Option<String>,
}

impl SystemOneTurnState {
    /// Effective turn cap: the user's explicit limit wins, else the router's.
    pub(crate) fn effective_max_turns(&self, user_max_turns: Option<usize>) -> Option<usize> {
        user_max_turns.or(self.router_max_turns)
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
        // Headless prompts route once pre-session (headless.rs), before the
        // session exists so the decision can prune MCP servers. The per-turn
        // route here would just repeat the same decision, so skip it.
        if self.attach_non_interactive.get() {
            tracing::debug!(
                "systemone: skipping per-turn route for headless session (routed pre-session)"
            );
            return;
        }
        // Re-read every turn: the config file is the source of truth, so
        // `/thinking`, `/effort` and `/model` apply to the very next prompt
        // with no session IPC.
        let cfg = xai_grok_systemone::SystemOneConfig::load();
        if !cfg.routing_active() {
            return;
        }
        let thinking_mode: ThinkingMode = cfg.thinking;
        let model_selection: ModelSelection = cfg.model_selection;
        let decision = xai_grok_systemone::route_for_task(&text, "turn", &cfg).await;
        // Resolve the two independent controls. Thinking resolution is a pure
        // function of the mode + decision — model selection never influences
        // it, and thinking never influences model selection.
        let thinking_applied = thinking_mode.resolve(&decision);
        tracing::info!(
            "systemone: tier={} routed_effort={} thinking={} thinking_mode={} model_selection={} max_turns={} source={} confidence={:.2} model_advisory={}",
            decision.tier.map(|t| t.as_str()).unwrap_or("fail-open"),
            decision.effort.as_str(),
            thinking_applied.as_str(),
            thinking_mode.as_str(),
            model_selection.as_str(),
            decision.max_turns,
            decision.source.as_str(),
            decision.confidence.unwrap_or(0.0),
            decision.model_id.as_deref().unwrap_or("-"),
        );
        self.apply_systemone_decision(&decision, thinking_mode, thinking_applied, model_selection)
            .await;
        // Record what this turn ran with for the TUI (file-based, fail-open).
        LastRoute::capture(&decision, thinking_mode, thinking_applied, model_selection).store();
    }

    /// Apply a routing decision to this turn. Never switches models.
    async fn apply_systemone_decision(
        self: &Arc<Self>,
        decision: &xai_grok_systemone::RouteDecision,
        thinking_mode: ThinkingMode,
        thinking_applied: Effort,
        model_selection: ModelSelection,
    ) {
        let want_effort = thinking_applied.reasoning_effort();
        if let Some(mut sampling) = self.chat_state_handle.get_sampling_config().await {
            if stands_down_on_effort(
                thinking_mode,
                sampling.reasoning_effort,
                self.systemone_turn.lock().thinking_owned,
            ) {
                tracing::debug!(
                    "systemone: standing down on effort (explicit {:?} set outside the thinking system)",
                    sampling.reasoning_effort,
                );
            } else if self
                .models_manager
                .model_supports_reasoning_effort(&sampling.model)
            {
                // Never touch `sampling.model` — the router is advisory on models.
                sampling.reasoning_effort = Some(want_effort);
                self.chat_state_handle.update_sampling_config(sampling);
                let mut state = self.systemone_turn.lock();
                state.thinking_owned = Some(want_effort);
                state.last_thinking_applied = Some(thinking_applied);
                tracing::debug!(
                    "systemone: applied thinking {:?} (mode {}) for this turn",
                    want_effort,
                    thinking_mode.as_str(),
                );
            }
        }

        // --- Model advisory: recorded for display, never acted on. ---
        // `model_selection` intentionally has no effect here: there is no
        // code path that switches models. Auto vs Pinned only changes what
        // the UI shows (and what headless `--model auto` does).
        self.systemone_turn.lock().last_model_advisory = decision.model_id.clone();
        let _ = model_selection;

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
                "routed_effort": decision.effort.as_str(),
                "thinking_mode": thinking_mode.as_str(),
                "thinking_applied": thinking_applied.as_str(),
                "model_selection": model_selection.as_str(),
                "model_advisory": decision.model_id,
                "max_turns": decision.max_turns,
                "source": decision.source.as_str(),
                "confidence": decision.confidence.unwrap_or(0.0),
            })),
        );
    }

    /// Mark an explicitly chosen effort as owned by the thinking system, so
    /// per-turn routing may update it. Called by the effort-setting paths
    /// (`/effort`, `/thinking <level>`, `/model <name> <effort>`, `--effort`).
    /// This replaces the old permanent `effort_user_locked` flag: ownership
    /// lets the router keep working under `Auto` instead of standing down
    /// forever, and the config-file mode decides pinning.
    pub(super) fn mark_systemone_thinking_owned(self: &Arc<Self>, effort: ReasoningEffort) {
        self.systemone_turn.lock().thinking_owned = Some(effort);
    }
}

/// Decide whether the thinking system stands down on effort for this turn.
///
/// Pure function of the user's thinking mode, the sampling config's current
/// effort, and which effort (if any) the thinking system itself owns — so
/// the ownership matrix is unit-testable without a session actor.
fn stands_down_on_effort(
    thinking_mode: ThinkingMode,
    sampling_effort: Option<ReasoningEffort>,
    thinking_owned: Option<ReasoningEffort>,
) -> bool {
    match thinking_mode {
        // A pinned (Fixed) thinking level is an explicit user choice: it
        // always applies and takes ownership, even when the sampling config
        // already carries an effort (`--effort`, `default_reasoning_effort`).
        ThinkingMode::Fixed(_) => false,
        // Auto respects a genuinely external effort only while the thinking
        // system owns nothing (explicit wins). Anything the thinking system
        // applied before — router-auto or a previous pinned level — is ours
        // to update, so `/thinking auto` after `/thinking high` hands control
        // back to the router.
        ThinkingMode::Auto => sampling_effort.is_some() && thinking_owned.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ReasoningEffort as R;

    #[test]
    fn fixed_thinking_always_applies_and_takes_ownership() {
        // A pinned level overrides an initial external/default sampling
        // effort: it applies and takes ownership.
        assert!(!stands_down_on_effort(
            ThinkingMode::Fixed(Effort::Low),
            Some(R::High),
            None
        ));
        assert!(!stands_down_on_effort(
            ThinkingMode::Fixed(Effort::Ultra),
            Some(R::Low),
            Some(R::Medium)
        ));
        assert!(!stands_down_on_effort(
            ThinkingMode::Fixed(Effort::Off),
            None,
            None
        ));
    }

    #[test]
    fn auto_thinking_respects_genuinely_external_effort() {
        // Auto stands down when an explicit outside effort exists and the
        // thinking system owns nothing: explicit wins.
        assert!(stands_down_on_effort(
            ThinkingMode::Auto,
            Some(R::Medium),
            None
        ));
        // But it may update what it applied before — router-auto...
        assert!(!stands_down_on_effort(
            ThinkingMode::Auto,
            Some(R::Medium),
            Some(R::Low)
        ));
        // ...or a previously pinned level: `/thinking auto` after
        // `/thinking high` hands control back to the router.
        assert!(!stands_down_on_effort(
            ThinkingMode::Auto,
            Some(R::High),
            Some(R::High)
        ));
        // No explicit effort anywhere: the routed effort applies.
        assert!(!stands_down_on_effort(ThinkingMode::Auto, None, None));
        assert!(!stands_down_on_effort(
            ThinkingMode::Auto,
            None,
            Some(R::Low)
        ));
    }
}
