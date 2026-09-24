//! Plan-then-execute gating, prompts, and the plan artifact.
//!
//! Ports ZCode 3.25.0's route-driven plan→execute: heavy/risky/multi-file
//! work gets a short planner turn that writes a `PLAN.md`, then an
//! executor turn that follows it. The gate is fail-open and deliberately
//! strict so simple tasks never pay planning tax.
//!
//! **Pinned model stays first-class:** the planner and executor both reuse
//! the session's pinned model. No model IDs are hard-coded anywhere — the
//! planner follows explicit config, then the router advisory, then the
//! first usable locally-available model; the executor prefers the
//! smallest usable model (which is the pinned model itself when only one
//! model is available).
//!
//! If the planner turn produces no usable plan (or its budget is
//! exhausted), execution fails open to a direct turn rather than inventing
//! a plan.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::effort::{EffortBehaviorPolicy, is_plan_then_execute_eligible};
use crate::{EnvReader, is_env_disabled};

/// Kill switch / explicit gate env. Falsy spellings (`0/false/no/off`)
/// disable plan-then-execute.
pub const PLAN_EXECUTE_ENV: &str = "GROK_LOCAL_PLAN_EXECUTE";

/// Signal confidence floor for the risky-label and multi-file paths.
pub const PLAN_EXECUTE_SIGNAL_CONFIDENCE_FLOOR: f64 = 0.6;

/// Name of the plan artifact written by the planner turn.
pub const PLAN_ARTIFACT_FILENAME: &str = "PLAN.md";

/// Default planner budgets (small and configurable via env).
pub const DEFAULT_PLANNER_MAX_STEPS: u32 = 12;
pub const DEFAULT_PLANNER_MAX_TOOL_CALLS: u32 = 30;

/// Tools the planner turn may call: read-only investigation plus `write`
/// (needed to write the plan artifact).
pub static PLAN_READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "read_file",
    "hashline_read",
    "grep",
    "grep_files",
    "hashline_grep",
    "glob",
    "list_dir",
    "lsp",
    "web_search",
    "web_fetch",
    "memory_search",
    "memory_get",
    "write",
];

/// Why the gate decided what it decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanExecuteGateReason {
    /// Kill switch or explicit config disabled it.
    Disabled,
    /// Explicit config forced a direct turn.
    ConfigOff,
    /// Explicit config forced planning.
    ConfigOn,
    /// xhigh/ultra policy or heavy route tier.
    EffortOrHeavyTier,
    /// Risky label at signal confidence.
    RiskyLabels,
    /// Multi-file signals at signal confidence.
    MultiFileSignals,
    /// Nothing matched — direct turn.
    Direct,
}

/// Gate outcome.
#[derive(Debug, Clone, Copy)]
pub struct PlanExecuteGateDecision {
    pub plan: bool,
    pub reason: PlanExecuteGateReason,
}

/// Input for [`decide_plan_then_execute`].
pub struct PlanExecuteGateInput<'a> {
    /// Resolved effort behavior policy, if any.
    pub policy: Option<&'a EffortBehaviorPolicy>,
    /// Route tier name (`economy`/`balanced`/`heavy`), if any.
    pub route_tier: Option<&'a str>,
    /// Normalized task labels (route labels + keyword inference).
    pub task_labels: &'a [String],
    /// Raw task text.
    pub task_text: &'a str,
    /// Route confidence, if any.
    pub route_confidence: Option<f64>,
    /// Explicit config pin: `Some(true)` forces plan, `Some(false)`
    /// forces direct, `None` lets the gate decide.
    pub explicit_config: Option<bool>,
    pub get_env: EnvReader<'a>,
}

static RISKY_LABEL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(ambiguous|risky|multi[- ]?file|multifile|refactor|migration|design|planning|complex)$")
        .expect("risky label regex")
});
static MULTI_FILE_SIGNAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(multi[- ]file|across .*files|refactor|migration|codebase|multiple files)\b")
        .expect("multi-file signal regex")
});
static PATH_LIKE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s("'`])((?:[~.]?/)?[\w.~-]+(?:/[\w.~-]+)+\.\w+)"#)
        .expect("path-like regex")
});
static BARE_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\w.~-]+\.(?:ts|tsx|js|mjs|cjs|json|md|py|rs|go|java|rb|toml|yaml|yml|cs|swift|kt|css|html)")
        .expect("bare file regex")
});
static MODEL_SIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*b\b").expect("model size regex"));

