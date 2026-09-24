//! Anchored compaction: archive-then-summarize with durable anchors.
//!
//! Ports ZCode 3.25.0's anchored compaction: before summarizing, the full
//! transcript is archived to `$GROK_HOME/agentflow/archives/` (rotated,
//! default keep 5), and anchors — decisions, file paths, errors, TODO
//! state — are extracted so the summary prompt can preserve them
//! explicitly. Boundary events (plan stage completed, tests passed,
//! subtask verified) trigger compaction early at a token-pressure
//! watermark (default 0.55) instead of waiting for the emergency
//! threshold.
//!
//! Everything fails open: if archive or anchor preparation fails, the
//! caller falls back to ordinary compaction.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use regex::Regex;

use crate::{EnvReader, is_env_disabled};

/// Kill switch / explicit gate env. Falsy spellings (`0/false/no/off`)
/// disable anchored compaction.
pub const ANCHORED_COMPACT_ENV: &str = "GROK_LOCAL_ANCHORED_COMPACT";

/// How many transcript archives to keep per session.
pub const ARCHIVE_KEEP_COUNT_DEFAULT: usize = 5;

/// Token-pressure watermark for boundary-triggered compaction (fraction of
/// the context window).
pub const BOUNDARY_COMPACT_WATERMARK_DEFAULT: f64 = 0.55;

/// Boundary event kinds that trigger early anchored compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryCompactEventKind {
    /// A plan-then-execute planner turn finished.
    PlanStageCompleted,
    /// A test command ran.
    TestsPassed,
    /// A TodoWrite batch marked work completed.
    SubtaskVerified,
}

impl BoundaryCompactEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BoundaryCompactEventKind::PlanStageCompleted => "plan_stage_completed",
            BoundaryCompactEventKind::TestsPassed => "tests_passed",
            BoundaryCompactEventKind::SubtaskVerified => "subtask_verified",
        }
    }
}

/// True when a falsy env spelling disables anchored compaction.
pub fn is_anchored_compaction_disabled(get_env: EnvReader<'_>) -> bool {
    is_env_disabled(get_env(ANCHORED_COMPACT_ENV).as_deref())
}

/// Archive keep count: `GROK_LOCAL_ANCHORED_COMPACT_KEEP`, default 5.
pub fn resolve_archive_keep_count(get_env: EnvReader<'_>) -> usize {
    get_env("GROK_LOCAL_ANCHORED_COMPACT_KEEP")
        .as_deref()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(ARCHIVE_KEEP_COUNT_DEFAULT)
}

/// Boundary watermark: `GROK_LOCAL_BOUNDARY_COMPACT_WATERMARK` in (0, 1),
/// default 0.55.
pub fn resolve_boundary_watermark(get_env: EnvReader<'_>) -> f64 {
    get_env("GROK_LOCAL_BOUNDARY_COMPACT_WATERMARK")
        .as_deref()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0 && *v < 1.0)
        .unwrap_or(BOUNDARY_COMPACT_WATERMARK_DEFAULT)
}

/// A boundary event triggers early compaction when token pressure
/// (used / window) is at or above the watermark.
pub fn evaluate_boundary_compact_decision(
    token_pressure: f64,
    has_boundary_event: bool,
    watermark: f64,
) -> bool {
    has_boundary_event && token_pressure.is_finite() && token_pressure >= watermark
}

// ---------------------------------------------------------------------------
// Anchor extraction
// ---------------------------------------------------------------------------

static DECISION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(decision|conclusion)\s*:.*$|\bdecided to\b.{0,120}|\bwe (?:will|should)\b.{0,120}")
        .expect("decision regex")
});
static ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*error\s*:.*$|\bfailed\b.{0,120}|\bexception\b.{0,120}|stack trace")
        .expect("error regex")
});
static TODO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*[-*]\s*\[[ x]\].*$|^\s*todo\s*:.*$|\btodo\b.{0,80}").expect("todo regex")
});
static ANCHOR_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s("'`])((?:[~.]?/)?[\w.~-]+(?:/[\w.~-]+)+\.\w+)"#)
        .expect("anchor path regex")
});
static ANCHOR_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\w.~-]+\.(?:ts|tsx|js|mjs|cjs|json|md|py|rs|go|java|rb|toml|yaml|yml|cs|swift|kt|css|html)")
        .expect("anchor file regex")
});
static TEST_COMMAND_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b((npm|pnpm|yarn|bun)\s+(test|run\s+test)|pytest|vitest|jest|go\s+test|cargo\s+test|make\s+test)\b")
        .expect("test command regex")
});
static FAILURE_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(failed|failure|error|exit code:?\s*[1-9])").expect("failure marker regex")
});
static COMPLETED_STATUS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""status"\s*:\s*"completed""#).expect("completed status regex"));
static COMMAND_ARG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"command"\s*:\s*"((?:[^"\\]|\\.)*)""#).expect("command arg regex")
});

