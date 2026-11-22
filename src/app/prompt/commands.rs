//! Command registration and execution for input prompts, confirmations and command completion.

use crate::app::App;
use crate::app::commands::AppEffect;
use crate::ui::PromptKind;

impl App {
    pub(in crate::app) fn confirmation_requires_fresh_key(&self, command: &str) -> bool {
        let Some(prompt) = self.input.prompt() else {
            return false;
        };
        // Vim lets a held y or n answer successive substitute confirmations.
        if self.search.help_is_open() || prompt.kind == PromptKind::SubstituteConfirm {
            return false;
        }
        if prompt.confirmation.is_none() && prompt.kind != PromptKind::GitReset {
            return false;
        }
        match command {
            "command_line.submit"
            | "command_line.cancel"
            | "surface.confirmation.select_yes"
            | "surface.confirmation.select_no"
            | "surface.confirmation.toggle" => true,
            // In a button prompt these choose a button, rather than moving a caret.
            "command_line.move_left"
            | "command_line.move_right"
            | "command_line.move_word_left"
            | "command_line.move_word_right"
            | "command_line.move_home"
            | "command_line.move_end" => prompt.confirmation.is_some(),
            _ => false,
        }
    }

    pub(in crate::app) fn process_prompt_effect(
        &mut self,
        effect: AppEffect,
    ) -> Result<(), AppEffect> {
        match effect {
            AppEffect::OpenPrompt { kind, initial } => self.open_prompt(kind, initial),
            AppEffect::OpenFilePrompt => {
                let alias = self
                    .configuration
                    .settings()
                    .input
                    .command_aliases
                    .iter()
                    .find(|(_, command)| command.as_str() == "file.open")
                    .map_or("file.open", |(alias, _)| alias.as_str());
                self.open_prompt(PromptKind::Command, format!("{alias} "));
            }
            AppEffect::PromptInsert(text) => self.prompt_insert(&text),
            AppEffect::PromptBackspace => self.prompt_backspace(),
            AppEffect::PromptDeleteWord => self.prompt_delete(|input| input.prompt_delete_word()),
            AppEffect::PromptDeleteToStart => {
                self.prompt_delete(|input| input.prompt_delete_to_start());
            }
            AppEffect::PromptMove(movement) => self.prompt_move(movement),
            AppEffect::PromptHistory(direction) => self.prompt_history(direction),
            AppEffect::PromptComplete(direction) => self.prompt_complete(direction),
            AppEffect::PromptConfirmationSelect(choice) => {
                self.input.select_confirmation(choice);
                self.submit_prompt();
            }
            AppEffect::PromptConfirmationToggle => {
                self.input.toggle_confirmation();
                self.project_input_prompt();
            }
            AppEffect::PromptSubmit => self.submit_prompt(),
            AppEffect::PromptCancel => {
                let declined_trust = self
                    .input
                    .prompt()
                    .is_some_and(|prompt| prompt.kind == PromptKind::WorkspaceTrust);
                self.close_prompt();
                if declined_trust {
                    self.notify_message(self.message("workspace_trust_declined", &[]), false);
                }
            }
            effect => return Err(effect),
        }
        Ok(())
    }
}
