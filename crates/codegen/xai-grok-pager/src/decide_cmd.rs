//! `grok-local decide`: ask the SystemOne decision engine a typed question.
//!
//! Backed by `POST /v1/systemone/decide` on the local SystemOne shim (the
//! shim proxies to the Jeff-1 sidecar when it is up, else answers with the
//! local GLiClass engine; confidence/temperature conventions are adapted from
//! Mapika/decider, Apache-2.0 — see `xai-grok-systemone/src/decide.rs`).
//! Reuses the existing SystemOne shim URL config (`urls` from
//! `~/.grok-local/config.toml` `[systemone]` or `GROK_LOCAL_SYSTEMONE_URLS`);
//! no new endpoint defaults are introduced here.
//!
//! Unlike agent-flow routing this command is NOT fail-open: a down shim, a
//! disabled router, or a rejected request surfaces as a clear error with a
//! non-zero exit.

use anyhow::{Context, Result, bail};
use std::io::Write;

use xai_grok_systemone::decide::{DecideDecision, DecideType, decide};
use xai_grok_systemone::{RouterStatus, SystemOneConfig, ensure_router};

/// Answer type flag for `grok-local decide`.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum DecideTypeArg {
    /// Pick one label from named criteria.
    #[default]
    Choice,
    /// Yes/no question answered with calibrated uncertainty.
    Noul,
    /// Pick a score level 0..n-1.
    Score,
}

impl From<DecideTypeArg> for DecideType {
    fn from(arg: DecideTypeArg) -> Self {
        match arg {
            DecideTypeArg::Choice => Self::Choice,
            DecideTypeArg::Noul => Self::Noul,
            DecideTypeArg::Score => Self::Score,
        }
    }
}

#[derive(Clone, Debug, clap::Args)]
#[command(
    after_help = "Ask SystemOne's decision engine a typed question and print the \
winning label with its probability distribution and confidence.\n\n\
Criteria come from repeatable --criterion flags (label=description) or a single \
--criteria-json string. For --type score, labels must be contiguous level indexes \
\"0\"..\"n-1\"; bare values without '=' are auto-indexed in order.\n\n\
Examples:\n  \
grok-local decide \"should I rewrite this module?\" \\\n    --criterion keep=\"leave the code as-is\" \\\n    --criterion rewrite=\"rewrite it cleanly\" \\\n    --state \"module is 800 lines, lightly tested\"\n  \
grok-local decide \"is this worth doing?\" --type noul --state \"the task description\"\n  \
grok-local decide \"how confident are we?\" --type score \\\n    --criterion bad --criterion okay --criterion great"
)]
pub struct DecideArgs {
    /// The question / instructions the decision engine should answer.
    pub question: String,
    /// Answer type: choice (pick a label), noul (yes/no with uncertainty),
    /// score (pick a level 0..n-1).
    #[arg(long = "type", value_enum, default_value = "choice")]
    pub decision_type: DecideTypeArg,
    /// State / context the decision is about (sent as the request's `state`).
    #[arg(long)]
    pub state: Option<String>,
    /// One criterion as `label=description` (repeatable). For `--type score`,
    /// labels must be level indexes "0".."n-1"; a bare value without '='
    /// is auto-indexed in flag order.
    #[arg(long = "criterion")]
    pub criteria: Vec<String>,
    /// Criteria as a JSON string: object for choice, list or "0".."n-1"
    /// object for score, {"yes","no"} descriptions (or null) for noul.
    #[arg(long = "criteria-json", conflicts_with = "criteria")]
    pub criteria_json: Option<String>,
    /// Emit machine-readable JSON output.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: DecideArgs) -> Result<()> {
    let cfg = SystemOneConfig::load();
    if !cfg.routing_active() {
        bail!("{}", xai_grok_systemone::decide::DecideError::Disabled);
    }
    let qtype = DecideType::from(args.decision_type);
    let criteria = build_criteria(&args)?;

    // Probe, and start the shim when it is down and auto-start is allowed
    // (the same behavior as agent-flow routing — "just works" out of the
    // box). If there is still no router, fail loudly: this is an explicit
    // user command, not a background heuristic.
    match ensure_router(&cfg).await {
        RouterStatus::AlreadyRunning | RouterStatus::Started => {}
        RouterStatus::Unavailable => {
            bail!(
                "SystemOne shim unavailable: nothing is listening on 127.0.0.1:{} \
                 and it could not be started automatically. Start it with \
                 `python3.11 -m systemone.shim --port {0}` (cwd ~/systemone-release), \
                 or check ~/.grok-local/systemone-shim.log",
                cfg.shim_port
            );
        }
    }

