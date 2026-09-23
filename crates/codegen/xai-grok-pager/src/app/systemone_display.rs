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
    guard.refreshed_at = Some(Instant::now());
}

/// Suffixes to append to the status-bar model label.
///
/// - Thinking always shows: `think:auto→high` when auto resolved to high on
///   the last routed turn, `think:auto` before the first route, `think:ultra`
///   when pinned.
/// - Model shows only when automatic: `model:auto→ornith-1.5-9b` with the
///   router's last advisory, `model:auto` before the first route. Pinned (the
///   default) adds no noise.
/// - Empty string when SystemOne routing is disabled.
pub fn systemone_status_suffixes() -> String {
    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    let stale = guard
        .refreshed_at
        .is_none_or(|t| t.elapsed() > Duration::from_secs(1));
    if stale {
        refresh_locked(&mut guard);
    }
    if !guard.active {
        return String::new();
    }
    let mut parts = Vec::with_capacity(2);
    let think = match guard.thinking {
        ThinkingMode::Auto => match &guard.last_thinking_applied {
            Some(applied) => format!("auto→{applied}"),
            None => "auto".to_string(),
        },
        ThinkingMode::Fixed(effort) => effort.as_str().to_string(),
    };
    parts.push(format!("think:{think}"));
    if guard.model_selection == ModelSelection::Auto {
        let model = match &guard.last_model_advisory {
            Some(advisory) => format!("auto→{advisory}"),
            None => "auto".to_string(),
        };
        parts.push(format!("model:{model}"));
    }
    format!(" · {}", parts.join(" · "))
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
}
