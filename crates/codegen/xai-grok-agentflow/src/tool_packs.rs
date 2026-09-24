//! Label-driven tool-pack schema pruning.
//!
//! Ports ZCode 3.25.0's tool packs: for each turn, the router's
//! `task_labels` plus keyword inference from the task text select a
//! task-relevant tool schema shortlist. Only schemas in the shortlist are
//! sent to the model; core and protocol tools are never pruned. Low
//! confidence, a missing route, or a kill switch fails open to the full
//! tool list.
//!
//! Unknown MCP servers fail open (kept), and a tool-pack miss — the model
//! calling a tool that was pruned from its schema list — fails open to the
//! full tool set for the rest of the turn plus a retry reminder.

use std::collections::HashSet;

use crate::EnvReader;

/// Confidence floor for pruning (ZCode 3.25.0 policy).
pub const TOOL_PACK_PRUNE_CONFIDENCE_THRESHOLD: f64 = 0.6;

/// Kill switch: `GROK_LOCAL_AGENTFLOW_PRUNE=0` disables pruning.
pub const TOOL_PACK_PRUNE_KILL_SWITCH_ENV: &str = "GROK_LOCAL_AGENTFLOW_PRUNE";

/// Tools that are always kept, whatever the labels are (read/edit/write
/// core, shell, search, and todo state).
const TOOL_PACK_CORE_TOOLS: &[&str] = &[
    "read",
    "read_file",
    "hashline_read",
    "edit",
    "search_replace",
    "apply_patch",
    "hashline_edit",
    "write",
    "bash",
    "run_terminal_cmd",
    "run_terminal_command",
    "glob",
    "list_dir",
    "grep",
    "grep_files",
    "hashline_grep",
    "todo_write",
    "todowrite",
];

/// Protocol tools that must never be pruned (task lifecycle, questions,
/// memory, terminal output).
const TOOL_PACK_PROTOCOL_TOOLS: &[&str] = &[
    "ask_user_question",
    "enter_plan_mode",
    "exit_plan_mode",
    "get_task_output",
    "get_terminal_command_output",
    "kill_task",
    "kill_terminal_command",
    "memory_get",
    "memory_search",
    "monitor",
    "skill",
    "wait_tasks",
];

/// One label → keep-tools / keep-servers rule.
pub struct ToolPackLabelRule {
    /// Canonical label (also the config/TOML key style).
    pub label: &'static str,
    /// Alternate spellings that resolve to this label.
    pub aliases: &'static [&'static str],
    /// Keywords (matched case-insensitively with word boundaries) that
    /// infer this label from the task text.
    pub keywords: &'static [&'static str],
    /// Built-in tool names to keep when this label is active.
    pub keep_tools: &'static [&'static str],
    /// MCP server-name substrings to keep when this label is active.
    pub keep_servers: &'static [&'static str],
}

