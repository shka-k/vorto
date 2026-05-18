//! Terminal cell-width helpers.
//!
//! `chars().count()` counts code points, but terminal layout cares
//! about *cells*: a CJK glyph (e.g. `あ`) renders as two cells, ASCII
//! as one, and most control / zero-width chars contribute nothing
//! visible. Mixing these up makes the editor cursor drift past
//! fullwidth text and makes popup panels mis-size when their content
//! contains Japanese, emoji, or other wide characters.
//!
//! These helpers are the single source of truth for that math.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Cell width of `ch`. Wide East-Asian glyphs and most emoji count
/// as 2; zero-width / control chars are clamped up to 1 so a cursor
/// or per-char rendering loop always has at least one cell to place
/// the glyph in.
pub fn char_cell_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1).max(1)
}

/// Total cell width of `s`. Tabs count as one cell here — callers
/// that expand tabs do so themselves; this helper is for content that
/// will never see tab expansion (status bar, popups, list items).
pub fn str_cell_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Visual column the character at `char_col` lands on within `line`,
/// once tabs are expanded to the next `tab_width`-aligned stop and
/// every other char contributes its terminal-cell width. Returns the
/// position *before* the char at `char_col` (i.e. the count of cells
/// consumed by the preceding prefix), so the result doubles as "x
/// offset to draw the cursor at".
///
/// Callers that work in App context use [`crate::app::App::char_col_visual`];
/// this is the underlying pure helper.
pub fn visual_col_of(line: &str, char_col: usize, tab_width: usize) -> usize {
    let mut v = 0usize;
    for ch in line.chars().take(char_col) {
        if ch == '\t' {
            v += tab_width - (v % tab_width);
        } else {
            v += char_cell_width(ch);
        }
    }
    v
}

/// Take as many leading characters of `s` as fit within `max` cells.
/// Returns the byte length of that prefix so callers can slice the
/// original string (`&s[..len]`) without an allocation.
pub fn prefix_byte_len_for_width(s: &str, max: usize) -> usize {
    let mut used = 0usize;
    let mut byte_end = 0usize;
    for (i, ch) in s.char_indices() {
        let w = char_cell_width(ch);
        if used + w > max {
            break;
        }
        used += w;
        byte_end = i + ch.len_utf8();
    }
    byte_end
}
