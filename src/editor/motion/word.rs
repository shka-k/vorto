//! Character-class word motions (`w` / `b` / `e` / `ge` and big-word
//! `W` / `B` / `E` / `gE`).
//!
//! Three character classes: `Word` (alphanumeric + `_`), `Punct` (other
//! non-whitespace), `Space`. Transitions between any two non-space
//! classes are word boundaries — so `foo(bar)` walks as `foo`, `(`,
//! `bar`, `)`, the same way vim's lowercase `w` does. The `big`
//! variants collapse `Word` and `Punct` into a single class.

use crate::editor::{CharClass, Cursor, classify};

/// Move forward one `w`-step: skip the rest of the current class,
/// then skip whitespace, landing on the first non-whitespace char of
/// the next class. Wraps to the next line when the current line is
/// exhausted.
pub(super) fn word_forward_char_class(lines: &[String], from: Cursor) -> Cursor {
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
    // Wrap to the next line. Empty lines are words in vim, so we stop
    // on the first one; otherwise we skip leading whitespace and land
    // on the first non-blank char.
    let mut row = from.row + 1;
    while row < lines.len() {
        let cs: Vec<char> = lines[row].chars().collect();
        if cs.is_empty() {
            return Cursor { row, col: 0 };
        }
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
pub(super) fn word_end_char_class(lines: &[String], from: Cursor, big: bool) -> Cursor {
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

/// `ge` / `gE` — back to the previous word's end. A position is a
/// "word end" when it's non-whitespace AND the char immediately to
/// its right is a different class (or end-of-line). For `gE` we
/// collapse Word and Punct into one class.
pub(super) fn word_end_back(lines: &[String], from: Cursor, big: bool) -> Cursor {
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

/// `W` — WORD forward: skip the current non-whitespace run, then any
/// whitespace, landing on the next non-whitespace char. Wraps to the
/// next line.
pub(super) fn big_word_forward(lines: &[String], from: Cursor) -> Cursor {
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
        if cs.is_empty() {
            return Cursor { row, col: 0 };
        }
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

/// `B` — WORD back: mirror of [`big_word_forward`].
pub(super) fn big_word_back(lines: &[String], from: Cursor) -> Cursor {
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

/// Move backward one `b`-step: step left one char, skip any
/// whitespace, then back up to the start of the contiguous run of the
/// same class. Wraps to the previous line at column 0.
pub(super) fn word_back_char_class(lines: &[String], from: Cursor) -> Cursor {
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
    fn word_forward_stops_on_empty_lines() {
        // Vim treats empty lines as words: `w` from the end of `foo`
        // should land on (1, 0), not skip past line 1 to `bar`.
        let l = lines("foo\n\nbar");
        assert_eq!(fwd(&l, 0, 2), (1, 0));
        // From the empty line, the next `w` lands on `bar`.
        assert_eq!(fwd(&l, 1, 0), (2, 0));
    }

    #[test]
    fn word_forward_skips_blank_lines_to_first_word() {
        // A line of only whitespace is *not* an empty line — vim skips
        // it the same way it skips leading whitespace on a wrap.
        let l = lines("foo\n   \nbar");
        assert_eq!(fwd(&l, 0, 2), (2, 0));
    }

    #[test]
    fn big_word_forward_stops_on_empty_lines() {
        let l = lines("foo\n\nbar");
        assert_eq!(big_fwd(&l, 0, 2), (1, 0));
        assert_eq!(big_fwd(&l, 1, 0), (2, 0));
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
}
