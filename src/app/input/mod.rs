//! Keyboard input dispatch.
//!
//! [`App::handle_key`] is the entry point. It routes to the prompt /
//! jump overlay first, then to the per-mode handlers in [`insert`],
//! [`visual`], and [`prompt`]. Normal-mode input flows through the token
//! pipeline in [`crate::app::eval`]; this module's role is everything
//! that *isn't* the Normal-mode operator/motion grammar.
//!
//! Mode-boundary book-keeping (visual anchor, cursor clamping) and the
//! prompt-opening helpers live here too, since they're called from
//! both the eval pipeline and the per-mode handlers.

mod insert;
mod prompt;
mod visual;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::PromptKind;
use crate::finder::FuzzyKind;
use crate::mode::Mode;

use crate::buffer_ref::BufferRef;

use super::{App, eval};

impl App {
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.prompt.is_open() {
            return self.handle_prompt_key(key);
        }

        // `gw` overlay swallows every key until the user picks a label
        // or cancels. Sits above the panic-button to keep Esc / Ctrl-C
        // local to the overlay (they cancel jump, not the whole app).
        if self.jump_state.is_some() {
            self.handle_jump_key(key);
            return Ok(());
        }

        // Global panic button.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        // Insert & Visual modes have small enough surfaces that they're
        // handled directly. The token pipeline is Normal-mode only — that
        // is where the rich operator/motion/text-object grammar lives.
        match self.mode {
            Mode::Insert => return self.handle_insert_key(key),
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                return self.handle_visual_key(key);
            }
            Mode::Normal => {}
        }

        // Normal mode: tokenize → classify → evaluate.
        match eval::tokenize(&self.config.keymap, &self.tokens, self.mode, key) {
            Some(t) => self.tokens.push(t),
            None => {
                self.tokens.clear();
                return Ok(());
            }
        }
        match eval::classify(&self.tokens) {
            eval::Parse::Complete(expr) => {
                self.tokens.clear();
                self.evaluate(expr, crate::action::Ctx::default())?;
            }
            eval::Parse::Incomplete => {}
            eval::Parse::Invalid => self.tokens.clear(),
        }
        Ok(())
    }

    pub(in crate::app) fn enter_mode(&mut self, mode: Mode) {
        // Set or clear the visual anchor at the mode boundary. Entering
        // any visual mode pins the anchor to the current cursor;
        // entering Normal/Insert drops it.
        if mode.is_visual() && !self.mode.is_visual() {
            self.visual_anchor = Some(self.buffer.cursor);
        } else if !mode.is_visual() {
            self.visual_anchor = None;
        }
        if mode == Mode::Normal {
            self.buffer.clamp_col(false);
        }
        self.mode = mode;
    }

    pub(in crate::app) fn open_prompt(&mut self, kind: PromptKind) {
        match kind {
            PromptKind::Command => self.prompt.open_command(),
            PromptKind::Search { forward } => self.prompt.open_search(forward),
            PromptKind::Fuzzy(FuzzyKind::Files) => self.prompt.open_files(&self.startup_cwd),
            PromptKind::Fuzzy(FuzzyKind::Lines) => self.prompt.open_lines(&self.buffer.lines),
            PromptKind::Fuzzy(FuzzyKind::Buffers) => self.open_buffer_picker(),
            // `Locations` pickers are built from server results, not opened
            // from a keymap — fall through to a no-op rather than a fresh
            // empty picker that would do nothing useful on submit.
            PromptKind::Fuzzy(FuzzyKind::Locations) => {}
        }
    }

    /// Build the MRU display list and open the buffer picker. Shows
    /// every recently-touched buffer, current one included, plus the
    /// scratch sentinel.
    ///
    /// Each entry carries three leading columns:
    ///   - `%` if it's the active buffer, otherwise blank.
    ///   - `~` if the file differs from HEAD (live diff for the
    ///     active buffer, `git status --porcelain` set for the rest).
    ///   - `+` if the buffer has unsaved edits.
    ///
    /// Always opens (even on empty MRU) so the user gets a visible
    /// "(no matches)" instead of silent nothing.
    fn open_buffer_picker(&mut self) {
        let cwd = &self.startup_cwd;
        let current_path = self
            .buffer
            .path
            .as_ref()
            .and_then(|p| p.canonicalize().ok());
        let on_scratch = self.buffer.path.is_none();
        let active_dirty = self.buffer.dirty;
        let active_vcs_changed = self.buffer.has_vcs_changes();
        // One `git status` invocation feeds the VCS marker for every
        // non-active File entry in the list — cheaper than diffing
        // each sleeping buffer individually.
        let vcs_set = crate::vcs::changed_files(cwd);

        let (items, refs): (Vec<_>, Vec<_>) = self
            .opened_paths
            .iter()
            .rev() // newest first
            .map(|r| {
                let (label, is_current) = match r {
                    BufferRef::Scratch => ("[scratch]".to_string(), on_scratch),
                    BufferRef::File(p) => {
                        let rel = p
                            .strip_prefix(cwd)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .to_string();
                        let is_current = current_path.as_ref() == Some(p);
                        (rel, is_current)
                    }
                };
                // Dirty is tracked on whichever copy is live: the
                // active buffer for `is_current`, the sleeping map
                // entry for everything else.
                let entry_dirty = if is_current {
                    active_dirty
                } else {
                    self.sleeping.get(r).is_some_and(|b| b.dirty)
                };
                // VCS marker. Scratch never has a VCS state. For the
                // active File we trust the live in-memory diff (catches
                // unsaved edits that `git status` can't see); for every
                // other File we fall back to the porcelain set, then
                // OR in the unsaved-dirty bit so an inactive edited
                // buffer still shows as changed even if its on-disk
                // copy matches HEAD.
                let entry_vcs = match r {
                    BufferRef::Scratch => false,
                    BufferRef::File(p) => {
                        if is_current {
                            active_vcs_changed
                        } else {
                            vcs_set.contains(p) || entry_dirty
                        }
                    }
                };
                let cur_col = if is_current { '%' } else { ' ' };
                let vcs_col = if entry_vcs { '~' } else { ' ' };
                let mod_col = if entry_dirty { '+' } else { ' ' };
                let display = format!("{}{}{} {}", cur_col, vcs_col, mod_col, label);
                (display, r.clone())
            })
            .unzip();
        self.prompt.open_buffers(items, refs);
    }
}
