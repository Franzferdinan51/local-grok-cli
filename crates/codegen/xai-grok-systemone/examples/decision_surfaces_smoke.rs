//! End-to-end smoke test for the Phase 3 decision surfaces.
//!
//! Run against a SystemOne shim with the new decision surface (e.g. a repo
//! shim on an alternate port — NEVER the live production shim):
//!
//! ```sh
//! cargo run -p xai-grok-systemone --example decision_surfaces_smoke \
//!     -- http://127.0.0.1:18765
//! ```
//!
//! What it verifies:
//! 1. A clear task against the live shim returns `ranked_tools`
//!    (`tool_scoring == "full"`) and they flow into MCP/server suggestions.
//! 2. `ranked_models` surfaces as an advisory diagnostic
//!    (`model_ranking=<top>`) without any model switch.
//! 3. `POST /v1/systemone/rank-plans` returns a scored ranking (or fails
//!    open on older shims).
//! 4. Against a mock shim returning `uncertain == true`: pruning is disabled
//!    (`prune=disabled(uncertain)` in the evidence line) even with pruning
//!    opted in, high confidence, and a cheap tier.
//!
//! Read-only: it only POSTs to the shim. It never starts, stops, or modifies
//! any shim, and never touches `lms`.

use std::io::{Read, Write};
use std::net::TcpListener;

use xai_grok_systemone::{
    PlanInput, RouteDecision, RouterStatus, SystemOneConfig, rank_plans, route_for_task,
    suggest_and_maybe_prune, suggestion_detail,
};

