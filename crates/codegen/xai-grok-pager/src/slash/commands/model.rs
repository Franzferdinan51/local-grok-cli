//! `/model` (alias `/m`): switch the model and optionally its reasoning effort.
//! Chained autocomplete: after picking a reasoning-supported model, the trailing space re-opens the dropdown.
//! Its first row, "(model only)", pins just the model and leaves thinking on its current `/thinking` setting;
//! the `low|medium|high|xhigh` rows are the explicit combined spelling (`/model <name> <effort>`) that pins both selectors.
//! `/model auto` selects automatic model selection: SystemOne picks per task (advisory only — the loaded model is never switched or unloaded).

use agent_client_protocol as acp;
use xai_grok_shell::sampling::types::{ReasoningEffortOption, supports_reasoning_effort_meta};

use crate::acp::model_state::ModelState;
use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use crate::slash::commands::effort_levels::build_effort_arg_items;
use xai_grok_systemone::{ModelSelection, SystemOneConfig, ThinkingMode};

/// Switch the active model (and optionally its reasoning effort).
pub struct ModelCommand;

impl SlashCommand for ModelCommand {
    slash_meta! {
        name: "model",
        aliases: ["m"],
        description: "Switch the active model, or /model auto for SystemOne-picked models",
        usage: "/model <name|auto> [effort]",
        takes_args: true,
        args_required: true,
        session_scoped: true,
        // The dashboard offers `/model` to pick the model for the next spawned agent (intercepted in `dispatch_dashboard_dispatch_slash`).
        offered_when_session_less: true,
        arg_placeholder: "<model|auto> [effort]",
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        if ctx.models.is_empty() {
            return None;
        }

        // Effort phase if input is "<reasoning-model> ", else model phase.
        if let Some(model_id) = detect_effort_phase(ctx.models, args_query) {
            return Some(build_effort_items(ctx.models, &model_id));
        }
        Some(build_model_items(ctx.models))
    }

    fn preselected_arg(&self, ctx: &AppCtx, args_query: &str) -> Option<String> {
        let model_id = detect_effort_phase(ctx.models, args_query)?;
        let model_name = ctx.models.display_name_for(&model_id);
        // A typed effort filter hands the opening row to the match ranking
        if !args_query.trim_end().eq_ignore_ascii_case(&model_name) {
            return None;
        }
        // Fresh effort menu: default to the "(model only)" row — picking a
        // model pins just the model. The effort rows stay available as the
        // explicit combined spelling (`/model <name> <effort>`).
        Some(model_name.into())
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /model <name|auto> [effort]".into());
        }

        // `/model auto`: automatic model selection. Persisted; no switch is
        // dispatched — the router's pick is advisory only and the loaded
        // model is never unloaded or swapped out from under the user.
        if trimmed.eq_ignore_ascii_case("auto") {
            if !SystemOneConfig::save_model_selection(ModelSelection::Auto) {
                tracing::warn!("systemone: /model auto could not persist model selection");
            }
            return CommandResult::Message(
                "Model selection: auto — SystemOne picks per task (advisory only; \
                 your loaded model stays put). Thinking selection is independent: \
                 use /thinking to pin or release the reasoning level."
                    .to_string(),
            );
        }

        // A named model means a pinned model: persist the selection so the
        // next session starts pinned too. Fail-open on the write.
        let persist_pinned = || {
            if !SystemOneConfig::save_model_selection(ModelSelection::Pinned) {
                tracing::warn!("systemone: /model could not persist pinned model selection");
            }
        };

        // Prefer an exact full-string catalog match first. Model display names often contain spaces ("Grok 4.5").
        // If we split on the last token first, a shorter catalog entry ("Grok") would steal the prefix and treat "4.5" as an effort level
        if let Some(id) = ctx.models.resolve_by_name_or_id(trimmed) {
            persist_pinned();
            return CommandResult::Action(Action::SetDefaultModel(id));
        }

        // A trailing effort token on a reasoning model makes a session-scoped switch (not persisted as default)
        // Resolve via the shared gate so a rejected level (e.g. `none` on grok-4.5) reports the effort error with the model's offered ids.
        // Without it the fall-through reports "Unknown model: … none"
        if let Some((prefix, token)) = split_trailing_token(trimmed)
            && let Some(id) = resolve_model(ctx.models, prefix)
            && ctx
                .models
                .available
                .get(&id)
                .map(supports_reasoning_effort)
                .unwrap_or(false)
        {
            return match ctx.models.resolve_effort_for_model(&id, token) {
                Ok(effort) => {
                    // Named model + effort: pin the model AND pin the thinking
                    // level (`/model <name> <effort>` is the combined spelling
                    // of pinning both selectors).
                    persist_pinned();
                    let mode =
                        ThinkingMode::Fixed(xai_grok_systemone::Effort::from_reasoning(effort));
                    if !SystemOneConfig::save_thinking(mode) {
                        tracing::warn!(
                            "systemone: /model could not persist thinking level; live switch still applied"
                        );
                    }
                    CommandResult::Action(Action::SwitchModel {
                        model_id: id,
                        effort: Some(effort),
                    })
                }
                Err(err) => CommandResult::Error(err.message()),
            };
        }

