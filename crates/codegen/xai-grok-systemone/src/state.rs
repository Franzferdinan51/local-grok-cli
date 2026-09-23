//! Last-route state file: the session records what each routed turn actually
//! ran with so the TUI can display it without any protocol changes.
//!
//! Written to `$GROK_HOME/systemone-last-route.json` after every routed turn
//! (tiny JSON write; negligible next to the route HTTP call itself). Reads are
//! fail-open: a missing or corrupt file yields `None`.
//!
//! This is deliberately file-based IPC between the session actor
//! (`xai-grok-shell`) and the TUI (`xai-grok-pager`): no ACP changes, no
//! new session commands, nothing that can break a session.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::route::{Effort, ModelSelection, RouteDecision, ThinkingMode};

fn state_path() -> PathBuf {
    xai_dirs::grok_home().join("systemone-last-route.json")
}

/// What the most recent routed turn actually ran with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastRoute {
    /// Unix timestamp of the route.
    pub ts: i64,
    pub tier: Option<String>,
    /// Effort the router returned.
    pub routed_effort: String,
    /// Effort actually applied after [`ThinkingMode`] resolution.
    pub thinking_applied: String,
    /// The user's thinking setting.
    pub thinking_mode: String,
    /// The user's model-selection setting.
    pub model_selection: String,
    /// Router's model pick (advisory only — the session never switches).
    pub model_advisory: Option<String>,
    pub confidence: Option<f64>,
    pub source: String,
}

impl LastRoute {
    /// Build from a decision plus the resolved settings for the turn.
    pub fn capture(
        decision: &RouteDecision,
        thinking_mode: ThinkingMode,
        thinking_applied: Effort,
        model_selection: ModelSelection,
    ) -> Self {
        Self {
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            tier: decision.tier.map(|t| t.as_str().to_string()),
            routed_effort: decision.effort.as_str().to_string(),
            thinking_applied: thinking_applied.as_str().to_string(),
            thinking_mode: thinking_mode.as_str().to_string(),
            model_selection: model_selection.as_str().to_string(),
            model_advisory: decision.model_id.clone(),
            confidence: decision.confidence,
            source: decision.source.as_str().to_string(),
        }
    }

    /// Persist to the state file. Fail-open: errors are swallowed.
    pub fn store(&self) {
        let path = state_path();
        if let Ok(text) = serde_json::to_string(self) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Read the state file. Returns `None` when missing or corrupt.
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(state_path()).ok()?;
        serde_json::from_str(&text).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemOneConfig;

    #[test]
    fn capture_round_trips() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        d.model_id = Some("ornith-1.5-9b".to_string());
        let lr = LastRoute::capture(&d, ThinkingMode::Auto, Effort::High, ModelSelection::Auto);
        assert_eq!(lr.thinking_mode, "auto");
        assert_eq!(lr.thinking_applied, "high");
        assert_eq!(lr.model_selection, "auto");
        assert_eq!(lr.model_advisory.as_deref(), Some("ornith-1.5-9b"));
        let text = serde_json::to_string(&lr).unwrap();
        let back: LastRoute = serde_json::from_str(&text).unwrap();
        assert_eq!(back.thinking_applied, "high");
    }
}