/// Durable anchors extracted from a transcript.
#[derive(Debug, Clone, Default)]
pub struct TranscriptAnchors {
    /// Decisions made (verbatim lines, order-stable, deduplicated).
    pub decisions: Vec<String>,
    /// File paths mentioned (order-stable, deduplicated).
    pub file_paths: Vec<String>,
    /// Errors encountered (verbatim lines, order-stable, deduplicated).
    pub errors: Vec<String>,
    /// TODO state lines (order-stable, deduplicated).
    pub todos: Vec<String>,
}

impl TranscriptAnchors {
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
            && self.file_paths.is_empty()
            && self.errors.is_empty()
            && self.todos.is_empty()
    }
}

fn collect_matches(text: &str, regex: &Regex) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for m in regex.find_iter(text) {
        let line = m.as_str().trim().to_string();
        if !line.is_empty() && seen.insert(line.clone()) {
            out.push(line);
        }
    }
    out
}

/// Extracts anchors from transcript text (all messages concatenated).
pub fn extract_transcript_anchors(transcript_text: &str) -> TranscriptAnchors {
    let mut file_paths = Vec::new();
    let mut seen_paths = HashSet::new();
    for captures in ANCHOR_PATH_RE
        .captures_iter(transcript_text)
        .chain(ANCHOR_FILE_RE.captures_iter(transcript_text))
    {
        if let Some(m) = captures.get(1).or_else(|| captures.get(0)) {
            let path = m.as_str().trim_matches(|c| "(\"'`".contains(c)).to_string();
            if !path.is_empty() && seen_paths.insert(path.clone()) {
                file_paths.push(path);
            }
        }
    }
    TranscriptAnchors {
        decisions: collect_matches(transcript_text, &DECISION_RE),
        file_paths,
        errors: collect_matches(transcript_text, &ERROR_RE),
        todos: collect_matches(transcript_text, &TODO_RE),
    }
}

// ---------------------------------------------------------------------------
// Archiving
// ---------------------------------------------------------------------------

static ARCHIVE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Directory for transcript archives: `$GROK_HOME/agentflow/archives/`.
pub fn transcript_archive_dir(grok_home: &Path) -> PathBuf {
    grok_home.join("agentflow").join("archives")
}

