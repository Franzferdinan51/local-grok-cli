//! Typed decisions via the SystemOne shim (`POST /v1/systemone/decide`).
//!
//! The SystemOne router grew a decision engine (newer shims, Phase 3+): typed
//! choice/noul/score questions answered with calibrated probabilities. The
//! shim proxies to the Jeff-1 sidecar when it is up (`backend: "jeff1"`) and
//! falls back to the local GLiClass engine otherwise (`backend: "fallback"`).
//! The confidence/temperature machinery behind the endpoint is adapted from
//! Mapika/decider (Apache-2.0); the shim's own implementation is original.
//!
//! Request schema: `{state, instructions, criteria, type}` —
//! `type` is one of `choice | noul | score`; `criteria` is `{label:
//! description}` for `choice`, `{yes, no}` descriptions (or null) for `noul`,
//! and `"0".."n-1"` level descriptions for `score`. Response:
//! `{type, label|level, probabilities|distribution, confidence, latency_ms,
//! backend}`.
//!
//! Unlike routing ([`crate::route_for_task`]) and plan ranking
//! ([`crate::rank_plans`]), this module is NOT fail-open: [`decide`] returns
//! [`DecideError`] on any failure. It backs the explicit `grok-local decide`
//! command, where a down shim must surface as a clear error and a non-zero
//! exit — never as a silent default. Decide URLs are derived from the existing
//! configured route URLs (same convention as `rank_plans`); no new endpoint
//! defaults are introduced.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::config::SystemOneConfig;

/// The answer type that was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecideType {
    /// Pick one label from named criteria.
    Choice,
    /// Yes/no question answered with calibrated uncertainty.
    Noul,
    /// Pick a score level `0`..`n-1`.
    Score,
}

impl DecideType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Noul => "noul",
            Self::Score => "score",
        }
    }
}

impl fmt::Display for DecideType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DecideType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "choice" => Ok(Self::Choice),
            "noul" => Ok(Self::Noul),
            "score" => Ok(Self::Score),
            other => Err(format!(
                "unknown decide type '{other}': expected choice, noul, or score"
            )),
        }
    }
}

/// One typed decision answered by the shim.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DecideDecision {
    /// The answer type that was resolved.
    pub decision_type: DecideType,
    /// Winning label (`choice`/`noul`) or level (`score`).
    pub winner: String,
    /// Every label/level with its probability, highest first.
    pub distribution: Vec<(String, f64)>,
    /// Calibrated confidence in the winner (0..1 when the shim reports it).
    pub confidence: f64,
    /// Server-side latency in milliseconds, when reported.
    pub latency_ms: Option<f64>,
    /// Which backend answered: `"jeff1"` or `"fallback"` (GLiClass).
    pub backend: Option<String>,
}

/// What went wrong when asking the shim for a decision.
#[derive(Debug, Clone)]
pub enum DecideError {
    /// Routing is disabled (master kill-switch `GROK_LOCAL_SYSTEMONE=0`,
    /// or no URLs configured).
    Disabled,
    /// No shim answered on any configured decide URL.
    ShimUnavailable { urls: Vec<String>, detail: String },
    /// The shim rejected the request (e.g. HTTP 400 with a validator message).
    InvalidRequest(String),
    /// A shim answered but its payload was not a decide response.
    InvalidResponse(String),
}

impl fmt::Display for DecideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(
                f,
                "SystemOne routing is disabled (GROK_LOCAL_SYSTEMONE=0 or no URLs configured); \
                 enable it to use decide"
            ),
            Self::ShimUnavailable { urls, detail } => write!(
                f,
                "SystemOne shim unavailable: no decide endpoint answered (tried: {}). {detail}",
                urls.join(", ")
            ),
            Self::InvalidRequest(msg) => write!(f, "shim rejected the decide request: {msg}"),
            Self::InvalidResponse(msg) => {
                write!(f, "shim returned an unrecognized decide payload: {msg}")
            }
        }
    }
}

impl std::error::Error for DecideError {}

/// Derive a `/v1/systemone/decide` URL from a configured `/route` URL.
/// Returns `None` when the URL is not a `/v1/systemone/route` endpoint (never
/// invent a path we don't recognize). Mirrors `rank_plans_url`.
fn decide_url(route_url: &str) -> Option<String> {
    route_url
        .strip_suffix("/v1/systemone/route")
        .map(|base| format!("{base}/v1/systemone/decide"))
}