/// The 20-rule label table (ports ZCode's TOOL_PACK_LABEL_TABLE).
pub static TOOL_PACK_LABEL_TABLE: &[ToolPackLabelRule] = &[
    ToolPackLabelRule {
        label: "code-review",
        aliases: &["review", "code review"],
        keywords: &[
            "review",
            "pull request",
            "pr review",
            "code review",
            "approve",
        ],
        keep_tools: &[
            "read_file",
            "grep",
            "grep_files",
            "hashline_read",
            "hashline_grep",
        ],
        keep_servers: &["github", "gitlab", "pr"],
    },
    ToolPackLabelRule {
        label: "testing",
        aliases: &["test", "tests"],
        keywords: &[
            "test",
            "tests",
            "testing",
            "pytest",
            "jest",
            "vitest",
            "cargo test",
            "go test",
            "coverage",
            "spec",
        ],
        keep_tools: &[
            "bash",
            "run_terminal_command",
            "read_file",
            "grep",
            "grep_files",
        ],
        keep_servers: &["test", "coverage"],
    },
    ToolPackLabelRule {
        label: "debugging",
        aliases: &["debug", "fix"],
        keywords: &[
            "debug",
            "debugging",
            "stack trace",
            "error",
            "exception",
            "fix the bug",
            "bug",
            "crash",
            "traceback",
            "panic",
        ],
        keep_tools: &[
            "read_file",
            "grep",
            "grep_files",
            "bash",
            "run_terminal_command",
            "edit",
            "hashline_read",
            "hashline_grep",
        ],
        keep_servers: &["sentry", "debug"],
    },
    ToolPackLabelRule {
        label: "refactoring",
        aliases: &["refactor"],
        keywords: &[
            "refactor",
            "refactoring",
            "restructure",
            "clean up",
            "cleanup",
            "rename",
        ],
        keep_tools: &[
            "read_file",
            "edit",
            "search_replace",
            "grep",
            "grep_files",
            "bash",
            "run_terminal_command",
            "lsp",
        ],
        keep_servers: &["lsp"],
    },
    ToolPackLabelRule {
        label: "documentation",
        aliases: &["docs", "doc"],
        keywords: &[
            "document",
            "documentation",
            "readme",
            "docstring",
            "changelog",
            "write docs",
        ],
        keep_tools: &[
            "read_file",
            "write",
            "edit",
            "search_replace",
            "glob",
            "list_dir",
        ],
        keep_servers: &["docs"],
    },
    ToolPackLabelRule {
        label: "git-operations",
        aliases: &["git", "vcs"],
        keywords: &[
            "git", "commit", "branch", "merge", "rebase", "diff", "stash", "checkout", "pull",
            "push",
        ],
        keep_tools: &["bash", "run_terminal_command", "read_file"],
        keep_servers: &["github", "gitlab", "git"],
    },
    ToolPackLabelRule {
        label: "database",
        aliases: &["db", "sql"],
        keywords: &[
            "database",
            "sql",
            "postgres",
            "mysql",
            "sqlite",
            "migration",
            "schema",
            "query",
            "table",
        ],
        keep_tools: &["bash", "run_terminal_command", "read_file", "write"],
        keep_servers: &[
            "postgres", "mysql", "sqlite", "database", "db", "supabase", "neon",
        ],
    },
    ToolPackLabelRule {
        label: "api-development",
        aliases: &["api", "rest", "endpoint"],
        keywords: &[
            "api", "endpoint", "rest", "graphql", "route", "handler", "webhook", "openapi",
        ],
        keep_tools: &[
            "read_file",
            "edit",
            "write",
            "bash",
            "run_terminal_command",
            "web_fetch",
        ],
        keep_servers: &["api", "postman", "http"],
    },
    ToolPackLabelRule {
        label: "web-search",
        aliases: &["search", "research"],
        keywords: &[
            "search the web",
            "look up",
            "research",
            "find documentation",
            "what is",
            "how to",
            "latest",
            "news",
        ],
        keep_tools: &["web_search", "web_fetch"],
        keep_servers: &["web", "search", "tavily", "brave"],
    },
    ToolPackLabelRule {
        label: "file-operations",
        aliases: &["files", "filesystem"],
        keywords: &[
            "file",
            "files",
            "directory",
            "folder",
            "move",
            "copy",
            "delete",
            "rename file",
            "list",
        ],
        keep_tools: &[
            "glob",
            "list_dir",
            "read_file",
            "write",
            "bash",
            "run_terminal_command",
        ],
        keep_servers: &["filesystem", "files"],
    },
    ToolPackLabelRule {
        label: "build",
        aliases: &["compile", "packaging"],
        keywords: &[
            "build",
            "compile",
            "bundle",
            "webpack",
            "vite",
            "tsc",
            "make",
            "cargo build",
            "gradle",
        ],
        keep_tools: &[
            "bash",
            "run_terminal_command",
            "read_file",
            "grep",
            "grep_files",
        ],
        keep_servers: &["build", "ci"],
    },
    ToolPackLabelRule {
        label: "deployment",
        aliases: &["deploy", "release"],
        keywords: &[
            "deploy",
            "deployment",
            "release",
            "publish",
            "docker",
            "kubernetes",
            "k8s",
            "terraform",
            "ci/cd",
            "pipeline",
        ],
        keep_tools: &["bash", "run_terminal_command", "read_file", "write"],
        keep_servers: &[
            "docker",
            "k8s",
            "kubernetes",
            "deploy",
            "aws",
            "gcp",
            "azure",
            "fly",
            "vercel",
        ],
    },
    ToolPackLabelRule {
        label: "performance",
        aliases: &["perf", "optimization"],
        keywords: &[
            "performance",
            "optimize",
            "optimization",
            "slow",
            "latency",
            "profile",
            "benchmark",
            "memory leak",
        ],
        keep_tools: &[
            "read_file",
            "bash",
            "run_terminal_command",
            "grep",
            "grep_files",
            "lsp",
        ],
        keep_servers: &["perf", "profil"],
    },
    ToolPackLabelRule {
        label: "security",
        aliases: &["sec", "vulnerability"],
        keywords: &[
            "security",
            "vulnerability",
            "cve",
            "exploit",
            "audit",
            "permission",
            "auth",
            "authentication",
            "secret",
        ],
        keep_tools: &[
            "read_file",
            "grep",
            "grep_files",
            "bash",
            "run_terminal_command",
        ],
        keep_servers: &["security", "snyk", "audit"],
    },
    ToolPackLabelRule {
        label: "data-analysis",
        aliases: &["data", "analytics"],
        keywords: &[
            "data",
            "csv",
            "dataframe",
            "pandas",
            "analyze",
            "statistics",
            "chart",
            "plot",
        ],
        keep_tools: &["read_file", "write", "bash", "run_terminal_command", "glob"],
        keep_servers: &["data", "analytics", "notebook"],
    },
    ToolPackLabelRule {
        label: "ui-development",
        aliases: &["ui", "frontend", "ux"],
        keywords: &[
            "ui",
            "frontend",
            "component",
            "css",
            "style",
            "layout",
            "button",
            "page",
            "screen",
            "design system",
        ],
        keep_tools: &[
            "read_file",
            "edit",
            "write",
            "glob",
            "grep",
            "grep_files",
            "image_gen",
            "image_edit",
        ],
        keep_servers: &["ui", "figma", "storybook"],
    },
    ToolPackLabelRule {
        label: "media",
        aliases: &["image", "video", "audio"],
        keywords: &[
            "image",
            "video",
            "audio",
            "generate",
            "edit image",
            "thumbnail",
            "poster",
            "artwork",
        ],
        keep_tools: &[
            "image_gen",
            "image_edit",
            "read_file",
            "write",
            "bash",
            "run_terminal_command",
        ],
        keep_servers: &["media", "image", "video"],
    },
    ToolPackLabelRule {
        label: "planning",
        aliases: &["plan", "design"],
        keywords: &[
            "plan",
            "planning",
            "design",
            "architecture",
            "proposal",
            "roadmap",
            "spec",
            "specification",
        ],
        keep_tools: &[
            "read_file",
            "glob",
            "list_dir",
            "grep",
            "grep_files",
            "write",
            "todo_write",
        ],
        keep_servers: &["plan", "notion", "linear"],
    },
    ToolPackLabelRule {
        label: "memory",
        aliases: &["recall", "remember"],
        keywords: &[
            "remember",
            "recall",
            "memory",
            "what did we",
            "last time",
            "previous session",
            "notes",
        ],
        keep_tools: &["memory_search", "memory_get", "read_file", "write"],
        keep_servers: &["memory"],
    },
    ToolPackLabelRule {
        label: "scheduling",
        aliases: &["schedule", "cron", "reminder"],
        keywords: &[
            "schedule",
            "cron",
            "reminder",
            "recurring",
            "every day",
            "daily",
            "weekly",
            "at 9am",
        ],
        keep_tools: &[
            "scheduler_list",
            "scheduler_delete",
            "read_file",
            "write",
            "bash",
            "run_terminal_command",
        ],
        keep_servers: &["scheduler", "cron", "calendar"],
    },
];