    let state = args.state.as_deref().unwrap_or("");
    let decision = decide(state, &args.question, criteria, qtype, &cfg)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut out = std::io::stdout().lock();
    let written = if args.json {
        let rendered = serde_json::to_string_pretty(&decision)?;
        writeln!(out, "{rendered}")
    } else {
        print_human(&decision, &mut out)
    };
    Ok(crate::util::ignore_broken_pipe(written)?)
}

fn print_human(d: &DecideDecision, out: &mut impl Write) -> std::io::Result<()> {
    let mut meta = format!("type: {}", d.decision_type);
    if let Some(backend) = &d.backend {
        meta.push_str(&format!(", backend: {backend}"));
    }
    if let Some(ms) = d.latency_ms {
        meta.push_str(&format!(", {ms:.1} ms"));
    }
    writeln!(out, "Decision: {}", d.winner)?;
    writeln!(out, "Confidence: {:.1}%  ({})", d.confidence * 100.0, meta)?;
    writeln!(out)?;
    writeln!(out, "Distribution:")?;
    for (label, p) in &d.distribution {
        writeln!(out, "  {label:<24} {:>6.1}%", p * 100.0)?;
    }
    Ok(())
}

/// Build the decide `criteria` payload from `--criterion` / `--criteria-json`.
///
/// - choice: `{label: description}` (descriptions may be null).
/// - noul: criteria optional; `{yes, no}` descriptions when given.
/// - score: `{label: description}` with labels contiguous `"0".."n-1"`;
///   bare values (no `=`) are auto-indexed in flag order.
fn build_criteria(args: &DecideArgs) -> Result<serde_json::Value> {
    if let Some(raw) = &args.criteria_json {
        let value: serde_json::Value =
            serde_json::from_str(raw).context("--criteria-json must be valid JSON")?;
        return validate_criteria_json(DecideType::from(args.decision_type), value);
    }
    let qtype = DecideType::from(args.decision_type);
    if args.criteria.is_empty() {
        // noul allows null criteria; choice/score are rejected downstream
        // with a clear InvalidRequest error.
        return Ok(serde_json::Value::Null);
    }
    let allow_bare = matches!(qtype, DecideType::Score);
    let mut auto_index: usize = 0;
    let mut entries: Vec<(String, Option<String>)> = Vec::new();
    for raw in &args.criteria {
        let (label, desc) = parse_criterion(raw, &mut auto_index, allow_bare)?;
        entries.push((label, desc));
    }
    if matches!(qtype, DecideType::Score) {
        validate_score_labels(&entries)?;
    }
    let mut map = serde_json::Map::new();
    for (label, desc) in entries {
        map.insert(
            label,
            desc.map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null),
        );
    }
    Ok(serde_json::Value::Object(map))
}

/// Split one `--criterion` value into (label, description). A bare value
/// without `=` gets the next auto index when `allow_bare` (score only).
fn parse_criterion(
    raw: &str,
    auto_index: &mut usize,
    allow_bare: bool,
) -> Result<(String, Option<String>)> {
    match raw.split_once('=') {
        Some((label, desc)) => {
            let label = label.trim();
            if label.is_empty() {
                bail!("criterion '{raw}' has an empty label; use label=description");
            }
            let desc = desc.trim();
            Ok((
                label.to_string(),
                (!desc.is_empty()).then(|| desc.to_string()),
            ))
        }
        None => {
            if !allow_bare {
                bail!("criterion '{raw}' must be label=description");
            }
            let label = auto_index.to_string();
            *auto_index += 1;
            let desc = raw.trim();
            Ok((label, (!desc.is_empty()).then(|| desc.to_string())))
        }
    }
}

/// Score criteria labels must form contiguous level indexes "0".."n-1".
fn validate_score_labels(entries: &[(String, Option<String>)]) -> Result<()> {
    let mut indexes: Vec<usize> = Vec::new();
    for (label, _) in entries {
        match label.parse::<usize>() {
            Ok(i) => indexes.push(i),
            Err(_) => {
                bail!("score criteria labels must be level indexes \"0\"..\"n-1\"; got '{label}'")
            }
        }
    }
    indexes.sort_unstable();
    for (expected, got) in indexes.iter().enumerate() {
        if expected != *got {
            bail!(
                "score criteria labels must be contiguous level indexes \"0\"..\"n-1\" \
                 (got labels {labels:?})",
                labels = entries.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>()
            );
        }
    }
    Ok(())
}

