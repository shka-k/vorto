//! `%` — bracket match across `()`, `[]`, `{}` with nesting awareness.

use crate::editor::Cursor;

/// Scans the cursor's line from `from.col` forward to find the first
/// bracket-like char; walks paired forward or backward (across lines)
/// honouring nesting. Returns `None` when no bracket is on the cursor's
/// line at or after the cursor.
pub(super) fn bracket_match(lines: &[String], from: Cursor) -> Option<Cursor> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(|s| s.to_string()).collect()
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
}