/// Finds the rule for a canonical label.
pub fn find_label_rule(label: &str) -> Option<&'static ToolPackLabelRule> {
    TOOL_PACK_LABEL_TABLE.iter().find(|r| r.label == label)
}

/// Normalizes a label: trims, lowercases, resolves aliases to the canonical
/// label. Unknown labels pass through unchanged.
pub fn normalize_tool_pack_label(label: &str) -> String {
    let trimmed = label.trim().to_ascii_lowercase();
    for rule in TOOL_PACK_LABEL_TABLE {
        if rule.label == trimmed {
            return rule.label.to_string();
        }
        if rule.aliases.iter().any(|a| *a == trimmed) {
            return rule.label.to_string();
        }
    }
    trimmed
}

/// Keyword match with word boundaries for plain-word keywords; substring
/// match otherwise. `text` must already be lowercased.
fn keyword_matches(keyword: &str, text: &str) -> bool {
    if keyword.chars().all(|c| c.is_ascii_alphanumeric()) {
        let bytes = text.as_bytes();
        let kw = keyword.as_bytes();
        let mut start = 0;
        while start + kw.len() <= bytes.len() {
            if let Some(pos) = text[start..].find(keyword) {
                let s = start + pos;
                let e = s + kw.len();
                let before_ok = s == 0 || !bytes[s - 1].is_ascii_alphanumeric();
                let after_ok = e >= bytes.len() || !bytes[e].is_ascii_alphanumeric();
                if before_ok && after_ok {
                    return true;
                }
                start = s + 1;
            } else {
                break;
            }
        }
        false
    } else {
        text.contains(keyword)
    }
}

