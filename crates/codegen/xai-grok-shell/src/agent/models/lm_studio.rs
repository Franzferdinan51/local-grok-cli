//! Discover models from a local LM Studio server.
//!
//! Prefers LM Studio's native `GET /api/v0/models` (loaded + context length)
//! and falls back to OpenAI-compatible `GET /v1/models`.

use std::num::NonZeroU64;

use indexmap::IndexMap;

use crate::agent::config::{
    EnvKeys, EndpointsConfig, LM_STUDIO_DUMMY_API_KEY, ModelEntry, ModelInfo,
};
use crate::sampling::ApiBackend;

/// One model advertised by LM Studio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub loaded: bool,
    pub max_context_length: Option<u64>,
    /// LM Studio `type`: `llm`, `vlm`, `embeddings`, …
    pub kind: String,
}

fn is_chat_model(kind: &str, id: &str) -> bool {
    let k = kind.trim().to_ascii_lowercase();
    let idl = id.to_ascii_lowercase();
    if k == "embeddings" || k == "embedding" {
        return false;
    }
    if idl.contains("embed") {
        return false;
    }
    k.is_empty() || k == "llm" || k == "vlm"
}

/// Parse LM Studio native `/api/v0/models` JSON.
pub fn parse_v0_models(body: &serde_json::Value) -> Vec<DiscoveredModel> {
    parse_data_array(body, true)
}

/// Parse OpenAI-compatible `/v1/models` JSON.
pub fn parse_v1_models(body: &serde_json::Value) -> Vec<DiscoveredModel> {
    parse_data_array(body, false)
}

fn parse_data_array(body: &serde_json::Value, v0: bool) -> Vec<DiscoveredModel> {
    let Some(data) = body.get("data").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in data {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let kind = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let loaded = if v0 {
            obj.get("state")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("loaded"))
        } else {
            true
        };
        let max_context_length = obj
            .get("max_context_length")
            .or_else(|| obj.get("maxContextLength"))
            .and_then(|v| v.as_u64());
        out.push(DiscoveredModel {
            id,
            loaded,
            max_context_length,
            kind,
        });
    }
    out
}

/// Prefer currently loaded chat models; if none are loaded, list downloaded chat models.
pub fn select_listed_models(models: Vec<DiscoveredModel>) -> Vec<DiscoveredModel> {
    let chat: Vec<DiscoveredModel> = models
        .into_iter()
        .filter(|m| is_chat_model(&m.kind, &m.id))
        .collect();
    let loaded: Vec<DiscoveredModel> = chat.iter().filter(|m| m.loaded).cloned().collect();
    if loaded.is_empty() { chat } else { loaded }
}

/// Turn discovered LM Studio models into the shell catalog (chat_completions, dummy key).
pub fn catalog_from_discovered(
    models: Vec<DiscoveredModel>,
    inference_base: &str,
) -> IndexMap<String, ModelEntry> {
    let listed = select_listed_models(models);
    let mut map = IndexMap::with_capacity(listed.len());
    for model in listed {
        let mut info = ModelInfo::fallback(&model.id);
        info.base_url = inference_base.to_string();
        info.name = Some(model.id.clone());
        info.model_family = Some("lm-studio".to_string());
        info.api_backend = ApiBackend::ChatCompletions;
        info.supports_backend_search = false;
        if let Some(cw) = model.max_context_length.and_then(NonZeroU64::new) {
            info.context_window = cw;
        }
        let entry = ModelEntry {
            info,
            api_key: Some(LM_STUDIO_DUMMY_API_KEY.to_owned()),
            env_key: Some(EnvKeys::new(["LM_API_TOKEN", "OPENAI_API_KEY"])),
            auth_provider: None,
            api_base_url: Some(inference_base.to_string()),
        };
        map.insert(model.id, entry);
    }
    map
}

/// Parse whichever LM Studio JSON shape we got.
pub fn catalog_from_lm_studio_json(
    body: &serde_json::Value,
    inference_base: &str,
) -> IndexMap<String, ModelEntry> {
    let looks_v0 = body
        .get("data")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.get("state"))
        .is_some();
    let discovered = if looks_v0 {
        parse_v0_models(body)
    } else {
        parse_v1_models(body)
    };
    catalog_from_discovered(discovered, inference_base)
}

