//! Visual-mode key handling: motion application, operator dispatch
//! (`y`/`d`/`c`/`~`/`J`), and the `g` two-key prefix that mirrors
//! Normal mode.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{MotionKind, Operator};
use crate::app::{App, Selection, Toast};
use crate::editor::Cursor;
use crate::mode::Mode;

impl App {
    pub(super) fn handle_visual_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // `g` is a two-key prefix in visual just like in normal — but
        // visual bypasses the token pipeline, so the one bit of state
        // we need lives on App as `visual_g_pending`.
        if std::mem::take(&mut self.visual_g_pending) {
            match key.code {
                KeyCode::Char('g') => self.buffer.move_file_start(),
                KeyCode::Char('e') => self.apply_visual_motion(MotionKind::WordEndBack),
                KeyCode::Char('E') => self.apply_visual_motion(MotionKind::BigWordEndBack),
                KeyCode::Char('_') => self.apply_visual_motion(MotionKind::LineLastNonBlank),
                // gs/gl/gc/gb — same aliases as Normal mode.
                KeyCode::Char('s') => self.apply_visual_motion(MotionKind::LineFirstNonBlank),
                KeyCode::Char('l') => self.apply_visual_motion(MotionKind::LineEnd),
                KeyCode::Char('c') => self.apply_visual_motion(MotionKind::ViewportMiddle),
                KeyCode::Char('b') => self.apply_visual_motion(MotionKind::ViewportBottom),
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
            // `I` / `A` — enter Insert at the start / end of the selection.
            // Block mode fans out: one cursor per selected row so each
            // typed char repeats down the column.
            KeyCode::Char('I') => self.visual_insert_at_start(),
            KeyCode::Char('A') => self.visual_append_at_end(),
            // `S` / `R` — linewise change (force).
            KeyCode::Char('S') | KeyCode::Char('R') => self.visual_change_lines(),
            // `C` — linewise change everywhere except VisualBlock, where
            // it changes from c0 to EOL on each row (with fan-out).
            KeyCode::Char('C') => self.visual_change_to_eol(),
            // `D` — linewise delete everywhere except VisualBlock, where
            // it deletes from c0 to EOL on each row.
            KeyCode::Char('D') => self.visual_delete_to_eol(),
            // `X` — always linewise delete.
            KeyCode::Char('X') => self.visual_delete_lines(),
            // `Y` — always linewise yank.
            KeyCode::Char('Y') => self.visual_yank_lines(),
            // `u` / `U` — lowercase / uppercase the selection. Vim
            // semantics: bare `u` in visual is *not* undo, it lowercases.
            KeyCode::Char('u') => {
                self.transform_case_selection(crate::editor::to_lower_keep_width)
            }
            KeyCode::Char('U') => {
                self.transform_case_selection(crate::editor::to_upper_keep_width)
            }
            KeyCode::Char('~') => self.toggle_case_selection(),
            KeyCode::Char('J') => self.join_selection_lines(),
            KeyCode::Char('>') => self.indent_selection(true),
            KeyCode::Char('<') => self.indent_selection(false),
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
    fn toggle_case_selection(&mut self) {
        self.transform_case_selection(crate::editor::flip_case_char_keep_width);
    }

    /// `u` / `U` / `~` in visual — apply a per-char transform across
    /// the selection. Charwise covers the span, linewise covers whole
    /// rows, block covers the rectangle. Exits visual when finished.
    fn transform_case_selection(&mut self, f: fn(char) -> char) {
        let Some(sel) = self.selection() else { return };
        self.buffer.snapshot();
        match sel {
            Selection::Char { from, to } => {
                let end = self.buffer.advance_one(to);
                self.buffer.transform_case_range(from, end, f);
            }
            Selection::Line { from_row, to_row } => {
                self.buffer.transform_case_lines(from_row, to_row, f);
            }
            Selection::Block { r0, c0, r1, c1 } => {
                self.buffer.transform_case_block(r0, c0, r1, c1, f);
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
            Selection::Char { from, to } => (from.row, to.row),
            Selection::Line { from_row, to_row } => (from_row, to_row),
            Selection::Block { r0, r1, .. } => (r0, r1),
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

    /// `>` / `<` in visual — shift every line covered by the selection
    /// one indent level right (`indent = true`) or left. Always
    /// line-wise, regardless of the visual sub-mode. Exits to Normal
    /// with the cursor on the first non-blank of the top selected row,
    /// matching vim's landing position.
    fn indent_selection(&mut self, indent: bool) {
        let Some(sel) = self.selection() else { return };
        let (from_row, to_row) = match sel {
            Selection::Char { from, to } => (from.row, to.row),
            Selection::Line { from_row, to_row } => (from_row, to_row),
            Selection::Block { r0, r1, .. } => (r0, r1),
        };
        self.buffer.snapshot();
        let indent_settings = self.indent_settings();
        for r in from_row..=to_row {
            if indent {
                self.buffer.indent_line(r, indent_settings);
            } else {
                self.buffer.dedent_line(r, indent_settings);
            }
        }
        self.buffer.cursor.row = from_row;
        let line = self.buffer.current_line();
        let col = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);
        self.buffer.cursor.col = col;
        self.enter_mode(Mode::Normal);
    }

    /// `I` in visual: position the cursor at the start of the selection
    /// and enter Insert. Block mode adds one extra cursor per row so the
    /// inserted text mirrors down the left edge of the block.
    fn visual_insert_at_start(&mut self) {
        let Some(sel) = self.selection() else { return };
        self.buffer.snapshot();
        self.buffer.extra_cursors.clear();
        match sel {
            Selection::Char { from, .. } => {
                self.buffer.cursor = from;
            }
            Selection::Line { from_row, to_row } => {
                // Fan out: one cursor per row at each row's first
                // non-blank, so typed text replicates down the indent.
                self.buffer.cursor = Cursor {
                    row: from_row,
                    col: first_non_blank(&self.buffer.lines[from_row]),
                };
                for r in (from_row + 1)..=to_row {
                    self.buffer.extra_cursors.push(Cursor {
                        row: r,
                        col: first_non_blank(&self.buffer.lines[r]),
                    });
                }
            }
            Selection::Block { r0, c0, r1, .. } => {
                self.buffer.cursor = Cursor { row: r0, col: c0 };
                for r in (r0 + 1)..=r1 {
                    let len = self.buffer.lines[r].chars().count();
                    self.buffer.extra_cursors.push(Cursor {
                        row: r,
                        col: c0.min(len),
                    });
                }
            }
        }
        self.enter_mode(Mode::Insert);
    }

    /// `A` in visual: position the cursor just past the end of the
    /// selection and enter Insert. Block mode mirrors the cursor onto
    /// each selected row at `c1 + 1` (clamped to that row's length).
    fn visual_append_at_end(&mut self) {
        let Some(sel) = self.selection() else { return };
        self.buffer.snapshot();
        self.buffer.extra_cursors.clear();
        match sel {
            Selection::Char { to, .. } => {
                let len = self.buffer.lines[to.row].chars().count();
                self.buffer.cursor = Cursor {
                    row: to.row,
                    col: (to.col + 1).min(len),
                };
            }
            Selection::Line { from_row, to_row } => {
                // Fan out: each row's cursor lands at its own EOL.
                self.buffer.cursor = Cursor {
                    row: from_row,
                    col: self.buffer.lines[from_row].chars().count(),
                };
                for r in (from_row + 1)..=to_row {
                    self.buffer.extra_cursors.push(Cursor {
                        row: r,
                        col: self.buffer.lines[r].chars().count(),
                    });
                }
            }
            Selection::Block { r0, r1, c1, .. } => {
                let primary_len = self.buffer.lines[r0].chars().count();
                self.buffer.cursor = Cursor {
                    row: r0,
                    col: (c1 + 1).min(primary_len),
                };
                for r in (r0 + 1)..=r1 {
                    let len = self.buffer.lines[r].chars().count();
                    self.buffer.extra_cursors.push(Cursor {
                        row: r,
                        col: (c1 + 1).min(len),
                    });
                }
            }
        }
        self.enter_mode(Mode::Insert);
    }

    /// `S` / `R` / `C` (non-block) in visual — drop every covered row
    /// and enter Insert. Cursor lands at col 0 of the deleted region.
    fn visual_change_lines(&mut self) {
        let Some((from_row, to_row)) = self.selection_row_span() else {
            return;
        };
        self.buffer.snapshot();
        self.buffer.extra_cursors.clear();
        self.buffer.delete_lines(from_row, to_row);
        self.buffer.cursor.col = 0;
        self.enter_mode(Mode::Insert);
    }

    /// `C` in visual — linewise change everywhere except VisualBlock,
    /// where it changes from `c0` to EOL on each row and fans cursors
    /// out so the user's typed text replicates down the column.
    fn visual_change_to_eol(&mut self) {
        let Some(sel) = self.selection() else { return };
        match sel {
            Selection::Block { r0, c0, r1, .. } => {
                self.buffer.snapshot();
                self.buffer.extra_cursors.clear();
                truncate_block_to_eol(self, r0, c0, r1);
                self.buffer.cursor = Cursor { row: r0, col: c0 };
                for r in (r0 + 1)..=r1 {
                    let len = self.buffer.lines[r].chars().count();
                    self.buffer.extra_cursors.push(Cursor {
                        row: r,
                        col: c0.min(len),
                    });
                }
                self.enter_mode(Mode::Insert);
            }
            _ => self.visual_change_lines(),
        }
    }

    /// `D` in visual — linewise delete everywhere except VisualBlock,
    /// where it deletes from `c0` to EOL on each row. Exits to Normal.
    fn visual_delete_to_eol(&mut self) {
        let Some(sel) = self.selection() else { return };
        match sel {
            Selection::Block { r0, c0, r1, .. } => {
                self.buffer.snapshot();
                truncate_block_to_eol(self, r0, c0, r1);
                self.buffer.cursor = Cursor { row: r0, col: c0 };
                self.buffer.clamp_col(false);
                self.enter_mode(Mode::Normal);
            }
            _ => self.visual_delete_lines(),
        }
    }

    /// `X` (and `D` for non-block) — drop every covered row.
    fn visual_delete_lines(&mut self) {
        let Some((from_row, to_row)) = self.selection_row_span() else {
            return;
        };
        self.buffer.snapshot();
        self.buffer.delete_lines(from_row, to_row);
        self.enter_mode(Mode::Normal);
    }

    /// `Y` — always linewise yank.
    fn visual_yank_lines(&mut self) {
        let Some((from_row, to_row)) = self.selection_row_span() else {
            return;
        };
        self.buffer.yank_lines(from_row, to_row);
        self.sync_yank_to_clipboard();
        self.push_toast(Toast::info("yanked"));
        self.buffer.cursor.row = from_row;
        self.buffer.cursor.col = 0;
        self.enter_mode(Mode::Normal);
    }

    /// Common helper: rows covered by the current selection, regardless
    /// of its sub-mode flavour.
    fn selection_row_span(&self) -> Option<(usize, usize)> {
        Some(match self.selection()? {
            Selection::Char { from, to } => (from.row, to.row),
            Selection::Line { from_row, to_row } => (from_row, to_row),
            Selection::Block { r0, r1, .. } => (r0, r1),
        })
    }

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
                        self.sync_yank_to_clipboard();
                        self.push_toast(Toast::info("yanked"));
                        self.buffer.cursor = from;
                    }
                    Operator::Delete => self.buffer.delete_range(from, end),
                    Operator::Change => {
                        self.buffer.delete_range(from, end);
                        self.enter_mode(Mode::Insert);
                    }
                    Operator::Indent | Operator::Dedent => {
                        unreachable!("indent/dedent dispatched via indent_selection")
                    }
                }
            }
            Selection::Line { from_row, to_row } => match op {
                Operator::Yank => {
                    self.buffer.yank_lines(from_row, to_row);
                    self.sync_yank_to_clipboard();
                    self.push_toast(Toast::info("yanked"));
                    self.buffer.cursor.row = from_row;
                    self.buffer.cursor.col = 0;
                }
                Operator::Delete => self.buffer.delete_lines(from_row, to_row),
                Operator::Change => {
                    self.buffer.delete_lines(from_row, to_row);
                    self.enter_mode(Mode::Insert);
                }
                Operator::Indent | Operator::Dedent => {
                    unreachable!("indent/dedent dispatched via indent_selection")
                }
            },
            Selection::Block { r0, c0, r1, c1 } => match op {
                Operator::Yank => {
                    self.buffer.yank_block(r0, c0, r1, c1);
                    self.sync_yank_to_clipboard();
                    self.push_toast(Toast::info("yanked"));
                    self.buffer.cursor = Cursor { row: r0, col: c0 };
                }
                Operator::Delete => self.buffer.delete_block(r0, c0, r1, c1),
                Operator::Change => {
                    // Fan-out: after the block delete, each row gets its
                    // own cursor at the left edge so typed text repeats
                    // down the column when Insert mode finishes.
                    self.buffer.extra_cursors.clear();
                    self.buffer.delete_block(r0, c0, r1, c1);
                    self.buffer.cursor = Cursor { row: r0, col: c0 };
                    for r in (r0 + 1)..=r1 {
                        let len = self.buffer.lines[r].chars().count();
                        self.buffer.extra_cursors.push(Cursor {
                            row: r,
                            col: c0.min(len),
                        });
                    }
                    self.enter_mode(Mode::Insert);
                }
                Operator::Indent | Operator::Dedent => {
                    unreachable!("indent/dedent dispatched via indent_selection")
                }
            },
        }
    }
}

fn first_non_blank(line: &str) -> usize {
    line.chars().position(|c| !c.is_whitespace()).unwrap_or(0)
}

/// `D`/`C` in VisualBlock — chop every row in `[r0..=r1]` so its
/// content stops at column `c0`. Implements the per-row right-side
/// truncation that `delete_block` doesn't cover (delete_block needs
/// a finite `c1`, and the longest row may not match the visual `c1`).
/// Cursor/yank are left to the caller.
fn truncate_block_to_eol(app: &mut App, r0: usize, c0: usize, r1: usize) {
    let r1 = r1.min(app.buffer.lines.len().saturating_sub(1));
    let max_len = (r0..=r1)
        .map(|r| app.buffer.lines[r].chars().count())
        .max()
        .unwrap_or(0);
    if max_len <= c0 {
        return;
    }
    // delete_block computes `hi = (c1 + 1).min(line_len)`, so passing
    // `max_len.saturating_sub(1)` covers every row to its own EOL.
    app.buffer
        .delete_block(r0, c0, r1, max_len.saturating_sub(1));
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
