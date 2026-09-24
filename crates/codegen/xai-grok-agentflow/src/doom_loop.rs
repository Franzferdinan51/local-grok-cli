//! Normalized near-duplicate tool-call fingerprinting and the doom-loop
//! escalation ladder.
//!
//! Ports ZCode 3.25.0's doom-loop escalation: repeated near-identical
//! tool calls escalate nudge → strategy-change → final, instead of running
//! to the turn limit. Normalization folds away the ways an agent
//! re-emits "the same" call with cosmetic differences (line endings,
//! path separators, key order, blank-line runs), while commands, offsets,
//! edit text, and array order stay meaningful.
//!
//! This complements the shell's existing exact-repeat stationarity guard
//! (which fires on many exact repeats); the fingerprint ladder fires much
//! earlier on near-duplicates and names untried tools.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::{EnvReader, is_env_disabled};

/// Kill switch / explicit gate env. Falsy spellings (`0/false/no/off`)
/// disable doom-loop escalation.
pub const DOOM_LOOP_ENV: &str = "GROK_LOCAL_DOOM_LOOP";

/// Streak length that fires each ladder stage.
pub const DOOM_LOOP_NUDGE_STREAK: u32 = 3;
pub const DOOM_LOOP_STRATEGY_STREAK: u32 = 4;
pub const DOOM_LOOP_FINAL_STREAK: u32 = 5;

/// Maximum untried-tool names to suggest in the strategy-change message.
pub const DOOM_LOOP_UNTRIED_LIMIT_DEFAULT: usize = 3;

/// Tool-argument keys inspected, in order, for the call "target" (used to
/// reset the streak when the agent moves to a different target).
pub const DOOM_LOOP_TARGET_KEYS: &[&str] = &[
    "path", "filePath", "file", "filename", "pattern", "command", "code", "url", "query", "text",
];

static URI_SCHEME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://").expect("uri scheme regex"));

/// One tool call as seen by the ladder.
#[derive(Debug, Clone)]
pub struct DoomLoopCall {
    pub id: String,
    pub name: String,
    pub input_json: String,
}

/// Ladder stage fired by one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoomLoopTransitionKind {
    None,
    Nudge,
    StrategyChange,
    Final,
}

/// Mutable per-turn ladder state. Reset at the start of each turn.
#[derive(Debug, Clone, Default)]
pub struct DoomLoopTurnState {
    pub fingerprint: Option<String>,
    pub target: Option<String>,
    pub used_tools: Vec<String>,
    /// 0 = none, 1 = nudged, 2 = strategy-changed, 3 = final.
    pub stage: u8,
    pub streak: u32,
}

/// Result of advancing the ladder for one call.
#[derive(Debug, Clone)]
pub struct DoomLoopTransition {
    pub kind: DoomLoopTransitionKind,
    pub streak: u32,
    pub tool_name: Option<String>,
}

/// A fired ladder stage, ready to be messaged.
#[derive(Debug, Clone)]
pub struct DoomLoopObservation {
    pub kind: DoomLoopTransitionKind,
    pub streak: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Untried tools from the active pack (strategy-change only).
    pub untried_tools: Vec<String>,
}

/// True when a falsy env spelling disables doom-loop escalation.
pub fn is_doom_loop_disabled(get_env: EnvReader<'_>) -> bool {
    is_env_disabled(get_env(DOOM_LOOP_ENV).as_deref())
}

/// Normalizes a string argument value: CRLF → LF, trailing whitespace per
/// line, 3+ blank lines → 2, backslashes → forward slashes unless the
/// string is a URI, lexical `.`/`..` collapse for path-like strings.
pub fn normalize_doom_loop_string(raw: &str) -> String {
    let mut text = raw.replace("\r\n", "\n").replace('\r', "\n");
    text = text
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    // Collapse 3+ blank lines into 2.
    let mut collapsed = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.split('\n') {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                collapsed.push('\n');
            }
        } else {
            blank_run = 0;
            collapsed.push_str(line);
            collapsed.push('\n');
        }
    }
    let mut normalized = collapsed;
    if normalized.ends_with('\n') {
        normalized.pop();
    }

    if !URI_SCHEME_RE.is_match(&normalized) {
        normalized = normalized.replace('\\', "/");
    }
    if is_path_like(&normalized) {
        normalized = collapse_dot_segments(&normalized);
    }
    normalized
}

fn is_path_like(text: &str) -> bool {
    text.contains('/') || text.starts_with('.') || text.starts_with('~')
}

