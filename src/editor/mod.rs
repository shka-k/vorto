//! Document model: `Buffer` (lines + cursor + undo + yank) and the
//! single-character/single-line editing primitives.
//!
//! Larger operations live in submodules so this file stays focused on
//! state:
//!
//! - [`motion`] — word/paragraph motions and `motion_target`.
//! - [`text_object`] — `iw`/`ip`/`i(` etc. resolution.
//! - [`ops`] — range/line/block delete + yank + paste.

mod motion;
mod ops;
mod text_object;

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::highlight::Highlighter;

#[derive(Default)]
pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub yank: String,
    /// Monotonically increases on every content-modifying call. Used by
    /// the highlighter to decide whether its cached tree is stale.
    pub version: u64,
    /// Per-buffer tree-sitter state, attached at file-open time when a
    /// matching grammar + query are available. `None` means "no syntax
    /// highlighting for this buffer", which is the safe fallback.
    pub highlighter: Option<Highlighter>,
    /// Topmost line currently visible in the viewport. Sticky — only
    /// moved when the cursor would otherwise leave the viewport (the
    /// UI layer updates it during `draw_buffer`, so it's wrapped in
    /// `Cell` to stay reachable through a shared `&Buffer`).
    pub scroll: Cell<usize>,
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
        let text = fs::read_to_string(path)?;
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
            version: 0,
            highlighter: None,
            scroll: Cell::new(0),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        })
    }

    pub fn save(&mut self) -> Result<()> {
        if let Some(p) = &self.path {
            fs::write(p, self.lines.join("\n"))?;
            self.dirty = false;
        }
        Ok(())
    }

    pub fn save_as(&mut self, path: &Path) -> Result<()> {
        fs::write(path, self.lines.join("\n"))?;
        self.path = Some(path.to_path_buf());
        self.dirty = false;
        Ok(())
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.version = self.version.wrapping_add(1);
    }

    /// Bump the version counter without touching `dirty`. Used when an
    /// external rewriter (e.g. LSP workspace edit application) wants to
    /// invalidate cached highlights without otherwise altering state.
    pub fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub fn refresh_highlights(&mut self) {
        let Some(h) = self.highlighter.as_mut() else {
            return;
        };
        let source = self.lines.join("\n");
        h.refresh(&source, self.version);
    }

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

    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor.row];
        let byte_idx = char_to_byte(line, self.cursor.col);
        line.insert(byte_idx, c);
        self.cursor.col += 1;
        self.touch();
    }

    pub fn insert_newline(&mut self) {
        let line = self.lines[self.cursor.row].clone();
        let byte_idx = char_to_byte(&line, self.cursor.col);
        let (left, right) = line.split_at(byte_idx);
        self.lines[self.cursor.row] = left.to_string();
        self.lines.insert(self.cursor.row + 1, right.to_string());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.touch();
    }

    pub fn insert_line_below(&mut self) {
        self.lines.insert(self.cursor.row + 1, String::new());
        self.cursor.row += 1;
        self.cursor.col = 0;
        self.touch();
    }

    pub fn insert_line_above(&mut self) {
        self.lines.insert(self.cursor.row, String::new());
        self.cursor.col = 0;
        self.touch();
    }

    pub fn delete_char_under_cursor(&mut self) {
        let line = &mut self.lines[self.cursor.row];
        if self.cursor.col < line.chars().count() {
            let byte_idx = char_to_byte(line, self.cursor.col);
            let ch = line[byte_idx..].chars().next().unwrap();
            line.replace_range(byte_idx..byte_idx + ch.len_utf8(), "");
            self.touch();
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
            self.touch();
        } else if self.cursor.row > 0 {
            // Join with the previous line.
            let line = self.lines.remove(self.cursor.row);
            self.cursor.row -= 1;
            self.cursor.col = self.lines[self.cursor.row].chars().count();
            self.lines[self.cursor.row].push_str(&line);
            self.touch();
        }
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
            lines: std::mem::replace(&mut self.lines, prev.lines),
            cursor: std::mem::replace(&mut self.cursor, prev.cursor),
            dirty: std::mem::replace(&mut self.dirty, prev.dirty),
        });
        self.version = self.version.wrapping_add(1);
        true
    }

    /// Reapply the most recently undone snapshot.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(Snapshot {
            lines: std::mem::replace(&mut self.lines, next.lines),
            cursor: std::mem::replace(&mut self.cursor, next.cursor),
            dirty: std::mem::replace(&mut self.dirty, next.dirty),
        });
        self.version = self.version.wrapping_add(1);
        true
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

// ────────────────────────────────────────────────────────────────────────
// Shared helpers, available to all editor submodules.
// ────────────────────────────────────────────────────────────────────────

/// Convert a 0-based character index into the corresponding byte offset
/// in `s`. Past-the-end indices clamp to `s.len()` so callers can use
/// the result as an exclusive end without bounds checking.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Punct,
    Space,
}

fn classify(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

fn is_blank_line(line: &str) -> bool {
    line.chars().all(|c| c.is_whitespace())
}
