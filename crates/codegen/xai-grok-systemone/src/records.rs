//! Append-only SystemOne decision records (JSONL).
//!
//! After every route/decide call the caller appends one record shaped for
//! SystemOne's calibration battery (`systemone/battery/fit_types.py`), whose
//! rows look like
//! `{"type": "choice"|"noul"|"score", "gold": int, "logits"|"probs": [...]}`.
//!
//! `gold` — the correct answer's index — is written **only when the outcome
//! is actually knowable**. At decision time it isn't: the router's tier pick
//! and the decide endpoint's winner are claims under test, not ground
//! truth. So route/decide records carry `type` + `probs` (+ `selected` /
//! `winner` metadata) and omit `gold`. Never fabricate it: a record that
//! pretends the router was right would poison the temperature fit.
//!
//! Storage is `$GROK_HOME/systemone/decision_records.jsonl` (the user config
//! dir), created on first write — no setup, no config, no new process.
//! Fail-open throughout: every I/O error is swallowed and an unwritable
//! home degrades silently to no logging.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::decide::DecideDecision;
use crate::route::RouteDecision;

/// Path of the append-only JSONL decision log.
fn records_path() -> PathBuf {
    xai_dirs::grok_home()
        .join("systemone")
        .join("decision_records.jsonl")
}

fn unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Best-effort append of one JSON line. Never panics, never returns an
/// error — logging must not break the call it records.
fn append_line_to(path: &Path, value: &serde_json::Value) {
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let mut line = serde_json::to_string(value)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(())
    })();
    if let Err(err) = result {
        tracing::debug!(path = %path.display(), %err, "decision record not written");
    }
}

/// Build the JSONL record for a route decision (pure: no I/O).
fn route_record(decision: &RouteDecision, task_kind: &str) -> serde_json::Value {
    let probs: Vec<f64> = decision
        .calibrated_probabilities
        .iter()
        .map(|(_, p)| *p)
        .collect();
    serde_json::json!({
        "type": "choice",
        "probs": probs,
        "ts": unix_ts(),
        "client": "grok-local",
        "task_kind": task_kind,
        "source": decision.source.as_str(),
        "tier": decision.tier.map(|t| t.as_str()),
        "selected": decision.tier.map(|t| t.as_str()),
        "confidence": decision.confidence,
        "margin": decision.margin,
        "uncertain": decision.uncertain,
        "calibrated": decision.calibrated,
    })
}

/// Record a route decision. `type` is always `"choice"` (the router picks
/// one tier); `probs` are the calibrated tier probabilities, highest first;
/// `gold` is omitted — the correct tier isn't knowable at route time.
pub fn record_route_decision(decision: &RouteDecision, task_kind: &str) {
    let value = route_record(decision, task_kind);
    append_line_to(&records_path(), &value);
}

/// Build the JSONL record for a typed decide decision (pure: no I/O).
fn decide_record(decision: &DecideDecision) -> serde_json::Value {
    let probs: Vec<f64> = decision.distribution.iter().map(|(_, p)| *p).collect();
    serde_json::json!({
        "type": decision.decision_type.as_str(),
        "probs": probs,
        "ts": unix_ts(),
        "client": "grok-local",
        "winner": decision.winner,
        "confidence": decision.confidence,
        "backend": decision.backend,
        "latency_ms": decision.latency_ms,
    })
}

/// Record a typed decide decision. `probs` follow the returned distribution
/// (highest first); `gold` is omitted — the decision engine's answer is the
/// claim under test, not ground truth.
pub fn record_decide_decision(decision: &DecideDecision) {
    let value = decide_record(decision);
    append_line_to(&records_path(), &value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemOneConfig;
    use crate::decide::DecideType;
    use crate::route::{Effort, Tier};

    fn route_fixture() -> RouteDecision {
        let mut d = RouteDecision::fail_open(&SystemOneConfig::default(), None);
        d.tier = Some(Tier::Balanced);
        d.effort = Effort::Medium;
        d.confidence = Some(0.72);
        d.margin = Some(0.18);
        d.uncertain = Some(false);
        d.calibrated = Some(true);
        d.calibrated_probabilities = vec![
            ("balanced".to_string(), 0.72),
            ("economy".to_string(), 0.2),
            ("heavy".to_string(), 0.08),
        ];
        d
    }

    #[test]
    fn route_record_shape_matches_fit_types_base() {
        let v = route_record(&route_fixture(), "turn");
        assert_eq!(v["type"], serde_json::json!("choice"));
        assert_eq!(
            v["probs"],
            serde_json::json!([0.72, 0.2, 0.08]),
            "probs highest-first, aligned with fit_types rows"
        );
        // gold is never fabricated: omitted when the outcome isn't knowable.
        assert!(v.get("gold").is_none());
        assert_eq!(v["selected"], serde_json::json!("balanced"));
        assert_eq!(v["task_kind"], serde_json::json!("turn"));
    }

    #[test]
    fn decide_record_shape_matches_fit_types_base() {
        let d = DecideDecision {
            decision_type: DecideType::Choice,
            winner: "rewrite".to_string(),
            distribution: vec![
                ("rewrite".to_string(), 0.79),
                ("keep".to_string(), 0.21),
            ],
            confidence: 0.79,
            latency_ms: Some(3.1),
            backend: Some("decider".to_string()),
        };
        let v = decide_record(&d);
        assert_eq!(v["type"], serde_json::json!("choice"));
        assert_eq!(v["probs"], serde_json::json!([0.79, 0.21]));
        assert!(v.get("gold").is_none());
        assert_eq!(v["winner"], serde_json::json!("rewrite"));
        assert_eq!(v["backend"], serde_json::json!("decider"));
    }

    #[test]
    fn append_is_append_only_and_survives_unwritable_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decision_records.jsonl");
        append_line_to(&path, &route_record(&route_fixture(), "turn"));
        append_line_to(&path, &route_record(&route_fixture(), "plan"));
        let text = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one JSON object per line, appended");
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(v["client"], serde_json::json!("grok-local"));
        }
        // Fail-open: an unwritable path degrades silently (no panic).
        append_line_to(Path::new("/proc/definitely-not-here/records.jsonl"), &route_record(&route_fixture(), "turn"));
    }
}
