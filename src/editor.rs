use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::action::{MotionKind, Object, Scope};
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
            version: 0,
            highlighter: None,
            scroll: Cell::new(0),
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

    /// Mark the buffer dirty and bump the version counter. Every
    /// content-mutating call funnels through here so the highlighter
    /// can detect staleness with a single integer compare.
    fn touch(&mut self) {
        self.dirty = true;
        self.version = self.version.wrapping_add(1);
    }

    /// Run the attached highlighter against the current buffer text if
    /// the cached parse is older than the buffer version. No-op when
    /// no highlighter is attached.
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
        self.cursor = self.peek_word_forward(self.cursor);
    }

    pub fn move_word_backward(&mut self) {
        self.cursor = self.peek_word_back(self.cursor);
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
            let line = self.lines.remove(self.cursor.row);
            self.cursor.row -= 1;
            self.cursor.col = self.current_line_len();
            self.lines[self.cursor.row].push_str(&line);
            self.touch();
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
        self.version = self.version.wrapping_add(1);
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
        self.version = self.version.wrapping_add(1);
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
            M::ParagraphForward => Cursor {
                row: paragraph_forward_row(&self.lines, from.row),
                col: 0,
            },
            M::ParagraphBack => Cursor {
                row: paragraph_back_row(&self.lines, from.row),
                col: 0,
            },
            // Search-relative motions need search state — caller computes
            // those separately (see App::jump_search).
            M::SearchNext | M::SearchPrev => from,
        }
    }

    pub fn move_paragraph_forward(&mut self) {
        self.cursor.row = paragraph_forward_row(&self.lines, self.cursor.row);
        self.cursor.col = 0;
        self.clamp_col(false);
    }

    pub fn move_paragraph_backward(&mut self) {
        self.cursor.row = paragraph_back_row(&self.lines, self.cursor.row);
        self.cursor.col = 0;
        self.clamp_col(false);
    }

    /// Next `w` target. Prefers tree-sitter leaf boundaries (so `foo(bar)`
    /// stops on `foo`, `(`, `bar`, `)` rather than treating the whole
    /// thing as one whitespace-delimited blob). Falls back to a
    /// vim-style character-class walker — words = `[A-Za-z0-9_]+`,
    /// punctuation = each contiguous run of other non-whitespace chars,
    /// whitespace separates them — when no grammar is attached.
    fn peek_word_forward(&self, from: Cursor) -> Cursor {
        if let Some(h) = &self.highlighter
            && let Some((r, c)) = h.next_token_start(from.row, from.col)
        {
            return Cursor { row: r, col: c };
        }
        word_forward_char_class(&self.lines, from)
    }

    /// Symmetric counterpart of [`peek_word_forward`] for `b`.
    fn peek_word_back(&self, from: Cursor) -> Cursor {
        if let Some(h) = &self.highlighter
            && let Some((r, c)) = h.prev_token_start(from.row, from.col)
        {
            return Cursor { row: r, col: c };
        }
        word_back_char_class(&self.lines, from)
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

    /// Find the cursor range covered by a text object.
    ///
    /// Two backends, dispatched on `object`:
    ///
    /// * **Char-scan** for quote/bracket/word objects: a single-line
    ///   search using delimiter pairs. Returns `None` if no matching
    ///   pair surrounds (or is to the right of) the cursor.
    /// * **Tree-sitter** for syntactic objects (function/class/parameter):
    ///   queries the attached `Highlighter`'s `textobjects.scm`. Can
    ///   span multiple lines. Returns `None` when no highlighter is
    ///   attached, the language has no textobjects query, or no node
    ///   of the requested kind contains the cursor.
    ///
    /// Scope semantics:
    ///   - `Inner`  → the content *between* the delimiters
    ///   - `Around` → the content *plus* the delimiters
    pub fn text_object_range(&self, scope: Scope, object: Object) -> Option<(Cursor, Cursor)> {
        if let Some(target) = ts_target(object, scope) {
            return self.text_object_range_ts(target);
        }
        if matches!(object, Object::Word) {
            return self.word_object_range(scope);
        }
        if matches!(object, Object::Paragraph) {
            return Some(self.paragraph_object_range(scope));
        }

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
        Some((Cursor { row, col: from_col }, Cursor { row, col: to_col }))
    }

    /// Tree-sitter backed branch of [`text_object_range`]. `target` is
    /// the textobjects capture name to look up (e.g. `function.inner`).
    fn text_object_range_ts(&self, target: &str) -> Option<(Cursor, Cursor)> {
        let h = self.highlighter.as_ref()?;
        let (sr, sc, er, ec) = h.find_text_object(target, self.cursor.row, self.cursor.col)?;
        Some((Cursor { row: sr, col: sc }, Cursor { row: er, col: ec }))
    }

    /// Vim-style `iw`/`aw`. Treats the line under the cursor as runs of
    /// word chars / punctuation / whitespace — the same three classes
    /// that drive `w`/`b`. `Inner` selects the run the cursor is in;
    /// `Around` additionally swallows trailing whitespace (or leading,
    /// if the run ends at end-of-line).
    fn word_object_range(&self, scope: Scope) -> Option<(Cursor, Cursor)> {
        let row = self.cursor.row;
        let chars: Vec<char> = self.lines[row].chars().collect();
        if chars.is_empty() {
            return None;
        }
        let col = self.cursor.col.min(chars.len() - 1);

        let class = classify(chars[col]);
        let mut start = col;
        while start > 0 && classify(chars[start - 1]) == class {
            start -= 1;
        }
        let mut end = col;
        while end < chars.len() && classify(chars[end]) == class {
            end += 1;
        }

        let (from_col, to_col) = match scope {
            Scope::Inner => (start, end),
            Scope::Around => {
                // Try to extend rightward with trailing whitespace.
                let mut e = end;
                while e < chars.len() && classify(chars[e]) == CharClass::Space {
                    e += 1;
                }
                if e != end {
                    (start, e)
                } else {
                    // No trailing space: fall back to leading.
                    let mut s = start;
                    while s > 0 && classify(chars[s - 1]) == CharClass::Space {
                        s -= 1;
                    }
                    (s, end)
                }
            }
        };
        Some((
            Cursor {
                row,
                col: from_col,
            },
            Cursor { row, col: to_col },
        ))
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

    /// Vim-style `ip`/`ap`. The "class" at the line level is `blank` vs
    /// `non-blank`; `ip` selects the run the cursor is on, `ap`
    /// additionally swallows the adjacent blank-line run (trailing if
    /// any, else leading) — same shape as [`word_object_range`] one
    /// dimension up. Always succeeds because every line belongs to
    /// either a paragraph or a blank run.
    fn paragraph_object_range(&self, scope: Scope) -> (Cursor, Cursor) {
        let row = self.cursor.row;
        let target_blank = is_blank_line(&self.lines[row]);

        let mut start = row;
        while start > 0 && is_blank_line(&self.lines[start - 1]) == target_blank {
            start -= 1;
        }
        let mut end = row;
        while end + 1 < self.lines.len()
            && is_blank_line(&self.lines[end + 1]) == target_blank
        {
            end += 1;
        }

        let (s, e) = match scope {
            Scope::Inner => (start, end),
            Scope::Around => {
                let mut ae = end;
                while ae + 1 < self.lines.len()
                    && is_blank_line(&self.lines[ae + 1]) != target_blank
                {
                    ae += 1;
                }
                if ae != end {
                    (start, ae)
                } else {
                    let mut as_ = start;
                    while as_ > 0
                        && is_blank_line(&self.lines[as_ - 1]) != target_blank
                    {
                        as_ -= 1;
                    }
                    (as_, end)
                }
            }
        };
        range_for_full_lines(&self.lines, s, e)
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
        // Syntactic objects are handled via tree-sitter — the
        // caller should never reach `delim` for these.
        Object::Function | Object::Class | Object::Parameter => ('\0', '\0'),
        // Paragraph is line-wise — also handled before reaching here.
        Object::Paragraph => ('\0', '\0'),
    }
}

fn is_blank_line(line: &str) -> bool {
    line.chars().all(|c| c.is_whitespace())
}

/// Forward paragraph motion target row. Skips past any current blank
/// run, then advances to the first blank line after the next
/// non-blank stretch — or the last line of the file when there isn't
/// one. Mirrors vim's `}`.
fn paragraph_forward_row(lines: &[String], from: usize) -> usize {
    let n = lines.len();
    if n == 0 {
        return 0;
    }
    let started_blank = is_blank_line(&lines[from]);
    let mut row = from + 1;
    // Only skip past the current run of blanks when we *started* in one;
    // otherwise the first blank we hit is exactly the target (the line
    // that ends the current paragraph).
    if started_blank {
        while row < n && is_blank_line(&lines[row]) {
            row += 1;
        }
    }
    while row < n && !is_blank_line(&lines[row]) {
        row += 1;
    }
    row.min(n - 1)
}

/// Mirror of [`paragraph_forward_row`] for `{`.
fn paragraph_back_row(lines: &[String], from: usize) -> usize {
    if from == 0 {
        return 0;
    }
    let started_blank = is_blank_line(&lines[from]);
    let mut row = from - 1;
    if started_blank {
        while row > 0 && is_blank_line(&lines[row]) {
            row -= 1;
        }
    }
    while row > 0 && !is_blank_line(&lines[row]) {
        row -= 1;
    }
    row
}

/// Build a `delete_range`-friendly `(from, to)` pair that covers the
/// whole-line slice `[start_row..=end_row]`. When `end_row` is the
/// last line of the buffer, the closing cursor lands at end-of-line
/// instead of `(end_row + 1, 0)` to avoid pointing past the buffer.
fn range_for_full_lines(lines: &[String], start_row: usize, end_row: usize) -> (Cursor, Cursor) {
    let from = Cursor {
        row: start_row,
        col: 0,
    };
    let to = if end_row + 1 < lines.len() {
        Cursor {
            row: end_row + 1,
            col: 0,
        }
    } else {
        Cursor {
            row: end_row,
            col: lines[end_row].chars().count(),
        }
    };
    (from, to)
}

/// Map a tree-sitter-backed [`Object`] + [`Scope`] to the capture name
/// the editor will ask the textobjects query for (e.g. `function.outer`).
/// Returns `None` for objects handled by the char-scan path.
fn ts_target(object: Object, scope: Scope) -> Option<&'static str> {
    Some(match (object, scope) {
        (Object::Function, Scope::Inner) => "function.inner",
        (Object::Function, Scope::Around) => "function.outer",
        (Object::Class, Scope::Inner) => "class.inner",
        (Object::Class, Scope::Around) => "class.outer",
        (Object::Parameter, Scope::Inner) => "parameter.inner",
        (Object::Parameter, Scope::Around) => "parameter.outer",
        _ => return None,
    })
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

// ────────────────────────────────────────────────────────────────────────
// Character-class word motion (tree-sitter-less fallback)
// ────────────────────────────────────────────────────────────────────────
//
// Used by `peek_word_forward` / `peek_word_back` when no syntax tree
// is attached. Three classes: `Word` (alphanumeric + `_`), `Punct`
// (other non-whitespace), `Space`. Transitions between any two classes
// form word boundaries — so `foo(bar)` walks as `foo`, `(`, `bar`, `)`,
// the same way vim's lowercase `w` does. (`W` ignores Punct/Word
// distinction; we're matching `w` semantics here.)

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

/// Move forward one `w`-step: skip the rest of the current class,
/// then skip whitespace, landing on the first non-whitespace char of
/// the next class. Wraps to the next line when the current line is
/// exhausted.
fn word_forward_char_class(lines: &[String], from: Cursor) -> Cursor {
    let chars: Vec<char> = lines[from.row].chars().collect();
    let mut i = from.col;

    if i < chars.len() {
        let start_class = classify(chars[i]);
        if start_class != CharClass::Space {
            while i < chars.len() && classify(chars[i]) == start_class {
                i += 1;
            }
        }
        while i < chars.len() && classify(chars[i]) == CharClass::Space {
            i += 1;
        }
    }

    if i >= chars.len() && from.row + 1 < lines.len() {
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

/// Move backward one `b`-step: step left one char, skip any
/// whitespace, then back up to the start of the contiguous run of the
/// same class. Wraps to the previous line at column 0.
#[cfg(test)]
mod word_class_tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(|s| s.to_string()).collect()
    }

    fn fwd(buf: &[String], row: usize, col: usize) -> (usize, usize) {
        let c = word_forward_char_class(buf, Cursor { row, col });
        (c.row, c.col)
    }
    fn back(buf: &[String], row: usize, col: usize) -> (usize, usize) {
        let c = word_back_char_class(buf, Cursor { row, col });
        (c.row, c.col)
    }

    #[test]
    fn word_walks_into_punctuation_not_through_it() {
        // `foo(bar)` should stop on each of foo, (, bar, ).
        let l = lines("foo(bar)");
        assert_eq!(fwd(&l, 0, 0), (0, 3)); // foo → `(`
        assert_eq!(fwd(&l, 0, 3), (0, 4)); // `(` → `bar`
        assert_eq!(fwd(&l, 0, 4), (0, 7)); // bar → `)`
    }

    #[test]
    fn word_skips_whitespace() {
        let l = lines("a   b");
        assert_eq!(fwd(&l, 0, 0), (0, 4));
    }

    #[test]
    fn back_groups_punctuation_runs() {
        // `=>` and `<-` are punctuation runs and should be one step.
        let l = lines("a => b");
        assert_eq!(back(&l, 0, 5), (0, 2)); // from `b` ← `=>`
        assert_eq!(back(&l, 0, 2), (0, 0)); // from `=>` ← `a`
    }

    #[test]
    fn paragraph_motion_finds_blank_lines() {
        // `foo / bar / "" / baz / qux / ""` — paragraphs at rows
        // 0-1 and 3-4 separated by blank lines at 2 and 5.
        let l = lines("foo\nbar\n\nbaz\nqux\n");
        assert_eq!(paragraph_forward_row(&l, 0), 2); // foo → blank
        assert_eq!(paragraph_forward_row(&l, 1), 2); // bar → blank
        assert_eq!(paragraph_forward_row(&l, 2), 5); // blank → next blank
        assert_eq!(paragraph_back_row(&l, 4), 2); // qux → blank
        assert_eq!(paragraph_back_row(&l, 3), 2); // baz → blank
        assert_eq!(paragraph_back_row(&l, 2), 0); // blank → file start
    }
}

fn word_back_char_class(lines: &[String], from: Cursor) -> Cursor {
    if from.col == 0 {
        if from.row > 0 {
            let row = from.row - 1;
            let col = lines[row].chars().count().saturating_sub(1);
            return Cursor { row, col };
        }
        return from;
    }
    let chars: Vec<char> = lines[from.row].chars().collect();
    let mut i = from.col.saturating_sub(1);
    while i > 0 && classify(chars[i]) == CharClass::Space {
        i -= 1;
    }
    let target_class = classify(chars[i]);
    while i > 0 && classify(chars[i - 1]) == target_class {
        i -= 1;
    }
    Cursor {
        row: from.row,
        col: i,
    }
}
