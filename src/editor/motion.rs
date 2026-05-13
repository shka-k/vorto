//! Word and paragraph motions.
//!
//! `motion_target` is the single entry point the evaluator and visual
//! mode use to resolve any [`MotionKind`] against the buffer. Word
//! motions prefer tree-sitter leaf boundaries when a highlighter is
//! attached, falling back to a vim-style character-class walker.

use super::{Buffer, CharClass, Cursor, classify, is_blank_line};
use crate::action::MotionKind;

impl Buffer {
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
            M::LineFirstNonBlank => Cursor {
                row: from.row,
                col: first_non_blank(&self.lines[from.row]),
            },
            M::LineLastNonBlank => Cursor {
                row: from.row,
                col: last_non_blank(&self.lines[from.row]),
            },
            M::WordForward => self.peek_word_forward(from),
            M::WordBack => self.peek_word_back(from),
            M::WordEnd => word_end_char_class(&self.lines, from, /* big */ false),
            M::BigWordForward => big_word_forward(&self.lines, from),
            M::BigWordBack => big_word_back(&self.lines, from),
            M::BigWordEnd => word_end_char_class(&self.lines, from, /* big */ true),
            M::WordEndBack => word_end_back(&self.lines, from, /* big */ false),
            M::BigWordEndBack => word_end_back(&self.lines, from, /* big */ true),
            M::BracketMatch => bracket_match(&self.lines, from).unwrap_or(from),
            // Search-word motions need access to `App.search` to set
            // the pattern — App resolves these before reaching here.
            M::SearchWordForward | M::SearchWordBack => from,
            M::FindChar { ch, forward, till } => {
                find_char(&self.lines[from.row], from, ch, forward, till)
            }
            // `;`/`,` are resolved into a concrete FindChar at the
            // App layer before reaching `motion_target`.
            M::RepeatFind { .. } => from,
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
            // H / M / L target a row relative to the currently visible
            // viewport. Before the first draw `viewport_height` is 0;
            // in that case treat the request as a no-op so we don't
            // teleport the cursor to row 0 at startup.
            M::ViewportTop | M::ViewportMiddle | M::ViewportBottom => {
                viewport_target(self, from, motion)
            }
            // <C-d>/<C-u>/<C-f>/<C-b> move the cursor; the existing
            // `compute_scroll` keeps the viewport in sync on next draw.
            M::HalfPageDown | M::HalfPageUp | M::PageDown | M::PageUp => {
                page_target(self, from, motion)
            }
            // Search-relative motions need search state — caller computes
            // those separately (see App::jump_search).
            M::SearchNext | M::SearchPrev => from,
        }
    }

    pub fn move_word_forward(&mut self) {
        self.cursor = self.peek_word_forward(self.cursor);
    }

    pub fn move_word_backward(&mut self) {
        self.cursor = self.peek_word_back(self.cursor);
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
        let ts = self
            .highlighter
            .as_ref()
            .and_then(|h| h.next_token_start(from.row, from.col));
        if let Some((r, c)) = ts {
            return Cursor { row: r, col: c };
        }
        word_forward_char_class(&self.lines, from)
    }

    /// Symmetric counterpart of [`peek_word_forward`] for `b`.
    fn peek_word_back(&self, from: Cursor) -> Cursor {
        let ts = self
            .highlighter
            .as_ref()
            .and_then(|h| h.prev_token_start(from.row, from.col));
        if let Some((r, c)) = ts {
            return Cursor { row: r, col: c };
        }
        word_back_char_class(&self.lines, from)
    }
}

/// Char index of the first non-whitespace character on a line, or `0`
/// when the line is entirely whitespace (vim's `^` behaviour).
fn first_non_blank(line: &str) -> usize {
    line.chars()
        .position(|c| !c.is_whitespace())
        .unwrap_or(0)
}

/// Char index of the last non-whitespace character on a line, or `0`
/// when the line is entirely whitespace.
fn last_non_blank(line: &str) -> usize {
    let chars: Vec<char> = line.chars().collect();
    for (i, c) in chars.iter().enumerate().rev() {
        if !c.is_whitespace() {
            return i;
        }
    }
    0
}