/// Minimal one-shot mock shim: serves one canned `/v1/systemone/route`
/// response, then exits. Returns the base URL (without the route path).
fn spawn_mock_route_server(canned: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock shim");
    let port = listener.local_addr().expect("mock addr").port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("mock accept");
        // Read the request: headers until \r\n\r\n, then Content-Length bytes.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let body_len: usize = loop {
            let n = stream.read(&mut chunk).expect("mock read");
            if n == 0 {
                break 0;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(hdr_end) = find_subslice(&buf, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..hdr_end]);
                let len = headers
                    .lines()
                    .find_map(|l| {
                        let l = l.trim().to_ascii_lowercase();
                        l.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buf.len() >= hdr_end + 4 + len {
                    break len;
                }
            }
        };
        let _ = body_len;
        let body = canned.as_bytes();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(resp.as_bytes()).expect("mock write head");
        stream.write_all(body).expect("mock write body");
        let _ = stream.flush();
    });
    format!("http://127.0.0.1:{port}")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn check(cond: bool, msg: &str) {
    if cond {
        println!("  PASS: {msg}");
    } else {
        eprintln!("  FAIL: {msg}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:18765".to_string());
    let route_url = format!("{base}/v1/systemone/route");

    // ---- 1. Live shim: clear task, ranked tools flow into suggestions ----
    println!("== live shim: {route_url}");
    let cfg = SystemOneConfig {
        urls: vec![route_url.clone()],
        prune_mcp_servers: true, // opt in so we can see the gates engage
        ..SystemOneConfig::default()
    };
    let task = "research the latest local LLM releases, compare their benchmarks, and write up which open-weight model is best for coding agents";
    let mut decision: RouteDecision = route_for_task(task, "prompt", &cfg).await;
    println!("  {}", decision.evidence_line(RouterStatus::AlreadyRunning));
    if decision.has_shim_tool_ranking() {
        check(true, "shim returned tool_scoring=full with ranked_tools");
        for t in decision.ranked_tools.iter().take(5) {
            println!(
                "    ranked_tool: id={} kind={:?} relevance={:.3}",
                t.id, t.kind, t.relevance
            );
        }
    } else {
        println!("  NOTE: shim has no tool ranking (older shim?) — old behavior expected");
    }
    if let Some(top) = decision.best_value_model() {
        check(
            true,
            &format!("ranked_models advisory present: best value = {top}"),
        );
    } else {
        println!("  NOTE: shim sent no ranked_models");
    }
    let (suggestions, allow) = suggest_and_maybe_prune(task, &mut decision, &cfg);
    println!("  suggestions: {}", suggestion_detail(&suggestions));
    println!("  prune allowlist: {allow:?}");
    if decision.has_shim_tool_ranking() {
        // The shim's ranking (not the keyword scorer) must drive suggestions.
        check(
            !suggestions.is_empty()
                && suggestions
                    .iter()
                    .all(|s| s.matched == vec!["shim-ranked".to_string()]),
            "suggestions come from the shim-ranked tools",
        );
        check(
            decision.suggested_mcp_servers
                == suggestions
                    .iter()
                    .map(|s| s.name.clone())
                    .collect::<Vec<_>>(),
            "suggested_mcp_servers reflects the shim ranking",
        );
        // Pruning is opt-in here and the tier is balanced (not cheap), so the
        // classic gates keep the full toolset — verify no accidental prune.
        check(
            allow.is_none(),
            "balanced tier: no pruning (cheap-tier gate holds)",
        );
    }
    println!("  {}", decision.evidence_line(RouterStatus::AlreadyRunning));

    // ---- 2. rank-plans against the live shim ----
    println!("== rank-plans: {base}/v1/systemone/rank-plans");
    let plans = vec![
        PlanInput {
            id: "direct".into(),
            text: "Answer the question directly from the task text in one pass.".into(),
        },
        PlanInput {
            id: "research".into(),
            text:
                "First search the web for background, then synthesize an answer from the results."
                    .into(),
        },
    ];
    let ranking = rank_plans(task, &plans, &cfg).await;
    for r in &ranking {
        println!(
            "    plan {}: score={:?} p_success={:?}",
            r.id, r.score, r.p_success
        );
    }
    if ranking.iter().any(|r| r.score.is_some()) {
        check(true, "rank-plans returned scored ranking");
    } else {
        println!("  NOTE: rank-plans failed open (older shim or disabled) — input order preserved");
        check(
            ranking.iter().map(|r| r.id.as_str()).collect::<Vec<_>>() == vec!["direct", "research"],
            "fail-open preserves input order",
        );
    }

    // ---- 3. Mock shim: uncertain == true disables pruning ----
    println!("== mock shim: uncertain route");
    let canned = r#"{
        "route": {
            "tier": "economy",
            "effort": "low",
            "confidence": 0.95,
            "rationale": "mock uncertain route",
            "uncertain": true,
            "margin": 0.05,
            "tool_scoring": "skipped",
            "ranked_tools": [],
            "ranked_models": [{"model_id": "mock-cheap", "tier": "economy", "utility": 0.8}]
        },
        "model": "mock-shim"
    }"#;
    let mock_base = spawn_mock_route_server(canned);
    let mock_cfg = SystemOneConfig {
        urls: vec![format!("{mock_base}/v1/systemone/route")],
        prune_mcp_servers: true,
        ..SystemOneConfig::default()
    };
    let mut mock_decision: RouteDecision =
        route_for_task("ambiguous task", "prompt", &mock_cfg).await;
    check(
        mock_decision.uncertain == Some(true),
        "uncertain=true parsed from mock /route",
    );
    check(
        mock_decision.tool_scoring.as_deref() == Some("skipped"),
        "tool_scoring=skipped parsed",
    );
    let (mock_suggestions, mock_allow) =
        suggest_and_maybe_prune("ambiguous task", &mut mock_decision, &mock_cfg);
    let _ = mock_suggestions;
    check(
        mock_allow.is_none(),
        "uncertain route: pruning disabled (allowlist None) despite opt-in + high confidence + cheap tier",
    );
    check(
        mock_decision.prune_note.as_deref() == Some("disabled(uncertain)"),
        "prune diagnostic note recorded",
    );
    let line = mock_decision.evidence_line(RouterStatus::AlreadyRunning);
    println!("  {line}");
    check(
        line.contains("uncertain=true"),
        "evidence line shows uncertain=true",
    );
    check(
        line.contains("prune=disabled(uncertain)"),
        "evidence line shows prune=disabled(uncertain)",
    );
    check(
        line.contains("model_ranking=mock-cheap"),
        "evidence line shows advisory model_ranking (no switch)",
    );

    println!("\nSMOKE OK: decision surfaces wired end-to-end, advisory-only, fail-open.");
}
