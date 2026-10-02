//! `grok-local decide`: ask the SystemOne decision layer a typed question.
//!
//! Backed by the decision endpoints on the local SystemOne shim:
//! `POST /v1/systemone/decide` (the shim answers with its decider backend —
//! Mapika/decider-4b v2.1, `backend: "decider"` — and falls back to the local
//! GLiClass engine, `backend: "fallback"`; historical shims may report
//! `"jeff1"`, which is parsed but never presented as the backend name — see
//! `xai-grok-systemone/src/decide.rs`), `POST /v1/systemone/permute` for
//! `--verify`, and `POST /v1/systemone/batch` for `--batch`.
//! Reuses the existing SystemOne shim URL config (`urls` from
//! `~/.grok-local/config.toml` `[systemone]` or `GROK_LOCAL_SYSTEMONE_URLS`);
//! no new endpoint defaults are introduced here.
//!
//! Unlike agent-flow routing this command is NOT fail-open: a down shim, a
//! disabled router, or a rejected request surfaces as a clear error with a
//! non-zero exit.

use anyhow::{Context, Result, bail};
use std::io::Write;

use xai_grok_systemone::decide::{
    BatchResults, DecideDecision, DecideType, PermuteVerdict, batch, decide, permute,
};
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
    after_help = "Ask SystemOne's decision layer a typed question and print the \
winning label with its probability distribution and confidence.\n\n\
Criteria come from repeatable --criterion flags (label=description) or a single \
--criteria-json string. For --type score, labels must be contiguous level indexes \
\"0\"..\"n-1\"; bare values without '=' are auto-indexed in order.\n\n\
--verify re-runs a choice under 8 option orders and reports whether the winner \
is stable: the Decide -> verify -> Act gate for high-stakes choices (choice only).\n\n\
--batch judges many questions in one call: the file holds a JSON array of \
/v1/systemone bodies ({state, questions, ...}, 1..32); per-item failures are \
reported per item, never as a failed command.\n\n\
Examples:\n  \
grok-local decide \"should I rewrite this module?\" \\\n    --criterion keep=\"leave the code as-is\" \\\n    --criterion rewrite=\"rewrite it cleanly\" \\\n    --state \"module is 800 lines, lightly tested\"\n  \
grok-local decide \"is this worth doing?\" --type noul --state \"the task description\"\n  \
grok-local decide \"how confident are we?\" --type score \\\n    --criterion bad --criterion okay --criterion great\n  \
grok-local decide \"ship it?\" --criterion yes --criterion no --verify\n  \
grok-local decide --batch questions.json --json"
)]
pub struct DecideArgs {
    /// The question / instructions the decision layer should answer.
    /// Required unless `--batch` is given.
    #[arg(required_unless_present = "batch")]
    pub question: Option<String>,
    /// Answer type: choice (pick a label), noul (yes/no with uncertainty),
    /// score (pick a level 0..n-1).
    #[arg(long = "type", value_enum)]
    pub decision_type: Option<DecideTypeArg>,
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
    /// Verify a choice by re-running it under 8 option orders (choice only).
    /// Prints a STABLE/UNSTABLE verdict; exit stays 0 either way.
    #[arg(long)]
    pub verify: bool,
    /// Judge a JSON array of /v1/systemone bodies (1..32) in one batch call.
    /// Conflicts with the single-question flags.
    #[arg(long)]
    pub batch: Option<String>,
    /// Emit machine-readable JSON output.
    #[arg(long)]
    pub json: bool,
}

/// Which decision flow the flags select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecideMode {
    /// One typed question.
    Single(DecideType),
    /// Many `/v1/systemone` bodies from a file.
    Batch,
}

/// Validate the flag combination before any I/O: batch mode takes no
/// single-question flags, `--verify` needs a choice, and a question is
/// required outside batch mode. Pure, so tests cover it without a shim.
fn resolve_mode(args: &DecideArgs) -> Result<DecideMode> {
    if args.batch.is_some() {
        if args.question.is_some() {
            bail!(
                "--batch takes no QUESTION: each item in the file carries its own state and questions"
            );
        }
        if args.decision_type.is_some() {
            bail!("--batch takes no --type: each item in the file carries its own question types");
        }
        if !args.criteria.is_empty() || args.criteria_json.is_some() {
            bail!(
                "--batch takes no --criterion/--criteria-json: each item in the file carries its own criteria"
            );
        }
        if args.state.is_some() {
            bail!("--batch takes no --state: each item in the file carries its own state");
        }
        if args.verify {
            bail!(
                "--verify needs a single --type choice question and cannot be combined with --batch"
            );
        }
        return Ok(DecideMode::Batch);
    }
    let qtype = DecideType::from(args.decision_type.unwrap_or_default());
    if args.verify && !matches!(qtype, DecideType::Choice) {
        bail!("--verify needs --type choice: only choices have option orders to permute");
    }
    if args
        .question
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        bail!("a QUESTION is required unless --batch is given");
    }
    Ok(DecideMode::Single(qtype))
}

