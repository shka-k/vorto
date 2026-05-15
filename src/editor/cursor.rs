//! Cursor primitives: arrow-key motions, line/file edges, column clamp,
//! and the one-step `advance_one` helper. Word/paragraph/find/viewport
//! motions live in [`super::motion`]; this file is the bare cursor
//! arithmetic the rest of the editor builds on.

use super::{Buffer, Cursor};

impl Buffer {
    pub fn current_line(&self) -> &str {
        &self.lines[self.cursor.row]
    }

    pub fn current_line_len(&self) -> usize {
        self.current_line().chars().count()
    }

    pub fn clamp_col(&mut self, allow_after_end: bool) {
        let max = self.current_line_len();
        let limit = if allow_after_end {
            max
        } else {
            max.saturating_sub(1)
        };
        self.cursor.col = self.cursor.col.min(limit);
    }

    pub fn move_left(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
    }

    pub fn move_right(&mut self, allow_after_end: bool) {
        let max = self.current_line_len();
        let limit = if allow_after_end {
            max
        } else {
            max.saturating_sub(1)
        };
        if self.cursor.col < limit {
            self.cursor.col += 1;
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor.row > 0 {
            self.cursor.row -= 1;
            self.clamp_col(false);
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor.row + 1 < self.lines.len() {
            self.cursor.row += 1;
            self.clamp_col(false);
        }
    }

    pub fn move_line_start(&mut self) {
        self.cursor.col = 0;
    }

    pub fn move_line_end(&mut self) {
        self.cursor.col = self.current_line_len().saturating_sub(1);
    }

    pub fn move_file_start(&mut self) {
        self.cursor.row = 0;
        self.cursor.col = 0;
    }

    pub fn move_file_end(&mut self) {
        self.cursor.row = self.lines.len().saturating_sub(1);
        self.cursor.col = 0;
        self.clamp_col(false);
    }

    /// Cursor one position past `c` (next char on the line, or first
    /// column of the next line if at line end). Used to turn an
    /// inclusive visual endpoint into the exclusive form `delete_range`
    /// and `yank_range` expect.
    pub fn advance_one(&self, c: Cursor) -> Cursor {
        let line_len = self.lines[c.row].chars().count();
        if c.col < line_len {
            Cursor {
                row: c.row,
                col: c.col + 1,
            }
        } else if c.row + 1 < self.lines.len() {
            Cursor {
                row: c.row + 1,
                col: 0,
            }
        } else {
            c
        }
    }
}
