//! SystemOne route client.
//!
//! POSTs the task to the router (`/v1/systemone/route`) and maps the returned
//! tier to a grok-local reasoning effort and loop caps. Semantics mirror the
//! `grok-local-acp-adapter` v0.5.2 `systemone_route()`:
//!
//! - tier -> caps: `edge`/`economy` -> low, `balanced` -> medium, `heavy` -> high.
//! - An explicit `effort` in the shim response wins over the tier caps
//!   ("shim knows best") — including the extended `xhigh`/`ultra` levels.
//! - Unknown tiers fall back to the high tier's caps (fail-open).
//! - ANY error/timeout -> fail-open decision from config defaults.
//!
//! The `model_id` in the response is advisory only: it is recorded on the
//! decision and logged, never acted on (no model switching, ever).
//!
//! # Thinking levels (v0.5.1)
//!
//! [`Effort`] is the full thinking scale: `Off | Low | Medium | High |
//! XHigh | Ultra`. [`ThinkingMode`] is the user's setting: `Auto` (the router
//! decides per task, and may reach `XHigh`/`Ultra` for genuinely heavy work)
//! or `Fixed(Effort)` (pinned by the user — the router stands down on effort).
//! [`ModelSelection`] (`Auto | Pinned`) is a fully independent control: it
//! never influences thinking resolution and thinking never influences it.

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

/// Thinking levels. `Off` disables reasoning; `XHigh` and `Ultra` are the
/// extended power levels the router may select on `Auto` for heavy tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Ultra,
}

impl Effort {
    /// All levels in UI order.
    pub const ALL: [&'static str; 6] = ["off", "low", "medium", "high", "xhigh", "ultra"];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(Self::Off),
            "low" | "minimal" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "x_high" => Some(Self::XHigh),
            "ultra" | "max" => Some(Self::Ultra),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Ultra => "ultra",
        }
    }

    /// Canonical loop caps: (max_turns, ralph_cap).
    /// Low/Medium/High mirror the adapter's `_EFFORT_CANONICAL`; XHigh/Ultra
    /// extend it for the heaviest work.
    pub fn canonical_caps(self) -> (u32, u32) {
        match self {
            Self::Off => (4, 4),
            Self::Low => (4, 4),
            Self::Medium => (6, 8),
            Self::High => (10, 12),
            Self::XHigh => (14, 16),
            Self::Ultra => (20, 24),
        }
    }

    /// The effort value the tier maps to (adapter `tier_efforts`).
    /// Note: tiers only ever map to Low/Medium/High. XHigh/Ultra are reached
    /// via the shim's explicit `effort` field ("shim knows best").
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
            Self::Off => xai_grok_sampling_types::ReasoningEffort::None,
            Self::Low => xai_grok_sampling_types::ReasoningEffort::Low,
            Self::Medium => xai_grok_sampling_types::ReasoningEffort::Medium,
            Self::High => xai_grok_sampling_types::ReasoningEffort::High,
            Self::XHigh => xai_grok_sampling_types::ReasoningEffort::Xhigh,
            Self::Ultra => xai_grok_sampling_types::ReasoningEffort::Max,
        }
    }

    /// Convert a sampling-layer effort back into a thinking level (used when
    /// the user sets effort explicitly via `/effort` or `--effort`: that
    /// becomes a pinned thinking level).
    pub fn from_reasoning(effort: xai_grok_sampling_types::ReasoningEffort) -> Self {
        use xai_grok_sampling_types::ReasoningEffort as R;
        match effort {
            R::None => Self::Off,
            R::Minimal => Self::Low,
            R::Low => Self::Low,
            R::Medium => Self::Medium,
            R::High => Self::High,
            R::Xhigh => Self::XHigh,
            R::Max => Self::Ultra,
        }
    }
}

/// The user's thinking-level setting. Fully independent from
/// [`ModelSelection`]: resolving one never touches the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingMode {
    /// The SystemOne router picks the effort per task (may reach XHigh/Ultra).
    Auto,
    /// Pinned level: the router stands down on effort for this session.
    Fixed(Effort),
}

impl ThinkingMode {
    /// All selectable values in UI order.
    pub const ALL: [&'static str; 7] = ["off", "low", "medium", "high", "xhigh", "ultra", "auto"];

    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().to_ascii_lowercase();
        if t == "auto" {
            return Some(Self::Auto);
        }
        Effort::parse(&t).map(Self::Fixed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fixed(e) => e.as_str(),
        }
    }

    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }

    /// Resolve the effective effort for a task. Pure function of the mode and
    /// the routing decision — model selection plays no part.
    pub fn resolve(self, decision: &RouteDecision) -> Effort {
        match self {
            Self::Auto => decision.effort,
            Self::Fixed(e) => e,
        }
    }
}

