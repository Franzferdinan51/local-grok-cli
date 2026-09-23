//! Configuration for native SystemOne routing.
//!
//! Sources, in increasing precedence:
//! 1. Built-in defaults (mirror the `grok-local-acp-adapter` v0.5.2 `[speed]` defaults).
//! 2. `~/.grok-local/config.toml` `[systemone]` section.
//! 3. Environment variables (the kill-switches live here and always win).
//!
//! Fail-open: any unreadable file or unparsable value is ignored and the
//! default is kept. A bad config must never break a session.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::route::{Effort, ModelSelection, ThinkingMode};

/// Master kill-switch. `0`/`false`/`off`/`no` (case-insensitive) disables ALL
/// SystemOne behavior: no probe, no shim start, no route call, no pruning.
pub const ENV_KILL_SWITCH: &str = "GROK_LOCAL_SYSTEMONE";
/// Opt-in conservative MCP pruning: `1`/`true`/`on`/`yes` enables it.
pub const ENV_PRUNE: &str = "GROK_LOCAL_SYSTEMONE_PRUNE";
/// Comma-separated router URLs; overrides the config file.
pub const ENV_URLS: &str = "GROK_LOCAL_SYSTEMONE_URLS";
/// Router HTTP timeout, in seconds.
pub const ENV_TIMEOUT_SECS: &str = "GROK_LOCAL_SYSTEMONE_TIMEOUT_SECS";
/// `1`/`true`: probe the router but never start the shim automatically.
pub const ENV_NO_AUTOSTART: &str = "GROK_LOCAL_SYSTEMONE_NO_AUTOSTART";
/// Override the shim release directory (default: `$SYSTEMONE_RELEASE_DIR`, then
/// `~/systemone-release`).
pub const ENV_RELEASE_DIR: &str = "SYSTEMONE_RELEASE_DIR";
/// Override the Python interpreter used to launch the shim (default: `python3.11`).
pub const ENV_PYTHON: &str = "SYSTEMONE_PYTHON";
/// Override the thinking level (`off|low|medium|high|xhigh|ultra|auto`).
/// Highest precedence after the kill-switch: lets one-shot invocations pin
/// thinking without touching the config file.
pub const ENV_THINKING: &str = "GROK_LOCAL_SYSTEMONE_THINKING";
/// Override the model selection (`auto|pinned`). Same precedence as
/// [`ENV_THINKING`].
pub const ENV_MODEL_SELECTION: &str = "GROK_LOCAL_SYSTEMONE_MODEL_SELECTION";

/// Default router endpoints: the local shim, then the Jeff-1 fallback.
/// Mirrors the adapter's `systemone_urls`.
fn default_urls() -> Vec<String> {
    vec![
        "http://127.0.0.1:8765/v1/systemone/route".to_string(),
        "http://127.0.0.1:8079/v1/systemone/route".to_string(),
    ]
}

#[derive(Debug, Clone)]
pub struct SystemOneConfig {
    /// Master switch. When false, [`SystemOneConfig::routing_active`] is false
    /// and callers must skip routing entirely.
    pub enabled: bool,
    /// Router endpoints, tried in order.
    pub urls: Vec<String>,
    /// Per-request HTTP timeout.
    pub timeout: Duration,
    /// Effort used when the router is unreachable (fail-open) or returns no tier.
    pub default_effort: Effort,
    /// When true and the router is down, start the shim automatically.
    pub auto_start_shim: bool,
    /// Port the shim is probed/started on.
    pub shim_port: u16,
    /// Opt-in: prune the per-session MCP server list to the route's suggestions.
    pub prune_mcp_servers: bool,
    /// Minimum route confidence for pruning to engage.
    pub prune_min_confidence: f64,
    /// Thinking-level setting: `Auto` (router decides per task) or a pinned
    /// level. Fully independent from [`SystemOneConfig::model_selection`].
    pub thinking: ThinkingMode,
    /// Model-selection setting: `Auto` (SystemOne picks per task — advisory
    /// only, the session never switches) or `Pinned` (use the session's
    /// active model). Fully independent from [`SystemOneConfig::thinking`].
    pub model_selection: ModelSelection,
}

impl Default for SystemOneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            urls: default_urls(),
            timeout: Duration::from_secs(3),
            // Fail-open effort mirrors grok-local's own default_reasoning_effort.
            default_effort: Effort::High,
            auto_start_shim: true,
            shim_port: 8765,
            prune_mcp_servers: false,
            prune_min_confidence: 0.85,
            thinking: ThinkingMode::Auto,
            model_selection: ModelSelection::Pinned,
        }
    }
}

impl SystemOneConfig {
    /// Load from defaults + `$GROK_HOME/config.toml` `[systemone]` + env.
    pub fn load() -> Self {
        Self::load_from(&xai_dirs::grok_home())
    }

