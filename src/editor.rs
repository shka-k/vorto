use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::action::{MotionKind, Object, Scope};

#[derive(Debug, Default)]
pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub yank: String,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
}

/// Frozen buffer state for the undo/redo history.
#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Cursor,
    dirty: bool,
}

/// Cap on the undo history so a long editing session doesn't grow without
/// bound. 200 is enough to be useful and well under any RAM concern for
/// small/medium files.
const MAX_UNDO_DEPTH: usize = 200;

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
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
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
    // Undo / redo
    // ────────────────────────────────────────────────────────────────────

    /// Save the current buffer state to the undo stack and clear redo.
    /// Callers should invoke this immediately *before* a mutation so the
    /// stored state represents "what to come back to" on undo.
    pub fn snapshot(&mut self) {
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            dirty: self.dirty,
        });
        self.redo_stack.clear();
        if self.undo_stack.len() > MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
    }

    /// Step back one snapshot. Returns false when the undo stack is empty.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            dirty: self.dirty,
        });
        self.lines = prev.lines;
        self.cursor = prev.cursor;
        self.dirty = prev.dirty;
        self.clamp_col(false);
        true
    }

    /// Step forward through redo history. Returns false when empty.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            dirty: self.dirty,
        });
        self.lines = next.lines;
        self.cursor = next.cursor;
        self.dirty = next.dirty;
        self.clamp_col(false);
        true
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

    /// Find the cursor range covered by a text object on the current line.
    /// Returns `None` if no matching object surrounds (or is to the right
    /// of) the cursor — caller should report "no match" to the user.
    ///
    /// Scope semantics:
    ///   - `Inner`  → the content *between* the delimiters
    ///   - `Around` → the content *plus* the delimiters
    ///
    /// Currently restricted to the cursor's line — multi-line text
    /// objects (e.g. `i{` spanning many lines) are out of scope.
    pub fn text_object_range(&self, scope: Scope, object: Object) -> Option<(Cursor, Cursor)> {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let chars: Vec<char> = self.lines[row].chars().collect();
        let (open, close) = delim(object);

        // Find the matching pair on the line.
        let (start_col, end_col) = if open == close {
            // Symmetric (quote-like): the nearest `open` <= col and the next `open` > col.
            let left = (0..=col).rev().find(|&i| chars.get(i) == Some(&open))?;
            let right = ((left + 1)..chars.len()).find(|&i| chars[i] == open)?;
            (left, right)
        } else {
            // Asymmetric (bracket-like): track depth so nested pairs match.
            let left = find_open_left(&chars, col, open, close)?;
            let right = find_close_right(&chars, left, open, close)?;
            (left, right)
        };

        let (from_col, to_col) = match scope {
            Scope::Inner => (start_col + 1, end_col),
            Scope::Around => (start_col, end_col + 1),
        };
        Some((
            Cursor { row, col: from_col },
            Cursor { row, col: to_col },
        ))
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

fn delim(object: Object) -> (char, char) {
    match object {
        Object::DoubleQuote => ('"', '"'),
        Object::SingleQuote => ('\'', '\''),
        Object::Paren => ('(', ')'),
        Object::Brace => ('{', '}'),
        Object::Bracket => ('[', ']'),
        // `iw` (inner word) — not really a delimited object; treat as a
        // pair of non-existent chars so callers can branch. For now, the
        // word object isn't matched specially: callers can use
        // motion_target(WordBack/WordForward) instead.
        Object::Word => ('\0', '\0'),
    }
}

/// Search left (and including) `from` for an unmatched `open`. Tracks
/// nesting depth so `({foo})` with cursor inside the inner braces sees
/// the inner `{`, not the outer `(`.
fn find_open_left(chars: &[char], from: usize, open: char, close: char) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = from;
    loop {
        let c = chars[i];
        if c == close {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

/// Search right from after an `open` position for the matching `close`,
/// honoring nesting.
fn find_close_right(chars: &[char], open_pos: usize, open: char, close: char) -> Option<usize> {
    let mut depth: i32 = 0;
    for (i, &c) in chars.iter().enumerate().skip(open_pos + 1) {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}
