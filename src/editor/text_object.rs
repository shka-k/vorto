//! Text-object range resolution (`iw`, `aw`, `i(`, `ip`, `if`, ...).
//!
//! Two backends, dispatched on the object kind:
//!
//! * **Char-scan** for quote/bracket/word/paragraph objects — single
//!   line for delimiter pairs, line-class scan for paragraphs.
//! * **Tree-sitter** for syntactic objects (function/class/parameter),
//!   via the buffer's attached highlighter and its `textobjects.scm`.

use super::{Buffer, CharClass, Cursor, classify, is_blank_line};
use crate::action::{Object, Scope};

impl Buffer {
    /// Find the cursor range covered by a text object.
    ///
    /// Scope semantics:
    ///   - `Inner`  → the content *between* the delimiters
    ///   - `Around` → the content *plus* the delimiters
    ///
    /// Returns `None` if no matching pair surrounds (or is to the right
    /// of) the cursor, no syntactic node is found for tree-sitter
    /// objects, or no highlighter is attached when required.
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
        Some((Cursor { row, col: from_col }, Cursor { row, col: to_col }))
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
        while end + 1 < self.lines.len() && is_blank_line(&self.lines[end + 1]) == target_blank {
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
                    while as_ > 0 && is_blank_line(&self.lines[as_ - 1]) != target_blank {
                        as_ -= 1;
                    }
                    (as_, end)
                }
            }
        };
        range_for_full_lines(&self.lines, s, e)
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