/// `ge` / `gE` — back to the previous word's end. A position is a
/// "word end" when it's non-whitespace AND the char immediately to
/// its right is a different class (or end-of-line). For `gE` we
/// collapse Word and Punct into one class.
fn word_end_back(lines: &[String], from: Cursor, big: bool) -> Cursor {
    let class_of = |c: char| {
        let raw = classify(c);
        if big && raw == CharClass::Punct {
            CharClass::Word
        } else {
            raw
        }
    };
    let is_end = |chars: &[char], i: usize| {
        if classify(chars[i]) == CharClass::Space {
            return false;
        }
        if i + 1 >= chars.len() {
            return true;
        }
        class_of(chars[i]) != class_of(chars[i + 1])
    };

    let mut row = from.row;
    // Step at least one position left, wrapping lines as needed.
    let mut start: Option<usize> = if from.col == 0 {
        None
    } else {
        Some(from.col - 1)
    };
    loop {
        let chars: Vec<char> = lines[row].chars().collect();
        if let Some(mut i) = start {
            loop {
                if i < chars.len() && is_end(&chars, i) {
                    return Cursor { row, col: i };
                }
                if i == 0 {
                    break;
                }
                i -= 1;
            }
        }
        if row == 0 {
            return Cursor { row: 0, col: 0 };
        }
        row -= 1;
        let len = lines[row].chars().count();
        start = if len > 0 { Some(len - 1) } else { None };
    }
}

/// `%` — bracket match. Scans the cursor's line from `from.col`
/// forward to find the first bracket-like char; walks paired forward
/// or backward (across lines) honouring nesting. Returns `None` when
/// no bracket is on the cursor's line.
fn bracket_match(lines: &[String], from: Cursor) -> Option<Cursor> {
    let line: Vec<char> = lines[from.row].chars().collect();
    // Find the first bracket at or after the cursor on this line.
    let (start_col, opener) = line
        .iter()
        .enumerate()
        .skip(from.col)
        .find_map(|(i, c)| bracket_pair(*c).map(|p| (i, p)))?;
    let (open, close, forward) = opener;
    if forward {
        scan_forward(lines, from.row, start_col, open, close)
    } else {
        scan_back(lines, from.row, start_col, open, close)
    }
}

/// Returns `(open, close, forward)` if `c` is one half of a bracket
/// pair. `forward=true` means we'll scan forward looking for `close`;
/// `forward=false` means we'll scan backward looking for `open`.
fn bracket_pair(c: char) -> Option<(char, char, bool)> {
    match c {
        '(' => Some(('(', ')', true)),
        '[' => Some(('[', ']', true)),
        '{' => Some(('{', '}', true)),
        ')' => Some(('(', ')', false)),
        ']' => Some(('[', ']', false)),
        '}' => Some(('{', '}', false)),
        _ => None,
    }
}

fn scan_forward(
    lines: &[String],
    row: usize,
    col: usize,
    open: char,
    close: char,
) -> Option<Cursor> {
    let mut depth = 0_i32;
    let mut r = row;
    let mut c = col;
    while r < lines.len() {
        let chars: Vec<char> = lines[r].chars().collect();
        while c < chars.len() {
            if chars[c] == open {
                depth += 1;
            } else if chars[c] == close {
                depth -= 1;
                if depth == 0 {
                    return Some(Cursor { row: r, col: c });
                }
            }
            c += 1;
        }
        r += 1;
        c = 0;
    }
    None
}

fn scan_back(
    lines: &[String],
    row: usize,
    col: usize,
    open: char,
    close: char,
) -> Option<Cursor> {
    let mut depth = 0_i32;
    let mut r = row as isize;
    let mut c = col as isize;
    while r >= 0 {
        let chars: Vec<char> = lines[r as usize].chars().collect();
        while c >= 0 {
            let ch = chars[c as usize];
            if ch == close {
                depth += 1;
            } else if ch == open {
                depth -= 1;
                if depth == 0 {
                    return Some(Cursor {
                        row: r as usize,
                        col: c as usize,
                    });
                }
            }
            c -= 1;
        }
        r -= 1;
        if r >= 0 {
            c = lines[r as usize].chars().count() as isize - 1;
        }
    }
    None
}

