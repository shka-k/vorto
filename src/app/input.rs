//! Keyboard input handling: dispatch by mode, the Insert and Visual
//! pipelines (the Normal-mode token evaluator lives in `eval`), prompt
//! key forwarding, and the mode-boundary book-keeping (visual anchor,
//! cursor clamping) that goes with it.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Ctx, InsertKey, LastChange, MotionKind, Operator, PromptKind};
use crate::editor::Cursor;
use crate::finder::FuzzyKind;
use crate::mode::Mode;
use crate::prompt::PromptOutcome;

use super::{App, BufferRef, Selection, Status, eval, root_cause};

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
                self.evaluate(expr, Ctx::default())?;
            }
            eval::Parse::Incomplete => {}
            eval::Parse::Invalid => self.tokens.clear(),
        }
        Ok(())
    }

    fn handle_insert_key(&mut self, key: KeyEvent) -> Result<()> {
        let no_ctrl = !key.modifiers.contains(KeyModifiers::CONTROL);
        if no_ctrl && let KeyCode::Char(c) = key.code {
            self.fan_out_insert_char(c);
            self.record_insert_key(InsertKey::Char(c));
            return Ok(());
        }
        match key.code {
            KeyCode::Esc => {
                self.finalize_insert_recording();
                self.enter_mode(Mode::Normal);
            }
            KeyCode::Enter => {
                // Multi-cursor with Enter is tricky (one cursor splits a
                // line, others on the same line need their row/col
                // recomputed). Punt for v1 — only the primary types
                // newlines, extras stay put.
                let indent = self.indent_settings();
                self.buffer.insert_newline(indent);
                self.record_insert_key(InsertKey::Newline);
            }
            KeyCode::Backspace => {
                self.fan_out_backspace();
                self.record_insert_key(InsertKey::Backspace);
            }
            KeyCode::Tab => {
                self.fan_out_insert_char('\t');
                self.record_insert_key(InsertKey::Char('\t'));
            }
            // Arrow keys break vim's `.` recording — drop the in-flight
            // session so the next `.` replays only the typing up to here.
            KeyCode::Left => {
                self.recording = None;
                self.buffer.move_left();
            }
            KeyCode::Right => {
                self.recording = None;
                self.buffer.move_right(true);
            }
            KeyCode::Up => {
                self.recording = None;
                self.buffer.move_up();
            }
            KeyCode::Down => {
                self.recording = None;
                self.buffer.move_down();
            }
            _ => {}
        }
        Ok(())
    }

    /// Apply `insert_char(c)` at the primary cursor and every extra
    /// cursor, adjusting positions so each extra ends up "after its own
    /// inserted character" in the final buffer.
    ///
    /// Strategy: tag positions with their original index (primary = 0),
    /// sort descending by `(row, col)`, and process in that order. The
    /// descending order guarantees that a later, lower-position edit
    /// can't shift any cursor we haven't processed yet. After each
    /// edit, every *already-processed* cursor on the same row gets
    /// `col += 1` — that one earlier cursor's character index has been
    /// pushed right by the insertion we just did at a lower column.
    fn fan_out_insert_char(&mut self, ch: char) {
        if self.buffer.extra_cursors.is_empty() {
            let indent = self.indent_settings();
            self.buffer.insert_char_smart(ch, indent);
            return;
        }
        let mut all = collect_cursors(self);
        all.sort_by_key(|(_, c)| std::cmp::Reverse((c.row, c.col)));
        let mut new_positions = vec![Cursor::default(); all.len()];
        for i in 0..all.len() {
            let (orig_idx, pos) = all[i];
            self.buffer.cursor = pos;
            self.buffer.insert_char(ch);
            new_positions[orig_idx] = self.buffer.cursor;
            for (other_orig_idx, _) in all.iter().take(i) {
                if new_positions[*other_orig_idx].row == pos.row {
                    new_positions[*other_orig_idx].col += 1;
                }
            }
        }
        scatter_cursors(self, new_positions);
    }

    /// Backspace fan-out. Two cases:
    ///   - cursor at col > 0: same-row deletion, mirrors `insert_char`
    ///     fan-out with `col -= 1` shifts on same row.
    ///   - cursor at col == 0: this would join with the previous line.
    ///     We skip fan-out for these (only primary joins) — handling
    ///     the row collapse on every extra is a separate bookkeeping
    ///     problem we're leaving to v2.
    fn fan_out_backspace(&mut self) {
        if self.buffer.extra_cursors.is_empty() {
            self.buffer.delete_char_before_smart();
            return;
        }
        let mut all = collect_cursors(self);
        all.sort_by_key(|(_, c)| std::cmp::Reverse((c.row, c.col)));
        let mut new_positions = vec![Cursor::default(); all.len()];
        for i in 0..all.len() {
            let (orig_idx, pos) = all[i];
            if pos.col == 0 {
                // Only the primary performs the line-join; extras at
                // col 0 stay put rather than risk a row collapse.
                if orig_idx == 0 {
                    self.buffer.cursor = pos;
                    self.buffer.delete_char_before();
                    new_positions[orig_idx] = self.buffer.cursor;
                } else {
                    new_positions[orig_idx] = pos;
                }
                continue;
            }
            self.buffer.cursor = pos;
            self.buffer.delete_char_before();
            new_positions[orig_idx] = self.buffer.cursor;
            for (other_orig_idx, _) in all.iter().take(i) {
                if new_positions[*other_orig_idx].row == pos.row
                    && new_positions[*other_orig_idx].col > 0
                {
                    new_positions[*other_orig_idx].col -= 1;
                }
            }
        }
        scatter_cursors(self, new_positions);
    }

    fn record_insert_key(&mut self, k: InsertKey) {
        if let Some(r) = self.recording.as_mut() {
            r.keys.push(k);
        }
    }

    /// Move the in-flight recording into `last_change`. Called on Esc
    /// out of an Insert session that began with a recordable trigger.
    fn finalize_insert_recording(&mut self) {
        if let Some(r) = self.recording.take() {
            self.last_change = Some(LastChange::Insert {
                trigger: r.trigger,
                keys: r.keys,
            });
        }
    }

    fn handle_visual_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // `g` is a two-key prefix in visual just like in normal — but
        // visual bypasses the token pipeline, so the one bit of state
        // we need lives on App as `visual_g_pending`.
        if std::mem::take(&mut self.visual_g_pending) {
            match key.code {
                KeyCode::Char('g') => self.buffer.move_file_start(),
                KeyCode::Char('e') => self.apply_visual_motion(MotionKind::WordEndBack),
                KeyCode::Char('E') => self.apply_visual_motion(MotionKind::BigWordEndBack),
                KeyCode::Char('_') => {
                    self.apply_visual_motion(MotionKind::LineLastNonBlank)
                }
                // gs/gl/gc/gb — same aliases as Normal mode.
                KeyCode::Char('s') => {
                    self.apply_visual_motion(MotionKind::LineFirstNonBlank)
                }
                KeyCode::Char('l') => self.apply_visual_motion(MotionKind::LineEnd),
                KeyCode::Char('c') => {
                    self.apply_visual_motion(MotionKind::ViewportMiddle)
                }
                KeyCode::Char('b') => {
                    self.apply_visual_motion(MotionKind::ViewportBottom)
                }
                // `gn` / `gN` — extend the selection to cover the next
                // (or previous) search match. Anchor stays put; the
                // shared helper just walks the active end out to the
                // match's last char.
                KeyCode::Char('n') => self.run_search_select(self.search.last_forward),
                KeyCode::Char('N') => self.run_search_select(!self.search.last_forward),
                _ => {}
            }
            return Ok(());
        }

        // Pure-motion keys that map straight onto `MotionKind` and
        // can use the shared `motion_target` path. Selection follows
        // automatically because the anchor stays fixed.
        if let Some(motion) = visual_motion_for(key) {
            self.apply_visual_motion(motion);
            return Ok(());
        }

        match key.code {
            KeyCode::Esc => self.enter_mode(Mode::Normal),
            KeyCode::Char('h') | KeyCode::Left => self.buffer.move_left(),
            KeyCode::Char('l') | KeyCode::Right => self.buffer.move_right(false),
            KeyCode::Char('j') | KeyCode::Down => self.buffer.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.buffer.move_up(),
            KeyCode::Char('0') | KeyCode::Home => self.buffer.move_line_start(),
            KeyCode::Char('G') => self.buffer.move_file_end(),
            // `g` prefix — defer to the next key (see top of fn).
            KeyCode::Char('g') => self.visual_g_pending = true,
            // `*` / `#` reuse the Normal-mode helper to seed search state.
            KeyCode::Char('*') => self.search_word_under_cursor(true),
            KeyCode::Char('#') => self.search_word_under_cursor(false),
            // `o` — swap the anchor and the cursor so the user can
            // extend the *other* end of the selection.
            KeyCode::Char('o') => self.swap_visual_endpoints(),
            // Toggle visual sub-modes: pressing the same trigger again
            // exits, a different one switches without losing the anchor.
            KeyCode::Char('v') if !ctrl => self.toggle_visual(Mode::Visual),
            KeyCode::Char('v') if ctrl => self.toggle_visual(Mode::VisualBlock),
            KeyCode::Char('V') => self.toggle_visual(Mode::VisualLine),
            KeyCode::Char('y') => {
                self.apply_visual_op(Operator::Yank);
                self.enter_mode(Mode::Normal);
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                self.buffer.snapshot();
                self.apply_visual_op(Operator::Delete);
                self.enter_mode(Mode::Normal);
            }
            KeyCode::Char('c') | KeyCode::Char('s') => {
                self.buffer.snapshot();
                self.apply_visual_op(Operator::Change);
            }
            KeyCode::Char('~') => self.toggle_case_selection(),
            KeyCode::Char('J') => self.join_selection_lines(),
            _ => {}
        }
        Ok(())
    }

    /// Resolve a motion against the current cursor and assign — the
    /// selection follows because the anchor is fixed.
    fn apply_visual_motion(&mut self, motion: MotionKind) {
        let target = self.buffer.motion_target(self.buffer.cursor, motion, 1);
        self.buffer.cursor = target;
    }


    fn swap_visual_endpoints(&mut self) {
        if let Some(anchor) = self.visual_anchor {
            let cur = self.buffer.cursor;
            self.buffer.cursor = anchor;
            self.visual_anchor = Some(cur);
        }
    }

    /// `~` in visual — toggle case across the entire selection.
    /// Charwise covers the span, linewise covers whole rows, block
    /// covers the rectangle. Exits visual when finished.
    fn toggle_case_selection(&mut self) {
        let Some(sel) = self.selection() else { return };
        self.buffer.snapshot();
        match sel {
            super::Selection::Char { from, to } => {
                let end = self.buffer.advance_one(to);
                self.buffer.toggle_case_range(from, end);
            }
            super::Selection::Line { from_row, to_row } => {
                self.buffer.toggle_case_lines(from_row, to_row);
            }
            super::Selection::Block { r0, c0, r1, c1 } => {
                self.buffer.toggle_case_block(r0, c0, r1, c1);
            }
        }
        self.enter_mode(Mode::Normal);
    }

    /// `J` in visual: join every selected line (any flavour of visual)
    /// into the first one. Equivalent to repeating `J` `n-1` times
    /// after positioning the cursor at the top of the selection.
    fn join_selection_lines(&mut self) {
        let Some(sel) = self.selection() else { return };
        let (from_row, to_row) = match sel {
            super::Selection::Char { from, to } => (from.row, to.row),
            super::Selection::Line { from_row, to_row } => (from_row, to_row),
            super::Selection::Block { r0, r1, .. } => (r0, r1),
        };
        if from_row == to_row {
            return;
        }
        self.buffer.snapshot();
        self.buffer.cursor.row = from_row;
        self.buffer.cursor.col = 0;
        for _ in 0..(to_row - from_row) {
            self.buffer.join_next_line();
        }
        self.enter_mode(Mode::Normal);
    }
}

