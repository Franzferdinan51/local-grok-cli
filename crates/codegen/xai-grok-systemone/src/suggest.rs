//! Route-driven MCP server suggestions and conservative pruning.
//!
//! Mirrors the `grok-local-acp-adapter` v0.5.2 `_suggest_mcp_servers()`:
//! keyword overlap between the task text (+ the router's `task_labels`) and
//! the configured MCP servers' names/descriptions. A name token scores 2,
//! a description token scores 1. Servers scoring 0 are not suggested.
//!
//! An empty suggestion list means "no confident match: use everything"
//! (fail-open), NOT "use none".
//!
//! # Phase 3: shim-ranked tools
//!
//! When the router returns the new decision surface with
//! `tool_scoring == "full"` and a non-empty `ranked_tools` list, the shim's
//! hybrid (keyword + zero-shot model) tool ranking drives suggestions
//! instead of the local keyword scorer. The existing prune gates are
//! unchanged — the ranking only changes *what* is suggested.
//!
//! Pruning is conservative and opt-in: it engages only when the route came
//! from a live router (not fail-open), the confidence meets the configured
//! threshold, the tier is a cheap one (`edge`/`economy`) — the cases where
//! a small, focused toolset is both safe and the point — and the router is
//! NOT uncertain. Everything else keeps the full configured server list.
//!
//! `uncertain == true` disables pruning unconditionally: an uncertain route
//! must never narrow the toolset. When `tool_scoring == "skipped"` or the
//! keys are absent (older shim), behavior is exactly the pre-Phase-3 one.

use std::collections::HashSet;

use crate::config::SystemOneConfig;
use crate::route::{RankedTool, RouteDecision, RouteSource, Tier};

/// Stopwords excluded from suggestion tokens. Same list as the adapter's
/// `_SUGGEST_STOPWORDS`.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "as", "at", "by", "from",
    "is", "are", "was", "were", "be", "do", "does", "did", "will", "would", "can", "could",
    "should", "have", "has", "had", "it", "its", "this", "that", "these", "those", "you", "your",
    "we", "they", "them", "his", "her", "our", "their", "my", "me", "i", "not", "no", "yes", "if",
    "then", "than", "so", "such", "too", "very", "just", "also", "only", "use", "using", "used",
    "via", "per", "within", "into", "out", "up", "down", "over", "under", "again", "once", "here",
    "there", "when", "where", "which", "who", "whom", "whose", "what", "why", "how", "all", "any",
    "each", "every", "some", "more", "most", "other", "own", "same", "now", "don",
];

/// A configured MCP server: name + description (or command) from
/// `~/.grok-local/config.toml` `[mcp_servers.<name>]`.
#[derive(Debug, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub description: String,
}

/// A ranked suggestion: which server matched and why.
#[derive(Debug, Clone)]
pub struct ServerSuggestion {
    pub name: String,
    pub score: u32,
    pub matched: Vec<String>,
}

/// Read the `[mcp_servers.*]` inventory from `$GROK_HOME/config.toml`.
/// Servers with `enabled = false` are skipped. Fail-open: `[]` on any problem.
pub fn inventory_from_config() -> Vec<McpServerInfo> {
    inventory_from_path(&SystemOneConfig::config_path())
}

fn inventory_from_path(path: &std::path::Path) -> Vec<McpServerInfo> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(_) => return Vec::new(),
    };
    let servers = match table.get("mcp_servers").and_then(toml::Value::as_table) {
        Some(servers) => servers,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for (name, srv) in servers {
        let table = match srv.as_table() {
            Some(table) => table,
            None => continue,
        };
        if table.get("enabled").and_then(toml::Value::as_bool) == Some(false) {
            continue;
        }
        let description = table
            .get("description")
            .and_then(toml::Value::as_str)
            .or_else(|| table.get("command").and_then(toml::Value::as_str))
            .unwrap_or("")
            .to_string();
        out.push(McpServerInfo {
            name: name.clone(),
            description,
        });
    }
    out
}

/// Split into lowercase alphanumeric tokens of length >= 3 (mirrors the
/// adapter's `[a-z0-9]{3,}` regex), minus stopwords.
fn tokens(text: &str) -> HashSet<String> {
    let stopwords: HashSet<&str> = STOPWORDS.iter().copied().collect();
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|tok| {
            let tok = tok.to_ascii_lowercase();
            if tok.len() >= 3 && !stopwords.contains(tok.as_str()) {
                Some(tok)
            } else {
                None
            }
        })
        .collect()
}

