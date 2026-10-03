use std::sync::Arc;

use agent_client_protocol as acp;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::actions::ActionRegistry;
use crate::app::agent_view::AgentPane;
use crate::app::agent_view::test_fixtures::make_agent;
use crate::views::modal::ActiveModal;

/// Ctrl+M, then Enter on a reasoning model: the effort sub-menu opens on
/// the "(model only)" default row, with the effort rows after it.
#[test]
fn arg_picker_effort_phase_opens_on_default_row() {
    let mut agent = make_agent();
    let id = acp::ModelId::new(Arc::from("reasoning-x"));
    agent.session.models.available.insert(
        id.clone(),
        acp::ModelInfo::new(id, "Reasoning X").meta(
            serde_json::json!({ "supportsReasoningEffort": true, "reasoningEffort": "high" })
                .as_object()
                .cloned(),
        ),
    );
    // Ctrl+M is the multiline toggle while the prompt is focused; the picker binding lives on the agent screen
    agent.set_active_pane(AgentPane::Scrollback, true);

    let registry = ActionRegistry::defaults();
    agent.handle_input(
        &Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)),
        &registry,
    );
    assert!(
        matches!(
            agent.active_modal.as_ref(),
            Some(ActiveModal::ArgPicker { command, args_query, .. })
                if command == "model" && args_query.is_empty()
        ),
        "Ctrl+M must open the /model picker in the model phase"
    );

    // Row 0 is always "Auto (SystemOne)"; step down to the reasoning model.
    agent.handle_input(
        &Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &registry,
    );
    agent.handle_input(
        &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &registry,
    );
    let Some(ActiveModal::ArgPicker {
        args_query,
        items,
        state,
        ..
    }) = agent.active_modal.as_ref()
    else {
        panic!("expected the /model picker to chain into the effort phase");
    };
    assert_eq!("Reasoning X ", args_query);
    assert_eq!(0, state.selected);
    assert_eq!(
        Some("Reasoning X"),
        items.first().map(|item| item.insert_text.as_str())
    );
    // Effort rows follow strongest-first: xhigh, high, medium, low.
    assert_eq!(
        Some("Reasoning X high"),
        items.get(2).map(|item| item.insert_text.as_str())
    );
}

/// Ctrl+M, then Enter twice on a multi-window reasoning model: model, then window, then the effort sub-menu.
#[test]
fn arg_picker_chains_through_the_window_phase_to_effort() {
    let mut agent = make_agent();
    let id = acp::ModelId::new(Arc::from("grok-4.7"));
    agent.session.models.available.insert(
        id.clone(),
        acp::ModelInfo::new(id, "Grok 4.7").meta(
            serde_json::json!({
                "supportsReasoningEffort": true,
                "totalContextTokens": 256_000,
                "contextWindows": [256_000, 500_000],
            })
            .as_object()
            .cloned(),
        ),
    );
    agent.set_active_pane(AgentPane::Scrollback, true);
    let registry = ActionRegistry::defaults();
    let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    agent.handle_input(
        &Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)),
        &registry,
    );
    // Row 0 is always "Auto (SystemOne)"; step down to the reasoning model.
    agent.handle_input(
        &Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &registry,
    );
    agent.handle_input(&enter, &registry);
    agent.handle_input(&enter, &registry);

    let Some(ActiveModal::ArgPicker {
        args_query, items, ..
    }) = agent.active_modal.as_ref()
    else {
        panic!("expected the /model picker to chain through the window phase");
    };
    assert_eq!("Grok 4.7 256k ", args_query);
    // Fork: the leading "(model only)" row pins model+window and keeps
    // thinking untouched; every other row continues the chain.
    let [model_only, rest @ ..] = items.as_slice() else {
        panic!("expected model-only + effort rows: {items:?}");
    };
    assert_eq!(model_only.insert_text, "Grok 4.7 256k");
    assert!(
        rest.iter()
            .all(|item| item.insert_text.starts_with("Grok 4.7 256k ")),
        "got {items:?}"
    );
}