/// True when a falsy env spelling disables plan-then-execute.
pub fn is_plan_then_execute_disabled(get_env: EnvReader<'_>) -> bool {
    is_env_disabled(get_env(PLAN_EXECUTE_ENV).as_deref())
}

fn has_risky_label(task_labels: &[String]) -> bool {
    task_labels
        .iter()
        .any(|label| RISKY_LABEL_RE.is_match(label.trim()))
}

/// Counts distinct file mentions in the task text (path-like and bare
/// filenames, case-insensitive).
pub fn count_file_mentions(task_text: &str) -> usize {
    let mut distinct: std::collections::HashSet<String> = std::collections::HashSet::new();
    for captures in PATH_LIKE_RE
        .captures_iter(task_text)
        .chain(BARE_FILE_RE.captures_iter(task_text))
    {
        if let Some(m) = captures.get(0).or_else(|| captures.get(1)) {
            distinct.insert(
                m.as_str()
                    .trim_matches(|c| "(\"'`".contains(c))
                    .to_ascii_lowercase(),
            );
        }
    }
    distinct.len()
}

fn has_multi_file_signals(task_text: &str) -> bool {
    if MULTI_FILE_SIGNAL_RE.is_match(task_text) {
        return true;
    }
    count_file_mentions(task_text) >= 3
}

/// The gate. Ordered: kill switch → explicit config → effort/heavy-tier
/// policy → risky labels (confidence ≥ 0.6) → multi-file signals
/// (confidence ≥ 0.6) → direct. Never panics; any uncertainty → direct.
pub fn decide_plan_then_execute(input: PlanExecuteGateInput<'_>) -> PlanExecuteGateDecision {
    let decide =
        |plan: bool, reason: PlanExecuteGateReason| PlanExecuteGateDecision { plan, reason };

    if is_plan_then_execute_disabled(input.get_env) {
        return decide(false, PlanExecuteGateReason::Disabled);
    }
    match input.explicit_config {
        Some(false) => return decide(false, PlanExecuteGateReason::ConfigOff),
        Some(true) => return decide(true, PlanExecuteGateReason::ConfigOn),
        None => {}
    }
    if is_plan_then_execute_eligible(input.policy, input.route_tier) {
        return decide(true, PlanExecuteGateReason::EffortOrHeavyTier);
    }
    let signal_ok = input
        .route_confidence
        .is_some_and(|c| c >= PLAN_EXECUTE_SIGNAL_CONFIDENCE_FLOOR);
    if signal_ok && has_risky_label(input.task_labels) {
        return decide(true, PlanExecuteGateReason::RiskyLabels);
    }
    if signal_ok && has_multi_file_signals(input.task_text) {
        return decide(true, PlanExecuteGateReason::MultiFileSignals);
    }
    decide(false, PlanExecuteGateReason::Direct)
}

/// Explicit model preferences for the two roles.
#[derive(Debug, Clone, Default)]
pub struct PlanExecuteModelPreferences {
    pub planner_model: Option<String>,
    pub executor_model: Option<String>,
    pub executor_prefs: Vec<String>,
}

