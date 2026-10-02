//! `/systemone`: routing status and last-route diagnostics.
//!
//! One screen answering "is SystemOne working?": whether routing is
//! active and where the config came from, the thinking/model-selection
//! settings, the router URLs, and what the last routed turn ran with
//! (tier, effort, applied thinking, confidence, source) — or the sticky
//! fail-open error when the router did not answer. Sync and fast: config
//! file + last-route file only, no network probes.

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};
use xai_grok_systemone::{LastRoute, SystemOneConfig};

/// Show SystemOne routing status and last-route diagnostics.
pub struct SystemoneCommand;

fn age_string(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(ts);
    let secs = (now - ts).max(0);
    if secs < 90 {
        format!("{secs}s ago")
    } else if secs < 5400 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Pure status renderer (split out for tests: no files, no clock reads
/// beyond the caller-supplied now).
pub fn render_status(
    cfg: &SystemOneConfig,
    last: Option<&LastRoute>,
    state_path: &std::path::Path,
) -> String {
    let mut lines = Vec::new();
    if cfg.routing_active() {
        lines.push(format!(
            "SystemOne routing: active (config: {})",
            SystemOneConfig::config_path().display()
        ));
    } else if !cfg.enabled {
        lines.push(
            "SystemOne routing: OFF (disabled in config or by GROK_LOCAL_SYSTEMONE=0)"
                .to_string(),
        );
    } else {
        lines.push(
            "SystemOne routing: OFF (no router URLs configured)".to_string(),
        );
    }
    lines.push(format!(
        "  thinking: {} · model selection: {} · cost bias: {}",
        cfg.thinking.as_str(),
        cfg.model_selection.as_str(),
        cfg.cost_bias.as_deref().unwrap_or("(shim default)"),
    ));
    lines.push(format!(
        "  router: {} (shim port {}, autostart {})",
        if cfg.urls.is_empty() {
            "(none)".to_string()
        } else {
            cfg.urls.join(", ")
        },
        cfg.shim_port,
        if cfg.auto_start_shim { "on" } else { "off" },
    ));
    match last {
        None => lines.push(format!(
            "  last route: none yet (send a prompt to route; state: {})",
            state_path.display()
        )),
        Some(route) => {
            if let Some(err) = &route.route_error {
                lines.push(format!(
                    "  last route: FAILED {} — {} (fail-open; the status bar shows router:down)",
                    age_string(route.ts),
                    err
                ));
            } else {
                lines.push(format!(
                    "  last route: {} · tier={} effort={} think={} conf={} src={}",
                    age_string(route.ts),
                    route.tier.as_deref().unwrap_or("-"),
                    route.routed_effort,
                    route.thinking_applied,
                    route
                        .confidence
                        .map(|c| format!("{c:.2}"))
                        .unwrap_or_else(|| "-".to_string()),
                    route.source,
                ));
            }
            if let Some(model) = &route.model_selected {
                lines.push(format!("  last model: {model}"));
            }
        }
    }
    lines.join("\n")
}

impl SlashCommand for SystemoneCommand {
    slash_meta! {
        name: "systemone",
        description: "Show SystemOne routing status and last-route diagnostics",
        usage: "/systemone",
        takes_args: false,
        args_required: false,
        session_scoped: true,
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        let cfg = SystemOneConfig::load();
        let last = LastRoute::load();
        CommandResult::Message(render_status(
            &cfg,
            last.as_ref(),
            &LastRoute::state_file_path(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_shows_active_and_last_route() {
        let cfg = SystemOneConfig::default();
        assert!(cfg.routing_active());
        let out = render_status(&cfg, None, std::path::Path::new("/tmp/x.json"));
        assert!(out.contains("active"), "{out}");
        assert!(out.contains("none yet"), "{out}");
        assert!(out.contains("thinking: auto"), "{out}");
    }

    #[test]
    fn status_shows_failure_sticky() {
        use xai_grok_systemone::LastRoute;

        let cfg = SystemOneConfig::default();
        let last = LastRoute {
            ts: 0,
            tier: None,
            routed_effort: "high".to_string(),
            thinking_applied: "high".to_string(),
            thinking_mode: "auto".to_string(),
            model_selection: "pinned".to_string(),
            model_advisory: None,
            model_selected: None,
            second_opinion: None,
            confidence: None,
            source: "fail-open".to_string(),
            model_ranking: Vec::new(),
            uncertain: false,
            prune_note: None,
            route_error: Some("router unreachable".to_string()),
        };
        let out = render_status(&cfg, Some(&last), std::path::Path::new("/tmp/x.json"));
        assert!(out.contains("FAILED"), "{out}");
        assert!(out.contains("router unreachable"), "{out}");
        assert!(out.contains("router:down"), "{out}");
    }

    #[test]
    fn status_shows_disabled() {
        let mut cfg = SystemOneConfig::default();
        cfg.enabled = false;
        let out = render_status(&cfg, None, std::path::Path::new("/tmp/x.json"));
        assert!(out.contains("OFF"), "{out}");
    }
}
