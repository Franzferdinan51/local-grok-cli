//! Typed decisions via the SystemOne decision layer.
//!
//! SystemOne is a decision layer first: typed choice/noul/score questions
//! answered with calibrated probabilities (cost/quality routing is one facet
//! of [`crate::route_for_task`], which is one facet of deciding). The shim
//! answers decisions with its decider backend (Mapika/decider-4b v2.1,
//! `backend: "decider"`) and falls back to the local GLiClass engine
//! otherwise (`backend: "fallback"`). Historical shims may still report
//! `backend: "jeff1"` — that value parses and passes through unchanged, but
//! it is never presented as the backend name in user-facing text.
//!
//! Three explicit decision calls live here:
//!
//! - [`decide`] (`POST /v1/systemone/decide`): one typed question.
//!   Request: `{state, instructions, criteria, type}` — `type` is one of
//!   `choice | noul | score`; `criteria` is `{label: description}` for
//!   `choice`, `{yes, no}` descriptions (or null) for `noul`, and
//!   `"0".."n-1"` level descriptions for `score`. Response: `{type,
//!   label|level, probabilities|distribution, confidence, latency_ms,
//!   backend}`.
//! - [`permute`] (`POST /v1/systemone/permute`): verify a choice by re-running
//!   it under `n_perm` option orders; reports per-order answers, argmax
//!   stability, and the per-option probability spread. Decide first, verify,
//!   then act.
//! - [`batch`] (`POST /v1/systemone/batch`): judge up to 32 TypeSafe bodies
//!   (`{state, questions, ...}` each) in one call; per-item failures come back
//!   as `{status, error}` results instead of failing the batch.
//!
//! Unlike routing ([`crate::route_for_task`]) and plan ranking
//! ([`crate::rank_plans`]), this module is NOT fail-open: every call returns
//! [`DecideError`] on failure. It backs the explicit `grok-local decide`
//! command, where a down shim must surface as a clear error and a non-zero
//! exit — never as a silent default. Endpoint URLs are derived from the
//! existing configured route URLs (same convention as `rank_plans`); no new
//! endpoint defaults are introduced.

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
    /// Which backend answered: `"decider"` (current), `"fallback"` (GLiClass),
    /// or whatever the shim sent — historical `"jeff1"` values parse and
    /// pass through for compatibility with older shims.
    pub backend: Option<String>,
}

/// What went wrong when asking the shim for a decision.
#[derive(Debug, Clone)]
pub enum DecideError {
    /// SystemOne is disabled (master kill-switch `GROK_LOCAL_SYSTEMONE=0`,
    /// or no URLs configured).
    Disabled,
    /// No shim answered on any configured endpoint URL for `op`
    /// (`"decide"`, `"permute"`, or `"batch"`).
    ShimUnavailable {
        op: &'static str,
        urls: Vec<String>,
        detail: String,
    },
    /// The shim rejected the request (e.g. HTTP 400 with a validator message).
    InvalidRequest { op: &'static str, msg: String },
    /// A shim answered but its payload was not an `op` response.
    InvalidResponse { op: &'static str, msg: String },
}

impl fmt::Display for DecideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(
                f,
                "SystemOne is disabled (GROK_LOCAL_SYSTEMONE=0 or no URLs configured); \
                 enable it to use the decision commands"
            ),
            Self::ShimUnavailable { op, urls, detail } => write!(
                f,
                "SystemOne shim unavailable: no {op} endpoint answered (tried: {}). {detail}",
                urls.join(", ")
            ),
            Self::InvalidRequest { op, msg } => {
                write!(f, "shim rejected the {op} request: {msg}")
            }
            Self::InvalidResponse { op, msg } => {
                write!(f, "shim returned an unrecognized {op} payload: {msg}")
            }
        }
    }
}

impl std::error::Error for DecideError {}