/// Archives a transcript (full messages + anchors) as JSON. Returns the
/// archive path. The filename is `<safe-session-id>-<epoch-ms>-<n>.json`
/// so lexicographic order is chronological (rotation relies on it).
pub fn archive_transcript(
    grok_home: &Path,
    session_id: &str,
    messages: &[(String, String)],
    anchors: &TranscriptAnchors,
) -> std::io::Result<PathBuf> {
    let dir = transcript_archive_dir(grok_home);
    std::fs::create_dir_all(&dir)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let counter = ARCHIVE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{}-{:013}-{}.json",
        sanitize_session_id(session_id),
        now_ms,
        counter
    ));
    let payload = serde_json::json!({
        "archived_at_ms": now_ms,
        "session_id": session_id,
        "message_count": messages.len(),
        "anchors": {
            "decisions": anchors.decisions,
            "file_paths": anchors.file_paths,
            "errors": anchors.errors,
            "todos": anchors.todos,
        },
        "messages": messages.iter().map(|(role, text)| serde_json::json!({
            "role": role,
            "text": text,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
    )?;
    Ok(path)
}

/// Rotates archives for a session, keeping the newest `keep` files.
/// Returns the number of archives removed. Never fails the caller — I/O
/// errors yield 0.
pub fn rotate_transcript_archives(grok_home: &Path, session_id: &str, keep: usize) -> usize {
    let dir = transcript_archive_dir(grok_home);
    let prefix = format!("{}-", sanitize_session_id(session_id));
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".json"))
            })
            .collect(),
        Err(_) => return 0,
    };
    if files.len() <= keep {
        return 0;
    }
    files.sort();
    let remove = files.len() - keep;
    let mut removed = 0;
    for path in files.into_iter().take(remove) {
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

// ---------------------------------------------------------------------------
// Boundary detection from tool calls
// ---------------------------------------------------------------------------

fn tool_command_argument(input_json: &str) -> Option<String> {
    COMMAND_ARG_RE
        .captures(input_json)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Detects a boundary event from a tool call. TodoWrite completions are
/// detected from the call arguments; test commands are detected from the
/// command argument (heuristic — see call-site docs). Returns `None` when
/// nothing fired.
pub fn detect_boundary_event_from_tool_call(
    tool_name: &str,
    input_json: &str,
) -> Option<BoundaryCompactEventKind> {
    let lowered = tool_name.to_ascii_lowercase();
    if lowered == "todo_write" || lowered == "todowrite" {
        if COMPLETED_STATUS_RE.is_match(input_json) {
            return Some(BoundaryCompactEventKind::SubtaskVerified);
        }
        return None;
    }
    if lowered == "bash" || lowered == "run_terminal_command" || lowered == "run_terminal_cmd" {
        if let Some(command) = tool_command_argument(input_json)
            && TEST_COMMAND_RE.is_match(&command)
            && !FAILURE_MARKER_RE.is_match(&command)
        {
            return Some(BoundaryCompactEventKind::TestsPassed);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Anchored summary prompt
// ---------------------------------------------------------------------------

/// Builds the anchored-summary instruction block. The caller prepends the
/// session's own compact instructions; this block forces anchor
/// preservation in the summary.
pub fn build_anchored_summary_prompt(anchors: &TranscriptAnchors) -> String {
    let mut out = String::from(
        "Before summarizing, note the transcript was archived in full — nothing is lost. \
         Your summary MUST preserve the following anchors verbatim (they are load-bearing \
         for the next turn):\n",
    );
    let mut section = |title: &str, items: &[String]| {
        out.push_str(&format!("\n## {title}\n"));
        if items.is_empty() {
            out.push_str("(none)\n");
        } else {
            for item in items.iter().take(50) {
                out.push_str(&format!("- {item}\n"));
            }
        }
    };
    section("Decisions", &anchors.decisions);
    section("Files touched / mentioned", &anchors.file_paths);
    section("Errors encountered", &anchors.errors);
    section("TODO state", &anchors.todos);
    out.push_str(
        "\nWrite the summary now: keep it under 400 words, lead with current goal and \
         next step, and reference the anchors above by their exact text.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_extraction_finds_all_four_kinds() {
        let text = "Decision: we will use the new router.\n\
                    Edited src/main.ts to add the route.\n\
                    Error: connection refused on :8765\n\
                    - [ ] wire up the shim\n\
                    Decided to ship it Friday.";
        let anchors = extract_transcript_anchors(text);
        assert!(!anchors.decisions.is_empty());
        assert!(anchors.file_paths.iter().any(|p| p.contains("src/main.ts")));
        assert!(!anchors.errors.is_empty());
        assert!(!anchors.todos.is_empty());
    }

    #[test]
    fn anchor_extraction_dedupes_and_ignores_empty() {
        let text = "Decision: ship it.\nDecision: ship it.\n";
        let anchors = extract_transcript_anchors(text);
        assert_eq!(anchors.decisions.len(), 1);
        assert!(extract_transcript_anchors("nothing notable here").is_empty());
    }

    #[test]
    fn boundary_decision_matrix() {
        assert!(evaluate_boundary_compact_decision(0.55, true, 0.55));
        assert!(evaluate_boundary_compact_decision(0.9, true, 0.55));
        assert!(!evaluate_boundary_compact_decision(0.54, true, 0.55));
        assert!(!evaluate_boundary_compact_decision(0.9, false, 0.55));
        assert!(!evaluate_boundary_compact_decision(f64::NAN, true, 0.55));
    }

    #[test]
    fn watermark_and_keep_count_resolution() {
        let get_env = |_: &str| None;
        assert!((resolve_boundary_watermark(&get_env) - 0.55).abs() < f64::EPSILON);
        assert_eq!(resolve_archive_keep_count(&get_env), 5);

        let get_env = |k: &str| {
            if k == "GROK_LOCAL_BOUNDARY_COMPACT_WATERMARK" {
                Some("0.7".to_string())
            } else if k == "GROK_LOCAL_ANCHORED_COMPACT_KEEP" {
                Some("3".to_string())
            } else {
                None
            }
        };
        assert!((resolve_boundary_watermark(&get_env) - 0.7).abs() < f64::EPSILON);
        assert_eq!(resolve_archive_keep_count(&get_env), 3);

        // Invalid values → defaults.
        let get_env = |k: &str| {
            if k == "GROK_LOCAL_BOUNDARY_COMPACT_WATERMARK" {
                Some("2.0".to_string())
            } else if k == "GROK_LOCAL_ANCHORED_COMPACT_KEEP" {
                Some("0".to_string())
            } else {
                None
            }
        };
        assert!((resolve_boundary_watermark(&get_env) - 0.55).abs() < f64::EPSILON);
        assert_eq!(resolve_archive_keep_count(&get_env), 5);
    }

    #[test]
    fn archive_roundtrip_and_rotation() {
        let home = tempfile::tempdir().unwrap();
        let anchors = extract_transcript_anchors("Decision: ship it.\nEdited src/a.ts.");
        let messages = vec![("user".to_string(), "hello".to_string())];
        let mut first: Option<PathBuf> = None;
        for _ in 0..7 {
            let path = archive_transcript(home.path(), "sess/1", &messages, &anchors).unwrap();
            if first.is_none() {
                first = Some(path);
            }
        }
        assert!(first.unwrap().to_string_lossy().contains("sess_1-"));
        // Rotation keeps the newest 5.
        let removed = rotate_transcript_archives(home.path(), "sess/1", 5);
        assert_eq!(removed, 2);
        let remaining: usize = std::fs::read_dir(transcript_archive_dir(home.path()))
            .unwrap()
            .count();
        assert_eq!(remaining, 5);
        // Other sessions are untouched.
        archive_transcript(home.path(), "other", &messages, &anchors).unwrap();
        assert_eq!(rotate_transcript_archives(home.path(), "other", 5), 0);
    }

    #[test]
    fn boundary_event_detection_from_tool_calls() {
        assert_eq!(
            detect_boundary_event_from_tool_call(
                "todo_write",
                r#"{"todos":[{"status":"completed"}]}"#
            ),
            Some(BoundaryCompactEventKind::SubtaskVerified)
        );
        assert_eq!(
            detect_boundary_event_from_tool_call(
                "todo_write",
                r#"{"todos":[{"status":"in_progress"}]}"#
            ),
            None
        );
        assert_eq!(
            detect_boundary_event_from_tool_call("bash", r#"{"command":"cargo test --all"}"#),
            Some(BoundaryCompactEventKind::TestsPassed)
        );
        assert_eq!(
            detect_boundary_event_from_tool_call("bash", r#"{"command":"ls /tmp"}"#),
            None
        );
        assert_eq!(
            detect_boundary_event_from_tool_call("read", r#"{"path":"a"}"#),
            None
        );
    }

    #[test]
    fn anchored_prompt_preserves_anchors() {
        let anchors = TranscriptAnchors {
            decisions: vec!["Decision: ship it.".to_string()],
            file_paths: vec!["src/a.ts".to_string()],
            errors: vec![],
            todos: vec![],
        };
        let prompt = build_anchored_summary_prompt(&anchors);
        assert!(prompt.contains("Decision: ship it."));
        assert!(prompt.contains("src/a.ts"));
        assert!(prompt.contains("(none)")); // empty errors section
    }

    #[test]
    fn anchored_compact_kill_switch_spelling() {
        assert!(is_anchored_compaction_disabled(&|k| (k
            == ANCHORED_COMPACT_ENV)
            .then(|| "0".to_string())));
        assert!(!is_anchored_compaction_disabled(&|_| None));
    }
}