/// Lexical `.`/`..` collapse without filesystem access. Unresolvable
/// leading `..` on relative paths is preserved.
fn collapse_dot_segments(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if stack.last().is_some_and(|s| *s != "..") {
                    stack.pop();
                } else if !absolute {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Recursively normalizes a JSON argument value: strings per
/// [`normalize_doom_loop_string`], objects with sorted keys, arrays in
/// order (order stays meaningful).
pub fn normalize_doom_loop_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(normalize_doom_loop_string(&s)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(normalize_doom_loop_value).collect())
        }
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| (k, normalize_doom_loop_value(v)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        other => other,
    }
}

/// Stable fingerprint for a tool call: JSON tool name + stable-normalized
/// argument JSON. Unparseable arguments fingerprint as their raw string.
pub fn fingerprint_tool_call(tool_name: &str, input_json: &str) -> String {
    let normalized: serde_json::Value = serde_json::from_str(input_json)
        .map(normalize_doom_loop_value)
        .unwrap_or_else(|_| serde_json::Value::String(input_json.to_string()));
    let stable = serde_json::to_string(&normalized).unwrap_or_default();
    let name_json = serde_json::to_string(tool_name).unwrap_or_default();
    format!("{name_json}:{stable}")
}

/// Extracts the call target (path/command/query/…) for streak resets.
/// Returns `None` when no target key is present.
pub fn extract_doom_loop_target(input_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(input_json).ok()?;
    let object = value.as_object()?;
    for key in DOOM_LOOP_TARGET_KEYS {
        if let Some(serde_json::Value::String(text)) = object.get(*key) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(normalize_doom_loop_string(trimmed));
            }
        }
    }
    None
}

/// Records a tool use for the untried-tool suggestion pool (deduped,
/// order-stable).
pub fn record_doom_loop_tool_use(state: &mut DoomLoopTurnState, tool_name: &str) {
    if !state
        .used_tools
        .iter()
        .any(|used| used.eq_ignore_ascii_case(tool_name))
    {
        state.used_tools.push(tool_name.to_string());
    }
}

/// Advances the ladder for one tool call. Same fingerprint + same target
/// extends the streak; anything else resets it. Each stage fires at most
/// once per streak.
pub fn advance_doom_loop(
    state: &mut DoomLoopTurnState,
    tool_name: &str,
    input_json: &str,
) -> DoomLoopTransition {
    let fingerprint = fingerprint_tool_call(tool_name, input_json);
    let target = extract_doom_loop_target(input_json);

    let same_fingerprint = state.fingerprint.as_deref() == Some(fingerprint.as_str());
    let same_target = state.target == target;
    if same_fingerprint && same_target {
        state.streak += 1;
    } else {
        state.streak = 1;
        state.stage = 0;
        state.fingerprint = Some(fingerprint);
        state.target = target;
    }

    let kind = match (state.streak, state.stage) {
        (streak, 0) if streak >= DOOM_LOOP_NUDGE_STREAK => {
            state.stage = 1;
            DoomLoopTransitionKind::Nudge
        }
        (streak, 1) if streak >= DOOM_LOOP_STRATEGY_STREAK => {
            state.stage = 2;
            DoomLoopTransitionKind::StrategyChange
        }
        (streak, 2) if streak >= DOOM_LOOP_FINAL_STREAK => {
            state.stage = 3;
            DoomLoopTransitionKind::Final
        }
        _ => DoomLoopTransitionKind::None,
    };

    DoomLoopTransition {
        kind,
        streak: state.streak,
        tool_name: Some(tool_name.to_string()),
    }
}