/// Ask SystemOne for a typed decision: `POST /v1/systemone/decide`.
///
/// `state` is free-form context for the decision, `instructions` the
/// question/instructions (must be non-empty), `criteria` the per-type
/// criteria payload (object or null), `qtype` the answer type. Tries every
/// configured route URL in order (derived to the matching decide path).
///
/// NOT fail-open: every failure mode returns a [`DecideError`] so an explicit
/// caller (the `grok-local decide` command) can report it and exit non-zero.
/// Never panics.
pub async fn decide(
    state: &str,
    instructions: &str,
    criteria: serde_json::Value,
    qtype: DecideType,
    cfg: &SystemOneConfig,
) -> Result<DecideDecision, DecideError> {
    if !cfg.routing_active() {
        return Err(DecideError::Disabled);
    }
    if instructions.trim().is_empty() {
        return Err(DecideError::InvalidRequest(
            "instructions must be a non-empty string".to_string(),
        ));
    }
    if matches!(criteria, serde_json::Value::Null)
        && matches!(qtype, DecideType::Choice | DecideType::Score)
    {
        return Err(DecideError::InvalidRequest(format!(
            "decide type '{qtype}' requires criteria; pass --criterion label=description (repeatable) or --criteria-json"
        )));
    }

    let body = serde_json::json!({
        "state": state,
        "instructions": instructions,
        "criteria": criteria,
        "type": qtype.as_str(),
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
            return Err(DecideError::ShimUnavailable {
                urls: cfg.urls.clone(),
                detail: format!("cannot build HTTP client: {err}"),
            });
        }
    };

    let mut tried: Vec<String> = Vec::new();
    let mut last_detail = "no decide-capable URL configured".to_string();
    for url in &cfg.urls {
        let Some(d_url) = decide_url(url) else {
            continue;
        };
        tried.push(d_url.clone());
        match client.post(&d_url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.json::<serde_json::Value>().await {
                        Ok(payload) => match parse_decide(&payload, qtype) {
                            Some(decision) => return Ok(decision),
                            None => {
                                last_detail = format!(
                                    "{d_url} answered 200 but the payload was not a decide response"
                                );
                                continue;
                            }
                        },
                        Err(err) => {
                            last_detail = format!("{d_url} answered 200 with invalid JSON: {err}");
                            continue;
                        }
                    }
                }
                if status.as_u16() == 400 {
                    // The shim validated and rejected our request; its message
                    // names the actual problem (missing criteria, bad type).
                    let msg = resp
                        .text()
                        .await
                        .ok()
                        .and_then(|t| {
                            serde_json::from_str::<serde_json::Value>(&t)
                                .ok()
                                .and_then(|v| v.get("error")?.as_str().map(str::to_string))
                                .or_else(|| {
                                    (!t.trim().is_empty())
                                        .then(|| t.chars().take(300).collect::<String>())
                                })
                        })
                        .unwrap_or_else(|| "bad request".to_string());
                    return Err(DecideError::InvalidRequest(msg));
                }
                // 404 (older shim without the endpoint), 503
                // (SYSTEMONE_DISABLE), 5xx: try the next URL.
                last_detail = format!("{d_url} answered HTTP {status}");
            }
            Err(err) => {
                last_detail = format!("{d_url}: {err}");
            }
        }
    }
    Err(DecideError::ShimUnavailable {
        urls: tried,
        detail: last_detail,
    })
}

/// Probability entries out of a shim payload's `probabilities` or
/// `distribution` object, highest first. Never panics.
fn sorted_probs(obj: &serde_json::Map<String, serde_json::Value>) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = obj
        .iter()
        .filter_map(|(k, val)| {
            val.as_f64()
                .filter(|p| p.is_finite())
                .map(|p| (k.clone(), p))
        })
        .collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// Parse a `/v1/systemone/decide` payload. `None` when the shape is not a
