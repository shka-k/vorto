//! Insert-mode key handling: completion popup, character/Backspace
//! fan-out across extra cursors, and the `LastChange::Insert`
//! recording for `.`.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{InsertKey, LastChange};
use crate::app::App;
use crate::app::completion::is_ident_continue;
use crate::editor::Cursor;
use crate::mode::Mode;

impl App {
    pub(super) fn handle_insert_key(&mut self, key: KeyEvent) -> Result<()> {
        // Completion popup, when open, intercepts navigation/accept
        // keys before they reach the normal insert handling. Char
        // input and Backspace fall through and trigger a re-filter
        // afterwards. Esc closes the popup *first* (so the next Esc
        // exits insert mode) — matches how every other editor behaves.
        if self.completion.is_some()
            && let Some(()) = self.handle_completion_key(key)
        {
            return Ok(());
        }

        // `<C-Space>` triggers a completion request. We do this before
        // the bare-char fast path so it doesn't get typed literally.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char(' ') {
            self.lsp_completion();
            return Ok(());
        }

        let no_ctrl = !key.modifiers.contains(KeyModifiers::CONTROL);
        if no_ctrl && let KeyCode::Char(c) = key.code {
            self.fan_out_insert_char(c);
            self.record_insert_key(InsertKey::Char(c));
            self.update_completion_filter();
            // Auto-trigger completion when the user starts typing an
            // identifier and no popup is open. Identifier-continue chars
            // only — punctuation, whitespace, and operators don't fire
            // a request on their own. If the popup is already open, the
            // re-filter above is enough; we don't refire because items
            // are stable for the same prefix-start.
            if self.completion.is_none() && self.lsp.has_lsp() && is_ident_continue(c) {
                self.lsp_completion();
            }
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
                self.cancel_completion();
            }
            KeyCode::Backspace => {
                self.fan_out_backspace();
                self.record_insert_key(InsertKey::Backspace);
                self.update_completion_filter();
            }
            KeyCode::Tab => {
                // Honor the buffer's indent settings: `use_tabs` mode
                // inserts a literal `\t`, soft-tab mode inserts enough
                // spaces to reach the next tab stop at column-multiple
                // `width`. Soft-tab uses char column for the stop math;
                // there shouldn't be `\t` characters in leading
                // whitespace when `use_tabs` is false, so visual vs.
                // char column converge in practice.
                let indent = self.indent_settings();
                if indent.use_tabs {
                    self.fan_out_insert_char('\t');
                    self.record_insert_key(InsertKey::Char('\t'));
                } else {
                    let stop = indent.width.max(1);
                    let col = self.buffer.cursor.col;
                    let n = stop - (col % stop);
                    for _ in 0..n {
                        self.fan_out_insert_char(' ');
                        self.record_insert_key(InsertKey::Char(' '));
                    }
                }
                self.update_completion_filter();
            }
            // Arrow keys break vim's `.` recording — drop the in-flight
            // session so the next `.` replays only the typing up to here.
            KeyCode::Left => {
                self.recording = None;
                self.buffer.move_left();
                self.cancel_completion();
            }
            KeyCode::Right => {
                self.recording = None;
                self.buffer.move_right(true);
                self.cancel_completion();
            }
            KeyCode::Up => {
                self.recording = None;
                self.buffer.move_up();
                self.cancel_completion();
            }
            KeyCode::Down => {
                self.recording = None;
                self.buffer.move_down();
                self.cancel_completion();
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle a key event while the completion popup is open. Returns
    /// `Some(())` when the key was absorbed by the popup (selection
    /// changed, popup closed, item accepted) — caller should bail.
    /// Returns `None` when the key should fall through to the normal
    /// insert-mode handling (typing a character that re-filters,
    /// backspace, etc.).
    fn handle_completion_key(&mut self, key: KeyEvent) -> Option<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.cancel_completion();
                Some(())
            }
            KeyCode::Up => {
                if let Some(s) = self.completion.as_mut() {
                    s.move_selection(-1);
                }
                Some(())
            }
            KeyCode::Down => {
                if let Some(s) = self.completion.as_mut() {
                    s.move_selection(1);
                }
                Some(())
            }
            KeyCode::Char('p') if ctrl => {
                if let Some(s) = self.completion.as_mut() {
                    s.move_selection(-1);
                }
                Some(())
            }
            KeyCode::Char('n') if ctrl => {
                if let Some(s) = self.completion.as_mut() {
                    s.move_selection(1);
                }
                Some(())
            }
            KeyCode::Tab => {
                if let Some(s) = self.completion.as_mut() {
                    s.move_selection(1);
                }
                Some(())
            }
            KeyCode::BackTab => {
                if let Some(s) = self.completion.as_mut() {
                    s.move_selection(-1);
                }
                Some(())
            }
            KeyCode::Enter => {
                self.accept_completion();
                Some(())
            }
            _ => None,
        }
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
            let indent = self.indent_settings();
            self.buffer.delete_char_before_smart(indent);
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
