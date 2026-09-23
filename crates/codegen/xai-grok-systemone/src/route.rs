//! SystemOne route client.
//!
//! POSTs the task to the router (`/v1/systemone/route`) and maps the returned
//! tier to a grok-local reasoning effort and loop caps. Semantics mirror the
//! `grok-local-acp-adapter` v0.5.2 `systemone_route()`:
//!
//! - tier -> caps: `edge`/`economy` -> low, `balanced` -> medium, `heavy` -> high.
//! - An explicit `effort` in the shim response wins over the tier caps
//!   ("shim knows best").
//! - Unknown tiers fall back to the high tier's caps (fail-open).
//! - ANY error/timeout -> fail-open decision from config defaults.
//!
//! The `model_id` in the response is advisory only: it is recorded on the
//! decision and logged, never acted on (no model switching, ever).

use std::time::Duration;

use crate::config::SystemOneConfig;
use crate::shim::RouterStatus;

/// SystemOne cost tier returned by the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Edge,
    Economy,
    Balanced,
    Heavy,
}

impl Tier {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "edge" => Some(Self::Edge),
            "economy" => Some(Self::Economy),
            "balanced" => Some(Self::Balanced),
            "heavy" => Some(Self::Heavy),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Economy => "economy",
            Self::Balanced => "balanced",
            Self::Heavy => "heavy",
        }
    }
}

/// Reasoning effort levels, mirroring the adapter's `low`/`medium`/`high`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Canonical loop caps, mirroring the adapter's `_EFFORT_CANONICAL`:
    /// (max_turns, ralph_cap).
    pub fn canonical_caps(self) -> (u32, u32) {
        match self {
            Self::Low => (4, 4),
            Self::Medium => (6, 8),
            Self::High => (10, 12),
        }
    }

    /// The effort value the tier maps to (adapter `tier_efforts`).
    fn for_tier(tier: Option<Tier>) -> Self {
        match tier {
            Some(Tier::Edge) | Some(Tier::Economy) => Self::Low,
            Some(Tier::Balanced) => Self::Medium,
            // Unknown tiers fail open to the high tier's caps, like the adapter.
            Some(Tier::Heavy) | None => Self::High,
        }
    }

    pub fn reasoning_effort(self) -> xai_grok_sampling_types::ReasoningEffort {
        match self {
            Self::Low => xai_grok_sampling_types::ReasoningEffort::Low,
            Self::Medium => xai_grok_sampling_types::ReasoningEffort::Medium,
            Self::High => xai_grok_sampling_types::ReasoningEffort::High,
        }
    }
}

/// Where a routing decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSource {
    /// The SystemOne router answered.
    SystemOne,
    /// The router was unreachable/errored/disabled; config defaults apply.
    FailOpen,
}

impl RouteSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SystemOne => "systemone",
            Self::FailOpen => "fail-open",
        }
    }
}

/// A routing decision. Always produced, never an error: check
/// [`RouteDecision::source`] and [`RouteDecision::error`] to see what happened.
#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub source: RouteSource,
    /// Router URL that answered (None when fail-open).
    pub url: Option<String>,
    pub tier: Option<Tier>,
    pub effort: Effort,
    /// Loop cap applied when the caller did not set `--max-turns`.
    pub max_turns: u32,
    pub confidence: Option<f64>,
    pub rationale: Option<String>,
    /// Advisory only: logged, never used to switch models.
    pub model_id: Option<String>,
    pub task_labels: Vec<String>,
    pub suggested_mcp_servers: Vec<String>,
    pub error: Option<String>,
}

impl RouteDecision {
    fn fail_open(cfg: &SystemOneConfig, error: Option<String>) -> Self {
        let (max_turns, _ralph_cap) = cfg.default_effort.canonical_caps();
        Self {
            source: RouteSource::FailOpen,
            url: None,
            tier: None,
            effort: cfg.default_effort,
            max_turns,
            confidence: None,
            rationale: None,
            model_id: None,
            task_labels: Vec::new(),
            suggested_mcp_servers: Vec::new(),
            error,
        }
    }

