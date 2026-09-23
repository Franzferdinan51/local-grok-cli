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
//!   `prune_mcp_servers`, `prune_min_confidence`.
//!
//! # Model switching
//!
//! Deliberately NOT implemented: the router's model id is advisory only and is
//! logged, never acted on. Ryan's standing rule — never unload a model he
//! loaded himself — is enforced by simply not having a code path that does it.

pub mod config;
pub mod route;
pub mod shim;
pub mod suggest;

pub use config::SystemOneConfig;
pub use route::{Effort, RouteDecision, RouteSource, Tier, route_for_task};
pub use shim::{RouterStatus, ensure_router};
pub use suggest::{
    McpServerInfo, ServerSuggestion, inventory_from_config, prune_allowlist,
    suggest_and_maybe_prune, suggest_mcp_servers, suggestion_detail,
};
