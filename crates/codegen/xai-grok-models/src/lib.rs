//! Default model IDs loaded from `default_models.json` at runtime.
//! Edit that JSON file to change them.
//!
//! At runtime each model is resolved from the first of these that is set: CLI flag, ENV var, config.toml, remote settings, these defaults.

use std::sync::LazyLock;

/// The raw JSON, embedded at compile time.
/// It is `pub` because `xai_grok_shell::models` re-exports it and `agent::config` reads it.
pub const DEFAULT_MODELS_JSON: &str = include_str!("../default_models.json");

#[derive(serde::Deserialize)]
struct DefaultModels {
    default: String,
    /// Falls back to `default` if not specified in JSON.
    web_search: Option<String>,
    /// Falls back to `default` if not specified in JSON.
    image_description: Option<String>,
    /// Falls back to `default` if not specified in JSON.
    session_summary: Option<String>,
    models: Vec<DefaultModelEntry>,
}

#[derive(serde::Deserialize)]
struct DefaultModelEntry {
    model: String,
}

static DEFAULTS: LazyLock<DefaultModels> = LazyLock::new(|| {
    let defaults: DefaultModels = serde_json::from_str(DEFAULT_MODELS_JSON)
        .expect("default_models.json: invalid JSON or missing 'default' field");

    // Baked-in JSON: a mismatch here is a developer error
    let model_ids: Vec<&str> = defaults.models.iter().map(|m| m.model.as_str()).collect();
    assert!(
        model_ids.contains(&defaults.default.as_str()),
        "default_models.json: 'default' is '{}' but 'models' array only has {model_ids:?}",
        defaults.default,
    );

    defaults
});

fn env_local_model() -> Option<String> {
    for name in ["LM_STUDIO_MODEL", "GROK_MODEL"] {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

static EFFECTIVE_DEFAULT: LazyLock<String> = LazyLock::new(|| {
    env_local_model().unwrap_or_else(|| DEFAULTS.default.clone())
});

/// Primary model for coding tasks and general fallback.
/// `LM_STUDIO_MODEL` / `GROK_MODEL` override the baked-in slug so the loaded
/// LM Studio model is used without a config.toml.
pub fn default_model() -> &'static str {
    &EFFECTIVE_DEFAULT
}

/// Model for web search tool synthesis. Falls back to default model.
pub fn default_web_search_model() -> &'static str {
    default_model()
}

/// Model for image describe. Falls back to default model.
pub fn default_image_description_model() -> &'static str {
    default_model()
}

/// Model for session title generation. Falls back to default model.
pub fn default_session_summary_model() -> &'static str {
    default_model()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baked_in_default_is_local_model() {
        assert_eq!(DEFAULTS.default, "local-model");
        assert!(
            DEFAULTS
                .models
                .iter()
                .any(|m| m.model == "local-model"),
            "catalog must include local-model"
        );
    }
}