    /// One greppable stderr line proving what routing did. This is the
    /// zero-setup verification evidence.
    pub fn evidence_line(&self, router: RouterStatus) -> String {
        let mut parts = vec![
            format!("status={}", router.as_str()),
            format!("source={}", self.source.as_str()),
            format!("tier={}", self.tier.map(Tier::as_str).unwrap_or("-")),
            format!("effort={}", self.effort.as_str()),
            format!("max_turns={}", self.max_turns),
        ];
        if let Some(conf) = self.confidence {
            parts.push(format!("confidence={conf:.2}"));
        }
        if let Some(model) = &self.model_id {
            parts.push(format!("model_advisory={model}"));
        }
        if !self.suggested_mcp_servers.is_empty() {
            parts.push(format!(
                "suggested_mcp={}",
                self.suggested_mcp_servers.join(",")
            ));
        }
        if let Some(err) = &self.error {
            let short: String = err.chars().take(160).collect();
            parts.push(format!("note={short}"));
        }
        if let Some(rationale) = &self.rationale {
            let short: String = rationale.chars().take(160).collect();
            parts.push(format!("rationale={short}"));
        }
        format!("systemone: {}", parts.join(" "))
    }
}

/// Ask SystemOne for a routing decision for a task.
///
/// Tries each configured URL with the configured timeout. On ANY error the
/// returned decision is fail-open from config defaults. Never returns an
/// error itself.
pub async fn route_for_task(task: &str, kind: &str, cfg: &SystemOneConfig) -> RouteDecision {
    if !cfg.routing_active() {
        return RouteDecision::fail_open(cfg, Some("routing disabled".to_string()));
    }
    let task_snippet: String = task.chars().take(500).collect();
    let body = serde_json::json!({
        "task": task_snippet,
        "kind": kind,
        "client": "grok-local",
    });

    // Localhost-only router client: the grok TLS policy is for remote hosts.
    #[allow(clippy::disallowed_methods)]
    let client = match reqwest::Client::builder()
        .timeout(cfg.timeout + Duration::from_secs(1))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return RouteDecision::fail_open(cfg, Some(format!("http client: {err}")));
        }
    };

    let mut last_err: Option<String> = None;
    for url in &cfg.urls {
        match client.post(url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                match resp.json::<serde_json::Value>().await {
                    Ok(payload) if status.is_success() => {
                        let mut decision = RouteDecision::fail_open(cfg, None);
                        apply_route_payload(&mut decision, &payload);
                        decision.source = RouteSource::SystemOne;
                        decision.url = Some(url.clone());
                        decision.error = None;
                        // Route-driven MCP suggestions are computed by the caller
                        // from the task + labels (see suggest.rs); record labels here.
                        return decision;
                    }
                    Ok(_) => {
                        last_err = Some(format!("{url}: router returned {status}"));
                    }
                    Err(err) => {
                        last_err = Some(format!("{url}: bad response: {err}"));
                    }
                }
            }
            Err(err) => {
                last_err = Some(format!("{url}: {err}"));
            }
        }
    }
    let mut decision = RouteDecision::fail_open(cfg, last_err);
    decision.error = decision
        .error
        .map(|e| format!("router unreachable ({e}); using defaults"));
    decision
}

