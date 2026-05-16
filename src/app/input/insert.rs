//! Insert-mode key handling: completion popup, character/Backspace
//! fan-out across extra cursors, and the `LastChange::Insert`
//! recording for `.`.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{InsertKey, LastChange};
use crate::app::App;
use crate::app::SignatureTrigger;
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
            // identifier and no popup is open. Identifier-continue
            // chars always qualify; for punctuation we defer to the
            // server-declared `triggerCharacters` from initialize
            // (e.g. `:` `.` `'` for rust-analyzer, `<` for tsserver).
            // Other punctuation, whitespace, and operators don't fire
            // a request on their own. If the popup is already open,
            // the re-filter above is enough; we don't refire because
            // items are stable for the same prefix-start.
            if self.completion.is_none() && self.lsp.has_lsp() {
                if self.lsp.is_completion_trigger_char(c) {
                    // Forward the actual trigger character so the
                    // server can switch to its trigger-character
                    // codepath (rust-analyzer needs this to surface
                    // path completions after `::`).
                    self.lsp_completion_triggered(c);
                } else if is_ident_continue(c) {
                    self.lsp_completion();
                }
            }
            self.update_signature_help_on_char(c);
            return Ok(());
        }
        match key.code {
            KeyCode::Esc => {
                self.finalize_insert_recording();
                self.cancel_signature_help();
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
                // Newline almost always ends the call argument we were
                // helping with (function call literals don't span lines
                // in any language the popup targets) — close rather
                // than retrigger.
                self.cancel_signature_help();
            }
            KeyCode::Backspace => {
                self.fan_out_backspace();
                self.record_insert_key(InsertKey::Backspace);
                self.update_completion_filter();
                self.update_signature_help_on_edit();
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                // Some terminals (e.g. macOS Terminal.app) report
                // Shift+Tab as `Tab` + SHIFT instead of `BackTab`. Treat
                // both as dedent so users don't need to know which they
                // have.
                self.fan_out_dedent();
                self.record_insert_key(InsertKey::Dedent);
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
            KeyCode::BackTab => {
                self.fan_out_dedent();
                self.record_insert_key(InsertKey::Dedent);
                self.update_completion_filter();
            }
            // Arrow keys break vim's `.` recording — drop the in-flight
            // session so the next `.` replays only the typing up to here.
            KeyCode::Left => {
                self.recording = None;
                self.buffer.move_left();
                self.cancel_completion();
                self.update_signature_help_on_edit();
            }
            KeyCode::Right => {
                self.recording = None;
                self.buffer.move_right(true);
                self.cancel_completion();
                self.update_signature_help_on_edit();
            }
            KeyCode::Up => {
                self.recording = None;
                self.buffer.move_up();
                self.cancel_completion();
                self.cancel_signature_help();
            }
            KeyCode::Down => {
                self.recording = None;
                self.buffer.move_down();
                self.cancel_completion();
                self.cancel_signature_help();
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
        // Direction of the nav request, if this key is a nav key.
        // `None` means the key isn't navigation.
        let nav_delta: Option<isize> = match key.code {
            KeyCode::Esc => {
                self.cancel_completion();
                return Some(());
            }
            KeyCode::Up | KeyCode::BackTab => Some(-1),
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => Some(-1),
            KeyCode::Down | KeyCode::Tab => Some(1),
            KeyCode::Char('p') if ctrl => Some(-1),
            KeyCode::Char('n') if ctrl => Some(1),
            KeyCode::Enter => {
                // Accept only when the popup is in selecting mode. In
                // preview mode (just opened by auto-trigger, no row
                // committed-to yet) let Enter fall through to insert a
                // literal newline.
                let selecting = self
                    .completion
                    .as_ref()
                    .map(|s| s.selecting)
                    .unwrap_or(false);
                if !selecting {
                    self.cancel_completion();
                    return None;
                }
                self.accept_completion();
                return Some(());
            }
            _ => return None,
        };
        if let Some(delta) = nav_delta
            && let Some(s) = self.completion.as_mut()
        {
            // First nav keypress only flips into selecting mode; the
            // initial row at `selected = 0` (or wherever refilter left
            // it) is what gets highlighted. Subsequent presses actually
            // step the selection.
            if !s.selecting {
                s.selecting = true;
                // For backward nav, jump to the last row so BackTab /
                // Up from preview mode lands at the bottom — matches
                // what users expect after typing Tab to "open" and
                // BackTab to "look at the bottom item".
                if delta < 0 && !s.filtered.is_empty() {
                    s.selected = s.filtered.len() - 1;
                }
            } else {
                s.move_selection(delta);
            }
        }
        // Now in selecting mode: pull `detail` / `documentation` for
        // the current row in case the server deferred them.
        self.resolve_current_completion_for_detail();
        Some(())
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

    /// Shift+Tab fan-out: dedent each *unique* row that holds a cursor
    /// once, then shift every cursor on that row left by the number of
    /// chars stripped. Per-row rather than per-cursor because two
    /// cursors on the same row would otherwise double-dedent (or worse,
    /// the second call would see a different leading-whitespace state
    /// than the first).
    fn fan_out_dedent(&mut self) {
        let indent = self.indent_settings();
        if self.buffer.extra_cursors.is_empty() {
            self.buffer.dedent_current_line(indent);
            return;
        }
        let all = collect_cursors(self);
        let mut rows: Vec<usize> = all.iter().map(|(_, c)| c.row).collect();
        rows.sort_unstable();
        rows.dedup();
        let mut removed: Vec<(usize, usize)> = Vec::with_capacity(rows.len());
        for row in rows {
            let before = self.buffer.lines[row].chars().count();
            self.buffer.dedent_line(row, indent);
            let after = self.buffer.lines[row].chars().count();
            removed.push((row, before - after));
        }
        let mut new_positions = vec![Cursor::default(); all.len()];
        for (orig_idx, pos) in &all {
            let n = removed
                .iter()
                .find(|(r, _)| *r == pos.row)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            new_positions[*orig_idx] = Cursor {
                row: pos.row,
                col: pos.col.saturating_sub(n),
            };
        }
        scatter_cursors(self, new_positions);
    }

    fn record_insert_key(&mut self, k: InsertKey) {
        if let Some(r) = self.recording.as_mut() {
            r.keys.push(k);
        }
    }

    /// Decide whether typing `c` should trigger or refresh the
    /// signature-help popup. Three cases:
    /// - Popup is open: refresh with a `ContentChange` retrigger so the
    ///   server can advance `activeParameter` as the user types args.
    ///   We don't bother distinguishing retrigger characters here — any
    ///   keystroke can shift which parameter the cursor is in.
    /// - Popup closed and `c` is a server-declared trigger char (`(` is
    ///   the common case): open from scratch.
    /// - Otherwise: nothing.
    fn update_signature_help_on_char(&mut self, c: char) {
        if !self.lsp.has_lsp() {
            return;
        }
        if self.signature.is_some() {
            self.lsp_signature_help(SignatureTrigger::ContentChange(Some(c)));
        } else if self.lsp.is_signature_help_trigger_char(c) {
            self.lsp_signature_help(SignatureTrigger::TriggerCharacter(c));
        }
    }

    /// Refresh the signature popup after a non-character edit (backspace
    /// or horizontal cursor move). No-op when the popup isn't open —
    /// backspacing in code where no help is showing shouldn't poke the
    /// server.
    fn update_signature_help_on_edit(&mut self) {
        if self.signature.is_none() || !self.lsp.has_lsp() {
            return;
        }
        self.lsp_signature_help(SignatureTrigger::ContentChange(None));
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