/// Resolve `H` / `M` / `L` against the buffer's current viewport. Reads
/// the topmost-visible row from `scroll` and the row count from
/// `viewport_height`, both updated by the UI on each draw. Falls back
/// to `from` while the viewport is unknown (height == 0).
fn viewport_target(buf: &Buffer, from: Cursor, motion: MotionKind) -> Cursor {
    let height = buf.viewport_height.get();
    if height == 0 {
        return from;
    }
    let top = buf.scroll.get();
    let last_row = buf.lines.len().saturating_sub(1);
    let bottom = (top + height).saturating_sub(1).min(last_row);
    let row = match motion {
        MotionKind::ViewportTop => top.min(last_row),
        MotionKind::ViewportBottom => bottom,
        MotionKind::ViewportMiddle => {
            let mid = top + (bottom - top) / 2;
            mid.min(last_row)
        }
        _ => return from,
    };
    let max_col = buf.lines[row].chars().count().saturating_sub(1);
    Cursor {
        row,
        col: from.col.min(max_col),
    }
}

/// Resolve `<C-d>` / `<C-u>` / `<C-f>` / `<C-b>` against the viewport
/// height. The cursor moves; the UI's existing `compute_scroll` keeps
/// the viewport pinned to the cursor on the next draw.
///
/// We use a sensible minimum step (1) so the motion never silently
/// stalls when the viewport hasn't been measured yet — that's how vim
/// behaves in `--clean -e` mode when no window is available.
fn page_target(buf: &Buffer, from: Cursor, motion: MotionKind) -> Cursor {
    let height = buf.viewport_height.get().max(1);
    let half = (height / 2).max(1);
    let last_row = buf.lines.len().saturating_sub(1);
    let row = match motion {
        MotionKind::HalfPageDown => (from.row + half).min(last_row),
        MotionKind::HalfPageUp => from.row.saturating_sub(half),
        MotionKind::PageDown => (from.row + height).min(last_row),
        MotionKind::PageUp => from.row.saturating_sub(height),
        _ => return from,
    };
    let max_col = buf.lines[row].chars().count().saturating_sub(1);
    Cursor {
        row,
        col: from.col.min(max_col),
    }
}

/// Forward paragraph motion target row. Skips past any current blank
/// run, then advances to the first blank line after the next
/// non-blank stretch — or the last line of the file when there isn't
/// one. Mirrors vim's `}`.
pub(super) fn paragraph_forward_row(lines: &[String], from: usize) -> usize {
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
pub(super) fn paragraph_back_row(lines: &[String], from: usize) -> usize {
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
        if i < chars.len() {
            return Cursor {
                row: from.row,
                col: i,
            };
        }
    }
    // Wrap to next line's first non-whitespace.
    let mut row = from.row + 1;
    while row < lines.len() {
        let cs: Vec<char> = lines[row].chars().collect();
        if let Some(col) = cs.iter().position(|&c| classify(c) != CharClass::Space) {
            return Cursor { row, col };
        }
        row += 1;
    }
    // No further word — stay at the end of the current line.
    Cursor {
        row: from.row,
        col: chars.len().saturating_sub(1),
    }
}

/// Move forward to the end of the current word (or to the end of the
/// next one if already on an end). `big=true` collapses `Word` and
/// `Punct` into one class — that's the `E` vs `e` distinction.
///
/// Wraps to the next line's first end when the current line is
/// exhausted. Stays put at file end.
fn word_end_char_class(lines: &[String], from: Cursor, big: bool) -> Cursor {
    let class_of = |c: char| {
        let raw = classify(c);
        if big && raw == CharClass::Punct {
            CharClass::Word
        } else {
            raw
        }
    };

    let mut row = from.row;
    let mut col = from.col;
    loop {
        let chars: Vec<char> = lines[row].chars().collect();
        // Step at least one char so a repeated `e` doesn't sit still.
        let mut i = col.saturating_add(1);
        // Skip past whitespace.
        while i < chars.len() && classify(chars[i]) == CharClass::Space {
            i += 1;
        }
        if i < chars.len() {
            let cls = class_of(chars[i]);
            // Advance through the run of the same class; final `i`
            // lands one past the last char, so step back one.
            while i + 1 < chars.len() && class_of(chars[i + 1]) == cls {
                i += 1;
            }
            return Cursor { row, col: i };
        }
        // Line exhausted — try the next line.
        if row + 1 >= lines.len() {
            return Cursor {
                row,
                col: chars.len().saturating_sub(1),
            };
        }
        row += 1;
        col = 0_usize.wrapping_sub(1); // so col+1 == 0 on the next loop
    }
}

