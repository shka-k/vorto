//! Word, paragraph, bracket, find, and viewport-relative motions.
//!
//! [`Buffer::motion_target`] is the single entry point the evaluator and
//! visual mode use to resolve any [`MotionKind`] against the buffer.
//! Word motions prefer tree-sitter leaf boundaries when a highlighter is
//! attached, falling back to a vim-style character-class walker.

mod bracket;
mod find;
mod viewport;
mod word;

use super::{Buffer, Cursor, is_blank_line};
use crate::action::MotionKind;

use bracket::bracket_match;
use find::find_char;
use viewport::{page_target, viewport_target};
use word::{
    big_word_back, big_word_forward, word_back_char_class, word_end_back, word_end_char_class,
    word_forward_char_class,
};

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

    /// Next `w` target. Uses vim's character-class walker — words =
    /// `[A-Za-z0-9_]+`, punctuation = each contiguous run of other
    /// non-whitespace chars, whitespace separates them. We deliberately
    /// don't consult tree-sitter here: grammars expose strings and
    /// comments as single leaves, but vim walks through them character
    /// class by character class.
    fn peek_word_forward(&self, from: Cursor) -> Cursor {
        word_forward_char_class(&self.lines, from)
    }

    /// Symmetric counterpart of [`peek_word_forward`] for `b`.
    fn peek_word_back(&self, from: Cursor) -> Cursor {
        word_back_char_class(&self.lines, from)
    }
}

/// Char index of the first non-whitespace character on a line, or `0`
/// when the line is entirely whitespace (vim's `^` behaviour).
fn first_non_blank(line: &str) -> usize {
    line.chars().position(|c| !c.is_whitespace()).unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(|s| s.to_string()).collect()
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
