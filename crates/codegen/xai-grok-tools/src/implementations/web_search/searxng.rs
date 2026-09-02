//! Local SearxNG search backend.
//!
//! Grok Local talks to a user-run SearxNG instance (default
//! `http://127.0.0.1:8888`) instead of xAI's hosted `/responses` web_search
//! tool, so search works without an xAI API key. `:8080` is often Open WebUI
//! on this host, which returns HTML and used to break JSON parsing.

use std::time::Duration;

use url::Url;

/// Env vars checked (first non-empty wins) before the compiled-in default.
const SEARXNG_URL_ENV: &[&str] = &["SEARXNG_URL", "GROK_SEARXNG_URL"];

/// Default local SearxNG origin (JSON at `{origin}/search?format=json`).
pub const DEFAULT_SEARXNG_URL: &str = "http://127.0.0.1:8888";

const SEARCH_TIMEOUT: Duration = Duration::from_secs(8);

/// Strip a trailing `/search` so `SEARXNG_URL=http://127.0.0.1:8888/search`
/// still hits `/search` once, not `/search/search`.
pub fn normalize_searxng_base(raw: &str) -> String {
    let mut s = raw.trim().trim_end_matches('/').to_string();
    if s.len() >= 7 && s.to_ascii_lowercase().ends_with("/search") {
        s.truncate(s.len() - "/search".len());
        s = s.trim_end_matches('/').to_string();
    }
    s
}

/// Resolved SearxNG origin: `SEARXNG_URL` / `GROK_SEARXNG_URL`, else localhost:8888.
pub fn default_searxng_url() -> String {
    for name in SEARXNG_URL_ENV {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return normalize_searxng_base(trimmed);
            }
        }
    }
    DEFAULT_SEARXNG_URL.to_string()
}

/// True when this client should use the SearxNG JSON API instead of xAI Responses.
pub fn is_searxng_endpoint(base_url: &str, model: &str) -> bool {
    if model.eq_ignore_ascii_case("searxng") {
        return true;
    }
    let lower = base_url.to_ascii_lowercase();
    lower.contains("searx")
        || lower.contains("127.0.0.1:8888")
        || lower.contains("localhost:8888")
        || lower.contains("[::1]:8888")
        || lower.contains("127.0.0.1:8080")
        || lower.contains("localhost:8080")
        || lower.contains("[::1]:8080")
}

/// True when a successful HTTP body is HTML (Open WebUI, a search form, …).
pub fn body_looks_like_html(body: &str) -> bool {
    let t = body.trim_start();
    t.starts_with('<')
        || t.to_ascii_lowercase().starts_with("<!doctype html")
        || t.to_ascii_lowercase().contains("<html")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearxngHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Parse a SearxNG `format=json` search body into ordered hits.
pub fn parse_searxng_results(body: &serde_json::Value) -> Vec<SearxngHit> {
    let Some(results) = body.get("results").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut hits = Vec::with_capacity(results.len());
    let mut seen = std::collections::HashSet::new();
    for item in results {
        let url = item
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let snippet = item
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        hits.push(SearxngHit {
            title,
            url,
            snippet,
        });
    }
    hits
}

/// Keep a hit when it matches the effective allow/block lists.
pub fn domain_permitted(
    url: &str,
    allowed: Option<&[String]>,
    excluded: Option<&[String]>,
) -> bool {
    let host = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()));
    let Some(host) = host else {
        return allowed.is_none();
    };
    if let Some(allowed) = allowed.filter(|d| !d.is_empty()) {
        return allowed.iter().any(|d| host_matches(&host, d));
    }
    if let Some(excluded) = excluded.filter(|d| !d.is_empty()) {
        return !excluded.iter().any(|d| host_matches(&host, d));
    }
    true
}