        CommandResult::Error(format!("Unknown model: {trimmed}"))
    }
}

/// Look up a model by case-insensitive display name OR model id match.
fn resolve_model(models: &ModelState, name: &str) -> Option<acp::ModelId> {
    models.resolve_by_name_or_id(name)
}

fn supports_reasoning_effort(info: &acp::ModelInfo) -> bool {
    supports_reasoning_effort_meta(info.meta.as_ref())
}

/// Split `args` into `(prefix, last_token)` on the final whitespace run.
/// Returns `None` when there is no interior whitespace to split on.
/// The token is resolved to an effort against the picked model's options by the caller.
fn split_trailing_token(args: &str) -> Option<(&str, &str)> {
    let (prefix, last) = args.rsplit_once(char::is_whitespace)?;
    let prefix = prefix.trim_end();
    if prefix.is_empty() || last.is_empty() {
        return None;
    }
    Some((prefix, last))
}

/// Returns the matched model id when `args_query` is `"<reasoning-model> ..."`.
/// Candidates are tried longest name first to disambiguate names that share a prefix.
fn detect_effort_phase(models: &ModelState, args_query: &str) -> Option<acp::ModelId> {
    let mut candidates: Vec<(&acp::ModelId, &str)> = models
        .available
        .iter()
        .filter(|(_, info)| supports_reasoning_effort(info))
        .map(|(id, info)| (id, info.name.as_str()))
        .collect();
    candidates.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));

    for (id, name) in candidates {
        if args_query
            .get(..name.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
            && args_query
                .get(name.len()..)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        {
            return Some(id.clone());
        }
    }
    None
}

/// One row per logical model.
/// Reasoning models get a trailing space in `insert_text` so the prompt widget chains into the effort sub-menu.
/// The first row is always `Auto`: SystemOne picks the model per task (advisory only — nothing is switched or unloaded).
fn build_model_items(models: &ModelState) -> Vec<ArgItem> {
    let current_id = models.current.as_ref();
    let auto_active = SystemOneConfig::load().model_selection == ModelSelection::Auto;
    let mut items: Vec<ArgItem> = Vec::with_capacity(models.available.len() + 1);
    items.push(ArgItem {
        display: if auto_active {
            "Auto (SystemOne) (current)".to_string()
        } else {
            "Auto (SystemOne)".to_string()
        },
        match_text: "auto".to_string(),
        insert_text: "auto".to_string(),
        description: "SystemOne picks the model per task (advisory; your loaded model stays put)"
            .to_string(),
    });
    for (id, info) in &models.available {
        let is_current = current_id == Some(id);
        let supports = supports_reasoning_effort(info);

        let display = if is_current {
            format!("{} (current)", info.name)
        } else {
            info.name.clone()
        };

        // A trailing space on reasoning models signals "more input expected" to the prompt widget
        // Enter then advances to the effort phase instead of submitting
        let insert_text = if supports {
            format!("{} ", info.name)
        } else {
            info.name.clone()
        };

        items.push(ArgItem {
            display,
            match_text: info.name.clone(),
            insert_text,
            description: info.description.clone().unwrap_or_default(),
        });
    }
    items
}

/// Rows for the `/model` chained effort phase: a leading "(model only)" row
/// plus one row per effort level. The model-only row is the default (row 0,
/// sort key `'!'` beats the effort rows' `'a'`/`'b'`/…): accepting it pins
/// just the model and leaves thinking on its own setting, so picking a
/// reasoning model never silently pins thinking. The effort rows keep
/// `insert_text` of `"ModelName high"` — the explicit combined spelling
/// (`/model <name> <effort>`) that pins both selectors at once.
fn build_effort_items(models: &ModelState, model_id: &acp::ModelId) -> Vec<ArgItem> {
    let info = match models.available.get(model_id) {
        Some(info) => info,
        None => return Vec::new(),
    };
    let is_current_model = models.current.as_ref() == Some(model_id);
    let options = models.reasoning_effort_options_for(model_id);
    let mut items = Vec::with_capacity(options.len() + 1);
    items.push(ArgItem {
        display: "(model only)".to_string(),
        match_text: format!("! {}", info.name),
        insert_text: info.name.clone(),
        description: "Pin just this model; thinking stays on its current /thinking setting"
            .to_string(),
    });
    items.extend(build_effort_arg_items(
        &options,
        models.reasoning_effort,
        is_current_model,
        |option| effort_insert_text(&info.name, option),
    ));
    items
}