/// Resolves the active label set: route `task_labels` (normalized) plus
/// labels inferred from task-text keyword hits. Deduplicated, order-stable.
pub fn resolve_active_labels(task_labels: &[String], task_text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |label: String| {
        if seen.insert(label.clone()) {
            out.push(label);
        }
    };
    for label in task_labels {
        push(normalize_tool_pack_label(label));
    }
    let text = task_text.to_ascii_lowercase();
    for rule in TOOL_PACK_LABEL_TABLE {
        if rule
            .keywords
            .iter()
            .any(|keyword| keyword_matches(keyword, &text))
        {
            push(rule.label.to_string());
        }
    }
    out
}

/// One tool schema as seen by the shortlist computation.
#[derive(Debug, Clone)]
pub struct ToolSchemaDescriptor {
    /// Tool name as registered (e.g. `read_file`, `github__create_pr`).
    pub name: String,
    /// Human-readable description (drives token estimation).
    pub description: String,
    /// JSON-encoded input schema (drives token estimation).
    pub params_json: String,
    /// MCP server name, when the tool comes from an MCP server.
    pub server: Option<String>,
    /// True when this is an MCP tool (grok-local: `server__tool` naming).
    pub is_mcp: bool,
}

/// Splits a grok-local tool name into (is_mcp, server). Built-in tools never
/// contain `__`; MCP tools use `server__tool` naming.
pub fn split_mcp_tool_name(name: &str) -> (bool, Option<String>) {
    match name.split_once("__") {
        Some((server, _)) if !server.is_empty() => (true, Some(server.to_string())),
        _ => (false, None),
    }
}

/// Input for [`compute_tool_shortlist`].
pub struct ComputeToolShortlistInput<'a> {
    pub tools: &'a [ToolSchemaDescriptor],
    pub task_labels: &'a [String],
    pub task_text: &'a str,
    pub tier: Option<&'a str>,
    pub confidence: Option<f64>,
    /// Optional override for the pruning confidence threshold. Values
    /// outside 0.0-1.0 are ignored; defaults to
    /// [`TOOL_PACK_PRUNE_CONFIDENCE_THRESHOLD`] (0.6).
    pub confidence_threshold: Option<f64>,
    /// Config-level prune switch (`mcp_pruning`); `Some(false)` disables.
    pub prune_config_enabled: Option<bool>,
    /// When set, forces the full tool list with this reason (e.g. a
    /// pack miss earlier this turn).
    pub force_full_reason: Option<&'a str>,
    pub get_env: EnvReader<'a>,
}

/// Result of [`compute_tool_shortlist`].
#[derive(Debug, Clone)]
pub struct ToolShortlist {
    /// True when the list was actually pruned.
    pub pruned: bool,
    /// Tool names kept (sent to the model).
    pub keep_names: Vec<String>,
    /// Tool names withheld.
    pub disallowed_names: Vec<String>,
    /// Active labels that drove the decision.
    pub labels: Vec<String>,
    /// Route confidence that drove the decision.
    pub confidence: Option<f64>,
    /// Human-readable reason for logs/telemetry.
    pub reason: String,
    /// Estimated schema tokens before pruning.
    pub schema_tokens_before: u64,
    /// Estimated schema tokens after pruning.
    pub schema_tokens_after: u64,
}

fn full_tool_shortlist(
    tools: &[ToolSchemaDescriptor],
    labels: Vec<String>,
    confidence: Option<f64>,
    reason: String,
) -> ToolShortlist {
    let before = estimate_tool_schema_tokens(tools);
    ToolShortlist {
        pruned: false,
        keep_names: tools.iter().map(|t| t.name.clone()).collect(),
        disallowed_names: Vec::new(),
        labels,
        confidence,
        reason,
        schema_tokens_before: before,
        schema_tokens_after: before,
    }
}