impl Default for ThinkingMode {
    fn default() -> Self {
        Self::Auto
    }
}

/// The user's model-selection setting. Fully independent from
/// [`ThinkingMode`]. `Pinned` means "use the session's active model" (the
/// long-standing default behavior, first-class). `Auto` lets SystemOne pick
/// per task — recorded as an advisory and shown in the UI; the session never
/// switches models on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelSelection {
    Auto,
    #[default]
    Pinned,
}

impl ModelSelection {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "pinned" | "manual" | "session" => Some(Self::Pinned),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pinned => "pinned",
        }
    }

    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
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
    pub(crate) fn fail_open(cfg: &SystemOneConfig, error: Option<String>) -> Self {
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
        self.evidence_line_with(router, self.effort, self.effort, ModelSelection::Pinned)
    }

    /// Evidence line with the *resolved* thinking level (after [`ThinkingMode`]
    /// resolution) and the model selection, so the line shows what the turn
    /// actually ran with — no black box.
    pub fn evidence_line_with(
        &self,
        router: RouterStatus,
        routed_effort: Effort,
        thinking_applied: Effort,
        model_selection: ModelSelection,
    ) -> String {
        let mut parts = vec![
            format!("status={}", router.as_str()),
            format!("source={}", self.source.as_str()),
            format!("tier={}", self.tier.map(Tier::as_str).unwrap_or("-")),
            format!("effort={}", routed_effort.as_str()),
            format!("thinking={}", thinking_applied.as_str()),
            format!("model_selection={}", model_selection.as_str()),
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
    fn canonical_caps_match_adapter_and_extend() {
        assert_eq!(Effort::Off.canonical_caps(), (4, 4));
        assert_eq!(Effort::Low.canonical_caps(), (4, 4));
        assert_eq!(Effort::Medium.canonical_caps(), (6, 8));
        assert_eq!(Effort::High.canonical_caps(), (10, 12));
        assert_eq!(Effort::XHigh.canonical_caps(), (14, 16));
        assert_eq!(Effort::Ultra.canonical_caps(), (20, 24));
    }

    #[test]
    fn effort_parse_covers_full_scale() {
        assert_eq!(Effort::parse("off"), Some(Effort::Off));
        assert_eq!(Effort::parse("none"), Some(Effort::Off));
        assert_eq!(Effort::parse("low"), Some(Effort::Low));
        assert_eq!(Effort::parse("minimal"), Some(Effort::Low));
        assert_eq!(Effort::parse("medium"), Some(Effort::Medium));
        assert_eq!(Effort::parse("HIGH"), Some(Effort::High));
        assert_eq!(Effort::parse("xhigh"), Some(Effort::XHigh));
        assert_eq!(Effort::parse("x-high"), Some(Effort::XHigh));
        assert_eq!(Effort::parse("ultra"), Some(Effort::Ultra));
        assert_eq!(Effort::parse("max"), Some(Effort::Ultra));
        assert_eq!(Effort::parse("turbo"), None);
        assert_eq!(Effort::ALL.len(), 6);
    }

    #[test]
    fn effort_maps_to_sampling_effort() {
        use xai_grok_sampling_types::ReasoningEffort as R;
        assert_eq!(Effort::Off.reasoning_effort(), R::None);
        assert_eq!(Effort::Low.reasoning_effort(), R::Low);
        assert_eq!(Effort::Medium.reasoning_effort(), R::Medium);
        assert_eq!(Effort::High.reasoning_effort(), R::High);
        assert_eq!(Effort::XHigh.reasoning_effort(), R::Xhigh);
        assert_eq!(Effort::Ultra.reasoning_effort(), R::Max);
    }

    #[test]
    fn effort_round_trips_through_sampling() {
        use xai_grok_sampling_types::ReasoningEffort as R;
        for (r, e) in [
            (R::None, Effort::Off),
            (R::Low, Effort::Low),
            (R::Medium, Effort::Medium),
            (R::High, Effort::High),
            (R::Xhigh, Effort::XHigh),
            (R::Max, Effort::Ultra),
        ] {
            assert_eq!(Effort::from_reasoning(r), e);
            assert_eq!(e.reasoning_effort(), r);
        }
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
    fn explicit_xhigh_ultra_effort_honored() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        let payload = serde_json::json!({
            "route": {"tier": "heavy", "effort": "xhigh", "confidence": 0.92},
        });
        apply_route_payload(&mut d, &payload);
        assert_eq!(d.effort, Effort::XHigh);
        assert_eq!(d.max_turns, 14);

        let mut d2 = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        let payload2 = serde_json::json!({
            "route": {"tier": "heavy", "effort": "ultra", "confidence": 0.95},
        });
        apply_route_payload(&mut d2, &payload2);
        assert_eq!(d2.effort, Effort::Ultra);
        assert_eq!(d2.max_turns, 20);
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
    fn thinking_mode_parse() {
        assert_eq!(ThinkingMode::parse("auto"), Some(ThinkingMode::Auto));
        assert_eq!(ThinkingMode::parse("AUTO"), Some(ThinkingMode::Auto));
        assert_eq!(
            ThinkingMode::parse("ultra"),
            Some(ThinkingMode::Fixed(Effort::Ultra))
        );
        assert_eq!(
            ThinkingMode::parse("off"),
            Some(ThinkingMode::Fixed(Effort::Off))
        );
        assert_eq!(ThinkingMode::parse("ludicrous"), None);
        assert_eq!(ThinkingMode::ALL.len(), 7);
        assert_eq!(ThinkingMode::default(), ThinkingMode::Auto);
    }

    #[test]
    fn thinking_mode_resolve() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        d.effort = Effort::XHigh;
        // Auto follows the decision.
        assert_eq!(ThinkingMode::Auto.resolve(&d), Effort::XHigh);
        // Fixed pins regardless of the decision.
        assert_eq!(ThinkingMode::Fixed(Effort::Low).resolve(&d), Effort::Low);
        assert_eq!(ThinkingMode::Fixed(Effort::Off).resolve(&d), Effort::Off);
    }

    #[test]
    fn model_selection_parse() {
        assert_eq!(ModelSelection::parse("auto"), Some(ModelSelection::Auto));
        assert_eq!(
            ModelSelection::parse("pinned"),
            Some(ModelSelection::Pinned)
        );
        assert_eq!(
            ModelSelection::parse("manual"),
            Some(ModelSelection::Pinned)
        );
        assert_eq!(ModelSelection::parse("grok-4"), None);
        assert_eq!(ModelSelection::default(), ModelSelection::Pinned);
        assert!(ModelSelection::Auto.is_auto());
        assert!(!ModelSelection::Pinned.is_auto());
    }

    /// Ryan's explicit correction: thinking and model selection are fully
    /// independent controls. Resolving thinking must never depend on the
    /// model selection, and vice versa.
    #[test]
    fn thinking_and_model_selection_are_independent() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        d.effort = Effort::High;
        d.model_id = Some("some-model".to_string());
        for mode in [
            ThinkingMode::Auto,
            ThinkingMode::Fixed(Effort::Off),
            ThinkingMode::Fixed(Effort::Low),
            ThinkingMode::Fixed(Effort::Medium),
            ThinkingMode::Fixed(Effort::High),
            ThinkingMode::Fixed(Effort::XHigh),
            ThinkingMode::Fixed(Effort::Ultra),
        ] {
            let with_auto = mode.resolve(&d);
            // Model selection cannot change the resolved thinking: resolve
            // takes no model-selection input by construction. Assert the
            // mapping is stable across both selections.
            for _sel in [ModelSelection::Auto, ModelSelection::Pinned] {
                assert_eq!(mode.resolve(&d), with_auto, "mode {mode:?}");
            }
        }
    }

    #[test]
    fn evidence_line_with_shows_resolved_thinking() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), Some("boom".into()));
        d.tier = Some(Tier::Economy);
        d.effort = Effort::High;
        d.max_turns = 10;
        let line = d.evidence_line_with(
            RouterStatus::AlreadyRunning,
            Effort::High,
            Effort::Ultra,
            ModelSelection::Auto,
        );
        assert!(line.starts_with("systemone: "));
        assert!(line.contains("tier=economy"));
        assert!(line.contains("effort=high"));
        assert!(line.contains("thinking=ultra"));
        assert!(line.contains("model_selection=auto"));
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
        assert!(line.contains("thinking=low"));
        assert!(line.contains("model_selection=pinned"));
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
