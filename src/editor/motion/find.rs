//! Single-line character search: `f{c}` / `F{c}` / `t{c}` / `T{c}`.

use crate::editor::Cursor;

/// `f{c}` / `F{c}` / `t{c}` / `T{c}`. Single-line — vim's char-find
/// never crosses line boundaries. Returns `from` unchanged when the
/// target isn't on the current line. `till=true` stops one short of
/// the hit.
pub(super) fn find_char(line: &str, from: Cursor, ch: char, forward: bool, till: bool) -> Cursor {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn find(line: &str, col: usize, ch: char, forward: bool, till: bool) -> usize {
        find_char(line, Cursor { row: 0, col }, ch, forward, till).col
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
}
