//! Regression guard: the agent-flow policy crate must never reference live
//! inference endpoints. Mirrors the ZCode-side `no-live-inference` guard:
//! no `chat/completions`, no `/v1/embeddings`, and no LM Studio probe
//! strings in the crate's source.

use std::path::Path;

/// Substrings that must never appear in the crate's source.
const FORBIDDEN: &[&str] = &[
    "chat/completions",
    "/v1/embeddings",
    "127.0.0.1:1234",
    "localhost:1234",
    "lmstudio",
    "lm_studio",
];

fn visit(dir: &Path, hits: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            visit(&path, hits);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for forbidden in FORBIDDEN {
            if text
                .to_ascii_lowercase()
                .contains(&forbidden.to_ascii_lowercase())
            {
                hits.push(format!("{} references {:?}", path.display(), forbidden));
            }
        }
    }
}

#[test]
fn agentflow_source_has_no_live_inference_references() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    visit(&src, &mut hits);
    assert!(
        hits.is_empty(),
        "agent-flow crate must not reference live inference endpoints:\n{}",
        hits.join("\n")
    );
}

#[test]
fn agentflow_source_has_no_hardcoded_model_ids() {
    // The crate resolves model choice from router decisions, the registry,
    // or explicit user config — never a compiled-in default. This scans for
    // the known model-id spellings that have leaked into defaults before.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    visit_model_ids(&src, &mut hits);
    assert!(
        hits.is_empty(),
        "agent-flow crate must not hard-code model IDs:\n{}",
        hits.join("\n")
    );
}

const MODEL_ID_MARKERS: &[&str] = &[
    "ornith-1.5-35b",
    "ornith-1.5-9b",
    "minicpm5-2b",
    "grok-4",
    "qwen",
];

fn visit_model_ids(dir: &Path, hits: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            visit_model_ids(&path, hits);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // Unit tests may use model-id *fixtures* to verify resolution logic;
        // the guard targets compiled-in defaults outside tests.
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("test"))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Unit tests may use model-id *fixtures* to verify resolution logic;
        // the guard targets compiled-in defaults. Test modules sit at the
        // end of each file, so everything from the first #[cfg(test)] on is
        // skipped.
        let code = text.split("#[cfg(test)]").next().unwrap_or("");
        for line in code.lines() {
            for marker in MODEL_ID_MARKERS {
                if line.to_ascii_lowercase().contains(marker) {
                    hits.push(format!("{} references {:?}", path.display(), marker));
                    break;
                }
            }
        }
    }
}
