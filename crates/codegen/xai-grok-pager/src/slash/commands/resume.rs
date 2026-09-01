//! `/resume`: open the session picker, or load a session by id/title.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct ResumeCommand;

impl SlashCommand for ResumeCommand {
    slash_meta! {
        name: "resume",
        description: "Resume a previous session",
        usage: "/resume [id-or-title]",
        takes_args: true,
        args_required: false,
        offered_when_session_less: true,
        arg_placeholder: "[id-or-title]",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        CommandResult::Action(resume_action(args))
    }
}

fn resume_action(args: &str) -> Action {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        Action::ShowSessionPicker
    } else {
        Action::LoadSession(trimmed.to_string(), None, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_resume_opens_picker() {
        assert!(matches!(resume_action(""), Action::ShowSessionPicker));
        assert!(matches!(resume_action("   "), Action::ShowSessionPicker));
    }

    #[test]
    fn resume_with_id_loads_that_session() {
        match resume_action("  01a05ed3-71a7-7bc3-bcd5-c369bc2c8a4f  ") {
            Action::LoadSession(id, cwd, chat) => {
                assert_eq!(id, "01a05ed3-71a7-7bc3-bcd5-c369bc2c8a4f");
                assert!(cwd.is_none());
                assert!(!chat);
            }
            other => panic!("expected LoadSession, got {other:?}"),
        }
    }
}
