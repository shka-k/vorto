//! Document model: `Buffer` (lines + cursor + undo + yank) and the
//! single-character/single-line editing primitives.
//!
//! Larger operations live in submodules so this file stays focused on
//! state:
//!
//! - [`motion`] — word/paragraph motions and `motion_target`.
//! - [`text_object`] — `iw`/`ip`/`i(` etc. resolution.
//! - [`ops`] — range/line/block delete + yank + paste.
//! - [`search`] — `/`/`?` find-next state and lookup over the buffer.
//! - [`insert`] — typing, newline, opener/closer auto-pair, dedent.
//! - [`history`] — undo / redo snapshot stacks.

mod history;
mod insert;
mod motion;
mod ops;
mod search;
mod text_object;

pub use search::SearchState;

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::syntax::Highlighter;
use crate::vcs::{self, LineStatus};

#[derive(Default)]
pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    /// Additional cursor positions for multi-cursor editing. The primary
    /// cursor lives in `cursor`; extras are *only* the non-primary ones,
    /// stored in insertion order so a pop semantic ("remove last added")
    /// is a simple `pop()`. Empty in the single-cursor common case.
    pub extra_cursors: Vec<Cursor>,
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
    /// Height (in rows) of the buffer viewport at the last draw. The
    /// UI writes this during `compute_scroll`; motion code reads it
    /// for `H`/`M`/`L` and `<C-d>`/`<C-u>`/`<C-f>`/`<C-b>`. `0` until
    /// the first frame is drawn — motions guard against that.
    pub viewport_height: Cell<usize>,
    /// Visual y (within the buffer viewport) of the row the cursor sits
    /// on at the last draw. Differs from `cursor.row - scroll` when
    /// inline diagnostics push subsequent rows down. The UI writes this
    /// in `draw_buffer`; `place_cursor` and cursor-anchored overlays
    /// read it.
    pub cursor_visual_y: Cell<u16>,
    // `pub` so the sleeping-buffer freezer can take the stacks
    // by move (and reinstall them on thaw) without going through
    // accessor boilerplate. Editor-internal mutations still go
    // through the `snapshot` / `undo` / `redo` methods.
    pub undo_stack: Vec<Snapshot>,
    pub redo_stack: Vec<Snapshot>,
    /// HEAD blob lines captured at file-open time. `None` when the
    /// buffer isn't backed by a file inside a git repo. `Some(empty)`
    /// when the file is in a repo but not yet tracked at HEAD — every
    /// current line will diff as `Added`.
    pub vcs_base: Option<Vec<String>>,
    /// Cached `(version, per-line status)` produced by diffing
    /// `vcs_base` against `lines`. Recomputed lazily when `version`
    /// moves; wrapped in `RefCell` so the UI can refresh it through
    /// the shared `&Buffer` it gets at draw time.
    pub vcs_diff: RefCell<Option<(u64, Vec<Option<LineStatus>>)>>,
}

/// Frozen buffer state for the undo/redo history. Exposed at the
/// crate boundary so the sleeping-buffer compressor can destructure
/// individual snapshots when it freezes a buffer; the editor module
/// itself still owns all the read/write logic.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    /// Multi-cursor extras at snapshot time. Empty when there are no
    /// extras (the common case). Undo restores them along with the
    /// primary cursor so the multi-cursor state round-trips.
    pub extra_cursors: Vec<Cursor>,
    pub dirty: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

/// Knobs the buffer needs to produce an indent string for a freshly
/// inserted line. `width` is the spaces-per-level fallback when a level
/// is added in spaces. `use_tabs` is the tie-breaker when the reference
/// line carries no indent of its own (empty file, top-level statement);
/// when the reference line *does* have leading whitespace, that style is
/// preserved so we don't mix tabs and spaces within a file.
#[derive(Debug, Clone, Copy)]
pub struct IndentSettings {
    pub width: usize,
    pub use_tabs: bool,
}

