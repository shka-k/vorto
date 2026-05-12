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
