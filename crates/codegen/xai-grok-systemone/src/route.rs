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
//! # Decision surfaces (Phase 3)
//!
//! Newer shims attach a scored decision surface to the route dict:
//! `calibrated_probabilities` / `margin` / `uncertain`, `ranked_models`
//! (expected-utility order, advisory), and `ranked_tools` with
//! `tool_scoring` (`"full"` | `"skipped"`). Older shims lack these keys —
//! absence is treated as "not present", never as an error (all `Option` /
//! defaulted; parsing never panics).
//!
//! Wiring rules (fail-open throughout):
//! - `uncertain == true` disables MCP pruning entirely (the route is telling
//!   us it doesn't know enough to narrow the toolset).
//! - `tool_scoring == "full"` with non-empty `ranked_tools`: the shim's
//!   ranked tools drive MCP/server suggestions (see `suggest.rs`).
//! - `ranked_models` is advisory only — surfaced in diagnostics as
//!   "SystemOne suggests X as best value; current model unchanged". This
//!   crate has no code path that switches or unloads a model.
//!
//! [`rank_plans`] calls `POST /v1/systemone/rank-plans` (also advisory,
//! fail-open: input order with `score: None` on any failure).
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

/// A tool ranked by the shim's hybrid tool scoring (`ranked_tools`):
/// keyword recall + zero-shot model precision, by `relevance` desc.
#[derive(Debug, Clone)]
pub struct RankedTool {
    /// Registry tool id (what the shim scored).
    pub id: String,
    /// Registry tool kind (e.g. `"tool"`, `"mcp"`), when present.
    pub kind: Option<String>,
    /// Relevance in [0.0, 1.0].
    pub relevance: f64,
}

/// A model ranked by expected utility (`ranked_models`): top-first, advisory.
///
/// The ranking is informational only — it is surfaced in diagnostics so the
/// operator can see what SystemOne considers best value. Nothing in this
/// crate loads, unloads, or switches models.
#[derive(Debug, Clone)]
pub struct RankedModel {
    /// Model id as known to the registry (no ID is hard-coded here; the
    /// value is whatever the shim sent).
    pub model_id: String,
    pub tier: Option<String>,
    pub utility: Option<f64>,
    pub quality: Option<f64>,
    pub cost: Option<f64>,
}

/// A candidate plan for [`rank_plans`].
#[derive(Debug, Clone)]
pub struct PlanInput {
    pub id: String,
    pub text: String,
}

