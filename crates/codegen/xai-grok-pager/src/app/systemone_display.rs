//! SystemOne status-bar suffixes for the TUI.
//!
//! Reads the `[systemone]` config and the last-route file (written by the
//! session process after each routed turn) with a 1-second throttle so
//! per-frame renders stay cheap. Pure display: never changes routing.
//!
//! Example: `grok-4.6 (high) · think:auto→high · model:auto→ornith-1.5-9b`.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use xai_grok_systemone::{LastRoute, ModelSelection, SystemOneConfig, ThinkingMode};

#[derive(Debug, Default)]
struct Cache {
    refreshed_at: Option<Instant>,
    active: bool,
    thinking: ThinkingMode,
    model_selection: ModelSelection,
    last_thinking_applied: Option<String>,
    last_model_advisory: Option<String>,
    /// The model actually selected for inference on the last routed turn
    /// (the best-value pick under `Auto`, else the session's current model).
    last_model_selected: Option<String>,
    /// Shim's best-value model ranking (top-first).
    last_model_ranking: Vec<String>,
    /// Decider-backed second opinion (tier, agreed?) on the last routed turn.
    last_second_opinion: Option<(String, bool)>,
    /// Fail-open reason when the router did not answer (sticky to next route).
    last_route_error: Option<String>,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

fn refresh_locked(guard: &mut Cache) {
    let cfg = SystemOneConfig::load();
    guard.active = cfg.routing_active();
    guard.thinking = cfg.thinking;
    guard.model_selection = cfg.model_selection;
    let last = LastRoute::load();
    guard.last_thinking_applied = last
        .as_ref()
        .map(|r| r.thinking_applied.as_str().to_string());
    guard.last_model_advisory = last.as_ref().and_then(|r| r.model_advisory.clone());
    guard.last_model_selected = last.as_ref().and_then(|r| r.model_selected.clone());
    guard.last_model_ranking = last
        .as_ref()
        .map(|r| r.model_ranking.clone())
        .unwrap_or_default();
    guard.last_second_opinion = last.as_ref().and_then(|r| {
        r.second_opinion
            .as_ref()
            .map(|op| (op.tier.clone(), op.agree))
    });
    guard.last_route_error = last.as_ref().and_then(|r| r.route_error.clone());
    guard.refreshed_at = Some(Instant::now());
}

/// Pure suffix renderer (split out for tests: no clock, no files).
fn render_suffixes(cache: &Cache) -> String {
    if !cache.active {
        return String::new();
    }
    let mut parts = Vec::with_capacity(4);
    let think = match cache.thinking {
        ThinkingMode::Auto => match &cache.last_thinking_applied {
            Some(applied) => format!("auto→{applied}"),
            None => "auto".to_string(),
        },
        ThinkingMode::Fixed(effort) => effort.as_str().to_string(),
    };
    parts.push(format!("think:{think}"));
    if cache.model_selection == ModelSelection::Auto {
        // What inference actually ran with first, then fallbacks.
        let model = match &cache.last_model_selected {
            Some(selected) => format!("auto→{selected}"),
            None => match cache.last_model_ranking.first() {
                Some(top) => format!("auto→{top}"),
                None => match &cache.last_model_advisory {
                    Some(advisory) => format!("auto→{advisory}"),
                    None => "auto".to_string(),
                },
            },
        };
        parts.push(format!("model:{model}"));
    }
    if let Some((tier, false)) = &cache.last_second_opinion {
        parts.push(format!("2nd-opinion:disagree→{tier}"));
    }
    if cache.last_route_error.is_some() {
        parts.push("router:down".to_string());
    }
    format!(" · {}", parts.join(" · "))
}

/// Suffixes to append to the status-bar model label.
///
/// - Thinking always shows: `think:auto→high` when auto resolved to high on
///   the last routed turn, `think:auto` before the first route, `think:ultra`
///   when pinned.
/// - Model shows only when automatic: `model:auto→ornith-1.5-9b` with the
///   model actually selected for inference — the best-value pick when it
///   resolved against the catalog, else the top of the shim's ranked-models
///   list, else the route's `model_id`. Pinned (the default) adds no noise.
/// - A decider-backed second opinion that *disagreed* with the route shows
///   as `2nd-opinion:disagree→balanced`; agreement stays quiet.
/// - A failed last route (fail-open) shows `router:down`, sticky until the
///   next route overwrites it, so silent fail-open stays visible.
/// - Empty string when SystemOne routing is disabled.
pub fn systemone_status_suffixes() -> String {
    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    let stale = guard
        .refreshed_at
        .is_none_or(|t| t.elapsed() > Duration::from_secs(1));
    if stale {
        refresh_locked(&mut guard);
    }
    render_suffixes(&guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_empty_when_disabled() {
        // Routing disabled via kill-switch: no suffix, no panic, no file reads
        // beyond config.
        unsafe { std::env::set_var("GROK_LOCAL_SYSTEMONE", "0") };
        // Force a refresh by clearing the cache timestamp.
        cache().lock().unwrap().refreshed_at = None;
        assert_eq!(systemone_status_suffixes(), "");
        unsafe { std::env::remove_var("GROK_LOCAL_SYSTEMONE") };
        cache().lock().unwrap().refreshed_at = None;
    }

    fn active_cache() -> Cache {
        Cache {
            active: true,
            thinking: ThinkingMode::Auto,
            model_selection: ModelSelection::Pinned,
            ..Cache::default()
        }
    }

    #[test]
    fn auto_resolution_shows() {
        let mut cache = active_cache();
        assert_eq!(render_suffixes(&cache), " · think:auto");
        cache.last_thinking_applied = Some("high".to_string());
        assert_eq!(render_suffixes(&cache), " · think:auto→high");
    }

    #[test]
    fn router_down_suffix_sticks_on_error() {
        let mut cache = active_cache();
        cache.last_thinking_applied = Some("high".to_string());
        cache.last_route_error = Some("router unreachable".to_string());
        assert_eq!(
            render_suffixes(&cache),
            " · think:auto→high · router:down"
        );
    }

    #[test]
    fn pinned_thinking_and_second_opinion() {
        use xai_grok_systemone::Effort;

        let mut cache = active_cache();
        cache.thinking = ThinkingMode::Fixed(Effort::Ultra);
        cache.last_second_opinion = Some(("balanced".to_string(), false));
        assert_eq!(
            render_suffixes(&cache),
            " · think:ultra · 2nd-opinion:disagree→balanced"
        );
        cache.last_second_opinion = Some(("heavy".to_string(), true));
        assert_eq!(render_suffixes(&cache), " · think:ultra");
    }
}