    /// Load with an explicit grok home (used by tests).
    pub fn load_from(grok_home: &Path) -> Self {
        let mut cfg = Self::default();
        let path = grok_home.join("config.toml");
        // NOTE: a full document must parse as `toml::Table`; `Value::from_str`
        // only parses a single TOML value and rejects `[section]` headers.
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(table) = text.parse::<toml::Table>()
            && let Some(section) = table.get("systemone")
        {
            cfg.apply_toml(section);
        }
        cfg.apply_env();
        cfg
    }

    /// True when routing should run at all. The single gate callers check.
    pub fn routing_active(&self) -> bool {
        self.enabled && !self.urls.is_empty()
    }

    /// Path of the config file this was loaded from (for diagnostics).
    pub fn config_path() -> PathBuf {
        xai_dirs::grok_home().join("config.toml")
    }

    /// Persist a thinking-level setting to `[systemone] thinking` in the
    /// config file (read-modify-write; other keys preserved). Fail-open:
    /// returns `false` on any IO/parse error, `true` on success.
    pub fn save_thinking(mode: ThinkingMode) -> bool {
        Self::save_systemone_key("thinking", mode.as_str())
    }

    /// Persist a model-selection setting to `[systemone] model_selection`.
    /// Fail-open: returns `false` on any IO/parse error, `true` on success.
    pub fn save_model_selection(sel: ModelSelection) -> bool {
        Self::save_systemone_key("model_selection", sel.as_str())
    }

    fn save_systemone_key(key: &str, value: &str) -> bool {
        let path = Self::config_path();
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return false;
        }
        let mut table: toml::Table = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or_default();
        let section = table
            .entry("systemone".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(map) = section.as_table_mut() {
            map.insert(key.to_string(), toml::Value::String(value.to_string()));
        } else {
            return false;
        }
        let text = table.to_string();
        // Write atomically-ish: temp file + rename, so a crash can't corrupt
        // the user's config.
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_err() {
            return false;
        }
        std::fs::rename(&tmp, &path).is_ok()
    }

    fn apply_toml(&mut self, section: &toml::Value) {
        let get = |key: &str| section.get(key);
        if let Some(v) = get("enabled").and_then(toml::Value::as_bool) {
            self.enabled = v;
        }
        if let Some(urls) = get("urls").and_then(toml::Value::as_array) {
            let parsed: Vec<String> = urls
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect();
            if !parsed.is_empty() {
                self.urls = parsed;
            }
        }
        if let Some(secs) = get("timeout_secs").and_then(toml::Value::as_integer)
            && secs > 0
        {
            self.timeout = Duration::from_secs(secs as u64);
        }
        if let Some(effort) = get("default_effort")
            .and_then(toml::Value::as_str)
            .and_then(Effort::parse)
        {
            self.default_effort = effort;
        }
        if let Some(v) = get("auto_start_shim").and_then(toml::Value::as_bool) {
            self.auto_start_shim = v;
        }
        if let Some(port) = get("shim_port").and_then(toml::Value::as_integer)
            && (1..=65535).contains(&port)
        {
            self.shim_port = port as u16;
        }
        if let Some(v) = get("prune_mcp_servers").and_then(toml::Value::as_bool) {
            self.prune_mcp_servers = v;
        }
        if let Some(conf) = get("prune_min_confidence").and_then(toml::Value::as_float)
            && (0.0..=1.0).contains(&conf)
        {
            self.prune_min_confidence = conf;
        }
        if let Some(mode) = get("thinking")
            .and_then(toml::Value::as_str)
            .and_then(ThinkingMode::parse)
        {
            self.thinking = mode;
        }
        if let Some(sel) = get("model_selection")
            .and_then(toml::Value::as_str)
            .and_then(ModelSelection::parse)
        {
            self.model_selection = sel;
        }
    }