/// Light client-side validation of a `--criteria-json` payload.
fn validate_criteria_json(
    qtype: DecideType,
    value: serde_json::Value,
) -> Result<serde_json::Value> {
    match qtype {
        DecideType::Choice => {
            if !value.is_object() || value.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                bail!(
                    "--criteria-json for --type choice must be a non-empty object of label -> description"
                );
            }
        }
        DecideType::Score => {
            let ok = match &value {
                serde_json::Value::Array(a) => !a.is_empty(),
                serde_json::Value::Object(o) => !o.is_empty(),
                _ => false,
            };
            if !ok {
                bail!(
                    "--criteria-json for --type score must be a non-empty list of level \
                     descriptions or an object keyed \"0\"..\"n-1\""
                );
            }
        }
        DecideType::Noul => {
            if !(value.is_null() || value.is_object() || value.is_array()) {
                bail!(
                    "--criteria-json for --type noul must be null, a {{\"yes\",\"no\"}} object, \
                     or a 2-item [yes, no] list"
                );
            }
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with(
        qtype: DecideTypeArg,
        criteria: &[&str],
        criteria_json: Option<&str>,
    ) -> DecideArgs {
        DecideArgs {
            question: "q?".to_string(),
            decision_type: qtype,
            state: None,
            criteria: criteria.iter().map(|s| s.to_string()).collect(),
            criteria_json: criteria_json.map(str::to_string),
            json: false,
        }
    }

    #[test]
    fn choice_criteria_build_object() {
        let args = args_with(
            DecideTypeArg::Choice,
            &["keep=leave it", "rewrite=start over"],
            None,
        );
        let v = build_criteria(&args).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"keep": "leave it", "rewrite": "start over"})
        );
    }

    #[test]
    fn choice_criteria_empty_description_becomes_null() {
        let args = args_with(DecideTypeArg::Choice, &["a="], None);
        let v = build_criteria(&args).unwrap();
        assert_eq!(v, serde_json::json!({"a": null}));
    }

    #[test]
    fn choice_criteria_rejects_bare_values() {
        let args = args_with(DecideTypeArg::Choice, &["justalabel"], None);
        let err = build_criteria(&args).unwrap_err();
        assert!(err.to_string().contains("label=description"));
    }

    #[test]
    fn choice_criteria_rejects_empty_label() {
        let args = args_with(DecideTypeArg::Choice, &["=desc"], None);
        assert!(build_criteria(&args).is_err());
    }

    #[test]
    fn score_criteria_auto_index_bare_values() {
        let args = args_with(DecideTypeArg::Score, &["bad", "okay", "great"], None);
        let v = build_criteria(&args).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"0": "bad", "1": "okay", "2": "great"})
        );
    }

    #[test]
    fn score_criteria_explicit_indexes_ok() {
        let args = args_with(
            DecideTypeArg::Score,
            &["0=terrible", "1=meh", "2=solid"],
            None,
        );
        let v = build_criteria(&args).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"0": "terrible", "1": "meh", "2": "solid"})
        );
    }

    #[test]
    fn score_criteria_rejects_non_contiguous() {
        let args = args_with(DecideTypeArg::Score, &["0=a", "2=b"], None);
        let err = build_criteria(&args).unwrap_err();
        assert!(err.to_string().contains("contiguous"));
    }

    #[test]
    fn score_criteria_rejects_named_labels() {
        let args = args_with(DecideTypeArg::Score, &["low=a", "high=b"], None);
        assert!(build_criteria(&args).is_err());
    }

    #[test]
    fn noul_criteria_optional() {
        let args = args_with(DecideTypeArg::Noul, &[], None);
        assert_eq!(build_criteria(&args).unwrap(), serde_json::Value::Null);
    }

    #[test]
    fn criteria_json_passthrough_valid() {
        let args = args_with(DecideTypeArg::Score, &[], Some(r#"["bad","okay","great"]"#));
        let v = build_criteria(&args).unwrap();
        assert_eq!(v, serde_json::json!(["bad", "okay", "great"]));
    }

    #[test]
    fn criteria_json_rejects_garbage() {
        let args = args_with(DecideTypeArg::Choice, &[], Some("{not json"));
        let err = build_criteria(&args).unwrap_err();
        assert!(err.to_string().contains("--criteria-json"));
    }

    #[test]
    fn criteria_json_rejects_empty_choice_object() {
        let args = args_with(DecideTypeArg::Choice, &[], Some("{}"));
        assert!(build_criteria(&args).is_err());
    }

    #[test]
    fn type_arg_converts() {
        assert_eq!(DecideType::from(DecideTypeArg::Choice), DecideType::Choice);
        assert_eq!(DecideType::from(DecideTypeArg::Noul), DecideType::Noul);
        assert_eq!(DecideType::from(DecideTypeArg::Score), DecideType::Score);
    }
}