/// Fold a `/v1/systemone/route` payload into a decision. Never panics.
fn apply_route_payload(decision: &mut RouteDecision, payload: &serde_json::Value) {
    let route = payload.get("route");
    let tier = route
        .and_then(|r| r.get("tier"))
        .and_then(serde_json::Value::as_str)
        .and_then(Tier::parse);
    decision.tier = tier;

    // Shim knows best: an explicit effort overrides the tier caps.
    let effort = route
        .and_then(|r| r.get("effort"))
        .and_then(serde_json::Value::as_str)
        .and_then(Effort::parse)
        .unwrap_or_else(|| Effort::for_tier(tier));
    decision.effort = effort;
    let (max_turns, _ralph_cap) = effort.canonical_caps();
    decision.max_turns = max_turns;

    decision.confidence = route
        .and_then(|r| r.get("confidence"))
        .and_then(serde_json::Value::as_f64);
    decision.rationale = route
        .and_then(|r| r.get("rationale"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    // Advisory only — recorded for the evidence line, never acted on.
    decision.model_id = route
        .and_then(|r| r.get("model_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    decision.task_labels = route
        .and_then(|r| r.get("task_labels"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_effort_mapping_mirrors_adapter() {
        assert_eq!(Effort::for_tier(Some(Tier::Edge)), Effort::Low);
        assert_eq!(Effort::for_tier(Some(Tier::Economy)), Effort::Low);
        assert_eq!(Effort::for_tier(Some(Tier::Balanced)), Effort::Medium);
        assert_eq!(Effort::for_tier(Some(Tier::Heavy)), Effort::High);
        // Unknown tiers fail open to high.
        assert_eq!(Effort::for_tier(None), Effort::High);
        assert_eq!(Tier::parse("weird"), None);
    }

    #[test]
    fn canonical_caps_match_adapter() {
        assert_eq!(Effort::Low.canonical_caps(), (4, 4));
        assert_eq!(Effort::Medium.canonical_caps(), (6, 8));
        assert_eq!(Effort::High.canonical_caps(), (10, 12));
    }

    #[test]
    fn explicit_effort_overrides_tier() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        let payload = serde_json::json!({
            "route": {"tier": "heavy", "effort": "low", "confidence": 0.9},
            "model": "ornith-1.5-9b",
        });
        apply_route_payload(&mut d, &payload);
        assert_eq!(d.tier, Some(Tier::Heavy));
        assert_eq!(d.effort, Effort::Low);
        assert_eq!(d.max_turns, 4);
        assert_eq!(d.confidence, Some(0.9));
        assert_eq!(d.model_id.as_deref(), Some("ornith-1.5-9b"));
    }

    #[test]
    fn route_model_id_prefers_route_field() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        let payload = serde_json::json!({
            "route": {"tier": "balanced", "model_id": "ornith-1.5-35b-a3b"},
            "model": "other",
        });
        apply_route_payload(&mut d, &payload);
        assert_eq!(d.model_id.as_deref(), Some("ornith-1.5-35b-a3b"));
        assert_eq!(d.effort, Effort::Medium);
        assert_eq!(d.max_turns, 6);
    }

    #[test]
    fn evidence_line_is_greppable() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), Some("boom".into()));
        d.tier = Some(Tier::Economy);
        d.effort = Effort::Low;
        d.max_turns = 4;
        let line = d.evidence_line(RouterStatus::AlreadyRunning);
        assert!(line.starts_with("systemone: "));
        assert!(line.contains("tier=economy"));
        assert!(line.contains("effort=low"));
        assert!(line.contains("source=fail-open"));
    }

    #[tokio::test]
    async fn unreachable_router_fails_open() {
        let cfg = SystemOneConfig {
            urls: vec!["http://127.0.0.1:1/v1/systemone/route".to_string()],
            // Nothing listens here: connection refused, fast.
            timeout: Duration::from_secs(2),
            ..SystemOneConfig::default()
        };
        let d = route_for_task("do a thing", "prompt", &cfg).await;
        assert_eq!(d.source, RouteSource::FailOpen);
        assert_eq!(d.effort, Effort::High);
        assert_eq!(d.max_turns, 10);
        assert!(d.error.as_deref().unwrap_or("").contains("using defaults"));
    }

    #[tokio::test]
    async fn disabled_routing_short_circuits() {
        let cfg = SystemOneConfig {
            enabled: false,
            ..SystemOneConfig::default()
        };
        let d = route_for_task("do a thing", "prompt", &cfg).await;
        assert_eq!(d.source, RouteSource::FailOpen);
        assert_eq!(d.error.as_deref(), Some("routing disabled"));
    }
}
