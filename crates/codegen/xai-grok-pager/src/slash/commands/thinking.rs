//! `/thinking`: choose the SystemOne thinking level.
//!
//! Seven choices, independent of model selection:
//! `off|low|medium|high|xhigh|ultra` pins the reasoning effort (the router
//! stands down on effort); `auto` lets the router pick the effort per task —
//! it may reach `xhigh`/`ultra` for heavy work. The choice persists in the
//! `[systemone]` config section and takes effect on the next prompt.
//!
//! This is the new spelling of `/effort`; `/effort <level>` is equivalent to
//! `/thinking <level>` for non-auto levels.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use xai_grok_systemone::{Effort, SystemOneConfig, ThinkingMode};

/// Set the SystemOne thinking level.
pub struct ThinkingCommand;

/// All seven thinking choices in display order.
pub const THINKING_CHOICES: &[&str] = &["off", "low", "medium", "high", "xhigh", "ultra", "auto"];

/// One-line description for each choice, shown in the picker.
fn choice_description(choice: &str) -> &'static str {
    match choice {
        "off" => "Minimal reasoning; fastest answers",
        "low" => "Light reasoning for simple tasks",
        "medium" => "Balanced reasoning (default router pick for easy tasks)",
        "high" => "Deep reasoning for complex work",
        "xhigh" => "Very deep reasoning; long tasks only",
        "ultra" => "Maximum reasoning effort",
        "auto" => "Let SystemOne pick per task (may use xhigh/ultra)",
        _ => "",
    }
}

impl ThinkingCommand {
    /// Suggest items for the picker, marking the currently active mode.
    fn build_arg_items(current: &str) -> Vec<ArgItem> {
        THINKING_CHOICES
            .iter()
            .map(|choice| {
                let active = *choice == current;
                ArgItem {
                    display: if active {
                        format!("{choice} (current)")
                    } else {
                        choice.to_string()
                    },
                    match_text: choice.to_string(),
                    insert_text: choice.to_string(),
                    description: choice_description(choice).to_string(),
                }
            })
            .collect()
    }
}

impl SlashCommand for ThinkingCommand {
    slash_meta! {
        name: "thinking",
        description: "Set the SystemOne thinking level (off|low|medium|high|xhigh|ultra|auto)",
        usage: "/thinking <off|low|medium|high|xhigh|ultra|auto>",
        takes_args: true,
        args_required: true,
        session_scoped: true,
        arg_placeholder: "<level>",
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        let current = SystemOneConfig::load().thinking.as_str().to_string();
        Some(Self::build_arg_items(&current))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        let cfg = SystemOneConfig::load();
        let current = cfg.thinking.as_str();

        if trimmed.is_empty() {
            return CommandResult::Error(format!(
                "Usage: /thinking <{}> (current: {current})",
                THINKING_CHOICES.join("|"),
            ));
        }

        let Some(mode) = ThinkingMode::parse(trimmed) else {
            return CommandResult::Error(format!(
                "Unknown thinking level '{trimmed}'. Choose one of: {} (current: {current})",
                THINKING_CHOICES.join(", "),
            ));
        };

        // Persist first: the config file is the source of truth and the
        // session re-reads it every turn. Fail-open: the live change below
        // still applies even if the write fails.
        if !SystemOneConfig::save_thinking(mode) {
            tracing::warn!("systemone: /thinking could not persist {trimmed}; applying live only");
        }

        match mode {
            ThinkingMode::Fixed(effort) => {
                let Some(model_id) = ctx.models.current.clone() else {
                    return CommandResult::Error("No active model".into());
                };
                // Same live wire path as `/effort`: the session marks the
                // effort as thinking-owned so Auto routing may update it later.
                CommandResult::Action(Action::SwitchModel {
                    model_id,
                    effort: Some(effort.reasoning_effort()),
                })
            }
            ThinkingMode::Auto => CommandResult::Message(format!(
                "Thinking: auto — SystemOne picks the reasoning level per task \
                 (may use xhigh/ultra when warranted). Model selection stays {}.",
                cfg.model_selection.as_str(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seven_choices_in_order() {
        assert_eq!(
            THINKING_CHOICES,
            &["off", "low", "medium", "high", "xhigh", "ultra", "auto"]
        );
    }

    #[test]
    fn picker_marks_current_mode() {
        let items = ThinkingCommand::build_arg_items("auto");
        assert_eq!(items.len(), 7);
        let auto = items.iter().find(|i| i.match_text == "auto").unwrap();
        assert!(auto.display.contains("(current)"));
        assert_eq!(auto.insert_text, "auto");
        let ultra = items.iter().find(|i| i.match_text == "ultra").unwrap();
        assert!(!ultra.display.contains("(current)"));
        assert!(!ultra.description.is_empty());
    }

    #[test]
    fn invalid_choice_is_rejected() {
        assert!(ThinkingMode::parse("ludicrous").is_none());
        assert!(ThinkingMode::parse("AUTO").is_some());
        assert_eq!(
            ThinkingMode::parse("xhigh"),
            Some(ThinkingMode::Fixed(Effort::XHigh))
        );
    }
}