/// Rank configured MCP servers against the task text + route task labels.
/// Returns only servers with score > 0, sorted by score desc (ties by name).
pub fn suggest_mcp_servers(
    task_text: &str,
    task_labels: &[String],
    inventory: &[McpServerInfo],
) -> Vec<ServerSuggestion> {
    let mut task_tokens = tokens(task_text);
    for label in task_labels {
        task_tokens.insert(label.to_ascii_lowercase());
    }
    let mut scored: Vec<ServerSuggestion> = Vec::new();
    for srv in inventory {
        let name_tokens = tokens(&srv.name);
        let desc_tokens = tokens(&srv.description);
        let mut matched: Vec<String> = task_tokens
            .iter()
            .filter(|t| name_tokens.contains(*t) || desc_tokens.contains(*t))
            .cloned()
            .collect();
        matched.sort();
        let score: u32 = matched
            .iter()
            .map(|t| if name_tokens.contains(t) { 2 } else { 1 })
            .sum();
        if score > 0 {
            scored.push(ServerSuggestion {
                name: srv.name.clone(),
                score,
                matched,
            });
        }
    }
    scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    scored
}

/// Build suggestions from the shim's ranked tools (Phase 3,
/// `tool_scoring == "full"`).
///
/// The shim already sorted by relevance desc; we keep that order (ties by
/// id). Relevance [0.0, 1.0] is scaled to the 0–100 `ServerSuggestion`
/// score. The `matched` field records the provenance so logs show these
/// came from the router, not the keyword scorer.
pub fn suggestions_from_ranked_tools(ranked: &[RankedTool]) -> Vec<ServerSuggestion> {
    let mut suggestions: Vec<ServerSuggestion> = ranked
        .iter()
        .map(|t| ServerSuggestion {
            name: t.id.clone(),
            score: (t.relevance.clamp(0.0, 1.0) * 100.0).round() as u32,
            matched: vec!["shim-ranked".to_string()],
        })
        .collect();
    suggestions.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    suggestions
}

/// Decide whether conservative MCP pruning engages for this decision.
///
/// All of these must hold:
/// - pruning is opted in (`[systemone] prune_mcp_servers` or
///   `GROK_LOCAL_SYSTEMONE_PRUNE=1`),
/// - the decision came from a live router (never fail-open),
/// - the route is NOT uncertain (`uncertain == true` disables pruning
///   unconditionally — an uncertain route must never narrow the toolset),
/// - confidence meets `prune_min_confidence`,
/// - the tier is cheap (`edge`/`economy`) — pruning a heavy task's toolset is
///   exactly the wrong economy,
/// - at least one server was suggested (empty = "use everything").
///
/// Returns `Some(allowlist)` when pruning engages, else `None`.
pub fn prune_allowlist(decision: &RouteDecision, cfg: &SystemOneConfig) -> Option<Vec<String>> {
    if !cfg.prune_mcp_servers {
        return None;
    }
    if decision.source != RouteSource::SystemOne {
        return None;
    }
    // Phase 3: the shim's own uncertainty verdict overrides everything.
    if decision.uncertain == Some(true) {
        return None;
    }
    let confidence = decision.confidence.unwrap_or(0.0);
    if confidence < cfg.prune_min_confidence {
        return None;
    }
    if !matches!(decision.tier, Some(Tier::Edge) | Some(Tier::Economy)) {
        return None;
    }
    if decision.suggested_mcp_servers.is_empty() {
        return None;
    }
    Some(decision.suggested_mcp_servers.clone())
}

/// Convenience: full pipeline from task text to an optional prune allowlist.
/// Returns `(suggestions, prune_allowlist)`.
///
/// When the decision carries the shim's tool ranking
/// (`tool_scoring == "full"` with ranked tools), suggestions come from the
/// ranking; otherwise the keyword scorer runs (pre-Phase-3 behavior).
/// An uncertain route disables pruning and records a diagnostic note on the
/// decision (`prune=disabled(uncertain)` in the evidence line) so the
/// operator can see the toolset was deliberately left wide.
pub fn suggest_and_maybe_prune(
    task_text: &str,
    decision: &mut RouteDecision,
    cfg: &SystemOneConfig,
) -> (Vec<ServerSuggestion>, Option<Vec<String>>) {
    let suggestions = if decision.has_shim_tool_ranking() {
        suggestions_from_ranked_tools(&decision.ranked_tools)
    } else {
        let inventory = inventory_from_config();
        suggest_mcp_servers(task_text, &decision.task_labels, &inventory)
    };
    decision.suggested_mcp_servers = suggestions.iter().map(|s| s.name.clone()).collect();
    let allow = prune_allowlist(decision, cfg);
    if allow.is_none() && decision.uncertain == Some(true) {
        decision.prune_note = Some("disabled(uncertain)".to_string());
    }
    (suggestions, allow)
}