/// `W` — WORD forward: skip the current non-whitespace run, then any
/// whitespace, landing on the next non-whitespace char. Wraps to the
/// next line.
fn big_word_forward(lines: &[String], from: Cursor) -> Cursor {
    let chars: Vec<char> = lines[from.row].chars().collect();
    let mut i = from.col;
    if i < chars.len() {
        while i < chars.len() && classify(chars[i]) != CharClass::Space {
            i += 1;
        }
        while i < chars.len() && classify(chars[i]) == CharClass::Space {
            i += 1;
        }
        if i < chars.len() {
            return Cursor {
                row: from.row,
                col: i,
            };
        }
    }
    let mut row = from.row + 1;
    while row < lines.len() {
        let cs: Vec<char> = lines[row].chars().collect();
        if let Some(col) = cs.iter().position(|&c| classify(c) != CharClass::Space) {
            return Cursor { row, col };
        }
        row += 1;
    }
    Cursor {
        row: from.row,
        col: chars.len().saturating_sub(1),
    }
}

/// `B` — WORD back: mirror of `big_word_forward`.
fn big_word_back(lines: &[String], from: Cursor) -> Cursor {
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
    while i > 0 && classify(chars[i - 1]) != CharClass::Space {
        i -= 1;
    }
    Cursor {
        row: from.row,
        col: i,
    }
}

/// `f{c}` / `F{c}` / `t{c}` / `T{c}`. Single-line — vim's char-find
/// never crosses line boundaries. Returns `from` unchanged when the
/// target isn't on the current line. `till=true` stops one short of
/// the hit.
fn find_char(line: &str, from: Cursor, ch: char, forward: bool, till: bool) -> Cursor {
    let chars: Vec<char> = line.chars().collect();
    if forward {
        let start = from.col.saturating_add(1);
        for (i, &c) in chars.iter().enumerate().skip(start) {
            if c == ch {
                let col = if till { i.saturating_sub(1) } else { i };
                return Cursor { row: from.row, col };
            }
        }
        from
    } else {
        if from.col == 0 {
            return from;
        }
        for i in (0..from.col).rev() {
            if chars[i] == ch {
                let col = if till {
                    (i + 1).min(chars.len().saturating_sub(1))
                } else {
                    i
                };
                return Cursor { row: from.row, col };
            }
        }
        from
    }
}

/// Move backward one `b`-step: step left one char, skip any
/// whitespace, then back up to the start of the contiguous run of the
/// same class. Wraps to the previous line at column 0.
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

#[cfg(test)]
mod tests {
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

    fn end(buf: &[String], row: usize, col: usize, big: bool) -> (usize, usize) {
        let c = word_end_char_class(buf, Cursor { row, col }, big);
        (c.row, c.col)
    }

    fn big_fwd(buf: &[String], row: usize, col: usize) -> (usize, usize) {
        let c = big_word_forward(buf, Cursor { row, col });
        (c.row, c.col)
    }

    fn big_back(buf: &[String], row: usize, col: usize) -> (usize, usize) {
        let c = big_word_back(buf, Cursor { row, col });
        (c.row, c.col)
    }

    fn find(line: &str, col: usize, ch: char, forward: bool, till: bool) -> usize {
        find_char(line, Cursor { row: 0, col }, ch, forward, till).col
    }

    #[test]
    fn word_end_lands_on_last_char_of_word() {
        // "foo bar": e from col 0 should land on 'o' (col 2),
        // next e on 'r' (col 6).
        let l = lines("foo bar");
        assert_eq!(end(&l, 0, 0, false), (0, 2));
        assert_eq!(end(&l, 0, 2, false), (0, 6));
    }

