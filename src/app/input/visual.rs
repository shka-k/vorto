//! Visual-mode key handling: motion application, operator dispatch
//! (`y`/`d`/`c`/`~`/`J`), and the `g` two-key prefix that mirrors
//! Normal mode.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{MotionKind, Operator};
use crate::app::{App, Selection, Status};
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
            Selection::Char { from, to } => {
                let end = self.buffer.advance_one(to);
                self.buffer.toggle_case_range(from, end);
            }
            Selection::Line { from_row, to_row } => {
                self.buffer.toggle_case_lines(from_row, to_row);
            }
            Selection::Block { r0, c0, r1, c1 } => {
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