/// Map a visual-mode key event to the motion it triggers, if any.
/// Returns `None` for keys that require special handling (operators,
/// prefixes, edits, mode toggles, etc.).
fn visual_motion_for(key: KeyEvent) -> Option<MotionKind> {
    use MotionKind as M;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Page motions (Ctrl-modified) need to be matched first so the
    // bare-letter cases below don't shadow them.
    if ctrl {
        return match key.code {
            KeyCode::Char('d') => Some(M::HalfPageDown),
            KeyCode::Char('u') => Some(M::HalfPageUp),
            KeyCode::Char('f') => Some(M::PageDown),
            KeyCode::Char('b') => Some(M::PageUp),
            _ => None,
        };
    }
    Some(match key.code {
        KeyCode::Char('w') => M::WordForward,
        KeyCode::Char('b') => M::WordBack,
        KeyCode::Char('e') => M::WordEnd,
        KeyCode::Char('W') => M::BigWordForward,
        KeyCode::Char('B') => M::BigWordBack,
        KeyCode::Char('E') => M::BigWordEnd,
        KeyCode::Char('^') => M::LineFirstNonBlank,
        KeyCode::Char('$') | KeyCode::End => M::LineEnd,
        KeyCode::Char('%') => M::BracketMatch,
        KeyCode::Char('H') => M::ViewportTop,
        KeyCode::Char('M') => M::ViewportMiddle,
        KeyCode::Char('L') => M::ViewportBottom,
        KeyCode::Char('{') => M::ParagraphBack,
        KeyCode::Char('}') => M::ParagraphForward,
        _ => return None,
    })
}

