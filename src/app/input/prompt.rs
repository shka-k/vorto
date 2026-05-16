//! Prompt key forwarding plus the outcome dispatch (commands, search,
//! file/buffer pickers, LSP rename/code-action submissions).

use anyhow::Result;
use crossterm::event::KeyEvent;

use crate::app::{App, Toast, root_cause};
use crate::prompt::PromptOutcome;

impl App {
    pub(super) fn handle_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
        let outcome = self.prompt.handle_key(key);
        self.apply_prompt_outcome(outcome)
    }

    fn apply_prompt_outcome(&mut self, outcome: PromptOutcome) -> Result<()> {
        match outcome {
            PromptOutcome::Nothing | PromptOutcome::Cancelled => Ok(()),
            PromptOutcome::RunCommand(line) => self.execute_command(&line),
            PromptOutcome::Search { forward, query } => {
                self.search.set(query, forward);
                if let Some(c) = self.search.find_next(&self.buffer, forward) {
                    self.buffer.cursor = c;
                } else {
                    self.push_toast(Toast::error("pattern not found"));
                }
                Ok(())
            }
            PromptOutcome::OpenRelativeFile(rel) => {
                // Items are root-relative paths (see `collect_files`). Re-
                // anchor against `startup_cwd` so the resulting buffer
                // path doesn't depend on whatever `current_dir()` is now.
                let path = self.startup_cwd.join(rel);
                self.open_path(&path)
            }
            PromptOutcome::GotoLine(row) => {
                self.buffer.cursor.row = row;
                self.buffer.cursor.col = 0;
                self.buffer.clamp_col(false);
                Ok(())
            }
            PromptOutcome::JumpToLocation(loc) => {
                if let Err(e) = self.jump_to_location(&loc) {
                    self.push_toast(Toast::error(format!("jump: {}", root_cause(&e))));
                }
                Ok(())
            }
            PromptOutcome::SubmitRename(new_name) => {
                self.submit_rename(new_name);
                Ok(())
            }
            PromptOutcome::OpenBuffer(r) => self.switch_to_buffer(r),
            PromptOutcome::SelectCodeAction(action) => {
                self.submit_code_action(action);
                Ok(())
            }
        }
    }
}