fn effort_insert_text(model_name: &str, option: &ReasoningEffortOption) -> String {
    format!("{model_name} {}", option.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use xai_grok_shell::sampling::types::ReasoningEffort;

    fn model_with_reasoning(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        let info = acp::ModelInfo::new(id.clone(), name.to_string())
            .meta(serde_json::Value::Object(meta).as_object().cloned());
        (id, info)
    }

    fn plain_model(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let info = acp::ModelInfo::new(id.clone(), name.to_string());
        (id, info)
    }

    static EMPTY_BUNDLE: crate::app::bundle::BundleState = crate::app::bundle::BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn dummy_exec_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn split_trailing_token_splits_on_final_whitespace() {
        assert_eq!(
            split_trailing_token("Reasoning X high"),
            Some(("Reasoning X", "high"))
        );
        assert_eq!(
            split_trailing_token("reasoning-x  xhigh"),
            Some(("reasoning-x", "xhigh"))
        );
        // No interior whitespace, so nothing to split off
        assert!(split_trailing_token("reasoning-x-pro").is_none());
    }

    #[test]
    fn empty_query_returns_one_row_per_logical_model() {
        let mut state = ModelState::default();
        let (rid, rinfo) = model_with_reasoning("reasoning-x", "Reasoning X");
        let (pid, pinfo) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(rid, rinfo);
        state.available.insert(pid, pinfo);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        let items = cmd.suggest_args(&ctx, "").unwrap();
        // One row per logical model, plus the leading "Auto (SystemOne)" row
        assert_eq!(
            items.len(),
            3,
            "model phase: one row per logical model + Auto"
        );

        // A reasoning model has a trailing space in insert_text
        // The prompt widget reads it to keep the dropdown open after Enter so the effort sub-menu can render
        let reasoning = items
            .iter()
            .find(|i| i.match_text == "Reasoning X")
            .unwrap();
        assert_eq!(reasoning.insert_text, "Reasoning X ");

        // A plain model has no trailing space, so Enter commits immediately
        let plain = items.iter().find(|i| i.match_text == "Grok 4.5").unwrap();
        assert_eq!(plain.insert_text, "Grok 4.5");
    }

    #[test]
    fn trailing_space_after_reasoning_model_enters_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        // The args query has a trailing space, so this is the effort phase.
        // Row 0 is "(model only)" (pins just the model, thinking untouched);
        // then the effort rows ordered xhigh to low (strongest first) per
        // EFFORT_LEVELS.
        let items = cmd.suggest_args(&ctx, "Reasoning X ").unwrap();
        assert_eq!(items.len(), 5);
        let [model_only, a, b, c, d] = items.as_slice() else {
            panic!("expected 5 items: {items:?}");
        };
        assert_eq!(model_only.display, "(model only)");
        assert_eq!(model_only.insert_text, "Reasoning X");
        assert_eq!(a.insert_text, "Reasoning X xhigh");
        assert_eq!(b.insert_text, "Reasoning X high");
        assert_eq!(c.insert_text, "Reasoning X medium");
        assert_eq!(d.insert_text, "Reasoning X low");
        // Display is just the level so the user sees a clean column.
        assert_eq!(a.display, "xhigh");
        // match_text carries the sort-key prefix that forces the matcher's alphabetical tiebreak to render rows in EFFORT_LEVELS order
        assert!(a.match_text.starts_with("a "));
        assert!(d.match_text.starts_with("d "));
    }

    #[test]
    fn preselected_arg_targets_default_row_only_for_fresh_effort_menu() {
        let mut state = ModelState::default();
        let id = acp::ModelId::new(Arc::from("reasoning-x"));
        let info = acp::ModelInfo::new(id.clone(), "Reasoning X").meta(
            serde_json::json!({ "supportsReasoningEffort": true, "reasoningEffort": "high" })
                .as_object()
                .cloned(),
        );
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        // The preselection must name a row `suggest_args` actually builds, or the consumers fall back to row 0.
        // The fresh menu defaults to the "(model only)" row so picking a
        // model pins just the model — thinking stays independent.
        let model_only_row = cmd
            .suggest_args(&ctx, "Reasoning X ")
            .and_then(|items| items.first().map(|item| item.insert_text.clone()));
        assert_eq!(Some("Reasoning X".to_owned()), model_only_row);
        assert_eq!(model_only_row, cmd.preselected_arg(&ctx, "Reasoning X "));
        assert_eq!(None, cmd.preselected_arg(&ctx, "Reasoning X h"));
        assert_eq!(None, cmd.preselected_arg(&ctx, ""));
    }

    #[test]
    fn partial_effort_query_still_in_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        // Still in effort phase; the matcher upstream narrows to the model-only row plus high and xhigh
        let items = cmd.suggest_args(&ctx, "Reasoning X h").unwrap();
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn partial_model_query_stays_in_model_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        // No trailing space: the user is still typing the model name.
        // suggest_args returns every model-phase row (the matcher upstream
        // narrows); the Auto row is always present.
        let items = cmd.suggest_args(&ctx, "Reason").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items
                .iter()
                .find(|i| i.match_text == "Reasoning X")
                .map(|item| item.insert_text.as_str()),
            Some("Reasoning X ")
        );
    }

    #[test]
    fn run_parses_model_plus_effort_when_supported() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X xhigh");
        match result {
            CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
                assert_eq!(model_id.0.as_ref(), "reasoning-x");
                assert_eq!(effort, Some(ReasoningEffort::Xhigh));
            }
            other => panic!("expected SwitchModel with effort, got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_unoffered_effort_with_effort_error_not_unknown_model() {
        // Regression: previously `resolve_effort_token_for` returned None and the handler fell through to `Unknown model: Reasoning X none`
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X none");
        match result {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("unknown effort level 'none'"),
                    "expected effort error, got {msg}"
                );
                assert!(
                    msg.contains("use one of:"),
                    "expected offered levels in message, got {msg}"
                );
                assert!(
                    !msg.to_lowercase().contains("unknown model"),
                    "must not misreport as unknown model: {msg}"
                );
                let offered = msg.split_once("; ").map(|(_, r)| r).unwrap_or("");
                assert!(
                    !offered.contains("none"),
                    "must not list none as offered: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn run_prefers_full_multi_word_model_name_over_prefix_plus_effort() {
        // The catalog has both "Grok" (reasoning) and "Grok 4.5"
        // `/model Grok 4.5` must select the full name, not treat "4.5" as an effort on "Grok"
        let mut state = ModelState::default();
        let (short_id, short_info) = model_with_reasoning("grok", "Grok");
        let (long_id, long_info) = model_with_reasoning("grok-4.5", "Grok 4.5");
        state.available.insert(short_id, short_info);
        state.available.insert(long_id.clone(), long_info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, long_id);
            }
            other => panic!("expected SetDefaultModel(Grok 4.5), got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_effort_for_non_reasoning_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5 high");
        // Falls through to "is the whole string a model name?", which it isn't, so we get an Unknown error
        assert!(matches!(result, CommandResult::Error(_)));
    }

    /// The bare `/model <name>` form dispatches `Action::SetDefaultModel(<ModelId>)` instead of the legacy `Action::SwitchModel { effort: None }`.
    /// The dispatcher routes it through both `Effect::SwitchModel` (session mutation) and `Effect::PersistSetting` (next-session default).
    /// The payload is the typed `acp::ModelId` (resolved at the slash boundary), not a String.
    #[test]
    fn run_bare_model_name_dispatches_set_default_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }

    /// `/model auto` must never dispatch a model switch: it only persists the
    /// automatic model selection. The router's per-task pick is advisory only —
    /// the loaded model is never switched or unloaded.
    /// The test restores the persisted selection so the machine is left as found.
    #[test]
    fn run_auto_dispatches_no_model_switch() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let previous = SystemOneConfig::load().model_selection;
        for args in ["auto", "AUTO", " Auto "] {
            let result = ModelCommand.run(&mut ctx, args);
            match result {
                CommandResult::Message(msg) => {
                    assert!(
                        msg.contains("advisory only"),
                        "`/model {args}` message should state advisory-only, got: {msg}"
                    );
                }
                other => panic!("`/model {args}` must not dispatch a switch, got {other:?}"),
            }
        }
        // Leave the persisted selection exactly as this test found it.
        let _ = SystemOneConfig::save_model_selection(previous);
    }

    /// The "(model only)" picker row commits a bare model name: the model is
    /// pinned via `SetDefaultModel` with no effort attached, so the thinking
    /// selector keeps its current `/thinking` setting.
    #[test]
    fn run_model_only_row_pins_model_without_effort() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }

    /// Case-insensitive matching against the catalog: `/model grok 4.5` resolves to the same `ModelId` as `/model Grok 4.5`.
    #[test]
    fn run_set_default_model_resolves_case_insensitively() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }
}