/// Computes the tool shortlist for a turn. Fail-open: any gating failure
/// (kill switch, config off, low/missing confidence, forced full) returns
/// the full tool list.
pub fn compute_tool_shortlist(input: ComputeToolShortlistInput<'_>) -> ToolShortlist {
    let labels = resolve_active_labels(input.task_labels, input.task_text);
    let full =
        |reason: String| full_tool_shortlist(input.tools, labels.clone(), input.confidence, reason);

    if let Some(reason) = input.force_full_reason {
        return full(format!("force-full: {reason}"));
    }
    if (input.get_env)(TOOL_PACK_PRUNE_KILL_SWITCH_ENV).as_deref() == Some("0") {
        return full(format!("kill-switch {TOOL_PACK_PRUNE_KILL_SWITCH_ENV}=0"));
    }
    if input.prune_config_enabled == Some(false) {
        return full("config prune disabled".to_string());
    }
    let threshold = input
        .confidence_threshold
        .filter(|t| (0.0..=1.0).contains(t))
        .unwrap_or(TOOL_PACK_PRUNE_CONFIDENCE_THRESHOLD);
    let confidence = match input.confidence {
        Some(c) if c >= threshold => c,
        _ => {
            return full(format!(
                "low confidence or missing route (threshold {threshold})"
            ));
        }
    };

    // Keep set: core + protocol (never pruned) + label-driven tools.
    let mut keep: HashSet<String> = HashSet::new();
    for name in TOOL_PACK_CORE_TOOLS
        .iter()
        .chain(TOOL_PACK_PROTOCOL_TOOLS.iter())
    {
        keep.insert(name.to_string());
    }
    let mut kept_server_substrings: HashSet<String> = HashSet::new();
    for label in &labels {
        if let Some(rule) = find_label_rule(label) {
            for name in rule.keep_tools {
                keep.insert(name.to_string());
            }
            for server in rule.keep_servers {
                kept_server_substrings.insert(server.to_string());
            }
        }
    }
    // All known server substrings, so unknown MCP servers fail open.
    let known_server_substrings: HashSet<&str> = TOOL_PACK_LABEL_TABLE
        .iter()
        .flat_map(|rule| rule.keep_servers.iter().copied())
        .collect();

    let mut keep_names: Vec<String> = Vec::new();
    let mut disallowed_names: Vec<String> = Vec::new();
    for tool in input.tools {
        let lowered = tool.name.to_ascii_lowercase();
        let mut keep_tool = keep.iter().any(|k| *k == lowered);
        if !keep_tool && tool.is_mcp {
            keep_tool = match tool.server.as_deref() {
                Some(server) => {
                    let lowered_server = server.to_ascii_lowercase();
                    kept_server_substrings
                        .iter()
                        .any(|sub| lowered_server.contains(sub.as_str()))
                        || !known_server_substrings
                            .iter()
                            .any(|sub| lowered_server.contains(sub))
                }
                // MCP tool with no identifiable server: fail open.
                None => true,
            };
        }
        if keep_tool {
            keep_names.push(tool.name.clone());
        } else {
            disallowed_names.push(tool.name.clone());
        }
    }

    let before = estimate_tool_schema_tokens(input.tools);
    let kept: Vec<&ToolSchemaDescriptor> = input
        .tools
        .iter()
        .filter(|t| keep_names.iter().any(|k| k == &t.name))
        .collect();
    let after = kept
        .iter()
        .map(|t| estimate_schema_tokens_for(t))
        .sum::<u64>();

    ToolShortlist {
        pruned: true,
        keep_names: keep_names.clone(),
        disallowed_names,
        labels: labels.clone(),
        confidence: Some(confidence),
        reason: format!(
            "route tier={} confidence={:.2} labels=[{}] kept={}/{}",
            input.tier.unwrap_or("none"),
            confidence,
            labels.join(","),
            keep_names.len(),
            input.tools.len()
        ),
        schema_tokens_before: before,
        schema_tokens_after: after,
    }
}

/// Rough schema token estimate: (name + description + params JSON) / 4.
pub fn estimate_tool_schema_tokens(tools: &[ToolSchemaDescriptor]) -> u64 {
    tools.iter().map(estimate_schema_tokens_for).sum()
}

fn estimate_schema_tokens_for(tool: &ToolSchemaDescriptor) -> u64 {
    ((tool.name.len() + tool.description.len() + tool.params_json.len()) / 4) as u64
}

/// Detects tool-pack misses: model calls for tools that are in the
/// disallowed set but were never in the sent schema list. Returns the
/// deduplicated missed tool names in call order.
pub fn detect_tool_pack_missed_calls(
    tool_calls: &[String],
    sent_tool_names: &[String],
    disallowed_names: &[String],
) -> Vec<String> {
    if disallowed_names.is_empty() {
        return Vec::new();
    }
    let sent: HashSet<&str> = sent_tool_names.iter().map(String::as_str).collect();
    let disallowed: HashSet<&str> = disallowed_names.iter().map(String::as_str).collect();
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for call in tool_calls {
        if !sent.contains(call.as_str())
            && disallowed.contains(call.as_str())
            && seen.insert(call.as_str())
        {
            out.push(call.clone());
        }
    }
    out
}