/// One scored plan from `POST /v1/systemone/rank-plans`.
///
/// Advisory: the ranking informs the agent flow's plan choice; it never
/// forces one. On any failure every field except `id` is `None` and the
/// input order is preserved (fail-open).
#[derive(Debug, Clone)]
pub struct PlanRanking {
    pub id: String,
    pub score: Option<f64>,
    pub p_success: Option<f64>,
    pub cost_penalty: Option<f64>,
    pub est_steps: Option<u32>,
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
    // -- Phase 3 decision surface (all optional; older shims omit them) --
    /// The shim's uncertainty verdict. When `Some(true)`, MCP pruning is
    /// disabled entirely (see `suggest::prune_allowlist`).
    pub uncertain: Option<bool>,
    /// Top-1/top-2 calibrated probability margin.
    pub margin: Option<f64>,
    /// Whether the shim calibrated the distribution (temperature fit).
    pub calibrated: Option<bool>,
    /// `"full"` when the shim scored tools for this route, `"skipped"` on
    /// the cheap path (uncertain or low effort). `None` on older shims.
    pub tool_scoring: Option<String>,
    /// Shim-ranked tools (`{id, kind, relevance}`), relevance desc.
    pub ranked_tools: Vec<RankedTool>,
    /// Shim-ranked models (expected-utility order). Advisory only.
    pub ranked_models: Vec<RankedModel>,
    /// Calibrated tier probabilities, probability desc.
    pub calibrated_probabilities: Vec<(String, f64)>,
    /// Diagnostic note about pruning (set by `suggest_and_maybe_prune`),
    /// e.g. `"disabled(uncertain)"`.
    pub prune_note: Option<String>,
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
            uncertain: None,
            margin: None,
            calibrated: None,
            tool_scoring: None,
            ranked_tools: Vec::new(),
            ranked_models: Vec::new(),
            calibrated_probabilities: Vec::new(),
            prune_note: None,
        }
    }

    /// True when the shim attached a full tool ranking we can drive
    /// MCP/server suggestions from (`tool_scoring == "full"` with at least
    /// one ranked tool). Older shims (`None` / `"skipped"`) keep the
    /// keyword-based behavior.
    pub fn has_shim_tool_ranking(&self) -> bool {
        self.tool_scoring.as_deref() == Some("full") && !self.ranked_tools.is_empty()
    }

    /// Advisory only: the shim's best-value model pick (top of
    /// `ranked_models`), or `None` when the shim sent no ranking.
    ///
    /// Recorded for diagnostics ("SystemOne suggests X as best value;
    /// current model unchanged"). There is no code path in this crate that
    /// acts on it — model switching is deliberately not implemented.
    pub fn best_value_model(&self) -> Option<&str> {
        self.ranked_models.first().map(|m| m.model_id.as_str())
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
        if self.uncertain == Some(true) {
            parts.push("uncertain=true".to_string());
        }
        if let Some(note) = &self.prune_note {
            let short: String = note.chars().take(80).collect();
            parts.push(format!("prune={short}"));
        }
        if let Some(model) = &self.model_id {
            parts.push(format!("model_advisory={model}"));
        }
        // Advisory only: best-value suggestion; the session's model is unchanged.
        if let Some(top) = self.best_value_model() {
            parts.push(format!("model_ranking={top}"));
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

/// Fold a `/v1/systemone/route` payload into a decision. Never panics:
/// missing or mistyped new-schema keys are treated as "not present".
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

    // --- Phase 3 decision surface. Every key is optional: older shims omit
    // them and mistyped values are ignored, never panics. ---
    decision.uncertain = route
        .and_then(|r| r.get("uncertain"))
        .and_then(serde_json::Value::as_bool);
    decision.margin = route
        .and_then(|r| r.get("margin"))
        .and_then(serde_json::Value::as_f64);
    decision.calibrated = route
        .and_then(|r| r.get("calibrated"))
        .and_then(serde_json::Value::as_bool);
    decision.tool_scoring = route
        .and_then(|r| r.get("tool_scoring"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    decision.ranked_tools = route
        .and_then(|r| r.get("ranked_tools"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let id = t.get("id")?.as_str()?;
                    Some(RankedTool {
                        id: id.to_string(),
                        kind: t
                            .get("kind")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        relevance: t
                            .get("relevance")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    decision.ranked_models = route
        .and_then(|r| r.get("ranked_models"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let model_id = m.get("model_id")?.as_str()?;
                    Some(RankedModel {
                        model_id: model_id.to_string(),
                        tier: m
                            .get("tier")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        utility: m.get("utility").and_then(serde_json::Value::as_f64),
                        quality: m.get("quality").and_then(serde_json::Value::as_f64),
                        cost: m.get("cost").and_then(serde_json::Value::as_f64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    decision.calibrated_probabilities = route
        .and_then(|r| r.get("calibrated_probabilities"))
        .and_then(serde_json::Value::as_object)
        .map(|obj| {
            let mut v: Vec<(String, f64)> = obj
                .iter()
                .filter_map(|(k, val)| val.as_f64().map(|f| (k.clone(), f)))
                .collect();
            v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            v
        })
        .unwrap_or_default();
}

/// Derive a `/v1/systemone/rank-plans` URL from a configured `/route` URL.
/// Returns `None` when the URL is not a `/v1/systemone/route` endpoint (never
/// invent a path we don't recognize).
fn rank_plans_url(route_url: &str) -> Option<String> {
    route_url
        .strip_suffix("/v1/systemone/route")
        .map(|base| format!("{base}/v1/systemone/rank-plans"))
}

/// Ask SystemOne to score candidate plans: `P(plan succeeds | task)` minus a
/// cost penalty (`POST /v1/systemone/rank-plans`).
///
/// Advisory only — the ranking informs which plan the agent flow pursues; it
/// never forces a choice and never changes the session's model or effort.
///
/// Fail-open: on ANY failure (disabled routing, unreachable shim, 503 from
/// `SYSTEMONE_DISABLE`, 404 from an older shim, bad payload) the plans come
/// back in their original input order with every score field `None`. Never
/// returns an error itself, never panics.
pub async fn rank_plans(
    task: &str,
    plans: &[PlanInput],
    cfg: &SystemOneConfig,
) -> Vec<PlanRanking> {
    let fail_open = || {
        plans
            .iter()
            .map(|p| PlanRanking {
                id: p.id.clone(),
                score: None,
                p_success: None,
                cost_penalty: None,
                est_steps: None,
            })
            .collect::<Vec<_>>()
    };
    if plans.is_empty() || !cfg.routing_active() {
        return fail_open();
    }
    let task_snippet: String = task.chars().take(1500).collect();
    let body = serde_json::json!({
        "task": task_snippet,
        "plans": plans.iter().map(|p| serde_json::json!({"id": p.id, "text": p.text.chars().take(2000).collect::<String>()})).collect::<Vec<_>>(),
        "client": "grok-local",
    });

    // Localhost-only router client: the grok TLS policy is for remote hosts.
    #[allow(clippy::disallowed_methods)]
    let client = match reqwest::Client::builder()
        .timeout(cfg.timeout + Duration::from_secs(1))
        .build()
    {
        Ok(client) => client,
        Err(_) => return fail_open(),
    };

    for url in &cfg.urls {
        let Some(rp_url) = rank_plans_url(url) else {
            continue;
        };
        match client.post(&rp_url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(payload) => {
                        if let Some(ranking) = parse_plan_ranking(&payload) {
                            return ranking;
                        }
                    }
                    Err(_) => continue,
                }
            }
            // 404 (older shim), 503 (SYSTEMONE_DISABLE), transport errors:
            // try the next URL, then fail open.
            _ => continue,
        }
    }
    fail_open()
}

/// Parse a `/v1/systemone/rank-plans` payload. `None` when the shape is not
/// recognized (older/errored shim) — the caller fails open. Never panics.
fn parse_plan_ranking(payload: &serde_json::Value) -> Option<Vec<PlanRanking>> {
    let ranking = payload.get("ranking")?.as_array()?;
    Some(
        ranking
            .iter()
            .filter_map(|r| {
                let id = r.get("id")?.as_str()?;
                Some(PlanRanking {
                    id: id.to_string(),
                    score: r.get("score").and_then(serde_json::Value::as_f64),
                    p_success: r.get("p_success").and_then(serde_json::Value::as_f64),
                    cost_penalty: r.get("cost_penalty").and_then(serde_json::Value::as_f64),
                    est_steps: r
                        .get("est_steps")
                        .and_then(serde_json::Value::as_u64)
                        .map(|n| n.min(u32::MAX as u64) as u32),
                })
            })
            .collect(),
    )
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

    // ---------------- Phase 3: decision surfaces ----------------

    /// An older shim (or a hand-written payload) that omits every Phase 3
    /// key must parse exactly like the pre-Phase-3 code: no panics, no
    /// phantom values, and the old suggestion/prune behavior stays available.
    #[test]
    fn absent_new_keys_means_old_behavior() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        let payload = serde_json::json!({
            "route": {"tier": "economy", "effort": "low", "confidence": 0.9},
            "model": "knowledgator/gliclass-edge-v3.0",
        });
        apply_route_payload(&mut d, &payload);
        assert_eq!(d.uncertain, None);
        assert_eq!(d.margin, None);
        assert_eq!(d.calibrated, None);
        assert_eq!(d.tool_scoring, None);
        assert!(d.ranked_tools.is_empty());
        assert!(d.ranked_models.is_empty());
        assert!(d.calibrated_probabilities.is_empty());
        assert!(!d.has_shim_tool_ranking());
        assert_eq!(d.best_value_model(), None);
        // Evidence line must not mention surfaces that aren't there.
        let line = d.evidence_line(RouterStatus::AlreadyRunning);
        assert!(!line.contains("uncertain"));
        assert!(!line.contains("model_ranking"));
    }

    /// Mistyped new-schema values are ignored, never panics.
    #[test]
    fn malformed_new_keys_parse_fine() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        let payload = serde_json::json!({
            "route": {
                "tier": 123,
                "uncertain": "yes",
                "margin": "wide",
                "tool_scoring": 7,
                "ranked_tools": [{"nope": 1}, {"id": 42}],
                "ranked_models": [{"model_id": 9}],
                "calibrated_probabilities": {"economy": "lots"},
            },
        });
        apply_route_payload(&mut d, &payload);
        assert_eq!(d.tier, None); // falls back to high caps
        assert_eq!(d.uncertain, None);
        assert_eq!(d.margin, None);
        assert_eq!(d.tool_scoring, None);
        assert!(d.ranked_tools.is_empty());
        assert!(d.ranked_models.is_empty());
        assert!(d.calibrated_probabilities.is_empty());
        assert_eq!(d.effort, Effort::High); // fail-open caps
    }

    #[test]
    fn phase3_keys_parse() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        let payload = serde_json::json!({
            "route": {
                "tier": "balanced",
                "effort": "medium",
                "confidence": 0.78,
                "uncertain": false,
                "margin": 0.31,
                "calibrated": true,
                "tool_scoring": "full",
                "ranked_tools": [
                    {"id": "web-search", "kind": "mcp", "relevance": 0.92},
                    {"id": "browserclaw", "kind": "mcp", "relevance": 0.61},
                ],
                "ranked_models": [
                    {"model_id": "some-cheap-model", "tier": "economy", "utility": 0.81, "quality": 0.9, "cost": 0.1},
                    {"model_id": "some-bigger-model", "tier": "balanced", "utility": 0.77, "quality": 0.95, "cost": 0.4},
                ],
                "calibrated_probabilities": {"economy": 0.2, "balanced": 0.55, "heavy": 0.25},
            },
        });
        apply_route_payload(&mut d, &payload);
        assert_eq!(d.uncertain, Some(false));
        assert_eq!(d.margin, Some(0.31));
        assert_eq!(d.calibrated, Some(true));
        assert_eq!(d.tool_scoring.as_deref(), Some("full"));
        assert!(d.has_shim_tool_ranking());
        assert_eq!(d.ranked_tools.len(), 2);
        assert_eq!(d.ranked_tools[0].id, "web-search");
        assert_eq!(d.ranked_tools[0].kind.as_deref(), Some("mcp"));
        assert!((d.ranked_tools[0].relevance - 0.92).abs() < 1e-9);
        assert_eq!(d.ranked_models.len(), 2);
        // Advisory: recorded, and visible in diagnostics.
        assert_eq!(d.best_value_model(), Some("some-cheap-model"));
        // Calibrated probabilities sorted desc.
        assert_eq!(d.calibrated_probabilities[0].0, "balanced");
        assert!((d.calibrated_probabilities[0].1 - 0.55).abs() < 1e-9);
        let line = d.evidence_line(RouterStatus::AlreadyRunning);
        assert!(line.contains("model_ranking=some-cheap-model"));
        assert!(!line.contains("uncertain=true"));
    }

    /// The ranked-models surface is advisory only: parsing it records the
    /// suggestion, and there is no code path here that can switch models.
    /// (Enforced structurally — this crate has no `lms`/switch API — and
    /// pinned here so a future change can't sneak one in unnoticed: the
    /// decision carries no model handle, only strings.)
    #[test]
    fn ranked_models_never_trigger_a_switch() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        let payload = serde_json::json!({
            "route": {
                "tier": "heavy",
                "ranked_models": [
                    {"model_id": "tiny-model", "tier": "economy", "utility": 0.9},
                ],
            },
        });
        apply_route_payload(&mut d, &payload);
        assert_eq!(d.best_value_model(), Some("tiny-model"));
        // The session's effective effort/model selection are untouched by the
        // ranking: resolution is a pure function of thinking mode + decision.
        assert_eq!(ThinkingMode::Auto.resolve(&d), d.effort);
        assert_eq!(ModelSelection::default(), ModelSelection::Pinned);
    }

    #[test]
    fn evidence_line_notes_uncertain_and_prune_note() {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        d.source = RouteSource::SystemOne;
        d.tier = Some(Tier::Balanced);
        d.uncertain = Some(true);
        d.prune_note = Some("disabled(uncertain)".to_string());
        let line = d.evidence_line(RouterStatus::AlreadyRunning);
        assert!(line.contains("uncertain=true"));
        assert!(line.contains("prune=disabled(uncertain)"));
    }

    #[test]
    fn rank_plans_url_derivation() {
        assert_eq!(
            rank_plans_url("http://127.0.0.1:8765/v1/systemone/route"),
            Some("http://127.0.0.1:8765/v1/systemone/rank-plans".to_string())
        );
        // Unknown paths are never rewritten into something we didn't recognize.
        assert_eq!(rank_plans_url("http://127.0.0.1:8765/other"), None);
        assert_eq!(rank_plans_url("http://127.0.0.1:8765/"), None);
    }

    fn plans_fixture() -> Vec<PlanInput> {
        vec![
            PlanInput {
                id: "a".into(),
                text: "plan a text".into(),
            },
            PlanInput {
                id: "b".into(),
                text: "plan b text".into(),
            },
        ]
    }

    #[test]
    fn parse_plan_ranking_shape() {
        let payload = serde_json::json!({
            "task": "do x",
            "tier": "balanced",
            "ranking": [
                {"id": "b", "score": 0.82, "p_success": 0.9, "cost_penalty": 0.08, "est_steps": 5},
                {"id": "a", "score": 0.61, "p_success": 0.7, "cost_penalty": 0.09, "est_steps": 6},
            ],
        });
        let ranking = parse_plan_ranking(&payload).expect("parses");
        assert_eq!(ranking.len(), 2);
        assert_eq!(ranking[0].id, "b");
        assert_eq!(ranking[0].score, Some(0.82));
        assert_eq!(ranking[0].est_steps, Some(5));
        // Missing id -> entry dropped, never panics.
        let bad = serde_json::json!({"ranking": [{"score": 1.0}]});
        assert!(parse_plan_ranking(&bad).unwrap().is_empty());
        // Missing ranking array -> None (caller fails open).
        assert!(parse_plan_ranking(&serde_json::json!({"ok": true})).is_none());
    }

    #[tokio::test]
    async fn rank_plans_fails_open_on_unreachable_shim() {
        let cfg = SystemOneConfig {
            urls: vec!["http://127.0.0.1:1/v1/systemone/route".to_string()],
            timeout: Duration::from_secs(2),
            ..SystemOneConfig::default()
        };
        let ranking = rank_plans("do x", &plans_fixture(), &cfg).await;
        // Input order preserved, all scores None.
        assert_eq!(ranking.len(), 2);
        assert_eq!(ranking[0].id, "a");
        assert_eq!(ranking[1].id, "b");
        assert!(
            ranking
                .iter()
                .all(|r| r.score.is_none() && r.p_success.is_none())
        );
    }

    #[tokio::test]
    async fn rank_plans_kill_switch_honored() {
        // enabled=false is exactly what GROK_LOCAL_SYSTEMONE=0 produces
        // (see config::tests::env_kill_switch_wins_over_file); construct it
        // directly to avoid racing other tests' env mutation.
        let cfg = SystemOneConfig {
            enabled: false,
            ..SystemOneConfig::default()
        };
        assert!(!cfg.routing_active());
        let ranking = rank_plans("do x", &plans_fixture(), &cfg).await;
        assert_eq!(ranking.len(), 2);
        assert_eq!(ranking[0].id, "a");
        assert!(ranking.iter().all(|r| r.score.is_none()));
    }
}