impl Default for IndentSettings {
    fn default() -> Self {
        Self {
            width: 4,
            use_tabs: false,
        }
    }
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
        let vcs_base = vcs::head_blob_lines(path);
        Ok(Self {
            lines,
            cursor: Cursor::default(),
            extra_cursors: Vec::new(),
            path: Some(path.to_path_buf()),
            dirty: false,
            yank: String::new(),
            version: 0,
            highlighter: None,
            scroll: Cell::new(0),
            viewport_height: Cell::new(0),
            cursor_visual_y: Cell::new(0),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            vcs_base,
            vcs_diff: RefCell::new(None),
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

    /// Re-fetch the HEAD base for this buffer's path. No-op when the
    /// buffer isn't backed by a file. Used by the sleep/wake path so a
    /// `<space>b` round-trip picks up any HEAD movement that happened
    /// while the buffer was inactive.
    pub fn refresh_vcs_base(&mut self) {
        let Some(p) = self.path.as_deref() else {
            return;
        };
        self.vcs_base = vcs::head_blob_lines(p);
        self.vcs_diff.borrow_mut().take();
    }

    /// Per-line VCS statuses, recomputed if the cached version is
    /// stale. Returns an empty slice when this buffer has no base
    /// (not in a git repo, or no path).
    pub fn vcs_statuses(&self) -> Vec<Option<LineStatus>> {
        let Some(base) = self.vcs_base.as_ref() else {
            return Vec::new();
        };
        {
            let cache = self.vcs_diff.borrow();
            if let Some((v, statuses)) = cache.as_ref()
                && *v == self.version
            {
                return statuses.clone();
            }
        }
        let statuses = vcs::diff_line_status(base, &self.lines);
        *self.vcs_diff.borrow_mut() = Some((self.version, statuses.clone()));
        statuses
    }

    /// True when this buffer differs from HEAD (any line marker is
    /// present). Cheap when the cache is hot; otherwise triggers a
    /// recompute. Returns false for buffers without a base.
    pub fn has_vcs_changes(&self) -> bool {
        if self.vcs_base.is_none() {
            return false;
        }
        self.vcs_statuses().iter().any(|s| s.is_some())
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

    /// Join the next line into the current one with a single space
    /// separator (vim's `J`). Strips leading whitespace on the joined
    /// line; if the current line ends in whitespace or is empty, no
    /// space is inserted. Cursor lands on the join boundary.
    pub fn join_next_line(&mut self) {
        if self.cursor.row + 1 >= self.lines.len() {
            return;
        }
        let next = self.lines.remove(self.cursor.row + 1);
        let next_trimmed = next.trim_start();
        let cur = &mut self.lines[self.cursor.row];
        let needs_space = !cur.is_empty()
            && !cur
                .chars()
                .last()
                .map(|c| c.is_whitespace())
                .unwrap_or(false)
            && !next_trimmed.is_empty();
        let join_col = cur.chars().count();
        if needs_space {
            cur.push(' ');
        }
        cur.push_str(next_trimmed);
        self.cursor.col = join_col;
        self.touch();
    }

    /// Toggle the case of the character under the cursor, then advance
    /// one column (vim's `~`). No-op on an empty line.
    pub fn toggle_case_under_cursor(&mut self) {
        let line = &mut self.lines[self.cursor.row];
        if self.cursor.col >= line.chars().count() {
            return;
        }
        let byte_idx = char_to_byte(line, self.cursor.col);
        let ch = line[byte_idx..].chars().next().unwrap();
        let replacement: String = if ch.is_uppercase() {
            ch.to_lowercase().collect()
        } else if ch.is_lowercase() {
            ch.to_uppercase().collect()
        } else {
            return; // not a cased letter — leave it and don't advance
        };
        line.replace_range(byte_idx..byte_idx + ch.len_utf8(), &replacement);
        self.touch();
        // Advance, allowing past-end only inside Insert (we're in Normal
        // here, so clamp to last col).
        let max = self.current_line_len().saturating_sub(1);
        if self.cursor.col < max {
            self.cursor.col += 1;
        }
    }

    /// Delete from `cursor` to the end of the current line (vim's `D`).
    /// The deleted text goes into the yank register.
    pub fn delete_to_eol(&mut self) {
        let line = self.lines[self.cursor.row].clone();
        let byte_idx = char_to_byte(&line, self.cursor.col);
        self.yank = line[byte_idx..].to_string();
        self.lines[self.cursor.row].truncate(byte_idx);
        self.touch();
        self.clamp_col(false);
    }

    /// Replace the entire current line with an empty string (vim's
    /// `S`). The full line content goes into the yank register.
    pub fn clear_current_line(&mut self) {
        self.yank = self.lines[self.cursor.row].clone();
        self.lines[self.cursor.row].clear();
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

    /// Toggle a single-line comment on the current line using `token`
    /// as the prefix (e.g. `"//"`, `"#"`). If the first non-blank run
    /// of the line already starts with `token`, the prefix (and a
    /// single trailing space, when present) is stripped; otherwise
    /// `token + " "` is inserted at the first non-blank column. Blank
    /// lines are skipped — vim-commentary semantics.
    pub fn toggle_line_comment(&mut self, token: &str) {
        let row = self.cursor.row;
        let line = &self.lines[row];
        let indent_chars = line.chars().take_while(|c| c.is_whitespace()).count();
        let indent_bytes = char_to_byte(line, indent_chars);
        let rest = &line[indent_bytes..];
        if rest.is_empty() {
            return;
        }
        if let Some(after_token) = rest.strip_prefix(token) {
            let trim_len = if after_token.starts_with(' ') {
                token.len() + 1
            } else {
                token.len()
            };
            self.lines[row].replace_range(indent_bytes..indent_bytes + trim_len, "");
            let removed_chars = token.chars().count() + (trim_len - token.len());
            if self.cursor.col > indent_chars {
                self.cursor.col = self.cursor.col.saturating_sub(removed_chars);
            }
        } else {
            let insert = format!("{} ", token);
            self.lines[row].insert_str(indent_bytes, &insert);
            let added_chars = insert.chars().count();
            if self.cursor.col >= indent_chars {
                self.cursor.col += added_chars;
            }
        }
        self.clamp_col(false);
        self.touch();
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