/// Derive a `/v1/systemone/<endpoint>` URL from a configured `/route` URL.
/// Returns `None` when the URL is not a `/v1/systemone/route` endpoint (never
/// invent a path we don't recognize). Mirrors `rank_plans_url`.
fn shim_endpoint_url(route_url: &str, endpoint: &str) -> Option<String> {
    route_url
        .strip_suffix("/v1/systemone/route")
        .map(|base| format!("{base}/v1/systemone/{endpoint}"))
}

/// Derive a `/v1/systemone/decide` URL from a configured `/route` URL.
fn decide_url(route_url: &str) -> Option<String> {
    shim_endpoint_url(route_url, "decide")
}

/// Derive a `/v1/systemone/permute` URL from a configured `/route` URL.
fn permute_url(route_url: &str) -> Option<String> {
    shim_endpoint_url(route_url, "permute")
}

/// Derive a `/v1/systemone/batch` URL from a configured `/route` URL.
fn batch_url(route_url: &str) -> Option<String> {
    shim_endpoint_url(route_url, "batch")
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
        return Err(DecideError::InvalidRequest {
            op: "decide",
            msg: "instructions must be a non-empty string".to_string(),
        });
    }
    if matches!(criteria, serde_json::Value::Null)
        && matches!(qtype, DecideType::Choice | DecideType::Score)
    {
        return Err(DecideError::InvalidRequest {
            op: "decide",
            msg: format!(
                "decide type '{qtype}' requires criteria; pass --criterion label=description (repeatable) or --criteria-json"
            ),
        });
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
                op: "decide",
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
                            Some(decision) => {
                                // Append-only decision record for calibration
                                // feedback (fail-open: logging never breaks
                                // the decide call).
                                crate::records::record_decide_decision(&decision);
                                return Ok(decision);
                            }
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
                    return Err(DecideError::InvalidRequest { op: "decide", msg });
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
        op: "decide",
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

/// POST a JSON body to `/v1/systemone/<endpoint>` on every configured shim URL
/// in order and return the first 2xx JSON payload. Same contract as the
/// [`decide`] loop: HTTP 400 surfaces the shim's own validator message
/// immediately ([`DecideError::InvalidRequest`]); anything else tries the next
/// URL and the end of the list is [`DecideError::ShimUnavailable`]. NOT
/// fail-open. Never panics.
async fn post_shim_json(
    op: &'static str,
    endpoint_url: fn(&str) -> Option<String>,
    body: &serde_json::Value,
    cfg: &SystemOneConfig,
) -> Result<serde_json::Value, DecideError> {
    // Localhost-only router client: the grok TLS policy is for remote hosts.
    #[allow(clippy::disallowed_methods)]
    let client = match reqwest::Client::builder()
        .timeout(cfg.timeout + Duration::from_secs(1))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return Err(DecideError::ShimUnavailable {
                op,
                urls: cfg.urls.clone(),
                detail: format!("cannot build HTTP client: {err}"),
            });
        }
    };

    let mut tried: Vec<String> = Vec::new();
    let mut last_detail = format!("no {op}-capable URL configured");
    for url in &cfg.urls {
        let Some(e_url) = endpoint_url(url) else {
            continue;
        };
        tried.push(e_url.clone());
        match client.post(&e_url).json(body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.json::<serde_json::Value>().await {
                        Ok(payload) => return Ok(payload),
                        Err(err) => {
                            last_detail = format!("{e_url} answered 200 with invalid JSON: {err}");
                            continue;
                        }
                    }
                }
                if status.as_u16() == 400 {
                    // The shim validated and rejected our request; its message
                    // names the actual problem.
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
                    return Err(DecideError::InvalidRequest { op, msg });
                }
                // 404 (older shim without the endpoint), 503
                // (SYSTEMONE_DISABLE), 5xx: try the next URL.
                last_detail = format!("{e_url} answered HTTP {status}");
            }
            Err(err) => {
                last_detail = format!("{e_url}: {err}");
            }
        }
    }
    Err(DecideError::ShimUnavailable {
        op,
        urls: tried,
        detail: last_detail,
    })
}