/// Reminder body injected after a pack miss; the next step gets the full
/// tool list and a chance to retry.
pub fn build_tool_pack_recovery_reminder_body(missed: &[String]) -> String {
    format!(
        "Note: the following tools were not listed in your available tools on the previous step \
         but are available now: {}. If you still need one of them, call it again with the correct \
         input schema.",
        missed.join(", ")
    )
}

/// Input for [`compute_server_shortlist`].
pub struct ComputeServerShortlistInput<'a> {
    pub servers: &'a [String],
    pub task_labels: &'a [String],
    pub task_text: &'a str,
    pub tier: Option<&'a str>,
    pub confidence: Option<f64>,
    /// Optional override for the pruning confidence threshold. Values
    /// outside 0.0-1.0 are ignored; defaults to
    /// [`TOOL_PACK_PRUNE_CONFIDENCE_THRESHOLD`] (0.6).
    pub confidence_threshold: Option<f64>,
    pub prune_config_enabled: Option<bool>,
    pub force_full_reason: Option<&'a str>,
    pub get_env: EnvReader<'a>,
}

/// Result of [`compute_server_shortlist`].
#[derive(Debug, Clone)]
pub struct ServerShortlist {
    pub pruned: bool,
    pub keep_servers: Vec<String>,
    pub disallowed_servers: Vec<String>,
    pub labels: Vec<String>,
    pub confidence: Option<f64>,
    pub reason: String,
}

