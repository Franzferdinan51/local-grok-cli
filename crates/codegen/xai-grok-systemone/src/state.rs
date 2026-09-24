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
//!
//! New fields are `#[serde(default)]` so state files written by older builds
//! still deserialize.

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
    /// Shim's best-value model ranking (top ids, expected-utility order).
    /// Advisory only — the session never switches models.
    #[serde(default)]
    pub model_ranking: Vec<String>,
    /// Whether the route was uncertain.
    #[serde(default)]
    pub uncertain: bool,
    /// Why pruning didn't engage, if it didn't (e.g. `"disabled(uncertain)"`).
    #[serde(default)]
    pub prune_note: Option<String>,
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
            model_ranking: decision
                .ranked_models
                .iter()
                .map(|m| m.model_id.clone())
                .collect(),
            uncertain: decision.uncertain == Some(true),
            prune_note: decision.prune_note.clone(),
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
    use crate::route::RankedModel;

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

    /// Phase 3: the ranked-models advisory and the uncertain/prune signals
    /// ride along in the state file.
    #[test]
    fn capture_phase3_fields() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        d.ranked_models = vec![
            RankedModel {
                model_id: "cheap-model".to_string(),
                tier: Some("economy".to_string()),
                utility: Some(0.8),
                quality: Some(0.9),
                cost: Some(0.1),
            },
            RankedModel {
                model_id: "big-model".to_string(),
                tier: Some("heavy".to_string()),
                utility: Some(0.7),
                quality: Some(0.95),
                cost: Some(0.5),
            },
        ];
        d.uncertain = Some(true);
        d.prune_note = Some("disabled(uncertain)".to_string());
        let lr = LastRoute::capture(&d, ThinkingMode::Auto, Effort::Medium, ModelSelection::Auto);
        assert_eq!(lr.model_ranking, vec!["cheap-model", "big-model"]);
        assert!(lr.uncertain);
        assert_eq!(lr.prune_note.as_deref(), Some("disabled(uncertain)"));
        // Round-trip through JSON.
        let back: LastRoute = serde_json::from_str(&serde_json::to_string(&lr).unwrap()).unwrap();
        assert_eq!(back.model_ranking.len(), 2);
    }

    /// State files written by older builds (without the new fields) still
    /// deserialize — the new fields default.
    #[test]
    fn old_state_files_still_parse() {
        let old = serde_json::json!({
            "ts": 123,
            "tier": "economy",
            "routed_effort": "low",
            "thinking_applied": "low",
            "thinking_mode": "auto",
            "model_selection": "pinned",
            "model_advisory": null,
            "confidence": 0.9,
            "source": "systemone",
        });
        let lr: LastRoute = serde_json::from_str(&old.to_string()).unwrap();
        assert!(lr.model_ranking.is_empty());
        assert!(!lr.uncertain);
        assert_eq!(lr.prune_note, None);
        assert_eq!(lr.tier.as_deref(), Some("economy"));
    }
}