    fn apply_env(&mut self) {
        if let Some(v) = std::env::var(ENV_KILL_SWITCH)
            .ok()
            .and_then(|s| parse_bool(&s))
        {
            self.enabled = v;
        }
        if let Some(v) = std::env::var(ENV_PRUNE).ok().and_then(|s| parse_bool(&s)) {
            self.prune_mcp_servers = v;
        }
        if let Some(v) = std::env::var(ENV_NO_AUTOSTART)
            .ok()
            .and_then(|s| parse_bool(&s))
        {
            self.auto_start_shim = !v;
        }
        if let Ok(raw) = std::env::var(ENV_URLS) {
            let parsed: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if !parsed.is_empty() {
                self.urls = parsed;
            }
        }
        if let Ok(raw) = std::env::var(ENV_TIMEOUT_SECS)
            && let Ok(secs) = raw.trim().parse::<u64>()
            && secs > 0
        {
            self.timeout = Duration::from_secs(secs);
        }
        // Session-scoped overrides (e.g. set by CLI flags for one invocation).
        // These win over the config file; slash commands (`/thinking`,
        // `/effort`, `/model`) write the file instead so the setting persists.
        if let Ok(raw) = std::env::var(ENV_THINKING)
            && let Some(mode) = ThinkingMode::parse(&raw)
        {
            self.thinking = mode;
        }
        if let Ok(raw) = std::env::var(ENV_MODEL_SELECTION)
            && let Some(sel) = ModelSelection::parse(&raw)
        {
            self.model_selection = sel;
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process env (cargo runs tests in parallel).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn scrub_speed_env() {
        for key in [
            ENV_KILL_SWITCH,
            ENV_PRUNE,
            ENV_URLS,
            ENV_TIMEOUT_SECS,
            ENV_NO_AUTOSTART,
            ENV_THINKING,
            ENV_MODEL_SELECTION,
        ] {
            unsafe { std::env::remove_var(key) };
        }
    }

    #[test]
    fn kill_switch_values_parse() {
        for s in ["0", "false", "off", "no", "FALSE", " Off "] {
            assert_eq!(parse_bool(s), Some(false), "{s}");
        }
        for s in ["1", "true", "on", "yes", "TRUE"] {
            assert_eq!(parse_bool(s), Some(true), "{s}");
        }
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool(""), None);
    }

    #[test]
    fn defaults_mirror_adapter() {
        let cfg = SystemOneConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.urls.len(), 2);
        assert!(cfg.urls[0].contains(":8765"));
        assert_eq!(cfg.timeout, Duration::from_secs(3));
        assert_eq!(cfg.default_effort, Effort::High);
        assert!(cfg.auto_start_shim);
        assert!(!cfg.prune_mcp_servers);
        assert!(cfg.routing_active());
        // v0.5.1 defaults: router-driven thinking, pinned model.
        assert_eq!(cfg.thinking, ThinkingMode::Auto);
        assert_eq!(cfg.model_selection, ModelSelection::Pinned);
    }

    #[test]
    fn toml_section_applies() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            [
                "[systemone]",
                "enabled = false",
                "timeout_secs = 7",
                "default_effort = \"low\"",
                "prune_mcp_servers = true",
                "prune_min_confidence = 0.9",
                "shim_port = 9999",
                "thinking = \"ultra\"",
                "model_selection = \"auto\"",
                "urls = [\"http://127.0.0.1:9999/v1/systemone/route\"]",
                "",
            ]
            .join("\n"),
        )
        .unwrap();
        // Scrub env so the test is hermetic.
        scrub_speed_env();
        let cfg = SystemOneConfig::load_from(dir.path());
        assert!(!cfg.enabled);
        assert!(!cfg.routing_active());
        assert_eq!(cfg.timeout, Duration::from_secs(7));
        assert_eq!(cfg.default_effort, Effort::Low);
        assert!(cfg.prune_mcp_servers);
        assert_eq!(cfg.prune_min_confidence, 0.9);
        assert_eq!(cfg.shim_port, 9999);
        assert_eq!(cfg.urls, vec!["http://127.0.0.1:9999/v1/systemone/route"]);
        assert_eq!(cfg.thinking, ThinkingMode::Fixed(Effort::Ultra));
        assert_eq!(cfg.model_selection, ModelSelection::Auto);
    }

    #[test]
    fn bad_toml_values_keep_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[systemone]\ntimeout_secs = -3\nshim_port = 99999\ndefault_effort = \"turbo\"\nurls = []\nthinking = \"ludicrous\"\nmodel_selection = \"grok-4\"\n",
        )
        .unwrap();
        scrub_speed_env();
        let cfg = SystemOneConfig::load_from(dir.path());
        assert_eq!(cfg.timeout, Duration::from_secs(3));
        assert_eq!(cfg.shim_port, 8765);
        assert_eq!(cfg.default_effort, Effort::High);
        assert_eq!(cfg.urls.len(), 2);
        assert_eq!(cfg.thinking, ThinkingMode::Auto);
        assert_eq!(cfg.model_selection, ModelSelection::Pinned);
    }

    #[test]
    fn env_thinking_and_model_selection_override_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[systemone]\nthinking = \"low\"\nmodel_selection = \"pinned\"\n",
        )
        .unwrap();
        scrub_speed_env();
        unsafe {
            std::env::set_var(ENV_THINKING, "ultra");
            std::env::set_var(ENV_MODEL_SELECTION, "auto");
        }
        let cfg = SystemOneConfig::load_from(dir.path());
        assert_eq!(cfg.thinking, ThinkingMode::Fixed(Effort::Ultra));
        assert_eq!(cfg.model_selection, ModelSelection::Auto);
        // Invalid values are ignored, the file value stands.
        unsafe { std::env::set_var(ENV_THINKING, "ludicrous") };
        let cfg = SystemOneConfig::load_from(dir.path());
        assert_eq!(cfg.thinking, ThinkingMode::Fixed(Effort::Low));
        scrub_speed_env();
    }

    #[test]
    fn env_kill_switch_wins_over_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[systemone]\nenabled = true\n",
        )
        .unwrap();
        scrub_speed_env();
        unsafe { std::env::set_var(ENV_KILL_SWITCH, "0") };
        let cfg = SystemOneConfig::load_from(dir.path());
        assert!(!cfg.routing_active());
        unsafe { std::env::remove_var(ENV_KILL_SWITCH) };
        let cfg = SystemOneConfig::load_from(dir.path());
        assert!(cfg.routing_active());
    }
}