/// Blocking GET of LM Studio's model list. `None` if the server is down or empty.
pub fn fetch_local_inference_catalog(
    endpoints: &EndpointsConfig,
) -> Option<IndexMap<String, ModelEntry>> {
    let base = endpoints.resolve_inference_base_url();
    if !crate::agent::config::is_local_inference_url(&base) {
        return None;
    }
    let origin = base.trim_end_matches('/');
    let root = origin.strip_suffix("/v1").unwrap_or(origin);
    let client = crate::http::shared_startup_blocking_client();
    let timeout = crate::http::STARTUP_FETCH_TIMEOUT;
    let bearer = format!("Bearer {LM_STUDIO_DUMMY_API_KEY}");
    let get_json = |url: &str| -> Option<serde_json::Value> {
        let response = client
            .get(url)
            .timeout(timeout)
            .header("Authorization", &bearer)
            .send()
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json().ok()
    };
    if let Some(json) = get_json(&format!("{root}/api/v0/models")) {
        let catalog = catalog_from_lm_studio_json(&json, origin);
        if !catalog.is_empty() {
            tracing::info!(count = catalog.len(), url = %format!("{root}/api/v0/models"), "LM Studio models discovered");
            return Some(catalog);
        }
    }
    if let Some(json) = get_json(&format!("{origin}/models")) {
        let catalog = catalog_from_lm_studio_json(&json, origin);
        if !catalog.is_empty() {
            tracing::info!(count = catalog.len(), url = %format!("{origin}/models"), "LM Studio /v1/models discovered");
            return Some(catalog);
        }
    }
    tracing::warn!(base = %base, "LM Studio returned no chat models");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v0_prefers_loaded_llms_and_drops_embeddings() {
        let body = serde_json::json!({
            "data": [
                {
                    "id": "text-embedding-nomic",
                    "type": "embeddings",
                    "state": "loaded",
                    "max_context_length": 8192
                },
                {
                    "id": "ornith-1.5-35b-a3b",
                    "type": "llm",
                    "state": "loaded",
                    "max_context_length": 32768
                },
                {
                    "id": "other-gguf",
                    "type": "llm",
                    "state": "not-loaded",
                    "max_context_length": 8192
                }
            ]
        });
        let listed = select_listed_models(parse_v0_models(&body));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "ornith-1.5-35b-a3b");
        let catalog = catalog_from_lm_studio_json(&body, "http://127.0.0.1:1234/v1");
        assert_eq!(catalog.len(), 1);
        let entry = catalog.get("ornith-1.5-35b-a3b").expect("discovered model");
        assert_eq!(entry.info.model, "ornith-1.5-35b-a3b");
        assert_eq!(entry.info.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
        assert_eq!(entry.info.context_window.get(), 32768);
        assert_eq!(entry.api_key.as_deref(), Some(LM_STUDIO_DUMMY_API_KEY));
        assert!(!entry.info.supports_backend_search);
        assert!(!catalog.contains_key("text-embedding-nomic"));
        assert!(!catalog.contains_key("other-gguf"));
        assert!(!catalog.contains_key("local-model"));
        assert!(!catalog.contains_key("grok-4.6"));
    }

    #[test]
    fn v0_falls_back_to_downloaded_llms_when_none_loaded() {
        let body = serde_json::json!({
            "data": [
                {"id": "aaa", "type": "llm", "state": "not-loaded"},
                {"id": "bbb", "type": "llm", "state": "not-loaded"}
            ]
        });
        let listed = select_listed_models(parse_v0_models(&body));
        assert_eq!(
            listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["aaa", "bbb"]
        );
    }

    #[test]
    fn v1_lists_chat_ids_and_drops_embeddings() {
        let body = serde_json::json!({
            "data": [
                {"id": "ornith-1.5-35b-a3b", "object": "model"},
                {"id": "text-embedding-nomic-embed-text-v1.5", "object": "model"},
                {"id": "second-local", "object": "model"}
            ]
        });
        let catalog = catalog_from_lm_studio_json(&body, "http://127.0.0.1:1234/v1");
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains_key("ornith-1.5-35b-a3b"));
        assert!(catalog.contains_key("second-local"));
        assert!(!catalog.contains_key("text-embedding-nomic-embed-text-v1.5"));
        assert!(!catalog.contains_key("local-model"));
        assert!(!catalog.contains_key("grok-4.6"));
        for entry in catalog.values() {
            assert_eq!(entry.info.base_url, "http://127.0.0.1:1234/v1");
            assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
        }
    }

    #[test]
    fn live_shaped_v0_fixture_only_loaded_chat_models() {
        let body = serde_json::json!({
            "data": [
                {
                    "id": "ornith-1.5-35b-a3b",
                    "type": "llm",
                    "state": "loaded",
                    "max_context_length": 262144
                },
                {
                    "id": "text-embedding-nomic-embed-text-v1.5",
                    "type": "embeddings",
                    "state": "loaded"
                },
                {
                    "id": "ornith-1.5-9b",
                    "type": "vlm",
                    "state": "not-loaded"
                },
                {
                    "id": "google/gemma-4-26b-a4b-qat",
                    "type": "vlm",
                    "state": "not-loaded"
                }
            ]
        });
        let catalog = catalog_from_lm_studio_json(&body, "http://127.0.0.1:1234/v1");
        let ids: Vec<_> = catalog.keys().cloned().collect();
        assert_eq!(ids, vec!["ornith-1.5-35b-a3b".to_string()]);
        assert!(!catalog.keys().any(|k| k.contains("embed") || k.starts_with("grok-")));
        let entry = &catalog["ornith-1.5-35b-a3b"];
        assert_eq!(entry.info.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
        assert_eq!(entry.info.context_window.get(), 262144);
    }
}