fn host_matches(host: &str, pattern: &str) -> bool {
    let pat = pattern.trim().trim_start_matches('.').to_ascii_lowercase();
    if pat.is_empty() {
        return false;
    }
    host == pat || host.ends_with(&format!(".{pat}"))
}

/// Render hits as the markdown the agent already consumes as `content`.
pub fn format_searxng_results(query: &str, hits: &[SearxngHit]) -> String {
    if hits.is_empty() {
        return format!("No search results found for `{query}`.");
    }
    let mut out = String::from("Search results:\n");
    for (i, hit) in hits.iter().enumerate() {
        let title = if hit.title.is_empty() {
            hit.url.as_str()
        } else {
            hit.title.as_str()
        };
        out.push_str(&format!("\n{}. [{title}]({})\n", i + 1, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    out
}

pub fn search_timeout() -> Duration {
    SEARCH_TIMEOUT
}

pub fn search_url(base_url: &str) -> String {
    format!("{}/search", normalize_searxng_base(base_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_url_is_localhost_searxng() {
        assert_eq!(DEFAULT_SEARXNG_URL, "http://127.0.0.1:8888");
    }

    #[test]
    fn strips_trailing_search_path() {
        assert_eq!(
            normalize_searxng_base("http://127.0.0.1:8888/search"),
            "http://127.0.0.1:8888"
        );
        assert_eq!(
            search_url("http://127.0.0.1:8888/search/"),
            "http://127.0.0.1:8888/search"
        );
    }

    #[test]
    fn detects_searxng_by_model_and_url() {
        assert!(is_searxng_endpoint("https://api.x.ai/v1", "searxng"));
        assert!(is_searxng_endpoint("http://127.0.0.1:8888", "anything"));
        assert!(is_searxng_endpoint("http://127.0.0.1:8080", "anything"));
        assert!(is_searxng_endpoint("http://searxng:8080", "web"));
        assert!(!is_searxng_endpoint("https://api.x.ai/v1", "grok-4"));
    }

    #[test]
    fn html_bodies_are_detected() {
        assert!(body_looks_like_html("<!doctype html><html>"));
        assert!(body_looks_like_html("  <html lang=en>"));
        assert!(!body_looks_like_html("{\"results\":[]}"));
    }

    #[test]
    fn parse_dedupes_and_keeps_order() {
        let body = serde_json::json!({
            "results": [
                {"title": "A", "url": "https://a.example/1", "content": "first"},
                {"title": "A again", "url": "https://a.example/1", "content": "dup"},
                {"title": "B", "url": "https://b.example/2", "content": "second"}
            ]
        });
        let hits = parse_searxng_results(&body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://a.example/1");
        assert_eq!(hits[1].title, "B");
    }

    #[test]
    fn domain_filter_allow_and_block() {
        assert!(domain_permitted(
            "https://docs.rust-lang.org/book",
            Some(&["rust-lang.org".into()]),
            None
        ));
        assert!(!domain_permitted(
            "https://spam.example/x",
            Some(&["rust-lang.org".into()]),
            None
        ));
        assert!(!domain_permitted(
            "https://www.reddit.com/r/rust",
            None,
            Some(&["reddit.com".into()])
        ));
        assert!(domain_permitted(
            "https://docs.rs/foo",
            None,
            Some(&["reddit.com".into()])
        ));
    }

    #[test]
    fn format_includes_query_on_empty_and_links_on_hits() {
        let empty = format_searxng_results("zig allocator", &[]);
        assert!(empty.contains("zig allocator"), "{empty}");
        let hits = [SearxngHit {
            title: "Zig".into(),
            url: "https://ziglang.org/".into(),
            snippet: "The Zig programming language".into(),
        }];
        let rendered = format_searxng_results("zig", &hits);
        assert!(rendered.contains("https://ziglang.org/"), "{rendered}");
        assert!(rendered.contains("[Zig]"), "{rendered}");
        assert!(
            rendered.contains("The Zig programming language"),
            "{rendered}"
        );
    }
}