/// Score detail for logging: `name=score(matched,tokens)`.
pub fn suggestion_detail(suggestions: &[ServerSuggestion]) -> String {
    suggestions
        .iter()
        .map(|s| format!("{}={}({})", s.name, s.score, s.matched.join(",")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn inventory() -> Vec<McpServerInfo> {
        vec![
            McpServerInfo {
                name: "cannabis-grow".to_string(),
                description: "AC Infinity tent sensors and grow-op controls".to_string(),
            },
            McpServerInfo {
                name: "web-search".to_string(),
                description: "search the web via searxng".to_string(),
            },
            McpServerInfo {
                name: "grok-local-speedstack".to_string(),
                description: "legacy python adapter".to_string(),
            },
        ]
    }

    /// A live-router decision with pruning opted in (matches the old
    /// `prune_gates` fixture, extended with the Phase 3 fields).
    fn routed_decision() -> RouteDecision {
        RouteDecision {
            source: RouteSource::SystemOne,
            url: Some("http://127.0.0.1:8765/v1/systemone/route".to_string()),
            tier: Some(Tier::Economy),
            effort: crate::route::Effort::Low,
            max_turns: 4,
            confidence: Some(0.9),
            rationale: None,
            model_id: None,
            task_labels: vec![],
            suggested_mcp_servers: vec!["web-search".to_string()],
            error: None,
            uncertain: None,
            margin: None,
            calibrated: None,
            tool_scoring: None,
            ranked_tools: Vec::new(),
            ranked_models: Vec::new(),
            calibrated_probabilities: Vec::new(),
            prune_note: None,
        }
    }

    fn prune_cfg() -> SystemOneConfig {
        SystemOneConfig {
            prune_mcp_servers: true,
            ..SystemOneConfig::default()
        }
    }

    #[test]
    fn name_tokens_outscore_description_tokens() {
        let suggestions = suggest_mcp_servers("check the cannabis tent sensors", &[], &inventory());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].name, "cannabis-grow");
        // "cannabis" hits the name (2); "tent"/"sensors" hit the description (1+1).
        assert_eq!(suggestions[0].score, 4);
    }

    #[test]
    fn task_labels_count_as_tokens() {
        let suggestions =
            suggest_mcp_servers("do the thing", &["searxng".to_string()], &inventory());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].name, "web-search");
    }

    #[test]
    fn no_match_means_use_everything() {
        let suggestions = suggest_mcp_servers("quantum banana philosophy", &[], &inventory());
        assert!(suggestions.is_empty());
    }

    #[test]
    fn stopwords_ignored() {
        // "the" / "and" must not match anything; only "cannabis" should.
        let suggestions = suggest_mcp_servers("the cannabis and the", &[], &inventory());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].name, "cannabis-grow");
    }

    #[test]
    fn tie_broken_by_name() {
        let inv = vec![
            McpServerInfo {
                name: "bbb".to_string(),
                description: "alpha".to_string(),
            },
            McpServerInfo {
                name: "aaa".to_string(),
                description: "alpha".to_string(),
            },
        ];
        let suggestions = suggest_mcp_servers("alpha", &[], &inv);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].name, "aaa");
    }

    #[test]
    fn prune_gates() {
        let cfg = prune_cfg();
        let mut decision = routed_decision();
        assert_eq!(
            prune_allowlist(&decision, &cfg),
            Some(vec!["web-search".to_string()])
        );

        // Fail-open never prunes.
        decision.source = RouteSource::FailOpen;
        assert_eq!(prune_allowlist(&decision, &cfg), None);
        decision.source = RouteSource::SystemOne;

        // Low confidence never prunes.
        decision.confidence = Some(0.5);
        assert_eq!(prune_allowlist(&decision, &cfg), None);
        decision.confidence = Some(0.9);

        // Heavy tier never prunes.
        decision.tier = Some(Tier::Heavy);
        assert_eq!(prune_allowlist(&decision, &cfg), None);
        decision.tier = Some(Tier::Edge);
        assert!(prune_allowlist(&decision, &cfg).is_some());

        // Empty suggestions never prune.
        decision.suggested_mcp_servers.clear();
        assert_eq!(prune_allowlist(&decision, &cfg), None);

        // Opt-out never prunes.
        decision.suggested_mcp_servers = vec!["web-search".to_string()];
        let cfg_off = SystemOneConfig::default();
        assert_eq!(prune_allowlist(&decision, &cfg_off), None);
    }

    #[test]
    fn inventory_skips_disabled_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            [
                "[mcp_servers.on]",
                "command = \"serve-on\"",
                "[mcp_servers.off]",
                "enabled = false",
                "command = \"serve-off\"",
                "[mcp_servers.nodesc]",
                "command = \"serve-nodesc\"",
                "",
            ]
            .join("\n"),
        )
        .unwrap();
        let inv = inventory_from_path(&path);
        assert_eq!(inv.len(), 2);
        let names: HashMap<&str, &str> = inv
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str()))
            .collect();
        assert_eq!(names["on"], "serve-on");
        assert_eq!(names["nodesc"], "serve-nodesc");

        let missing = inventory_from_path(&dir.path().join("nope.toml"));
        assert!(missing.is_empty());
    }

    // ---------------- Phase 3 tests ----------------

    /// `uncertain == true` disables pruning unconditionally, even when every
    /// other gate (opt-in, live router, high confidence, cheap tier,
    /// non-empty suggestions) passes.
    #[test]
    fn uncertain_disables_pruning() {
        let cfg = prune_cfg();
        let mut decision = routed_decision();
        // All classic gates pass...
        assert!(prune_allowlist(&decision, &cfg).is_some());
        // ...until the shim says it's uncertain.
        decision.uncertain = Some(true);
        assert_eq!(prune_allowlist(&decision, &cfg), None);
        // Explicit `false` keeps the old behavior.
        decision.uncertain = Some(false);
        assert!(prune_allowlist(&decision, &cfg).is_some());
    }

    /// An older shim (no new keys) keeps the keyword suggestion path and the
    /// pre-Phase-3 prune behavior: pruning still engages when opted in.
    #[test]
    fn missing_ranked_tools_keeps_old_behavior() {
        let cfg = prune_cfg();
        let decision = routed_decision();
        assert!(!decision.has_shim_tool_ranking());
        assert_eq!(decision.uncertain, None);
        // Classic gates unchanged: prune still engages on a live, confident,
        // cheap-tier route with suggestions.
        assert_eq!(
            prune_allowlist(&decision, &cfg),
            Some(vec!["web-search".to_string()])
        );
    }

    #[test]
    fn shim_ranked_tools_become_suggestions() {
        let ranked = vec![
            RankedTool {
                id: "web-search".to_string(),
                kind: Some("mcp".to_string()),
                relevance: 0.92,
            },
            RankedTool {
                id: "browserclaw".to_string(),
                kind: Some("mcp".to_string()),
                relevance: 0.61,
            },
        ];
        let suggestions = suggestions_from_ranked_tools(&ranked);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].name, "web-search");
        assert_eq!(suggestions[0].score, 92);
        assert_eq!(suggestions[1].name, "browserclaw");
        assert_eq!(suggestions[1].score, 61);
        assert_eq!(suggestions[0].matched, vec!["shim-ranked".to_string()]);
        // Out-of-range relevance is clamped, never panics.
        let weird = vec![RankedTool {
            id: "x".to_string(),
            kind: None,
            relevance: 2.5,
        }];
        assert_eq!(suggestions_from_ranked_tools(&weird)[0].score, 100);
    }

    /// Full pipeline: a `tool_scoring == "full"` decision uses the shim's
    /// ranking for suggestions; the classic prune gates still decide whether
    /// pruning engages.
    #[test]
    fn suggest_pipeline_uses_shim_ranking() {
        let cfg = prune_cfg();
        let mut decision = routed_decision();
        decision.tool_scoring = Some("full".to_string());
        decision.ranked_tools = vec![RankedTool {
            id: "web-search".to_string(),
            kind: Some("mcp".to_string()),
            relevance: 0.9,
        }];
        // "unrelated text with no keyword overlap" must not produce keyword
        // suggestions — the shim ranking drives instead.
        let (suggestions, allow) = suggest_and_maybe_prune(
            "unrelated text with no keyword overlap",
            &mut decision,
            &cfg,
        );
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].name, "web-search");
        assert_eq!(
            decision.suggested_mcp_servers,
            vec!["web-search".to_string()]
        );
        // Classic gates still apply: opted-in + live + confident + cheap tier.
        assert_eq!(allow, Some(vec!["web-search".to_string()]));
        assert_eq!(decision.prune_note, None);
    }

    /// Full pipeline: an uncertain route records the diagnostic note and
    /// passes all candidate tools through (no pruning).
    #[test]
    fn suggest_pipeline_notes_uncertain_no_prune() {
        let cfg = prune_cfg();
        let mut decision = routed_decision();
        decision.uncertain = Some(true);
        decision.tool_scoring = Some("skipped".to_string());
        let (suggestions, allow) =
            suggest_and_maybe_prune("check the cannabis tent sensors", &mut decision, &cfg);
        // Suggestions still computed (whatever the keyword path found)...
        assert_eq!(
            decision.suggested_mcp_servers,
            suggestions
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        );
        // ...but pruning is disabled and the note is recorded.
        assert_eq!(allow, None);
        assert_eq!(decision.prune_note.as_deref(), Some("disabled(uncertain)"));
        let line = decision.evidence_line(crate::shim::RouterStatus::AlreadyRunning);
        assert!(line.contains("uncertain=true"));
        assert!(line.contains("prune=disabled(uncertain)"));
    }
}