/// Untried tools from the candidate pool (active tool pack when available,
/// else the sent schema list), excluding the looping tool and already-used
/// tools. Order-stable, deduplicated, capped at `limit`.
pub fn untried_doom_loop_tools(
    pack_keep_names: Option<&[String]>,
    sent_tool_names: &[String],
    used_tool_names: &[String],
    looping_tool_name: &str,
    limit: usize,
) -> Vec<String> {
    let pool: Vec<String> = match pack_keep_names {
        Some(names) => names.to_vec(),
        None => sent_tool_names.to_vec(),
    };
    let looping = looping_tool_name.to_ascii_lowercase();
    let used: HashSet<String> = used_tool_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for name in pool {
        let lowered = name.to_ascii_lowercase();
        if lowered == looping || used.contains(&lowered) || !seen.insert(lowered.clone()) {
            continue;
        }
        out.push(name);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Runs the ladder over one batch of tool calls, returning an observation
/// per fired stage.
pub fn detect_doom_loop_transitions(
    calls: &[DoomLoopCall],
    state: &mut DoomLoopTurnState,
    pack_keep_names: Option<&[String]>,
    sent_tool_names: &[String],
    untried_limit: usize,
) -> Vec<DoomLoopObservation> {
    let mut observations = Vec::new();
    for call in calls {
        record_doom_loop_tool_use(state, &call.name);
        let transition = advance_doom_loop(state, &call.name, &call.input_json);
        if transition.kind == DoomLoopTransitionKind::None {
            continue;
        }
        let untried_tools = if transition.kind == DoomLoopTransitionKind::StrategyChange {
            untried_doom_loop_tools(
                pack_keep_names,
                sent_tool_names,
                &state.used_tools,
                &call.name,
                untried_limit,
            )
        } else {
            Vec::new()
        };
        observations.push(DoomLoopObservation {
            kind: transition.kind,
            streak: transition.streak,
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            untried_tools,
        });
    }
    observations
}

/// Reminder body for the nudge stage.
pub fn build_doom_loop_nudge_body(tool_name: &str, streak: u32) -> String {
    format!(
        "You have called `{tool_name}` {streak} times with effectively the same input. \
         Pause and reconsider: is the result what you expected? If not, change your approach \
         instead of retrying the identical call."
    )
}

/// Reminder body for the forced strategy-change stage.
pub fn build_doom_loop_strategy_body(
    tool_name: &str,
    streak: u32,
    untried_tools: &[String],
) -> String {
    let suggestion = if untried_tools.is_empty() {
        "try a fundamentally different approach".to_string()
    } else {
        format!("consider these untried tools: {}", untried_tools.join(", "))
    };
    format!(
        "STRATEGY CHANGE REQUIRED: `{tool_name}` has now been called {streak} times with \
         effectively the same input and the loop is not converging. Stop repeating this call. \
         {suggestion}; re-read the relevant files with fresh eyes, or ask the user for \
         clarification instead of guessing again."
    )
}

/// Final-stage message for unattended/headless turns: one last redirect,
/// then the ladder disarms for this streak.
pub fn build_doom_loop_final_unattended_body(tool_name: &str, streak: u32) -> String {
    format!(
        "FINAL REDIRECT: `{tool_name}` has been called {streak} times with effectively the same \
         input. This is the last automatic redirect for this loop. Take a completely different \
         approach now — different tool, different target, or summarize what is blocking you and \
         stop. Do not call `{tool_name}` with the same input again."
    )
}

/// Final-stage message for interactive turns: resumable pause with a
/// resume offer.
pub fn build_doom_loop_pause_body(tool_name: &str, streak: u32) -> String {
    format!(
        "Paused: `{tool_name}` was called {streak} times with effectively the same input and the \
         loop is not converging. The session state is saved and this turn is resumable — reply \
         with guidance (for example \"try X instead\" or \"continue\") and the turn will pick \
         up from here. Nothing was lost."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, input_json: &str) -> DoomLoopCall {
        DoomLoopCall {
            id: id.to_string(),
            name: name.to_string(),
            input_json: input_json.to_string(),
        }
    }

    #[test]
    fn fingerprint_normalizes_key_order_and_line_endings() {
        let a = fingerprint_tool_call("read", r#"{"path":"a.txt","x":1}"#);
        let b = fingerprint_tool_call("read", "{\"x\":1,\r\n\"path\":\"a.txt\"}");
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_normalizes_path_separators_and_dot_segments() {
        let a = fingerprint_tool_call("read", r#"{"path":"src/../src/a.txt"}"#);
        let b = fingerprint_tool_call("read", r#"{"path":"src\\a.txt"}"#);
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_keeps_commands_and_offsets_meaningful() {
        let a = fingerprint_tool_call("bash", r#"{"command":"ls /tmp"}"#);
        let b = fingerprint_tool_call("bash", r#"{"command":"ls /var"}"#);
        assert_ne!(a, b);
        let c = fingerprint_tool_call("read", r#"{"path":"a.txt","offset":10}"#);
        let d = fingerprint_tool_call("read", r#"{"path":"a.txt","offset":20}"#);
        assert_ne!(c, d);
    }

    #[test]
    fn fingerprint_preserves_array_order_but_ignores_blank_runs() {
        let a = fingerprint_tool_call("edit", r#"{"lines":["a","b"]}"#);
        let b = fingerprint_tool_call("edit", r#"{"lines":["b","a"]}"#);
        assert_ne!(a, b);
        let c = fingerprint_tool_call("write", "{\"text\":\"a\\n\\n\\n\\nb\"}");
        let d = fingerprint_tool_call("write", "{\"text\":\"a\\n\\nb\"}");
        assert_eq!(c, d);
    }

    #[test]
    fn fingerprint_does_not_touch_uris() {
        let a = normalize_doom_loop_string("https://example.com/a\\b");
        assert_eq!(a, "https://example.com/a\\b");
    }

    #[test]
    fn uri_scheme_with_port_and_query() {
        assert!(URI_SCHEME_RE.is_match("HTTPS://x.io:1/a?b=1"));
        assert!(!URI_SCHEME_RE.is_match("not a uri"));
    }

    #[test]
    fn unresolvable_leading_dotdot_preserved() {
        assert_eq!(collapse_dot_segments("../../a"), "../../a");
        assert_eq!(collapse_dot_segments("/a/../../b"), "/b");
        assert_eq!(collapse_dot_segments("a/./b"), "a/b");
    }

    #[test]
    fn target_extraction_picks_first_present_key() {
        assert_eq!(
            extract_doom_loop_target(r#"{"pattern":"foo","path":"a.txt"}"#),
            Some("a.txt".to_string())
        );
        assert_eq!(
            extract_doom_loop_target(r#"{"command":"cargo test"}"#),
            Some("cargo test".to_string())
        );
        assert_eq!(extract_doom_loop_target(r#"{"other":1}"#), None);
        assert_eq!(extract_doom_loop_target("not json"), None);
    }

    #[test]
    fn ladder_fires_nudge_strategy_final_then_quiet() {
        let mut state = DoomLoopTurnState::default();
        let input = r#"{"path":"src/a.ts"}"#;
        let mut kinds = Vec::new();
        for _ in 0..7 {
            kinds.push(advance_doom_loop(&mut state, "read", input).kind);
        }
        assert_eq!(
            kinds,
            vec![
                DoomLoopTransitionKind::None,
                DoomLoopTransitionKind::None,
                DoomLoopTransitionKind::Nudge,
                DoomLoopTransitionKind::StrategyChange,
                DoomLoopTransitionKind::Final,
                DoomLoopTransitionKind::None,
                DoomLoopTransitionKind::None,
            ]
        );
    }

    #[test]
    fn different_target_resets_the_streak() {
        let mut state = DoomLoopTurnState::default();
        for _ in 0..3 {
            advance_doom_loop(&mut state, "read", r#"{"path":"a.ts"}"#);
        }
        assert_eq!(state.stage, 1); // nudged
        let t = advance_doom_loop(&mut state, "read", r#"{"path":"b.ts"}"#);
        assert_eq!(t.kind, DoomLoopTransitionKind::None);
        assert_eq!(state.streak, 1);
        assert_eq!(state.stage, 0);
    }

    #[test]
    fn cosmetic_differences_do_not_reset_the_streak() {
        let mut state = DoomLoopTurnState::default();
        advance_doom_loop(&mut state, "read", "{\"path\":\"a.ts\"}");
        advance_doom_loop(&mut state, "read", "{\"path\": \"a.ts\"\r\n}");
        let t = advance_doom_loop(&mut state, "read", "{\"path\":\"a.ts\"}");
        assert_eq!(t.kind, DoomLoopTransitionKind::Nudge);
        assert_eq!(t.streak, 3);
    }

    #[test]
    fn untried_tools_exclude_looping_and_used() {
        let keep = vec![
            "read".to_string(),
            "grep".to_string(),
            "bash".to_string(),
            "lsp".to_string(),
        ];
        let untried = untried_doom_loop_tools(
            Some(&keep),
            &[],
            &["read".to_string(), "bash".to_string()],
            "read",
            3,
        );
        assert_eq!(untried, vec!["grep".to_string(), "lsp".to_string()]);
    }

    #[test]
    fn batch_detection_collects_observations_with_untried_on_strategy() {
        let mut state = DoomLoopTurnState::default();
        let calls: Vec<DoomLoopCall> = (0..5)
            .map(|i| call(&format!("c{i}"), "read", r#"{"path":"a.ts"}"#))
            .collect();
        let keep = vec!["read".to_string(), "grep".to_string()];
        let obs = detect_doom_loop_transitions(&calls, &mut state, Some(&keep), &[], 3);
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[0].kind, DoomLoopTransitionKind::Nudge);
        assert!(obs[0].untried_tools.is_empty());
        assert_eq!(obs[1].kind, DoomLoopTransitionKind::StrategyChange);
        assert_eq!(obs[1].untried_tools, vec!["grep".to_string()]);
        assert_eq!(obs[2].kind, DoomLoopTransitionKind::Final);
    }

    #[test]
    fn message_bodies_name_tool_and_streak() {
        assert!(build_doom_loop_nudge_body("read", 3).contains("`read`"));
        assert!(build_doom_loop_strategy_body("read", 4, &["grep".to_string()]).contains("grep"));
        assert!(build_doom_loop_final_unattended_body("read", 5).contains("FINAL REDIRECT"));
        assert!(build_doom_loop_pause_body("read", 5).contains("resumable"));
    }

    #[test]
    fn doom_loop_kill_switch_spelling() {
        assert!(is_doom_loop_disabled(
            &|k| (k == DOOM_LOOP_ENV).then(|| "off".to_string())
        ));
        assert!(!is_doom_loop_disabled(&|_| None));
    }
}