/// One permutation-probe run: the option order tried and its outcome.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PermuteRun {
    /// Option labels in the order judged for this run.
    pub order: Vec<String>,
    /// Winning label for this order.
    pub choice: String,
    /// Every label with its probability, highest first.
    pub distribution: Vec<(String, f64)>,
}

/// Verdict of a permutation probe (`POST /v1/systemone/permute`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PermuteVerdict {
    /// True when every probed order picked the same winner.
    pub stable: bool,
    /// Largest per-option probability spread across orders (0..1).
    pub max_spread: f64,
    /// Per-option spread (max − min probability), highest first.
    pub spread: Vec<(String, f64)>,
    /// Per-order runs, first the given order then seeded shuffles.
    pub runs: Vec<PermuteRun>,
    /// Orders probed.
    pub n_perm: u32,
    /// Shuffle seed the shim used.
    pub seed: i64,
    /// Judge model name, when reported.
    pub model: Option<String>,
    /// Server-side latency in milliseconds, when reported.
    pub latency_ms: Option<f64>,
}

/// Parse a `/v1/systemone/permute` payload. `None` when the shape is not a
/// permute response (older/errored shim). Never panics.
fn parse_permute(payload: &serde_json::Value) -> Option<PermuteVerdict> {
    let runs_raw = payload.get("runs")?.as_array()?;
    if runs_raw.is_empty() {
        return None;
    }
    let mut runs = Vec::with_capacity(runs_raw.len());
    for run in runs_raw {
        let order: Vec<String> = run
            .get("order")?
            .as_array()?
            .iter()
            .filter_map(|o| o.as_str().map(str::to_string))
            .collect();
        let probs_obj = run.get("probabilities")?.as_object()?;
        let distribution = sorted_probs(probs_obj);
        if distribution.is_empty() {
            return None;
        }
        let choice = run
            .get("choice")
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .or_else(|| distribution.first().map(|(k, _)| k.clone()))?;
        runs.push(PermuteRun {
            order,
            choice,
            distribution,
        });
    }
    let stable = payload
        .get("argmax_stable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| {
            // Older shims predate the flag: derive it from the runs.
            let first = runs.first().map(|r| r.choice.as_str());
            runs.iter().all(|r| Some(r.choice.as_str()) == first)
        });
    let spread = payload
        .get("spread")
        .and_then(serde_json::Value::as_object)
        .map(sorted_probs)
        .unwrap_or_default();
    let max_spread = spread.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
    let n_perm = payload
        .get("n_perm")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(runs.len() as u32);
    let seed = payload
        .get("seed")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let latency_ms = payload
        .get("latency_ms")
        .and_then(serde_json::Value::as_f64)
        .filter(|l| l.is_finite());
    Some(PermuteVerdict {
        stable,
        max_spread,
        spread,
        runs,
        n_perm,
        seed,
        model,
        latency_ms,
    })
}

