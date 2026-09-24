//! Native SystemOne routing for grok-local.
//!
//! This crate bakes the SystemOne "speed stack" dispatcher directly into the
//! Rust binary so it works out-of-the-box with zero manual setup: no external
//! Python adapter to wire, no config entries to add, no shim to launch by hand.
//!
//! What it does, per task:
//! - [`ensure_router`]: probes `http://127.0.0.1:8765/healthz`; if the router is
//!   down it starts the SystemOne shim itself (detached, lock-guarded).
//! - [`route_for_task`]: POSTs the task to the router and maps the returned
//!   tier to a reasoning effort and loop caps, mirroring the semantics of the
//!   `grok-local-acp-adapter` v0.5.2 (`integrations/grok-local-acp-adapter/`).
//! - [`suggest_mcp_servers`] / [`prune_allowlist`]: route-driven MCP server
//!   suggestions, with conservative opt-in pruning.
//!
//! # Decision surfaces (Phase 3)
//!
//! Newer shims attach a scored surface to the route: `uncertain`, `margin`,
//! `calibrated_probabilities`, `ranked_models` (expected-utility order,
//! advisory), `ranked_tools` with `tool_scoring` (`"full"` | `"skipped"`).
//! - [`RouteDecision::uncertain`] disables pruning unconditionally.
//! - [`RouteDecision::has_shim_tool_ranking`]: when true, the shim's ranked
//!   tools drive MCP/server suggestions (see
//!   [`suggest::suggestions_from_ranked_tools`]).
//! - [`rank_plans`] scores candidate plans via `POST /v1/systemone/rank-plans`
//!   (advisory, fail-open).
//! Older shims omit these keys: absence is "not present", never an error.
//!
//! # Thinking levels (v0.5.1)
//!
//! [`ThinkingMode`] is the user's thinking setting: `Auto` (the router picks
//! the effort per task, and may reach `XHigh`/`Ultra` for heavy work) or a
//! pinned level (`Off | Low | Medium | High | XHigh | Ultra`). [`ModelSelection`]
//! (`Auto | Pinned`) is a fully independent control: the two never influence
//! each other. The routing layer only *resolves* these settings to concrete
//! values; all UI lives outside this crate (kept deliberately separate so the
//! agent-flow work can build on this layer without touching UI code).
//!
//! # Fail-open contract
//!
//! Every public entry point is infallible from the caller's perspective: if the
//! router is unreachable, errors, times out, or is disabled, the caller gets a
//! default decision and the session proceeds exactly as if routing did not
//! exist. Nothing here may break a session.
//!
//! # Kill-switches
//!
//! - `GROK_LOCAL_SYSTEMONE=0` (also `false`/`off`/`no`): disables ALL routing
//!   behavior — no probe, no shim start, no route call, no pruning.
//! - `GROK_LOCAL_SYSTEMONE_NO_AUTOSTART=1`: probe only, never start the shim.
//! - `GROK_LOCAL_SYSTEMONE_PRUNE=1`: opts in to conservative MCP pruning
//!   (off by default).
//! - `~/.grok-local/config.toml` `[systemone]` section: `enabled`, `urls`,
//!   `timeout_secs`, `default_effort`, `auto_start_shim`, `shim_port`,
//!   `prune_mcp_servers`, `prune_min_confidence`, `thinking`, `model_selection`.
//!
//! # Model switching
//!
//! Deliberately NOT implemented: the router's model id and `ranked_models`
//! are advisory only and are logged, never acted on. Ryan's standing rule —
//! never unload a model he loaded himself — is enforced by simply not having
//! a code path that does it.

pub mod config;
pub mod route;
pub mod shim;
pub mod state;
pub mod suggest;

pub use config::SystemOneConfig;
pub use route::{
    Effort, ModelSelection, PlanInput, PlanRanking, RankedModel, RankedTool, RouteDecision,
    RouteSource, ThinkingMode, Tier, rank_plans, route_for_task,
};
pub use shim::{RouterStatus, ensure_router};
pub use state::LastRoute;
pub use suggest::{
    McpServerInfo, ServerSuggestion, inventory_from_config, prune_allowlist,
    suggest_and_maybe_prune, suggest_mcp_servers, suggestion_detail, suggestions_from_ranked_tools,
};
