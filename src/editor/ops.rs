//! Range-level edits and the yank register.
//!
//! Buffer mutations that operate on a span (line, char range, column
//! block) and stash the deleted/copied text into `Buffer.yank`. Single-
//! character edits live in [`super`] alongside the buffer state itself.

use super::{Buffer, Cursor, char_to_byte};

impl Buffer {
    pub fn delete_line(&mut self) {
        if self.lines.len() == 1 {
            self.yank = self.lines[0].clone();
            self.lines[0].clear();
        } else {
            self.yank = self.lines.remove(self.cursor.row);
            if self.cursor.row >= self.lines.len() {
                self.cursor.row = self.lines.len() - 1;
            }
        }
        self.clamp_col(false);
        self.touch();
    }

    pub fn yank_line(&mut self) {
        self.yank = self.lines[self.cursor.row].clone();
    }

    pub fn paste_after(&mut self) {
        if self.yank.is_empty() {
            return;
        }
        self.lines.insert(self.cursor.row + 1, self.yank.clone());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.touch();
    }

    /// Remove text between two cursors (inclusive of `from`, exclusive of
    /// `to`). The order of `from`/`to` doesn't matter — they're sorted
    /// internally. After deletion the cursor lands at the lower endpoint.
    pub fn delete_range(&mut self, from: Cursor, to: Cursor) {
        let (from, to) = order(from, to);
        if from == to {
            return;
        }
        if from.row == to.row {
            let line = &mut self.lines[from.row];
            let fb = char_to_byte(line, from.col);
            let tb = char_to_byte(line, to.col);
            line.replace_range(fb..tb, "");
        } else {
            let from_byte = char_to_byte(&self.lines[from.row], from.col);
            let to_byte = char_to_byte(&self.lines[to.row], to.col);
            let head: String = self.lines[from.row][..from_byte].to_string();
            let tail: String = self.lines[to.row][to_byte..].to_string();
            self.lines[from.row] = head + &tail;
            let drain_end = (to.row + 1).min(self.lines.len());
            self.lines.drain((from.row + 1)..drain_end);
        }
        self.cursor = from;
        self.clamp_col(false);
        self.touch();
    }

    /// Copy text between two cursors into the yank register.
    pub fn yank_range(&mut self, from: Cursor, to: Cursor) {
        let (from, to) = order(from, to);
        if from == to {
            self.yank.clear();
            return;
        }
        if from.row == to.row {
            let line = &self.lines[from.row];
            let fb = char_to_byte(line, from.col);
            let tb = char_to_byte(line, to.col);
            self.yank = line[fb..tb].to_string();
        } else {
            let mut text = String::new();
            let from_byte = char_to_byte(&self.lines[from.row], from.col);
            text.push_str(&self.lines[from.row][from_byte..]);
            text.push('\n');
            for i in (from.row + 1)..to.row {
                text.push_str(&self.lines[i]);
                text.push('\n');
            }
            let to_byte = char_to_byte(&self.lines[to.row], to.col);
            text.push_str(&self.lines[to.row][..to_byte]);
            self.yank = text;
        }
    }

    /// Yank a run of whole lines (inclusive of both endpoints).
    pub fn yank_lines(&mut self, from_row: usize, to_row: usize) {
        let (a, b) = (from_row.min(to_row), from_row.max(to_row));
        let b = b.min(self.lines.len().saturating_sub(1));
        self.yank = self.lines[a..=b].join("\n");
    }

    /// Delete a run of whole lines (inclusive). Also stashes them in
    /// the yank register, matching vim's `dd` / visual-line `d`.
    pub fn delete_lines(&mut self, from_row: usize, to_row: usize) {
        let (a, b) = (from_row.min(to_row), from_row.max(to_row));
        let b = b.min(self.lines.len().saturating_sub(1));
        self.yank = self.lines[a..=b].join("\n");
        if a == 0 && b + 1 >= self.lines.len() {
            self.lines.clear();
            self.lines.push(String::new());
            self.cursor.row = 0;
        } else {
            self.lines.drain(a..=b);
            self.cursor.row = a.min(self.lines.len().saturating_sub(1));
        }
        self.cursor.col = 0;
        self.clamp_col(false);
        self.touch();
    }

    /// Yank a column rectangle `[r0..=r1] × [c0..=c1]` into the yank
    /// register, rows joined by `\n`. Lines shorter than `c1` simply
    /// contribute their truncated slice.
    pub fn yank_block(&mut self, r0: usize, c0: usize, r1: usize, c1: usize) {
        let (r0, r1) = (r0.min(r1), r0.max(r1));
        let (c0, c1) = (c0.min(c1), c0.max(c1));
        let r1 = r1.min(self.lines.len().saturating_sub(1));
        let mut text = String::new();
        for r in r0..=r1 {
            if r > r0 {
                text.push('\n');
            }
            let line = &self.lines[r];
            let chars: Vec<char> = line.chars().collect();
            let lo = c0.min(chars.len());
            let hi = (c1 + 1).min(chars.len());
            if lo < hi {
                text.extend(&chars[lo..hi]);
            }
        }
        self.yank = text;
    }

    /// Delete a column rectangle, stashing into yank. Shorter lines are
    /// trimmed at their end rather than padded.
    pub fn delete_block(&mut self, r0: usize, c0: usize, r1: usize, c1: usize) {
        let (r0, r1) = (r0.min(r1), r0.max(r1));
        let (c0, c1) = (c0.min(c1), c0.max(c1));
        let r1 = r1.min(self.lines.len().saturating_sub(1));
        self.yank_block(r0, c0, r1, c1);
        for r in r0..=r1 {
            let line = self.lines[r].clone();
            let nchars = line.chars().count();
            let lo = c0.min(nchars);
            let hi = (c1 + 1).min(nchars);
            if lo >= hi {
                continue;
            }
            let lo_b = char_to_byte(&line, lo);
            let hi_b = char_to_byte(&line, hi);
            self.lines[r].replace_range(lo_b..hi_b, "");
        }
        self.cursor.row = r0;
        self.cursor.col = c0;
        self.clamp_col(false);
        self.touch();
    }
}

fn order(a: Cursor, b: Cursor) -> (Cursor, Cursor) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}