/// Verify a choice by re-running it under `n_perm` option orders:
/// `POST /v1/systemone/permute`.
///
/// `criteria` is the same `{label: description}` object a choice [`decide`]
/// takes (at least 2 options); `n_perm` must be 2..=32 (the shim's cap).
/// Returns argmax stability plus the per-option probability spread — the
/// Decide → verify → Act gate for high-stakes choices.
///
/// NOT fail-open: every failure mode returns a [`DecideError`]. Never panics.
pub async fn permute(
    state: &str,
    instructions: &str,
    criteria: serde_json::Value,
    n_perm: u32,
    seed: i64,
    cfg: &SystemOneConfig,
) -> Result<PermuteVerdict, DecideError> {
    if !cfg.routing_active() {
        return Err(DecideError::Disabled);
    }
    if instructions.trim().is_empty() {
        return Err(DecideError::InvalidRequest {
            op: "permute",
            msg: "instructions must be a non-empty string".to_string(),
        });
    }
    let n_options = criteria.as_object().map(|o| o.len()).unwrap_or(0);
    if n_options < 2 {
        return Err(DecideError::InvalidRequest {
            op: "permute",
            msg: "permute needs a choice criteria object with at least 2 options".to_string(),
        });
    }
    if !(2..=32).contains(&n_perm) {
        return Err(DecideError::InvalidRequest {
            op: "permute",
            msg: format!("n_perm must be in 2..32 (got {n_perm})"),
        });
    }

    let body = serde_json::json!({
        "state": state,
        "question": {
            "type": "choice",
            "criteria": criteria,
            "instructions": instructions,
        },
        "n_perm": n_perm,
        "seed": seed,
        "client": "grok-local",
    });
    let payload = post_shim_json("permute", permute_url, &body, cfg).await?;
    parse_permute(&payload).ok_or_else(|| DecideError::InvalidResponse {
        op: "permute",
        msg: "payload was not a permute response".to_string(),
    })
}

/// One judged item of a batch call.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BatchItemResult {
    /// Per-item HTTP-style status: 200 judged, 4xx/5xx per-item failure.
    pub status: u16,
    /// Failure message for non-200 items, when the shim sent one.
    pub error: Option<String>,
    /// Full per-item object exactly as the shim sent it (200 items carry
    /// `answers`, `latency_ms`, and possibly `cached`).
    pub payload: serde_json::Value,
}

/// Outcome of a bulk judge call (`POST /v1/systemone/batch`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BatchResults {
    /// Per-item results in request order.
    pub results: Vec<BatchItemResult>,
    /// Judge model name, when reported.
    pub model: Option<String>,
    /// Items the shim judged.
    pub n_items: usize,
}

/// Parse a `/v1/systemone/batch` payload. `None` when the shape is not a
/// batch response (older/errored shim). Never panics.
fn parse_batch(payload: &serde_json::Value) -> Option<BatchResults> {
    let items = payload.get("results")?.as_array()?;
    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let obj = item.as_object()?;
        let status = obj
            .get("status")
            .and_then(serde_json::Value::as_u64)
            .and_then(|s| u16::try_from(s).ok())
            .unwrap_or(0);
        let error = obj
            .get("error")
            .and_then(|e| e.as_str())
            .map(str::to_string);
        results.push(BatchItemResult {
            status,
            error,
            payload: serde_json::Value::Object(obj.clone()),
        });
    }
    if results.is_empty() {
        return None;
    }
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let n_items = payload
        .get("n_items")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(results.len());
    Some(BatchResults {
        results,
        model,
        n_items,
    })
}