impl App {

    fn toggle_visual(&mut self, target: Mode) {
        if self.mode == target {
            self.enter_mode(Mode::Normal);
        } else {
            // Switch sub-mode but keep the anchor — pressing `V` from
            // charwise visual should extend the selection line-wise.
            self.mode = target;
        }
    }

    fn apply_visual_op(&mut self, op: Operator) {
        let Some(sel) = self.selection() else { return };
        match sel {
            Selection::Char { from, to } => {
                let end = self.buffer.advance_one(to);
                match op {
                    Operator::Yank => {
                        self.buffer.yank_range(from, end);
                        self.status = Status::info("yanked");
                        self.buffer.cursor = from;
                    }
                    Operator::Delete => self.buffer.delete_range(from, end),
                    Operator::Change => {
                        self.buffer.delete_range(from, end);
                        self.enter_mode(Mode::Insert);
                    }
                }
            }
            Selection::Line { from_row, to_row } => match op {
                Operator::Yank => {
                    self.buffer.yank_lines(from_row, to_row);
                    self.status = Status::info("yanked");
                    self.buffer.cursor.row = from_row;
                    self.buffer.cursor.col = 0;
                }
                Operator::Delete => self.buffer.delete_lines(from_row, to_row),
                Operator::Change => {
                    self.buffer.delete_lines(from_row, to_row);
                    self.enter_mode(Mode::Insert);
                }
            },
            Selection::Block { r0, c0, r1, c1 } => match op {
                Operator::Yank => {
                    self.buffer.yank_block(r0, c0, r1, c1);
                    self.status = Status::info("yanked");
                    self.buffer.cursor = Cursor { row: r0, col: c0 };
                }
                Operator::Delete => self.buffer.delete_block(r0, c0, r1, c1),
                Operator::Change => {
                    self.buffer.delete_block(r0, c0, r1, c1);
                    self.enter_mode(Mode::Insert);
                }
            },
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
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
                    self.status = Status::error("pattern not found");
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
                    self.status = Status::error(format!("jump: {}", root_cause(&e)));
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

    pub(super) fn enter_mode(&mut self, mode: Mode) {
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

    pub(super) fn open_prompt(&mut self, kind: PromptKind) {
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

/// Snapshot every active cursor (primary first, then extras in their
/// stored order) tagged with its original index. The original index is
/// what `scatter_cursors` writes back to: index 0 is the primary,
/// 1..N go back into `extra_cursors[0..N-1]` so the pop ordering of
/// `<C-p>` is preserved.
fn collect_cursors(app: &App) -> Vec<(usize, Cursor)> {
    std::iter::once((0usize, app.buffer.cursor))
        .chain(
            app.buffer
                .extra_cursors
                .iter()
                .enumerate()
                .map(|(i, c)| (i + 1, *c)),
        )
        .collect()
}

/// Inverse of `collect_cursors`. `positions[0]` is the new primary;
/// `positions[1..]` becomes the new extras (preserving their original
/// ordering). Dedupes any extra that landed on the primary or on an
/// earlier extra, since coincident cursors are visually indistinguishable
/// and would just amplify subsequent edits.
fn scatter_cursors(app: &mut App, positions: Vec<Cursor>) {
    app.buffer.cursor = positions[0];
    let primary = positions[0];
    let mut extras: Vec<Cursor> = Vec::with_capacity(positions.len() - 1);
    for c in positions.into_iter().skip(1) {
        if c == primary || extras.contains(&c) {
            continue;
        }
        extras.push(c);
    }
    app.buffer.extra_cursors = extras;
}