    #[test]
    fn word_end_walks_into_punctuation_not_through_it() {
        // `foo(bar)` with `e`: foo → ( → bar → ) — punct runs are their
        // own word for lowercase `e`.
        let l = lines("foo(bar)");
        assert_eq!(end(&l, 0, 0, false), (0, 2)); // foo end at 'o'
        assert_eq!(end(&l, 0, 2, false), (0, 3)); // (
        assert_eq!(end(&l, 0, 3, false), (0, 6)); // bar end at 'r'
        assert_eq!(end(&l, 0, 6, false), (0, 7)); // )
    }

    #[test]
    fn big_word_end_collapses_punctuation() {
        // `foo(bar)` with `E`: the whole token is one big WORD — `E`
        // goes straight to the trailing `)` at col 7.
        let l = lines("foo(bar)");
        assert_eq!(end(&l, 0, 0, true), (0, 7));
    }

    #[test]
    fn big_word_forward_skips_punctuation() {
        // `foo(bar) baz`: W from col 0 → `baz` (col 9), not `(`.
        let l = lines("foo(bar) baz");
        assert_eq!(big_fwd(&l, 0, 0), (0, 9));
    }

    #[test]
    fn big_word_back_skips_punctuation() {
        // mirror: B from `baz` → start of `foo(bar)`.
        let l = lines("foo(bar) baz");
        assert_eq!(big_back(&l, 0, 9), (0, 0));
    }

    #[test]
    fn find_char_forward_lands_on_target() {
        // f-x in "hello x world": from col 0 jumps to col 6.
        assert_eq!(find("hello x world", 0, 'x', true, false), 6);
    }

    #[test]
    fn till_char_forward_stops_one_short() {
        // t-x in "hello x world": from col 0 lands on col 5 (the space).
        assert_eq!(find("hello x world", 0, 'x', true, true), 5);
    }

    #[test]
    fn find_char_backward_lands_on_target() {
        // F-h in "hello x world" starting on the 'w' (col 8) → 'h' at col 0.
        assert_eq!(find("hello x world", 8, 'h', false, false), 0);
    }

    #[test]
    fn till_char_backward_stops_one_after() {
        // T-h backward from col 8 → col 1 (the 'e' just after 'h').
        assert_eq!(find("hello x world", 8, 'h', false, true), 1);
    }

    #[test]
    fn find_char_missing_returns_origin() {
        // No 'z' in "hello": cursor doesn't move.
        assert_eq!(find("hello", 0, 'z', true, false), 0);
    }

    fn buf_with(lines_in: &str, scroll: usize, height: usize) -> Buffer {
        let mut b = Buffer::new();
        b.lines = lines(lines_in);
        b.scroll.set(scroll);
        b.viewport_height.set(height);
        b
    }

    #[test]
    fn viewport_h_m_l_with_full_window() {
        // 10 lines, viewport rows 0..10. H=0, M=4 (mid floor), L=9.
        let b = buf_with("a\nb\nc\nd\ne\nf\ng\nh\ni\nj", 0, 10);
        let from = Cursor { row: 5, col: 0 };
        assert_eq!(
            viewport_target(&b, from, MotionKind::ViewportTop).row,
            0
        );
        assert_eq!(
            viewport_target(&b, from, MotionKind::ViewportMiddle).row,
            4
        );
        assert_eq!(
            viewport_target(&b, from, MotionKind::ViewportBottom).row,
            9
        );
    }

    #[test]
    fn viewport_clamps_to_file_end() {
        // Viewport says rows 5..15 but file only has 8 lines.
        // L should clamp to last row (7) rather than overshoot.
        let b = buf_with("a\nb\nc\nd\ne\nf\ng\nh", 5, 10);
        let from = Cursor { row: 6, col: 0 };
        assert_eq!(
            viewport_target(&b, from, MotionKind::ViewportBottom).row,
            7
        );
        assert_eq!(
            viewport_target(&b, from, MotionKind::ViewportTop).row,
            5
        );
    }

    #[test]
    fn page_motions_step_by_height_and_half() {
        // 20 rows, viewport height = 10. From row 0:
        //   <C-d> → row 5 (half), <C-f> → row 10 (full), and back.
        let lines_str = (0..20).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let b = buf_with(&lines_str, 0, 10);
        let from = Cursor { row: 0, col: 0 };
        assert_eq!(page_target(&b, from, MotionKind::HalfPageDown).row, 5);
        assert_eq!(page_target(&b, from, MotionKind::PageDown).row, 10);
        let mid = Cursor { row: 15, col: 0 };
        assert_eq!(page_target(&b, mid, MotionKind::HalfPageUp).row, 10);
        assert_eq!(page_target(&b, mid, MotionKind::PageUp).row, 5);
    }