/// decide response (older/errored shim) — the caller tries the next URL.
/// Never panics.
fn parse_decide(payload: &serde_json::Value, requested: DecideType) -> Option<DecideDecision> {
    let decision_type = payload
        .get("type")
        .and_then(|t| t.as_str())
        .and_then(|t| t.parse::<DecideType>().ok())
        .unwrap_or(requested);
    let dist_obj = payload
        .get("probabilities")
        .or_else(|| payload.get("distribution"))?
        .as_object()?;
    let distribution = sorted_probs(dist_obj);
    if distribution.is_empty() {
        return None;
    }
    // Winner: the explicit label/level key when present, else the top of the
    // distribution (the shim always sends the key; the fallback keeps us
    // robust against sidecar-shaped payloads that don't).
    let winner = payload
        .get("label")
        .or_else(|| payload.get("level"))
        .and_then(|w| w.as_str())
        .map(str::to_string)
        .or_else(|| distribution.first().map(|(k, _)| k.clone()))?;
    let confidence = payload
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .filter(|c| c.is_finite())
        .unwrap_or(0.0);
    let latency_ms = payload
        .get("latency_ms")
        .and_then(serde_json::Value::as_f64)
        .filter(|l| l.is_finite());
    let backend = payload
        .get("backend")
        .and_then(|b| b.as_str())
        .map(str::to_string);
    Some(DecideDecision {
        decision_type,
        winner,
        distribution,
        confidence,
        latency_ms,
        backend,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_type_from_str() {
        assert_eq!("choice".parse::<DecideType>().unwrap(), DecideType::Choice);
        assert_eq!("noul".parse::<DecideType>().unwrap(), DecideType::Noul);
        assert_eq!("score".parse::<DecideType>().unwrap(), DecideType::Score);
        assert_eq!("CHOICE".parse::<DecideType>().unwrap(), DecideType::Choice);
        assert!("rank".parse::<DecideType>().is_err());
        assert!("".parse::<DecideType>().is_err());
    }

    #[test]
    fn decide_url_derivation_mirrors_rank_plans() {
        assert_eq!(
            decide_url("http://127.0.0.1:8765/v1/systemone/route"),
            Some("http://127.0.0.1:8765/v1/systemone/decide".to_string())
        );
        assert_eq!(
            decide_url("http://127.0.0.1:8079/v1/systemone/route"),
            Some("http://127.0.0.1:8079/v1/systemone/decide".to_string())
        );
        // Never invent a path we don't recognize.
        assert_eq!(decide_url("http://127.0.0.1:8765/healthz"), None);
        assert_eq!(decide_url("http://127.0.0.1:8765/"), None);
    }

    #[test]
    fn parse_choice_payload() {
        let payload = serde_json::json!({
            "type": "choice",
            "label": "rewrite",
            "probabilities": {"keep": 0.21, "rewrite": 0.79},
            "confidence": 0.79,
            "latency_ms": 34.2,
            "backend": "fallback",
        });
        let d = parse_decide(&payload, DecideType::Choice).expect("parses");
        assert_eq!(d.decision_type, DecideType::Choice);
        assert_eq!(d.winner, "rewrite");
        assert_eq!(
            d.distribution,
            vec![("rewrite".to_string(), 0.79), ("keep".to_string(), 0.21)]
        );
        assert!((d.confidence - 0.79).abs() < 1e-9);
        assert_eq!(d.latency_ms, Some(34.2));
        assert_eq!(d.backend.as_deref(), Some("fallback"));
    }

    #[test]
    fn parse_score_payload_uses_distribution_key() {
        let payload = serde_json::json!({
            "type": "score",
            "level": "2",
            "distribution": {"0": 0.1, "1": 0.3, "2": 0.6},
            "confidence": 0.6,
            "latency_ms": 41.0,
            "backend": "jeff1",
        });
        let d = parse_decide(&payload, DecideType::Score).expect("parses");
        assert_eq!(d.decision_type, DecideType::Score);
        assert_eq!(d.winner, "2");
        assert_eq!(d.distribution.first().map(|(k, _)| k.as_str()), Some("2"));
        assert_eq!(d.backend.as_deref(), Some("jeff1"));
    }

    #[test]
    fn parse_noul_payload() {
        let payload = serde_json::json!({
            "type": "noul",
            "label": "yes",
            "probabilities": {"yes": 0.62, "no": 0.38},
            "confidence": 0.62,
            "latency_ms": 28.7,
            "backend": "fallback",
        });
        let d = parse_decide(&payload, DecideType::Noul).expect("parses");
        assert_eq!(d.winner, "yes");
        assert_eq!(d.distribution.len(), 2);
    }

    #[test]
    fn parse_winner_falls_back_to_top_of_distribution() {
        let payload = serde_json::json!({
            "type": "choice",
            "probabilities": {"a": 0.4, "b": 0.6},
            "confidence": 0.6,
        });
        let d = parse_decide(&payload, DecideType::Choice).expect("parses");
        assert_eq!(d.winner, "b");
        assert_eq!(d.latency_ms, None);
        assert_eq!(d.backend, None);
    }

    #[test]
    fn parse_rejects_non_decide_payloads() {
        assert!(parse_decide(&serde_json::json!({"ok": true}), DecideType::Choice).is_none());
        assert!(
            parse_decide(
                &serde_json::json!({"type": "choice", "probabilities": {}}),
                DecideType::Choice
            )
            .is_none()
        );
        // Non-finite probabilities are dropped.
        assert!(
            parse_decide(
                &serde_json::json!({"type": "choice", "probabilities": {"a": "high"}}),
                DecideType::Choice
            )
            .is_none()
        );
    }

    /// Spin a tiny local HTTP server as the "shim": the test mocks the HTTP
    /// layer (no real SystemOne needed) and asserts the request body carries
    /// the exact decide contract plus the parsed decision.
    #[tokio::test]
    async fn decide_posts_contract_to_shim_and_parses() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let seen_body: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_body_srv = seen_body.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            // Read headers to find Content-Length, then the body.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let header_end = loop {
                let n = sock.read(&mut tmp).await.expect("read");
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = find_subslice(&buf, b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
            let content_len: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < header_end + content_len {
                let n = sock.read(&mut tmp).await.expect("read");
                buf.extend_from_slice(&tmp[..n]);
            }
            *seen_body_srv.lock().unwrap() = buf[header_end..].to_vec();
            let reply = br#"HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 136
Connection: close

{"type":"choice","label":"rewrite","probabilities":{"keep":0.21,"rewrite":0.79},"confidence":0.79,"latency_ms":3.1,"backend":"fallback"}"#;
            sock.write_all(reply).await.expect("write");
        });

        let cfg = SystemOneConfig {
            urls: vec![format!("http://127.0.0.1:{port}/v1/systemone/route")],
            timeout: Duration::from_secs(5),
            ..SystemOneConfig::default()
        };
        let criteria = serde_json::json!({"keep": "leave it", "rewrite": "start over"});
        let d = decide(
            "some state",
            "should I rewrite?",
            criteria.clone(),
            DecideType::Choice,
            &cfg,
        )
        .await
        .expect("decide succeeds against the mock shim");

        assert_eq!(d.decision_type, DecideType::Choice);
        assert_eq!(d.winner, "rewrite");
        assert_eq!(
            d.distribution,
            vec![("rewrite".to_string(), 0.79), ("keep".to_string(), 0.21)]
        );
        assert!((d.confidence - 0.79).abs() < 1e-9);
        assert_eq!(d.backend.as_deref(), Some("fallback"));

        // The request body must carry the decide contract exactly.
        let raw = seen_body.lock().unwrap().clone();
        let sent: serde_json::Value = serde_json::from_slice(&raw).expect("request body is JSON");
        assert_eq!(sent["state"], serde_json::json!("some state"));
        assert_eq!(sent["instructions"], serde_json::json!("should I rewrite?"));
        assert_eq!(sent["criteria"], criteria);
        assert_eq!(sent["type"], serde_json::json!("choice"));
    }

    #[tokio::test]
    async fn decide_shim_down_returns_clear_error() {
        let cfg = SystemOneConfig {
            urls: vec!["http://127.0.0.1:1/v1/systemone/route".to_string()],
            // Nothing listens here: connection refused, fast.
            timeout: Duration::from_secs(2),
            ..SystemOneConfig::default()
        };
        let err = decide(
            "s",
            "q?",
            serde_json::json!({"a": "b"}),
            DecideType::Choice,
            &cfg,
        )
        .await
        .expect_err("shim down must error, not fail open");
        let msg = err.to_string();
        match err {
            DecideError::ShimUnavailable { urls, detail } => {
                assert_eq!(urls.len(), 1);
                assert!(urls[0].ends_with("/v1/systemone/decide"));
                assert!(!detail.is_empty());
                assert!(msg.contains("shim unavailable"), "clear message: {msg}");
            }
            other => panic!("expected ShimUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_disabled_short_circuits() {
        let cfg = SystemOneConfig {
            enabled: false,
            ..SystemOneConfig::default()
        };
        let err = decide(
            "s",
            "q?",
            serde_json::json!({"a": "b"}),
            DecideType::Choice,
            &cfg,
        )
        .await
        .expect_err("disabled routing must error");
        assert!(matches!(err, DecideError::Disabled));
    }

    #[tokio::test]
    async fn decide_empty_instructions_rejected_client_side() {
        let cfg = SystemOneConfig::default();
        let err = decide(
            "s",
            "   ",
            serde_json::json!({"a": "b"}),
            DecideType::Choice,
            &cfg,
        )
        .await
        .expect_err("empty instructions must error");
        assert!(matches!(err, DecideError::InvalidRequest(_)));
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