/// Reads model preferences from the environment (no hard-coded IDs):
/// `GROK_LOCAL_PLAN_EXECUTE_PLANNER_MODEL`,
/// `GROK_LOCAL_PLAN_EXECUTE_EXECUTOR_MODEL`,
/// `GROK_LOCAL_PLAN_EXECUTE_EXECUTOR_PREFS` (comma-separated).
pub fn read_plan_execute_model_preferences(get_env: EnvReader<'_>) -> PlanExecuteModelPreferences {
    let one = |key: &str| {
        get_env(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let prefs = get_env("GROK_LOCAL_PLAN_EXECUTE_EXECUTOR_PREFS")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    PlanExecuteModelPreferences {
        planner_model: one("GROK_LOCAL_PLAN_EXECUTE_PLANNER_MODEL"),
        executor_model: one("GROK_LOCAL_PLAN_EXECUTE_EXECUTOR_MODEL"),
        executor_prefs: prefs,
    }
}

/// Embedding models are never usable for planning or execution.
pub fn is_usable_plan_execute_model(model_id: &str) -> bool {
    let trimmed = model_id.trim();
    !trimmed.is_empty() && !trimmed.to_ascii_lowercase().contains("embed")
}

/// Parses a `NNb` parameter-count hint from a model id (e.g. `9b`, `35b`).
pub fn parse_model_size_billions(model_id: &str) -> Option<f64> {
    MODEL_SIZE_RE
        .captures(model_id)?
        .get(1)?
        .as_str()
        .parse::<f64>()
        .ok()
}

/// Models chosen for the two roles. Both default to the pinned model when
/// it is the only usable one — nothing is ever unloaded or switched.
#[derive(Debug, Clone)]
pub struct PlanExecuteRoleModels {
    pub planner_model: String,
    pub executor_model: String,
}

/// Resolves planner/executor models: explicit config → router advisory →
/// smallest usable locally-available model. Errors when no usable model is
/// available (the caller then fails open to a direct turn).
pub fn resolve_plan_execute_models(
    available_model_ids: &[String],
    router_advisory: Option<&str>,
    preferences: &PlanExecuteModelPreferences,
) -> Result<PlanExecuteRoleModels, String> {
    let usable: Vec<&str> = available_model_ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| is_usable_plan_execute_model(id))
        .collect();
    if usable.is_empty() {
        return Err(
            "plan-then-execute needs at least one usable locally-available model".to_string(),
        );
    }

    let preferred_in = |id: &str| usable.iter().any(|u| *u == id);
    let planner_model = preferences
        .planner_model
        .as_deref()
        .filter(|id| preferred_in(id))
        .or_else(|| router_advisory.filter(|id| preferred_in(id)))
        .unwrap_or(usable[0])
        .to_string();

    let smallest = usable
        .iter()
        .min_by(|a, b| {
            let size_a = parse_model_size_billions(a).unwrap_or(f64::INFINITY);
            let size_b = parse_model_size_billions(b).unwrap_or(f64::INFINITY);
            size_a
                .partial_cmp(&size_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
        .unwrap_or(usable[0]);

    let executor_model = preferences
        .executor_model
        .as_deref()
        .filter(|id| preferred_in(id))
        .or_else(|| {
            preferences
                .executor_prefs
                .iter()
                .map(|s| s.trim())
                .find(|id| preferred_in(id))
        })
        .unwrap_or(smallest)
        .to_string();

    Ok(PlanExecuteRoleModels {
        planner_model,
        executor_model,
    })
}

/// Planner turn budgets: `GROK_LOCAL_PLAN_EXECUTE_PLANNER_MAX_STEPS` /
/// `_MAX_TOOL_CALLS`, defaulting to 12/30. Invalid values → defaults.
pub fn read_planner_budgets(get_env: EnvReader<'_>) -> (u32, u32) {
    let read = |key: &str, default: u32| {
        get_env(key)
            .as_deref()
            .and_then(crate::budgets::parse_budget_value)
            .unwrap_or(default)
    };
    (
        read(
            "GROK_LOCAL_PLAN_EXECUTE_PLANNER_MAX_STEPS",
            DEFAULT_PLANNER_MAX_STEPS,
        ),
        read(
            "GROK_LOCAL_PLAN_EXECUTE_PLANNER_MAX_TOOL_CALLS",
            DEFAULT_PLANNER_MAX_TOOL_CALLS,
        ),
    )
}

/// Planner system prompt. The planner investigates read-only and writes
/// the plan to `plan_path` with the `write` tool.
pub fn build_planner_prompt(task: &str, plan_path: &str) -> String {
    format!(
        "You are the PLANNER in a plan-then-execute workflow. Your job is to produce a \
         concrete, step-by-step implementation plan — NOT to implement anything.\n\n\
         TASK:\n{task}\n\n\
         RULES:\n\
         - Investigate read-only (read/grep/glob). Do NOT write or edit any source files, \
         and do NOT run anything that mutates state.\n\
         - Write the plan to {plan_path} with the write tool. Structure it as:\n\
           1. Goal (one line)\n\
           2. Steps (numbered, each naming the files to touch and the exact change)\n\
           3. Verification (how to check each step worked: commands or checks)\n\
           4. Risks / open questions\n\
         - Keep the plan tight: it will be executed verbatim by the next turn.\n\
         - If the task is already fully clear and tiny, say so in the plan and keep it to one step."
    )
}

/// Executor system prompt: follow the approved plan step by step.
pub fn build_executor_prompt(plan_markdown: &str) -> String {
    format!(
        "You are the EXECUTOR in a plan-then-execute workflow. An approved plan was produced \
         for this task — follow it step by step.\n\n\
         APPROVED PLAN:\n{plan_markdown}\n\n\
         RULES:\n\
         - Execute the plan's steps in order. Verify each step the way the plan says to.\n\
         - If a step cannot be completed as written, adapt minimally, explain the deviation, \
         and continue — do not invent a new plan from scratch.\n\
         - Report what you did and what verification passed when you finish."
    )
}

/// Directory for a session's plan artifacts: `$GROK_HOME/agentflow/plans/<safe-session-id>/`.
pub fn plan_artifact_dir(grok_home: &Path, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    grok_home.join("agentflow").join("plans").join(safe)
}

/// Writes the plan artifact. The caller writes via the tool layer; this is
/// the testable filesystem helper.
pub fn write_plan_artifact(dir: &Path, markdown: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(PLAN_ARTIFACT_FILENAME);
    std::fs::write(&path, markdown)?;
    Ok(path)
}

/// Reads the plan artifact, returning `None` when missing or blank.
pub fn read_plan_artifact(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(PLAN_ARTIFACT_FILENAME))
        .ok()
        .filter(|content| !content.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_input<'a>(
        labels: &'a [String],
        text: &'a str,
        tier: Option<&'a str>,
        confidence: Option<f64>,
        policy: Option<&'a EffortBehaviorPolicy>,
        explicit: Option<bool>,
        get_env: EnvReader<'a>,
    ) -> PlanExecuteGateInput<'a> {
        PlanExecuteGateInput {
            policy,
            route_tier: tier,
            task_labels: labels,
            task_text: text,
            route_confidence: confidence,
            explicit_config: explicit,
            get_env,
        }
    }

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gate_ordering_kill_switch_first() {
        let get_env = |k: &str| (k == PLAN_EXECUTE_ENV).then(|| "0".to_string());
        let xhigh = crate::effort::default_policy(crate::effort::EffortTier::XHigh);
        let d = decide_plan_then_execute(gate_input(
            &labels(&[]),
            "refactor the auth module",
            Some("heavy"),
            Some(0.99),
            Some(&xhigh),
            Some(true),
            &get_env,
        ));
        assert!(!d.plan);
        assert_eq!(d.reason, PlanExecuteGateReason::Disabled);
    }

    #[test]
    fn explicit_config_forces_either_way() {
        let get_env = |_: &str| None;
        let xhigh = crate::effort::default_policy(crate::effort::EffortTier::XHigh);
        let d = decide_plan_then_execute(gate_input(
            &labels(&[]),
            "x",
            Some("heavy"),
            Some(0.99),
            Some(&xhigh),
            Some(false),
            &get_env,
        ));
        assert_eq!(
            (d.plan, d.reason),
            (false, PlanExecuteGateReason::ConfigOff)
        );

        let low = crate::effort::default_policy(crate::effort::EffortTier::Low);
        let d = decide_plan_then_execute(gate_input(
            &labels(&[]),
            "x",
            Some("economy"),
            Some(0.1),
            Some(&low),
            Some(true),
            &get_env,
        ));
        assert_eq!((d.plan, d.reason), (true, PlanExecuteGateReason::ConfigOn));
    }

    #[test]
    fn heavy_tier_or_xhigh_policy_plans() {
        let get_env = |_: &str| None;
        let xhigh = crate::effort::default_policy(crate::effort::EffortTier::XHigh);
        let d = decide_plan_then_execute(gate_input(
            &labels(&[]),
            "do the thing",
            Some("economy"),
            Some(0.1),
            Some(&xhigh),
            None,
            &get_env,
        ));
        assert_eq!(d.reason, PlanExecuteGateReason::EffortOrHeavyTier);
        assert!(d.plan);

        let low = crate::effort::default_policy(crate::effort::EffortTier::Low);
        let d = decide_plan_then_execute(gate_input(
            &labels(&[]),
            "do the thing",
            Some("heavy"),
            Some(0.1),
            Some(&low),
            None,
            &get_env,
        ));
        assert!(d.plan);
        assert_eq!(d.reason, PlanExecuteGateReason::EffortOrHeavyTier);
    }

    #[test]
    fn risky_labels_need_confidence_floor() {
        let get_env = |_: &str| None;
        let low = crate::effort::default_policy(crate::effort::EffortTier::Low);
        let d = decide_plan_then_execute(gate_input(
            &labels(&["ambiguous"]),
            "do something",
            Some("balanced"),
            Some(0.6),
            Some(&low),
            None,
            &get_env,
        ));
        assert_eq!(d.reason, PlanExecuteGateReason::RiskyLabels);
        assert!(d.plan);

        let d = decide_plan_then_execute(gate_input(
            &labels(&["ambiguous"]),
            "do something",
            Some("balanced"),
            Some(0.59),
            Some(&low),
            None,
            &get_env,
        ));
        assert_eq!(d.reason, PlanExecuteGateReason::Direct);
        assert!(!d.plan);
    }

    #[test]
    fn multi_file_signals_need_confidence_and_three_files() {
        let get_env = |_: &str| None;
        let low = crate::effort::default_policy(crate::effort::EffortTier::Low);
        let text = "update src/a.ts, src/b.ts and src/c.ts to use the new API";
        let d = decide_plan_then_execute(gate_input(
            &labels(&[]),
            text,
            Some("balanced"),
            Some(0.8),
            Some(&low),
            None,
            &get_env,
        ));
        assert_eq!(d.reason, PlanExecuteGateReason::MultiFileSignals);
        assert!(d.plan);

        // Only two files → direct.
        let d = decide_plan_then_execute(gate_input(
            &labels(&[]),
            "update src/a.ts and src/b.ts",
            Some("balanced"),
            Some(0.8),
            Some(&low),
            None,
            &get_env,
        ));
        assert_eq!(d.reason, PlanExecuteGateReason::Direct);
    }

    #[test]
    fn model_resolution_prefers_smallest_and_never_embeds() {
        let prefs = PlanExecuteModelPreferences::default();
        let models = vec![
            "ornith-1.5-35b-a3b".to_string(),
            "text-embedding-3-small".to_string(),
        ];
        let resolved = resolve_plan_execute_models(&models, None, &prefs).unwrap();
        // Planner: first usable. Executor: smallest usable.
        assert_eq!(resolved.planner_model, "ornith-1.5-35b-a3b");
        assert_eq!(resolved.executor_model, "ornith-1.5-35b-a3b");
    }

    #[test]
    fn model_resolution_prefers_smaller_model_for_executor() {
        let prefs = PlanExecuteModelPreferences::default();
        let models = vec![
            "ornith-1.5-35b-a3b".to_string(),
            "ornith-1.5-9b".to_string(),
        ];
        let resolved = resolve_plan_execute_models(&models, None, &prefs).unwrap();
        assert_eq!(resolved.executor_model, "ornith-1.5-9b");
    }

    #[test]
    fn model_resolution_errors_when_nothing_usable() {
        let prefs = PlanExecuteModelPreferences::default();
        let err =
            resolve_plan_execute_models(&["text-embedding-3-small".to_string()], None, &prefs)
                .unwrap_err();
        assert!(err.contains("usable"));
    }

    #[test]
    fn planner_budgets_default_and_env_override() {
        let get_env = |_: &str| None;
        assert_eq!(read_planner_budgets(&get_env), (12, 30));
        let get_env =
            |k: &str| (k == "GROK_LOCAL_PLAN_EXECUTE_PLANNER_MAX_STEPS").then(|| "5".to_string());
        assert_eq!(read_planner_budgets(&get_env).0, 5);
    }

    #[test]
    fn plan_artifact_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let plan_dir = plan_artifact_dir(dir.path(), "sess:1/2");
        // Unsafe chars are sanitized.
        assert!(!plan_dir.to_string_lossy().contains(':'));
        write_plan_artifact(&plan_dir, "# Plan\n\n1. Do it.").unwrap();
        assert_eq!(
            read_plan_artifact(&plan_dir).unwrap(),
            "# Plan\n\n1. Do it."
        );
        // Missing dir → None.
        assert!(read_plan_artifact(&dir.path().join("nope")).is_none());
    }

    #[test]
    fn prompts_embed_task_plan_and_path() {
        let prompt = build_planner_prompt("migrate the DB", "/tmp/x/PLAN.md");
        assert!(prompt.contains("migrate the DB") && prompt.contains("/tmp/x/PLAN.md"));
        let exec = build_executor_prompt("## Plan\n1. step");
        assert!(exec.contains("## Plan"));
    }
}