    #[test]
    fn first_and_last_non_blank() {
        assert_eq!(first_non_blank("    hello"), 4);
        assert_eq!(first_non_blank(""), 0);
        assert_eq!(first_non_blank("   "), 0);
        assert_eq!(last_non_blank("hello   "), 4);
        assert_eq!(last_non_blank("hello"), 4);
        assert_eq!(last_non_blank("   "), 0);
    }

    fn end_back(buf: &[String], row: usize, col: usize, big: bool) -> (usize, usize) {
        let c = word_end_back(buf, Cursor { row, col }, big);
        (c.row, c.col)
    }

    #[test]
    fn word_end_back_lands_on_previous_word_end() {
        // "foo bar baz": from 'b' of baz (col 8) → 'r' of bar (col 6).
        // From 'r' of bar (col 6) → 'o' of foo (col 2).
        let l = lines("foo bar baz");
        assert_eq!(end_back(&l, 0, 8, false), (0, 6));
        assert_eq!(end_back(&l, 0, 6, false), (0, 2));
    }

    #[test]
    fn word_end_back_treats_punctuation_as_its_own_word() {
        // "foo(bar)": from ')' (col 7) → 'r' of bar (col 6) since `)` is
        // its own one-char word for lowercase ge.
        let l = lines("foo(bar)");
        assert_eq!(end_back(&l, 0, 7, false), (0, 6));
        // Big-word: punct merges with surrounding word, so the whole
        // "foo(bar)" is one WORD ending at col 7 — ge from col 7 has
        // nothing to step back to → file start.
        assert_eq!(end_back(&l, 0, 7, true), (0, 0));
    }

    fn match_pair(s: &str, col: usize) -> Option<usize> {
        let l = lines(s);
        bracket_match(&l, Cursor { row: 0, col }).map(|c| c.col)
    }

    #[test]
    fn bracket_match_finds_matching_close_and_open() {
        // From '(' at col 3 in "foo(bar)" → ')' at col 7.
        assert_eq!(match_pair("foo(bar)", 3), Some(7));
        // From ')' at col 7 → '(' at col 3.
        assert_eq!(match_pair("foo(bar)", 7), Some(3));
    }

    #[test]
    fn bracket_match_respects_nesting() {
        // "((a))" — from col 0 the matching `)` is the outermost at col 4.
        assert_eq!(match_pair("((a))", 0), Some(4));
        // From the inner `(` at col 1, match is at col 3.
        assert_eq!(match_pair("((a))", 1), Some(3));
    }

    #[test]
    fn bracket_match_returns_none_on_non_bracket_line() {
        // No brackets at or after the cursor — cursor doesn't move.
        assert_eq!(match_pair("plain text", 0), None);
    }

    #[test]
    fn bracket_match_skips_to_first_bracket_after_cursor() {
        // Cursor on 'f' of "foo(bar)": should jump to the matching `)` of
        // the `(` at col 3.
        assert_eq!(match_pair("foo(bar)", 0), Some(7));
    }

    #[test]
    fn bracket_match_works_across_lines() {
        let l = lines("foo(\n  bar,\n  baz\n)");
        // From `(` on line 0 col 3 → `)` on line 3 col 0.
        let from = Cursor { row: 0, col: 3 };
        let r = bracket_match(&l, from).unwrap();
        assert_eq!((r.row, r.col), (3, 0));
    }

    #[test]
    fn page_motions_clamp_to_file_bounds() {
        let b = buf_with("a\nb\nc", 0, 10);
        // From last row, <C-d> stays put; from first row, <C-u> stays put.
        let last = Cursor { row: 2, col: 0 };
        assert_eq!(page_target(&b, last, MotionKind::HalfPageDown).row, 2);
        let first = Cursor { row: 0, col: 0 };
        assert_eq!(page_target(&b, first, MotionKind::HalfPageUp).row, 0);
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
