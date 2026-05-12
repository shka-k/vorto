use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::action::MotionKind;

#[derive(Debug, Default)]
pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub yank: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

impl Buffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Default::default()
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = if path.exists() {
            fs::read_to_string(path)?
        } else {
            String::new()
        };
        let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Ok(Self {
            lines,
            cursor: Cursor::default(),
            path: Some(path.to_path_buf()),
            dirty: false,
            yank: String::new(),
        })
    }

    pub fn save(&mut self) -> Result<()> {
        if let Some(path) = &self.path {
            fs::write(path, self.lines.join("\n"))?;
            self.dirty = false;
        }
        Ok(())
    }

    pub fn save_as(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        self.save()
    }

    pub fn current_line(&self) -> &str {
        &self.lines[self.cursor.row]
    }

    pub fn current_line_len(&self) -> usize {
        self.current_line().chars().count()
    }

    pub fn clamp_col(&mut self, allow_after_end: bool) {
        let max = self.current_line_len();
        let limit = if allow_after_end || max == 0 {
            max
        } else {
            max - 1
        };
        if self.cursor.col > limit {
            self.cursor.col = limit;
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
    }

    pub fn move_right(&mut self, allow_after_end: bool) {
        let max = self.current_line_len();
        let limit = if allow_after_end || max == 0 {
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
        let max = self.current_line_len();
        self.cursor.col = max.saturating_sub(1);
    }

    pub fn move_file_start(&mut self) {
        self.cursor.row = 0;
        self.cursor.col = 0;
    }

    pub fn move_file_end(&mut self) {
        self.cursor.row = self.lines.len().saturating_sub(1);
        self.clamp_col(false);
    }

    pub fn move_word_forward(&mut self) {
        let line = self.current_line().to_string();
        let chars: Vec<char> = line.chars().collect();
        let mut i = self.cursor.col;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() && self.cursor.row + 1 < self.lines.len() {
            self.cursor.row += 1;
            self.cursor.col = 0;
        } else {
            self.cursor.col = i.min(chars.len().saturating_sub(1));
        }
    }

    pub fn move_word_backward(&mut self) {
        let line = self.current_line().to_string();
        let chars: Vec<char> = line.chars().collect();
        if self.cursor.col == 0 {
            if self.cursor.row > 0 {
                self.cursor.row -= 1;
                self.cursor.col = self.current_line_len().saturating_sub(1);
            }
            return;
        }
        let mut i = self.cursor.col;
        i = i.saturating_sub(1);
        while i > 0 && chars[i].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor.col = i;
    }

    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor.row];
        let byte_idx = char_to_byte(line, self.cursor.col);
        line.insert(byte_idx, c);
        self.cursor.col += 1;
        self.dirty = true;
    }

    pub fn insert_newline(&mut self) {
        let line = self.lines[self.cursor.row].clone();
        let byte_idx = char_to_byte(&line, self.cursor.col);
        let (left, right) = line.split_at(byte_idx);
        self.lines[self.cursor.row] = left.to_string();
        self.lines.insert(self.cursor.row + 1, right.to_string());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.dirty = true;
    }

    pub fn insert_line_below(&mut self) {
        self.lines.insert(self.cursor.row + 1, String::new());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.dirty = true;
    }

    pub fn insert_line_above(&mut self) {
        self.lines.insert(self.cursor.row, String::new());
        self.cursor.col = 0;
        self.dirty = true;
    }

    pub fn delete_char_under_cursor(&mut self) {
        let line = &mut self.lines[self.cursor.row];
        if self.cursor.col < line.chars().count() {
            let byte_idx = char_to_byte(line, self.cursor.col);
            let ch = line[byte_idx..].chars().next().unwrap();
            line.replace_range(byte_idx..byte_idx + ch.len_utf8(), "");
            self.dirty = true;
            self.clamp_col(false);
        }
    }

    pub fn delete_char_before(&mut self) {
        if self.cursor.col > 0 {
            let line = &mut self.lines[self.cursor.row];
            let byte_idx = char_to_byte(line, self.cursor.col - 1);
            let ch = line[byte_idx..].chars().next().unwrap();
            line.replace_range(byte_idx..byte_idx + ch.len_utf8(), "");
            self.cursor.col -= 1;
            self.dirty = true;
        } else if self.cursor.row > 0 {
            let line = self.lines.remove(self.cursor.row);
            self.cursor.row -= 1;
            self.cursor.col = self.current_line_len();
            self.lines[self.cursor.row].push_str(&line);
            self.dirty = true;
        }
    }

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
        self.dirty = true;
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
        self.dirty = true;
    }

    // ────────────────────────────────────────────────────────────────────
    // Range-aware motion + edit helpers (for operator + motion: dw, yw, …)
    // ────────────────────────────────────────────────────────────────────

    /// Where would `motion` (repeated `count` times) land if started from
    /// `from`? Pure — does not modify cursor state.
    pub fn motion_target(&self, from: Cursor, motion: MotionKind, count: u32) -> Cursor {
        let mut c = from;
        for _ in 0..count.max(1) {
            c = self.motion_step(c, motion);
        }
        c
    }

    fn motion_step(&self, from: Cursor, motion: MotionKind) -> Cursor {
        use MotionKind as M;
        match motion {
            M::Left => Cursor {
                row: from.row,
                col: from.col.saturating_sub(1),
            },
            M::Right => {
                let max = self.lines[from.row].chars().count();
                let limit = max.saturating_sub(1);
                Cursor {
                    row: from.row,
                    col: (from.col + 1).min(limit),
                }
            }
            M::Up => {
                let row = from.row.saturating_sub(1);
                let max = self.lines[row].chars().count();
                Cursor {
                    row,
                    col: from.col.min(max.saturating_sub(1)),
                }
            }
            M::Down => {
                let row = (from.row + 1).min(self.lines.len().saturating_sub(1));
                let max = self.lines[row].chars().count();
                Cursor {
                    row,
                    col: from.col.min(max.saturating_sub(1)),
                }
            }
            M::LineStart => Cursor {
                row: from.row,
                col: 0,
            },
            M::LineEnd => {
                let max = self.lines[from.row].chars().count();
                Cursor {
                    row: from.row,
                    col: max.saturating_sub(1),
                }
            }
            M::WordForward => self.peek_word_forward(from),
            M::WordBack => self.peek_word_back(from),
            M::FileStart => Cursor { row: 0, col: 0 },
            M::FileEnd => {
                let last = self.lines.len().saturating_sub(1);
                let max = self.lines[last].chars().count();
                Cursor {
                    row: last,
                    col: max.saturating_sub(1),
                }
            }
            // Search-relative motions need search state — caller computes
            // those separately (see App::jump_search).
            M::SearchNext | M::SearchPrev => from,
        }
    }

    fn peek_word_forward(&self, from: Cursor) -> Cursor {
        let chars: Vec<char> = self.lines[from.row].chars().collect();
        let mut i = from.col;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() && from.row + 1 < self.lines.len() {
            Cursor {
                row: from.row + 1,
                col: 0,
            }
        } else {
            Cursor {
                row: from.row,
                col: i.min(chars.len().saturating_sub(1)),
            }
        }
    }

    fn peek_word_back(&self, from: Cursor) -> Cursor {
        let chars: Vec<char> = self.lines[from.row].chars().collect();
        if from.col == 0 {
            if from.row > 0 {
                let row = from.row - 1;
                let col = self.lines[row].chars().count().saturating_sub(1);
                return Cursor { row, col };
            }
            return from;
        }
        let mut i = from.col.saturating_sub(1);
        while i > 0 && chars[i].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        Cursor {
            row: from.row,
            col: i,
        }
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
        self.dirty = true;
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
}

fn order(a: Cursor, b: Cursor) -> (Cursor, Cursor) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}