pub async fn run(args: DecideArgs) -> Result<()> {
    let mode = resolve_mode(&args)?;
    let cfg = SystemOneConfig::load();
    if !cfg.routing_active() {
        bail!("{}", xai_grok_systemone::decide::DecideError::Disabled);
    }
    match mode {
        DecideMode::Batch => run_batch(&args, &cfg).await,
        DecideMode::Single(qtype) => run_single(&args, &cfg, qtype).await,
    }
}

/// Probe, and start the shim when it is down and auto-start is allowed
/// (the same behavior as agent-flow routing — "just works" out of the
/// box). If there is still no shim, fail loudly: this is an explicit
/// user command, not a background heuristic.
async fn ensure_shim_or_bail(cfg: &SystemOneConfig) -> Result<()> {
    match ensure_router(cfg).await {
        RouterStatus::AlreadyRunning | RouterStatus::Started => Ok(()),
        RouterStatus::Unavailable => {
            // Report the actual configured endpoint: with a custom remote
            // shim URL this is not localhost, and a remote shim cannot be
            // auto-started from here.
            let endpoint = cfg
                .urls
                .first()
                .map(|u| u.strip_suffix("/route").unwrap_or(u).to_string())
                .unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.shim_port));
            let host = endpoint
                .split("://")
                .nth(1)
                .unwrap_or("")
                .split(['/', ':'])
                .next()
                .unwrap_or("");
            let loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";
            if loopback {
                bail!(
                    "SystemOne shim unavailable: nothing is listening at {endpoint} \
                     and it could not be started automatically. Start it with \
                     `python3.11 -m systemone.shim --port {0}` (cwd ~/systemone-release), \
                     or check ~/.grok-local/systemone-shim.log",
                    cfg.shim_port
                );
            }
            bail!(
                "SystemOne shim unavailable: nothing is answering at the configured \
                 remote endpoint {endpoint}, and a remote shim cannot be started \
                 automatically. Check that the shim is running on the remote host, \
                 or run `grok-local onboard` to pick a different shim URL."
            );
        }
    }
}

/// One typed question, plus `--verify` when requested.
async fn run_single(args: &DecideArgs, cfg: &SystemOneConfig, qtype: DecideType) -> Result<()> {
    let criteria = build_criteria(args)?;
    ensure_shim_or_bail(cfg).await?;

    // `resolve_mode` guarantees a non-empty question here.
    let question = args.question.as_deref().unwrap_or("");
    let state = args.state.as_deref().unwrap_or("");
    let verify_criteria = args.verify.then(|| criteria.clone());
    let decision = decide(state, question, criteria, qtype, cfg)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let verdict = match verify_criteria {
        Some(vc) => Some(
            permute(state, question, vc, 8, 0, cfg)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        ),
        None => None,
    };

    let mut out = std::io::stdout().lock();
    let written = if args.json {
        let rendered = match &verdict {
            Some(v) => serde_json::to_string_pretty(
                &serde_json::json!({"decision": decision, "verification": v}),
            )?,
            None => serde_json::to_string_pretty(&decision)?,
        };
        writeln!(out, "{rendered}")
    } else {
        let mut r = print_human(&decision, &mut out);
        if r.is_ok()
            && let Some(v) = &verdict
        {
            r = print_verification(v, &mut out);
        }
        r
    };
    Ok(crate::util::ignore_broken_pipe(written)?)
}

/// Judge a file of `/v1/systemone` bodies in one batch call.
async fn run_batch(args: &DecideArgs, cfg: &SystemOneConfig) -> Result<()> {
    // `resolve_mode` guarantees `--batch` is present here.
    let path = args.batch.as_deref().unwrap_or("");
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read batch file '{path}'"))?;
    let items = parse_batch_items(&text)?;
    ensure_shim_or_bail(cfg).await?;

    let results = batch(items, cfg)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut out = std::io::stdout().lock();
    let written = if args.json {
        let rendered = serde_json::to_string_pretty(&results)?;
        writeln!(out, "{rendered}")
    } else {
        print_batch_human(&results, &mut out)
    };
    Ok(crate::util::ignore_broken_pipe(written)?)
}

/// Parse a `--batch` file: a JSON array of 1..32 `/v1/systemone` bodies.
/// Item shapes are the shim's to validate (per-item `{status, error}`);
/// only the array envelope is checked here. Pure, tested without a shim.
fn parse_batch_items(text: &str) -> Result<Vec<serde_json::Value>> {
    let value: serde_json::Value = serde_json::from_str(text)
        .context("--batch file must hold a JSON array of /v1/systemone bodies")?;
    let items = value
        .as_array()
        .context("--batch file must hold a JSON array of /v1/systemone bodies")?;
    if items.is_empty() || items.len() > 32 {
        bail!("--batch file must hold 1..32 items (got {})", items.len());
    }
    Ok(items.clone())
}