/// Judge up to 32 TypeSafe bodies in one call: `POST /v1/systemone/batch`.
///
/// Each item is a `/v1/systemone`-shaped body (`{state, questions, ...}`).
/// Per-item failures come back as non-200 [`BatchItemResult`]s instead of
/// failing the batch — item shapes are the shim's to validate, so anything
/// JSON goes out and the shim reports what it could not judge.
///
/// NOT fail-open: transport-level failures return a [`DecideError`]. Never
/// panics.
pub async fn batch(
    items: Vec<serde_json::Value>,
    cfg: &SystemOneConfig,
) -> Result<BatchResults, DecideError> {
    if !cfg.routing_active() {
        return Err(DecideError::Disabled);
    }
    if items.is_empty() || items.len() > 32 {
        return Err(DecideError::InvalidRequest {
            op: "batch",
            msg: format!("batch needs 1..32 items (got {})", items.len()),
        });
    }
    let body = serde_json::json!({
        "items": items,
        "client": "grok-local",
    });
    let payload = post_shim_json("batch", batch_url, &body, cfg).await?;
    parse_batch(&payload).ok_or_else(|| DecideError::InvalidResponse {
        op: "batch",
        msg: "payload was not a batch response".to_string(),
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
            "backend": "decider",
        });
        let d = parse_decide(&payload, DecideType::Score).expect("parses");
        assert_eq!(d.decision_type, DecideType::Score);
        assert_eq!(d.winner, "2");
        assert_eq!(d.distribution.first().map(|(k, _)| k.as_str()), Some("2"));
        assert_eq!(d.backend.as_deref(), Some("decider"));
    }

    #[test]
    fn parse_passes_through_historical_jeff1_backend() {
        // Older shims reported `backend: "jeff1"`; the value must still parse
        // and pass through (wire compatibility), even though user-facing
        // text never presents it as the backend name.
        let payload = serde_json::json!({
            "type": "noul",
            "label": "yes",
            "probabilities": {"yes": 0.6, "no": 0.4},
            "confidence": 0.6,
            "backend": "jeff1",
        });
        let d = parse_decide(&payload, DecideType::Noul).expect("parses");
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
            DecideError::ShimUnavailable { op, urls, detail } => {
                assert_eq!(op, "decide");
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
        assert!(matches!(
            err,
            DecideError::InvalidRequest { op: "decide", .. }
        ));
    }

    #[test]
    fn endpoint_url_derivation_permute_batch() {
        assert_eq!(
            permute_url("http://127.0.0.1:8765/v1/systemone/route"),
            Some("http://127.0.0.1:8765/v1/systemone/permute".to_string())
        );
        assert_eq!(
            batch_url("http://127.0.0.1:8765/v1/systemone/route"),
            Some("http://127.0.0.1:8765/v1/systemone/batch".to_string())
        );
        // Never invent a path we don't recognize.
        assert_eq!(permute_url("http://127.0.0.1:8765/healthz"), None);
        assert_eq!(batch_url("http://127.0.0.1:8765/"), None);
    }

    #[test]
    fn parse_permute_stable_payload() {
        let payload = serde_json::json!({
            "runs": [
                {"order": ["keep", "rewrite"],
                 "probabilities": {"keep": 0.2, "rewrite": 0.8},
                 "choice": "rewrite"},
                {"order": ["rewrite", "keep"],
                 "probabilities": {"keep": 0.25, "rewrite": 0.75},
                 "choice": "rewrite"},
            ],
            "argmax_stable": true,
            "spread": {"keep": 0.05, "rewrite": 0.05},
            "n_perm": 2,
            "seed": 7,
            "model": "mock",
            "latency_ms": 1.5,
        });
        let v = parse_permute(&payload).expect("parses");
        assert!(v.stable);
        assert!((v.max_spread - 0.05).abs() < 1e-9);
        assert_eq!(v.runs.len(), 2);
        assert_eq!(v.runs[0].choice, "rewrite");
        assert_eq!(v.runs[0].order, vec!["keep", "rewrite"]);
        assert_eq!(v.n_perm, 2);
        assert_eq!(v.seed, 7);
        assert_eq!(v.model.as_deref(), Some("mock"));
        assert_eq!(v.latency_ms, Some(1.5));
    }

    #[test]
    fn parse_permute_unstable_without_flag_derives_from_runs() {
        // Older shims predate `argmax_stable`: the verdict still derives.
        let payload = serde_json::json!({
            "runs": [
                {"order": ["a", "b"],
                 "probabilities": {"a": 0.6, "b": 0.4},
                 "choice": "a"},
                {"order": ["b", "a"],
                 "probabilities": {"a": 0.45, "b": 0.55},
                 "choice": "b"},
            ],
            "spread": {"a": 0.15, "b": 0.15},
            "n_perm": 2,
            "seed": 0,
        });
        let v = parse_permute(&payload).expect("parses");
        assert!(!v.stable);
        assert!((v.max_spread - 0.15).abs() < 1e-9);
    }

    #[test]
    fn parse_permute_rejects_junk() {
        assert!(parse_permute(&serde_json::json!({"ok": true})).is_none());
        assert!(parse_permute(&serde_json::json!({"runs": []})).is_none());
        assert!(
            parse_permute(&serde_json::json!({"runs": [
                {"order": ["a", "b"], "probabilities": {}, "choice": "a"}
            ]}))
            .is_none()
        );
    }

    #[test]
    fn parse_batch_mixed_results() {
        let payload = serde_json::json!({
            "results": [
                {"status": 200, "answers": {"q": {"choice": "a"}},
                 "latency_ms": 1.0},
                {"status": 400, "error": "bad request: boom"},
            ],
            "model": "mock",
            "n_items": 2,
        });
        let b = parse_batch(&payload).expect("parses");
        assert_eq!(b.results.len(), 2);
        assert_eq!(b.n_items, 2);
        assert_eq!(b.model.as_deref(), Some("mock"));
        assert_eq!(b.results[0].status, 200);
        assert_eq!(b.results[0].error, None);
        assert_eq!(
            b.results[0].payload["answers"]["q"]["choice"],
            serde_json::json!("a")
        );
        assert_eq!(b.results[1].status, 400);
        assert_eq!(b.results[1].error.as_deref(), Some("bad request: boom"));
    }

    #[test]
    fn parse_batch_rejects_junk() {
        assert!(parse_batch(&serde_json::json!({"ok": true})).is_none());
        assert!(parse_batch(&serde_json::json!({"results": []})).is_none());
        assert!(parse_batch(&serde_json::json!({"results": ["not-an-object"]})).is_none());
    }

    #[tokio::test]
    async fn permute_validation_rejects_bad_input_client_side() {
        let cfg = SystemOneConfig::default();
        let good_criteria = serde_json::json!({"a": "x", "b": "y"});
        // Disabled short-circuits.
        let off = SystemOneConfig {
            enabled: false,
            ..SystemOneConfig::default()
        };
        assert!(matches!(
            permute("s", "q?", good_criteria.clone(), 8, 0, &off).await,
            Err(DecideError::Disabled)
        ));
        // Empty instructions.
        assert!(matches!(
            permute("s", "  ", good_criteria.clone(), 8, 0, &cfg).await,
            Err(DecideError::InvalidRequest { op: "permute", .. })
        ));
        // Fewer than 2 options.
        assert!(matches!(
            permute("s", "q?", serde_json::json!({"a": "x"}), 8, 0, &cfg).await,
            Err(DecideError::InvalidRequest { op: "permute", .. })
        ));
        // n_perm outside the shim's 2..32 cap.
        for bad in [0, 1, 33, 100] {
            assert!(
                matches!(
                    permute("s", "q?", good_criteria.clone(), bad, 0, &cfg).await,
                    Err(DecideError::InvalidRequest { op: "permute", .. })
                ),
                "n_perm={bad} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn batch_validation_rejects_bad_sizes_client_side() {
        let cfg = SystemOneConfig::default();
        assert!(matches!(
            batch(vec![], &cfg).await,
            Err(DecideError::InvalidRequest { op: "batch", .. })
        ));
        let too_many: Vec<serde_json::Value> = (0..33).map(|_| serde_json::json!({})).collect();
        assert!(matches!(
            batch(too_many, &cfg).await,
            Err(DecideError::InvalidRequest { op: "batch", .. })
        ));
    }

    /// Spawn a one-shot mock shim: captures the request line + body, replies
    /// with `reply_body` as `application/json`. Returns (port, seen).
    async fn spawn_mock_shim(
        reply_body: &'static str,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<(String, Vec<u8>)>>) {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let seen: Arc<Mutex<(String, Vec<u8>)>> = Arc::new(Mutex::new((String::new(), Vec::new())));
        let seen_srv = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let header_end = loop {
                let n = sock.read(&mut tmp).await.expect("read");
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = find_subslice(&buf, b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
            let request_line = head.lines().next().unwrap_or("").to_string();
            let content_len: usize = head
                .to_lowercase()
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < header_end + content_len {
                let n = sock.read(&mut tmp).await.expect("read");
                buf.extend_from_slice(&tmp[..n]);
            }
            *seen_srv.lock().unwrap() = (request_line, buf[header_end..].to_vec());
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply_body}",
                reply_body.len()
            );
            sock.write_all(reply.as_bytes()).await.expect("write");
        });
        (port, seen)
    }

    fn cfg_with_port(port: u16) -> SystemOneConfig {
        SystemOneConfig {
            urls: vec![format!("http://127.0.0.1:{port}/v1/systemone/route")],
            timeout: Duration::from_secs(5),
            ..SystemOneConfig::default()
        }
    }

    #[tokio::test]
    async fn permute_posts_contract_to_shim_and_parses() {
        let (port, seen) = spawn_mock_shim(
            r#"{"runs":[{"order":["keep","rewrite"],"probabilities":{"keep":0.2,"rewrite":0.8},"choice":"rewrite"},{"order":["rewrite","keep"],"probabilities":{"keep":0.25,"rewrite":0.75},"choice":"rewrite"}],"argmax_stable":true,"spread":{"keep":0.05,"rewrite":0.05},"n_perm":2,"seed":7,"model":"mock","latency_ms":1.5}"#,
        )
        .await;
        let cfg = cfg_with_port(port);
        let criteria = serde_json::json!({"keep": "leave it", "rewrite": "start over"});
        let v = permute(
            "some state",
            "should I rewrite?",
            criteria.clone(),
            2,
            7,
            &cfg,
        )
        .await
        .expect("permute succeeds against the mock shim");
        assert!(v.stable);
        assert_eq!(v.runs.len(), 2);
        assert_eq!(v.n_perm, 2);
        assert_eq!(v.seed, 7);

        let (request_line, raw) = seen.lock().unwrap().clone();
        assert!(
            request_line.contains("POST /v1/systemone/permute"),
            "hits the permute endpoint: {request_line}"
        );
        let sent: serde_json::Value = serde_json::from_slice(&raw).expect("request body is JSON");
        assert_eq!(sent["state"], serde_json::json!("some state"));
        assert_eq!(sent["question"]["type"], serde_json::json!("choice"));
        assert_eq!(sent["question"]["criteria"], criteria);
        assert_eq!(
            sent["question"]["instructions"],
            serde_json::json!("should I rewrite?")
        );
        assert_eq!(sent["n_perm"], serde_json::json!(2));
        assert_eq!(sent["seed"], serde_json::json!(7));
    }

    #[tokio::test]
    async fn batch_posts_contract_to_shim_and_parses() {
        let (port, seen) = spawn_mock_shim(
            r#"{"results":[{"status":200,"answers":{"q":{"choice":"a"}},"latency_ms":1.0},{"status":400,"error":"bad request: boom"}],"model":"mock","n_items":2}"#,
        )
        .await;
        let cfg = cfg_with_port(port);
        let items = vec![
            serde_json::json!({"state": "s1", "questions": []}),
            serde_json::json!({"state": "s2"}),
        ];
        let b = batch(items.clone(), &cfg)
            .await
            .expect("batch succeeds against the mock shim");
        assert_eq!(b.results.len(), 2);
        assert_eq!(b.results[0].status, 200);
        assert_eq!(b.results[1].status, 400);

        let (request_line, raw) = seen.lock().unwrap().clone();
        assert!(
            request_line.contains("POST /v1/systemone/batch"),
            "hits the batch endpoint: {request_line}"
        );
        let sent: serde_json::Value = serde_json::from_slice(&raw).expect("request body is JSON");
        assert_eq!(sent["items"], serde_json::Value::Array(items));
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
