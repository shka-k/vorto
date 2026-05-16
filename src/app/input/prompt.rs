//! Prompt key forwarding plus the outcome dispatch (commands, search,
//! file/buffer pickers, LSP rename/code-action submissions).

use anyhow::Result;
use crossterm::event::KeyEvent;

use crate::app::{App, Toast, root_cause};
use crate::prompt::PromptOutcome;

impl App {
    pub(super) fn handle_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
        let outcome = self.prompt.handle_key(key, &self.startup_cwd);
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
                match self.open_path(&path) {
                    Ok(()) => self.run_scroll(crate::effect::ScrollAnchor::Center),
                    // Bubbling this up would terminate the event loop —
                    // a picker entry that fails to load (e.g. a stray
                    // symlink to a directory) should leave the user in
                    // their current buffer with a visible error.
                    Err(e) => {
                        self.push_toast(Toast::error(format!("open: {}", root_cause(&e))))
                    }
                }
                Ok(())
            }
            PromptOutcome::GotoLine(row) => {
                self.buffer.cursor.row = row;
                self.buffer.cursor.col = 0;
                self.buffer.clamp_col(false);
                Ok(())
            }
            PromptOutcome::JumpToLocation(loc) => {
                match self.jump_to_location(&loc) {
                    Ok(()) => {
                        // Picker-driven jump — the user explicitly chose
                        // this match, so park it in the middle of the
                        // viewport instead of just bringing it into view.
                        self.run_scroll(crate::effect::ScrollAnchor::Center);
                    }
                    Err(e) => {
                        self.push_toast(Toast::error(format!("jump: {}", root_cause(&e))));
                    }
                }
                Ok(())
            }
            PromptOutcome::SubmitRename(new_name) => {
                self.submit_rename(new_name);
                Ok(())
            }
            PromptOutcome::OpenBuffer(r) => {
                self.switch_to_buffer(r)?;
                // Picker-driven buffer switch — center the restored
                // cursor in the viewport so the user lands on a
                // recognizable context rather than wherever the saved
                // scroll position happened to leave it.
                self.run_scroll(crate::effect::ScrollAnchor::Center);
                Ok(())
            }
            PromptOutcome::SelectCodeAction(action) => {
                self.submit_code_action(action);
                Ok(())
            }
        }
    }
}