fn print_human(d: &DecideDecision, out: &mut impl Write) -> std::io::Result<()> {
    let mut meta = format!("type: {}", d.decision_type);
    if let Some(backend) = &d.backend {
        // Historical shims reported "jeff1"; user-facing text presents it
        // under the current backend name ("decider").
        let display = if backend == "jeff1" {
            "decider"
        } else {
            backend
        };
        meta.push_str(&format!(", backend: {display}"));
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

/// Human-readable `--verify` verdict: one line when stable, plus per-run
/// detail when the winner flips (so the caller sees which orders disagree).
fn print_verification(v: &PermuteVerdict, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out)?;
    let agree = v
        .runs
        .iter()
        .filter(|r| v.runs.first().is_some_and(|first| r.choice == first.choice))
        .count();
    if v.stable {
        writeln!(
            out,
            "Verification: STABLE — {}/{} orders agree (max spread {:.1}%)",
            agree,
            v.runs.len(),
            v.max_spread * 100.0
        )?;
    } else {
        writeln!(
            out,
            "Verification: UNSTABLE — the winner flips across option orders \
             (max spread {:.1}%); do not act on this decision",
            v.max_spread * 100.0
        )?;
        for (i, run) in v.runs.iter().enumerate() {
            let top = run
                .distribution
                .first()
                .map(|(_, p)| p * 100.0)
                .unwrap_or(0.0);
            writeln!(
                out,
                "  order {} [{}] -> {} ({top:.0}%)",
                i + 1,
                run.order.join(", "),
                run.choice
            )?;
        }
    }
    Ok(())
}

/// Human-readable `--batch` outcome: one line per item. 200 items show their
/// judged `answers` compactly; failed items show the shim's own error.
fn print_batch_human(b: &BatchResults, out: &mut impl Write) -> std::io::Result<()> {
    let n = b.results.len();
    let mut head = format!("Batch: {n} item{} judged", if n == 1 { "" } else { "s" });
    if let Some(model) = &b.model {
        head.push_str(&format!(" (model: {model})"));
    }
    writeln!(out, "{head}")?;
    for (i, item) in b.results.iter().enumerate() {
        if item.status == 200 {
            writeln!(out, "item {i}: ok")?;
            if let Some(answers) = item.payload.get("answers") {
                let compact = serde_json::to_string(answers).unwrap_or_default();
                writeln!(out, "  answers: {compact}")?;
            }
        } else {
            match &item.error {
                Some(e) => writeln!(out, "item {i}: status {}: {e}", item.status)?,
                None => writeln!(out, "item {i}: status {}", item.status)?,
            }
        }
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
    let qtype = DecideType::from(args.decision_type.unwrap_or_default());
    if let Some(raw) = &args.criteria_json {
        let value: serde_json::Value =
            serde_json::from_str(raw).context("--criteria-json must be valid JSON")?;
        return validate_criteria_json(qtype, value);
    }
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
            question: Some("q?".to_string()),
            decision_type: Some(qtype),
            state: None,
            criteria: criteria.iter().map(|s| s.to_string()).collect(),
            criteria_json: criteria_json.map(str::to_string),
            verify: false,
            batch: None,
            json: false,
        }
    }

    fn batch_args() -> DecideArgs {
        DecideArgs {
            question: None,
            decision_type: None,
            state: None,
            criteria: Vec::new(),
            criteria_json: None,
            verify: false,
            batch: Some("items.json".to_string()),
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

    #[test]
    fn mode_single_defaults_to_choice() {
        let mut args = args_with(DecideTypeArg::Choice, &[], None);
        args.decision_type = None;
        assert_eq!(
            resolve_mode(&args).unwrap(),
            DecideMode::Single(DecideType::Choice)
        );
    }

    #[test]
    fn mode_verify_needs_choice() {
        let mut args = args_with(DecideTypeArg::Noul, &[], None);
        args.verify = true;
        let err = resolve_mode(&args).unwrap_err();
        assert!(err.to_string().contains("--verify needs --type choice"));
    }

    #[test]
    fn mode_verify_accepts_choice() {
        let mut args = args_with(DecideTypeArg::Choice, &["a=x", "b=y"], None);
        args.verify = true;
        assert_eq!(
            resolve_mode(&args).unwrap(),
            DecideMode::Single(DecideType::Choice)
        );
    }

    #[test]
    fn mode_single_needs_question() {
        let mut args = args_with(DecideTypeArg::Choice, &["a=x", "b=y"], None);
        args.question = None;
        let err = resolve_mode(&args).unwrap_err();
        assert!(err.to_string().contains("QUESTION is required"));
    }

    #[test]
    fn mode_batch_ok() {
        assert_eq!(resolve_mode(&batch_args()).unwrap(), DecideMode::Batch);
    }

    #[test]
    fn mode_batch_rejects_single_question_flags() {
        let mut args = batch_args();
        args.question = Some("q?".to_string());
        assert!(
            resolve_mode(&args)
                .unwrap_err()
                .to_string()
                .contains("--batch takes no QUESTION")
        );
        let mut args = batch_args();
        args.decision_type = Some(DecideTypeArg::Score);
        assert!(
            resolve_mode(&args)
                .unwrap_err()
                .to_string()
                .contains("--batch takes no --type")
        );
        let mut args = batch_args();
        args.criteria = vec!["a=x".to_string()];
        assert!(
            resolve_mode(&args)
                .unwrap_err()
                .to_string()
                .contains("--batch takes no --criterion")
        );
        let mut args = batch_args();
        args.state = Some("s".to_string());
        assert!(
            resolve_mode(&args)
                .unwrap_err()
                .to_string()
                .contains("--batch takes no --state")
        );
        let mut args = batch_args();
        args.verify = true;
        assert!(
            resolve_mode(&args)
                .unwrap_err()
                .to_string()
                .contains("cannot be combined with --batch")
        );
    }

    #[test]
    fn batch_items_parse_array() {
        let items = parse_batch_items(r#"[{"state": "s1"}, {"state": "s2"}]"#).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["state"], serde_json::json!("s1"));
    }

    #[test]
    fn batch_items_reject_garbage_envelope() {
        assert!(parse_batch_items("{not json").is_err());
        assert!(parse_batch_items(r#"{"state": "s"}"#).is_err());
        assert!(parse_batch_items("[]").is_err());
        let too_many = format!("[{}]", vec!["{}"; 33].join(","));
        let err = parse_batch_items(&too_many).unwrap_err();
        assert!(err.to_string().contains("1..32"));
    }

    fn verdict_fixture(stable: bool) -> PermuteVerdict {
        use xai_grok_systemone::decide::PermuteRun;
        PermuteVerdict {
            stable,
            max_spread: if stable { 0.05 } else { 0.35 },
            spread: vec![("a".to_string(), 0.35), ("b".to_string(), 0.3)],
            runs: vec![
                PermuteRun {
                    order: vec!["a".to_string(), "b".to_string()],
                    choice: "a".to_string(),
                    distribution: vec![("a".to_string(), 0.8), ("b".to_string(), 0.2)],
                },
                PermuteRun {
                    order: vec!["b".to_string(), "a".to_string()],
                    choice: if stable {
                        "a".to_string()
                    } else {
                        "b".to_string()
                    },
                    distribution: if stable {
                        vec![("a".to_string(), 0.78), ("b".to_string(), 0.22)]
                    } else {
                        vec![("b".to_string(), 0.55), ("a".to_string(), 0.45)]
                    },
                },
            ],
            n_perm: 2,
            seed: 0,
            model: Some("mock".to_string()),
            latency_ms: Some(1.0),
        }
    }

    #[test]
    fn verification_prints_stable_line() {
        let mut buf = Vec::new();
        print_verification(&verdict_fixture(true), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("STABLE"), "{text}");
        assert!(text.contains("2/2"), "{text}");
        assert!(!text.contains("order 1"), "{text}");
    }

    #[test]
    fn verification_prints_unstable_with_runs() {
        let mut buf = Vec::new();
        print_verification(&verdict_fixture(false), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("UNSTABLE"), "{text}");
        assert!(text.contains("do not act"), "{text}");
        assert!(text.contains("order 1"), "{text}");
        assert!(text.contains("order 2"), "{text}");
    }

    #[test]
    fn batch_human_prints_mixed_items() {
        use xai_grok_systemone::decide::BatchItemResult;
        let results = BatchResults {
            results: vec![
                BatchItemResult {
                    status: 200,
                    error: None,
                    payload: serde_json::json!({"answers": {"q": {"choice": "a"}}}),
                },
                BatchItemResult {
                    status: 400,
                    error: Some("bad request: boom".to_string()),
                    payload: serde_json::json!({"status": 400}),
                },
            ],
            model: Some("mock".to_string()),
            n_items: 2,
        };
        let mut buf = Vec::new();
        print_batch_human(&results, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("2 items judged"), "{text}");
        assert!(text.contains("item 0: ok"), "{text}");
        assert!(text.contains("\"choice\":\"a\""), "{text}");
        assert!(
            text.contains("item 1: status 400: bad request: boom"),
            "{text}"
        );
    }
}