/// Per-server shortlist for MCP server suggestions. Same fail-open gating
/// as [`compute_tool_shortlist`]: a server is pruned only when it matches a
/// known-but-irrelevant server substring; unknown servers are always kept.
pub fn compute_server_shortlist(input: ComputeServerShortlistInput<'_>) -> ServerShortlist {
    let labels = resolve_active_labels(input.task_labels, input.task_text);
    let full = |reason: String| ServerShortlist {
        pruned: false,
        keep_servers: input.servers.to_vec(),
        disallowed_servers: Vec::new(),
        labels: labels.clone(),
        confidence: input.confidence,
        reason,
    };

    if let Some(reason) = input.force_full_reason {
        return full(format!("force-full: {reason}"));
    }
    if (input.get_env)(TOOL_PACK_PRUNE_KILL_SWITCH_ENV).as_deref() == Some("0") {
        return full(format!("kill-switch {TOOL_PACK_PRUNE_KILL_SWITCH_ENV}=0"));
    }
    if input.prune_config_enabled == Some(false) {
        return full("config prune disabled".to_string());
    }
    let threshold = input
        .confidence_threshold
        .filter(|t| (0.0..=1.0).contains(t))
        .unwrap_or(TOOL_PACK_PRUNE_CONFIDENCE_THRESHOLD);
    let confidence = match input.confidence {
        Some(c) if c >= threshold => c,
        _ => {
            return full(format!(
                "low confidence or missing route (threshold {threshold})"
            ));
        }
    };

    let mut kept_substrings: HashSet<&str> = HashSet::new();
    for label in &labels {
        if let Some(rule) = find_label_rule(label) {
            kept_substrings.extend(rule.keep_servers.iter().copied());
        }
    }
    let known_substrings: HashSet<&str> = TOOL_PACK_LABEL_TABLE
        .iter()
        .flat_map(|rule| rule.keep_servers.iter().copied())
        .collect();

    let mut keep_servers: Vec<String> = Vec::new();
    let mut disallowed_servers: Vec<String> = Vec::new();
    for server in input.servers {
        let lowered = server.to_ascii_lowercase();
        let is_kept = kept_substrings.iter().any(|sub| lowered.contains(sub))
            || !known_substrings.iter().any(|sub| lowered.contains(sub));
        if is_kept {
            keep_servers.push(server.clone());
        } else {
            disallowed_servers.push(server.clone());
        }
    }

    ServerShortlist {
        pruned: true,
        keep_servers: keep_servers.clone(),
        disallowed_servers,
        labels: labels.clone(),
        confidence: Some(confidence),
        reason: format!(
            "route tier={} confidence={:.2} labels=[{}] kept={}/{}",
            input.tier.unwrap_or("none"),
            confidence,
            labels.join(","),
            keep_servers.len(),
            input.servers.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolSchemaDescriptor {
        let (is_mcp, server) = split_mcp_tool_name(name);
        ToolSchemaDescriptor {
            name: name.to_string(),
            description: format!("description of {name}"),
            params_json: r#"{"type":"object"}"#.to_string(),
            server,
            is_mcp,
        }
    }

    fn input<'a>(
        tools: &'a [ToolSchemaDescriptor],
        labels: &'a [String],
        text: &'a str,
        confidence: Option<f64>,
    ) -> ComputeToolShortlistInput<'a> {
        ComputeToolShortlistInput {
            tools,
            task_labels: labels,
            task_text: text,
            tier: Some("balanced"),
            confidence,
            confidence_threshold: None,
            prune_config_enabled: Some(true),
            force_full_reason: None,
            get_env: &|_| None,
        }
    }

    fn tools_named(names: &[&str]) -> Vec<ToolSchemaDescriptor> {
        names.iter().map(|n| tool(n)).collect()
    }

    #[test]
    fn low_confidence_fails_open_to_full_list() {
        let tools = tools_named(&["read_file", "image_gen", "bash"]);
        let labels: Vec<String> = vec![];
        let shortlist = compute_tool_shortlist(input(&tools, &labels, "do anything", Some(0.2)));
        assert!(!shortlist.pruned);
        assert_eq!(shortlist.keep_names.len(), 3);
        assert!(shortlist.disallowed_names.is_empty());
    }

    #[test]
    fn missing_route_fails_open() {
        let tools = tools_named(&["read_file", "image_gen"]);
        let labels: Vec<String> = vec![];
        let shortlist = compute_tool_shortlist(input(&tools, &labels, "do anything", None));
        assert!(!shortlist.pruned);
        assert_eq!(shortlist.keep_names.len(), 2);
    }

    #[test]
    fn kill_switch_disables_pruning() {
        let tools = tools_named(&["read_file", "image_gen", "bash"]);
        let labels: Vec<String> = vec!["testing".to_string()];
        let get_env = |k: &str| (k == TOOL_PACK_PRUNE_KILL_SWITCH_ENV).then(|| "0".to_string());
        let shortlist = compute_tool_shortlist(ComputeToolShortlistInput {
            get_env: &get_env,
            ..input(&tools, &labels, "run the tests", Some(0.95))
        });
        assert!(!shortlist.pruned);
        assert_eq!(shortlist.keep_names.len(), 3);
    }

    #[test]
    fn config_off_disables_pruning() {
        let tools = tools_named(&["read_file", "image_gen"]);
        let labels: Vec<String> = vec![];
        let shortlist = compute_tool_shortlist(ComputeToolShortlistInput {
            prune_config_enabled: Some(false),
            ..input(&tools, &labels, "run the tests", Some(0.95))
        });
        assert!(!shortlist.pruned);
    }

    #[test]
    fn core_and_protocol_tools_are_never_pruned() {
        let tools = tools_named(&["read_file", "bash", "ask_user_question", "memory_search"]);
        let labels = vec!["media".to_string()];
        let shortlist = compute_tool_shortlist(input(&tools, &labels, "fix it", Some(0.9)));
        assert!(shortlist.pruned);
        for name in ["read_file", "bash", "ask_user_question", "memory_search"] {
            assert!(
                shortlist.keep_names.iter().any(|k| k == name),
                "{name} must survive pruning"
            );
        }
    }

    #[test]
    fn label_keep_tools_survive_irrelevant_ones_pruned() {
        let tools = tools_named(&["read_file", "image_gen", "bash", "lsp"]);
        let labels = vec!["media".to_string()];
        let shortlist = compute_tool_shortlist(input(&tools, &labels, "make artwork", Some(0.9)));
        assert!(shortlist.keep_names.iter().any(|k| k == "image_gen"));
        assert!(shortlist.disallowed_names.iter().any(|k| k == "lsp"));
        assert!(shortlist.schema_tokens_after < shortlist.schema_tokens_before);
        assert!(shortlist.reason.contains("labels=[media]"));
    }

    #[test]
    fn keyword_inference_adds_labels() {
        let labels = resolve_active_labels(&[], "please run the pytest suite and check coverage");
        assert!(labels.contains(&"testing".to_string()));
        let labels = resolve_active_labels(&[], "what is the capital of Ohio");
        assert!(labels.contains(&"web-search".to_string()));
        let labels = resolve_active_labels(&[], "search the web for the latest model news");
        assert!(labels.contains(&"web-search".to_string()));
    }

    #[test]
    fn route_labels_normalize_aliases() {
        let labels = resolve_active_labels(&["Review".to_string(), "DB".to_string()], "x");
        assert!(labels.contains(&"code-review".to_string()));
        assert!(labels.contains(&"database".to_string()));
    }

    #[test]
    fn unknown_mcp_server_fails_open() {
        let tools = tools_named(&["read_file", "mysterybox__do_thing"]);
        let labels = vec!["testing".to_string()];
        let shortlist = compute_tool_shortlist(input(&tools, &labels, "run tests", Some(0.9)));
        assert!(
            shortlist
                .keep_names
                .iter()
                .any(|k| k == "mysterybox__do_thing")
        );
    }

    #[test]
    fn irrelevant_mcp_server_is_pruned_but_relevant_kept() {
        let tools = tools_named(&["github__create_pr", "figma__get_design"]);
        let labels = vec!["code-review".to_string()];
        let shortlist = compute_tool_shortlist(input(&tools, &labels, "review the PR", Some(0.9)));
        assert!(
            shortlist
                .keep_names
                .iter()
                .any(|k| k == "github__create_pr")
        );
        assert!(
            shortlist
                .disallowed_names
                .iter()
                .any(|k| k == "figma__get_design")
        );
    }

    #[test]
    fn forced_full_wins_over_pruning() {
        let tools = tools_named(&["read_file", "lsp"]);
        let labels = vec!["media".to_string()];
        let shortlist = compute_tool_shortlist(ComputeToolShortlistInput {
            force_full_reason: Some("pack-miss"),
            ..input(&tools, &labels, "make artwork", Some(0.95))
        });
        assert!(!shortlist.pruned);
        assert_eq!(shortlist.keep_names.len(), 2);
        assert!(shortlist.reason.contains("force-full"));
    }

    #[test]
    fn missed_call_detection_requires_sent_and_disallowed() {
        let missed = detect_tool_pack_missed_calls(
            &[
                "lsp".to_string(),
                "read_file".to_string(),
                "lsp".to_string(),
            ],
            &["read_file".to_string()],
            &["lsp".to_string(), "image_gen".to_string()],
        );
        // lsp was called but never sent and was disallowed → miss (deduped).
        assert_eq!(missed, vec!["lsp".to_string()]);
    }

    #[test]
    fn no_miss_when_pack_was_full() {
        let missed = detect_tool_pack_missed_calls(&["lsp".to_string()], &["lsp".to_string()], &[]);
        assert!(missed.is_empty());
    }

    #[test]
    fn recovery_reminder_names_the_tools() {
        let body = build_tool_pack_recovery_reminder_body(&[
            "lsp".to_string(),
            "figma__get_design".to_string(),
        ]);
        assert!(body.contains("lsp") && body.contains("figma__get_design"));
        assert!(body.contains("call it again"));
    }

    #[test]
    fn server_shortlist_prunes_irrelevant_known_servers() {
        let servers = vec![
            "github".to_string(),
            "figma".to_string(),
            "mystery".to_string(),
        ];
        let labels = vec!["code-review".to_string()];
        let result = compute_server_shortlist(ComputeServerShortlistInput {
            servers: &servers,
            task_labels: &labels,
            task_text: "review the PR",
            tier: Some("balanced"),
            confidence: Some(0.9),
            confidence_threshold: None,
            prune_config_enabled: Some(true),
            force_full_reason: None,
            get_env: &|_| None,
        });
        assert!(result.pruned);
        assert!(result.keep_servers.contains(&"github".to_string()));
        assert!(result.disallowed_servers.contains(&"figma".to_string()));
        // Unknown servers fail open.
        assert!(result.keep_servers.contains(&"mystery".to_string()));
    }

    #[test]
    fn token_estimation_scales_with_schema_size() {
        let small = tool("a");
        let big = ToolSchemaDescriptor {
            description: "x".repeat(400),
            params_json: "y".repeat(400),
            ..tool("b")
        };
        assert!(
            estimate_tool_schema_tokens(std::slice::from_ref(&big))
                > estimate_tool_schema_tokens(std::slice::from_ref(&small))
        );
    }

    #[test]
    fn word_boundary_keyword_matching() {
        // "test" should not match inside "latest".
        assert!(!keyword_matches("test", "the latest news"));
        assert!(keyword_matches("test", "run the tests"));
        assert!(keyword_matches("cargo test", "run cargo test now"));
    }
}
