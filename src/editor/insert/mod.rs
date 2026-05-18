//! Insertion-side primitives: typing characters and newlines, opening
//! lines, replacing the char under the cursor, and the auto-pair /
//! auto-indent heuristics layered on top.
//!
//! `*_smart` variants ([`Buffer::insert_char_smart`],
//! [`Buffer::delete_char_before_smart`]) are the single-cursor paths the
//! input layer uses; the multi-cursor fan-out drives raw
//! [`Buffer::insert_char`] / [`Buffer::delete_char_before`] directly so
//! the per-cursor `col` shift bookkeeping stays valid.
//!
//! The pure helpers are split out so this file stays focused on the
//! buffer-mutating logic:
//!
//! - [`autopair`] — opener→closer mapping and the should-pair gate.
//! - [`indent_calc`] — indent-string arithmetic (extend / trim / build).

mod autopair;
mod indent_calc;

use super::{Buffer, IndentSettings, char_to_byte};
use autopair::{auto_pair_closer, is_auto_pair_closer, should_auto_pair};
use indent_calc::{
    add_one_indent_level, compute_new_line_indent, copy_leading_indent, strip_one_indent_level,
};

impl Buffer {
    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor.row];
        let byte_idx = char_to_byte(line, self.cursor.col);
        line.insert(byte_idx, c);
        self.cursor.col += 1;
        self.touch();
    }

    /// Insert `c` at the cursor with three modern-editor behaviours
    /// layered on top of [`insert_char`]:
    ///
    /// 1. **Skip-over** — if `c` is a paired closer (`)` / `]` / `}` /
    ///    quote) and the next character on the line is already the same
    ///    closer, we just advance the cursor. This is what makes
    ///    `()`-then-type-`)` land outside the pair instead of producing
    ///    `())`.
    /// 2. **Dedent on close** — `}` / `)` / `]` typed on a line that's
    ///    pure whitespace before the cursor pulls the line back one
    ///    indent level first.
    /// 3. **Auto-pair** — opener (`(` `[` `{` quote) inserts its closer
    ///    right after, leaving the cursor between. Suppressed when
    ///    grabbing the closer would capture an existing identifier
    ///    (next char is alphanumeric / `_`), and additionally for
    ///    quotes when the previous char looks like word context (an
    ///    apostrophe in `it's`) or is the same quote (cursor inside an
    ///    empty `""`).
    ///
    /// Single-cursor only: the multi-cursor fan-out path goes through
    /// raw [`insert_char`] to keep cursor-shift bookkeeping simple.
    pub fn insert_char_smart(&mut self, c: char, indent: IndentSettings) {
        let next = self.char_at_cursor();
        let prev = self.char_before_cursor();

        if is_auto_pair_closer(c) && next == Some(c) {
            self.cursor.col += 1;
            return;
        }

        if matches!(c, '}' | ')' | ']') && self.line_is_blank_before_cursor() {
            self.dedent_current_line(indent);
        }

        self.insert_char(c);

        if let Some(closer) = auto_pair_closer(c)
            && should_auto_pair(c, prev, next)
        {
            self.insert_char(closer);
            self.cursor.col -= 1;
        }
    }

    /// Char at the cursor's logical position, or `None` past end-of-line.
    pub fn char_at_cursor(&self) -> Option<char> {
        self.lines
            .get(self.cursor.row)
            .and_then(|line| line.chars().nth(self.cursor.col))
    }

    /// Char immediately before the cursor on the current row, or `None`
    /// at column 0.
    pub fn char_before_cursor(&self) -> Option<char> {
        if self.cursor.col == 0 {
            return None;
        }
        self.lines
            .get(self.cursor.row)
            .and_then(|line| line.chars().nth(self.cursor.col - 1))
    }

    /// True when every character on the cursor row strictly *before*
    /// the cursor column is whitespace. An empty line (cursor at
    /// col 0) qualifies too, vacuously.
    pub fn line_is_blank_before_cursor(&self) -> bool {
        let line = &self.lines[self.cursor.row];
        line.chars()
            .take(self.cursor.col)
            .all(|c| c.is_whitespace())
    }

    /// Add one indent level at the start of `row`. Picks tabs vs
    /// spaces by looking at the row's existing leading whitespace:
    /// any `\t` in the leading run means tab, otherwise spaces; an
    /// empty leading run falls back to `indent.use_tabs`. Cursor
    /// follows the shift when it's on this row.
    pub fn indent_line(&mut self, row: usize, indent: IndentSettings) {
        if row >= self.lines.len() {
            return;
        }
        let line = &self.lines[row];
        let leading: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        let use_tabs = if leading.is_empty() {
            indent.use_tabs
        } else {
            leading.contains('\t')
        };
        let prefix: String = if use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(indent.width.max(1))
        };
        let added_chars = prefix.chars().count();
        self.lines[row].insert_str(0, &prefix);
        if self.cursor.row == row {
            self.cursor.col += added_chars;
        }
        self.touch();
    }

    /// Strip one indent level from the start of `row`. Same rounding
    /// rules as [`dedent_current_line`] — tab-terminated leading
    /// whitespace drops one trailing `\t`; space-terminated rounds
    /// down to the nearest multiple of `indent.width` strictly below
    /// the current count. Cursor follows on the affected row.
    pub fn dedent_line(&mut self, row: usize, indent: IndentSettings) {
        if row >= self.lines.len() {
            return;
        }
        let line = self.lines[row].clone();
        let leading: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        if leading.is_empty() {
            return;
        }
        let remove_chars = if leading.ends_with('\t') {
            1
        } else {
            let trailing_spaces = leading.chars().rev().take_while(|c| *c == ' ').count();
            let w = indent.width.max(1);
            let target = (trailing_spaces.saturating_sub(1) / w) * w;
            trailing_spaces - target
        };
        if remove_chars == 0 {
            return;
        }
        let leading_char_count = leading.chars().count();
        let delete_start_char = leading_char_count - remove_chars;
        let delete_start_byte = char_to_byte(&line, delete_start_char);
        let delete_end_byte = char_to_byte(&line, delete_start_char + remove_chars);
        self.lines[row].replace_range(delete_start_byte..delete_end_byte, "");
        if self.cursor.row == row {
            self.cursor.col = self.cursor.col.saturating_sub(remove_chars);
        }
        self.touch();
    }

    /// Strip one indent level from the start of the cursor row,
    /// adjusting `cursor.col` to follow. Tab-terminated leading
    /// whitespace drops one trailing `\t`; space-terminated leading
    /// whitespace rounds *down* to the nearest multiple of
    /// `indent.width` strictly below the current column count
    /// (so 8 → 4, 7 → 4, 4 → 0 with width 4).
    pub fn dedent_current_line(&mut self, indent: IndentSettings) {
        let line = self.lines[self.cursor.row].clone();
        let leading: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        if leading.is_empty() {
            return;
        }
        let remove_chars = if leading.ends_with('\t') {
            1
        } else {
            let trailing_spaces = leading.chars().rev().take_while(|c| *c == ' ').count();
            let w = indent.width.max(1);
            let target = (trailing_spaces.saturating_sub(1) / w) * w;
            trailing_spaces - target
        };
        let leading_char_count = leading.chars().count();
        let delete_start_char = leading_char_count - remove_chars;
        let delete_start_byte = char_to_byte(&line, delete_start_char);
        let delete_end_byte = char_to_byte(&line, delete_start_char + remove_chars);
        self.lines[self.cursor.row].replace_range(delete_start_byte..delete_end_byte, "");
        self.cursor.col = self.cursor.col.saturating_sub(remove_chars);
        self.touch();
    }

    pub fn insert_newline(&mut self, indent: IndentSettings) {
        // Splitting at column 0 just inserts a blank line *above* the
        // current content. The right half is the original line verbatim
        // — running auto-indent on it would re-indent text that the
        // user already placed at column 0 (e.g. tree-sitter's
        // `@indent.begin` fires on `func main() {` and would otherwise
        // push it one level deeper).
        if self.cursor.col == 0 {
            self.lines.insert(self.cursor.row, String::new());
            self.cursor.row += 1;
            self.touch();
            return;
        }
        let line = self.lines[self.cursor.row].clone();
        let byte_idx = char_to_byte(&line, self.cursor.col);
        let (left, right) = line.split_at(byte_idx);
        let left_owned = left.to_string();
        let right_owned = right.to_string();
        // Newline-specific indent rule (narrower than `o`/`O`): copy
        // the left half's leading whitespace, then add one level only
        // when tree-sitter's @indent.begin fires *and* the new line
        // isn't itself starting with an opener. We deliberately skip
        // the trailing-`{`/`(`/`[` heuristic — pressing Enter at the
        // end of `func main() {` shouldn't push the cursor into the
        // body, and splitting at `func main |{` shouldn't push the
        // brace deeper than the header.
        let prev = left_owned.chars().last();
        let next_ch = right_owned.chars().next();
        let base = copy_leading_indent(&left_owned, indent);
        let ts_begin = self
            .highlighter
            .as_ref()
            .is_some_and(|h| h.indent_begins_at(self.cursor.row));
        let next_is_opener = matches!(next_ch, Some('{' | '(' | '['));
        let mut new_indent = if ts_begin && !next_is_opener {
            add_one_indent_level(&base, indent)
        } else {
            base
        };

        // Empty-pair split: pressing Enter between an opener and its
        // matching closer (auto-paired or hand-typed) drops the closer
        // onto its own row at the original line's *base* indent, with
        // a blank +1-indented row between for the cursor. Without this
        // the closer would ride the inner indent and look like
        // `    }` inside `fn foo() {`, which the user expects to snap
        // back to column 0.
        let is_empty_pair = match (prev, next_ch) {
            (Some(p), Some(n)) => auto_pair_closer(p) == Some(n),
            _ => false,
        };
        if is_empty_pair {
            let base_indent = copy_leading_indent(&left_owned, indent);
            let mut closer_line = base_indent.clone();
            closer_line.push_str(&right_owned);
            let middle = add_one_indent_level(&base_indent, indent);
            self.cursor.col = middle.chars().count();
            self.lines[self.cursor.row] = left_owned;
            self.lines.insert(self.cursor.row + 1, middle);
            self.lines.insert(self.cursor.row + 2, closer_line);
            self.cursor.row += 1;
            self.touch();
            return;
        }

        // Closer at the start of the right half: the new line carries
        // the closer, so strip one indent level from the body indent —
        // without this, splitting before a `}` keeps the body's indent
        // and the closer sits one level too deep.
        if matches!(next_ch, Some('}' | ')' | ']')) {
            new_indent = strip_one_indent_level(&new_indent, indent);
        }

        self.lines[self.cursor.row] = left_owned;
        let mut next = new_indent.clone();
        next.push_str(&right_owned);
        self.lines.insert(self.cursor.row + 1, next);
        self.cursor.row += 1;
        self.cursor.col = new_indent.chars().count();
        self.touch();
    }

    /// Insert `s` at the cursor verbatim, treating any of `\n`, `\r\n`,
    /// or bare `\r` as a line break. No auto-indent, no auto-pair, no
    /// dedent-on-close — this is the bracketed-paste path, so the
    /// pasted text's existing indentation must survive intact. The
    /// cursor lands at the end of the inserted run.
    ///
    /// Why all three separators: bracketed paste hands us whatever the
    /// terminal forwards, and the convention differs by emulator —
    /// xterm and macOS Terminal.app use bare `\r`, Linux terminals use
    /// `\n`, Windows-attached sessions use `\r\n`. Splitting on only
    /// one leaves embedded control bytes in the buffer and the user
    /// sees `^M` artifacts on screen.
    pub fn insert_text_raw(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
        let segments: Vec<&str> = normalized.split('\n').collect();
        let row = self.cursor.row;
        let col = self.cursor.col;
        let byte_idx = char_to_byte(&self.lines[row], col);
        if segments.len() == 1 {
            self.lines[row].insert_str(byte_idx, segments[0]);
            self.cursor.col += segments[0].chars().count();
        } else {
            let tail = self.lines[row].split_off(byte_idx);
            self.lines[row].push_str(segments[0]);
            let last_idx = segments.len() - 1;
            for (i, seg) in segments.iter().enumerate().skip(1) {
                let mut new_line = String::with_capacity(
                    seg.len() + if i == last_idx { tail.len() } else { 0 },
                );
                new_line.push_str(seg);
                if i == last_idx {
                    new_line.push_str(&tail);
                }
                self.lines.insert(row + i, new_line);
            }
            self.cursor.row = row + last_idx;
            self.cursor.col = segments[last_idx].chars().count();
        }
        self.touch();
    }

    pub fn insert_line_below(&mut self, indent: IndentSettings) {
        let reference = self.lines[self.cursor.row].clone();
        let new_indent =
            compute_new_line_indent(&reference, self.cursor.row, &self.highlighter, indent);
        let col = new_indent.chars().count();
        self.lines.insert(self.cursor.row + 1, new_indent);
        self.cursor.row += 1;
        self.cursor.col = col;
        self.touch();
    }

    pub fn insert_line_above(&mut self, indent: IndentSettings) {
        // For `O`, match the indent of the line being pushed down —
        // the tree-sitter `@indent.begin` opening (if any) belongs to
        // that line, so we copy its leading whitespace verbatim
        // without adding an extra level.
        let new_indent = copy_leading_indent(&self.lines[self.cursor.row], indent);
        let col = new_indent.chars().count();
        self.lines.insert(self.cursor.row, new_indent);
        self.cursor.col = col;
        self.touch();
    }

    /// Delete the character under the cursor (vim's `x`). No-op past
    /// end of line. Cursor follows via `clamp_col` so it doesn't end up
    /// past-the-end after deleting the last char.
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

    /// Backspace primitive: delete the char before the cursor, or join
    /// with the previous line at column 0. The auto-pair / smart-indent
    /// behaviour wraps this in [`delete_char_before_smart`].
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

    /// Replace the character under the cursor with `ch`. No-op on an
    /// empty line — vim's `r` errors there; we silently skip.
    pub fn replace_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cursor.row];
        if self.cursor.col >= line.chars().count() {
            return;
        }
        let byte_idx = char_to_byte(line, self.cursor.col);
        let old_ch = line[byte_idx..].chars().next().unwrap();
        line.replace_range(byte_idx..byte_idx + old_ch.len_utf8(), &ch.to_string());
        self.touch();
    }

    /// Backspace with auto-pair awareness and smart-indent dedent:
    /// - When the char being deleted is an opener and the next char is
    ///   its matching closer, both go.
    /// - When the cursor sits in pure leading whitespace (every char on
    ///   the row before the cursor is whitespace, and `col > 0`), one
    ///   full indent level is removed instead of a single space —
    ///   standard "smart backspace" / "tab stops" behaviour. Closer-led
    ///   lines (`}` / `)` / `]`) collapse the same way as a side
    ///   effect, mirroring the dedent-on-type rule for closers.
    /// - At `col == 0` when the row above is blank (whitespace only) and
    ///   the current row's first non-whitespace char is a closer, do
    ///   the join *and* dedent the joined row by one level. The blank
    ///   row above carries no contextual indent, so an orphaned closer
    ///   left at a deeper level should collapse instead of just sliding
    ///   up at the same depth.
    /// - Otherwise falls through to [`Buffer::delete_char_before`].
    ///
    /// Single-cursor only — the multi-cursor fan-out keeps using the
    /// dumb version so the per-cursor `col -= 1` shift stays valid.
    pub fn delete_char_before_smart(&mut self, indent: IndentSettings) {
        let prev = self.char_before_cursor();
        let next = self.char_at_cursor();
        if let (Some(p), Some(n)) = (prev, next)
            && auto_pair_closer(p) == Some(n)
        {
            let line = &mut self.lines[self.cursor.row];
            let start_byte = char_to_byte(line, self.cursor.col - 1);
            let end_byte = char_to_byte(line, self.cursor.col + 1);
            line.replace_range(start_byte..end_byte, "");
            self.cursor.col -= 1;
            self.touch();
            return;
        }
        if self.cursor.col > 0 && self.line_is_blank_before_cursor() {
            // Only collapse a full indent level when the char immediately
            // before the cursor matches the configured indent character.
            // Otherwise (e.g. `use_tabs=true` but the leading run is
            // spaces, or `use_tabs=false` but it's a stray tab) the
            // "indent character" is effectively absent — fall through to
            // a plain single-char backspace instead of nibbling chars
            // that aren't part of the user's indent unit.
            let indent_char = if indent.use_tabs { '\t' } else { ' ' };
            if self.char_before_cursor() == Some(indent_char) {
                self.dedent_current_line(indent);
                return;
            }
        }
        if self.cursor.col == 0 && self.cursor.row > 0 {
            let prev_blank = self.lines[self.cursor.row - 1]
                .chars()
                .all(|c| c.is_whitespace());
            let curr_starts_with_closer = matches!(
                self.lines[self.cursor.row]
                    .chars()
                    .find(|c| !c.is_whitespace()),
                Some('}' | ')' | ']')
            );
            if prev_blank && curr_starts_with_closer {
                self.delete_char_before();
                self.dedent_current_line(indent);
                return;
            }
        }
        self.delete_char_before();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> IndentSettings {
        IndentSettings {
            width: 4,
            use_tabs: false,
        }
    }

    #[test]
    fn indent_line_adds_spaces_for_empty_leading() {
        let mut b = Buffer::new();
        b.lines = vec!["let x = 1;".into()];
        b.cursor.row = 0;
        b.cursor.col = 4;
        b.indent_line(0, settings());
        assert_eq!(b.lines[0], "    let x = 1;");
        assert_eq!(b.cursor.col, 8);
    }

    #[test]
    fn indent_line_uses_tab_when_leading_has_tab() {
        let mut b = Buffer::new();
        b.lines = vec!["\tx".into()];
        b.cursor.row = 0;
        b.indent_line(0, settings());
        assert_eq!(b.lines[0], "\t\tx");
    }

    #[test]
    fn indent_line_falls_back_to_use_tabs_on_blank_leading() {
        let mut b = Buffer::new();
        b.lines = vec!["x".into()];
        let s = IndentSettings {
            width: 4,
            use_tabs: true,
        };
        b.indent_line(0, s);
        assert_eq!(b.lines[0], "\tx");
    }

    #[test]
    fn dedent_line_removes_one_level_of_spaces() {
        let mut b = Buffer::new();
        b.lines = vec!["        x".into()];
        b.cursor.row = 0;
        b.cursor.col = 8;
        b.dedent_line(0, settings());
        assert_eq!(b.lines[0], "    x");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn dedent_line_rounds_partial_indent_down() {
        let mut b = Buffer::new();
        b.lines = vec!["       x".into()]; // 7 spaces
        b.dedent_line(0, settings());
        assert_eq!(b.lines[0], "    x");
    }

    #[test]
    fn dedent_line_strips_trailing_tab() {
        let mut b = Buffer::new();
        b.lines = vec!["\t\tx".into()];
        b.dedent_line(0, settings());
        assert_eq!(b.lines[0], "\tx");
    }

    #[test]
    fn dedent_line_noop_on_no_leading_whitespace() {
        let mut b = Buffer::new();
        b.lines = vec!["x".into()];
        b.cursor.col = 0;
        b.dedent_line(0, settings());
        assert_eq!(b.lines[0], "x");
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn open_below_copies_leading_whitespace() {
        let mut b = Buffer::new();
        b.lines = vec!["    let x = 1;".into(), "    let y = 2;".into()];
        b.cursor.row = 0;
        b.insert_line_below(settings());
        assert_eq!(b.lines[1], "    ");
        assert_eq!(b.cursor.row, 1);
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn open_below_adds_level_after_opening_brace() {
        let mut b = Buffer::new();
        b.lines = vec!["fn foo() {".into(), "}".into()];
        b.cursor.row = 0;
        b.insert_line_below(settings());
        assert_eq!(b.lines[1], "    ");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn open_below_uses_tabs_when_reference_does() {
        let mut b = Buffer::new();
        b.lines = vec!["\tfn foo() {".into(), "}".into()];
        b.cursor.row = 0;
        b.insert_line_below(settings());
        assert_eq!(b.lines[1], "\t\t");
    }

    #[test]
    fn open_above_copies_indent_without_adding_level() {
        let mut b = Buffer::new();
        b.lines = vec!["    let x = 1;".into()];
        b.cursor.row = 0;
        b.insert_line_above(settings());
        assert_eq!(b.lines[0], "    ");
        assert_eq!(b.cursor.row, 0);
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn newline_at_col_zero_leaves_original_line_unindented() {
        // Splitting at the very start of a line should drop a blank
        // line above and leave the original content where it sat —
        // even when the line opens a block (e.g. `func main() {`),
        // which would otherwise trip the trailing-opener / tree-sitter
        // `@indent.begin` rule and prepend an indent.
        let mut b = Buffer::new();
        b.lines = vec!["func main() {".into()];
        b.cursor.row = 0;
        b.cursor.col = 0;
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "");
        assert_eq!(b.lines[1], "func main() {");
        assert_eq!(b.cursor.row, 1);
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn newline_at_col_zero_preserves_existing_indent() {
        // Same shortcut, but the original line already has indent —
        // the right half keeps it verbatim, no extra level added.
        let mut b = Buffer::new();
        b.lines = vec!["    let x = 1;".into()];
        b.cursor.row = 0;
        b.cursor.col = 0;
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "");
        assert_eq!(b.lines[1], "    let x = 1;");
        assert_eq!(b.cursor.row, 1);
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn newline_after_trailing_opener_does_not_auto_indent() {
        // Pressing Enter at end of `func main() {` no longer adds an
        // indent level on its own — the trailing `{`/`(`/`[` heuristic
        // was too eager. Tree-sitter's @indent.begin handles real
        // cases; here there's no highlighter, so the new line stays
        // at base indent.
        let mut b = Buffer::new();
        b.lines = vec!["func main() {".into()];
        b.cursor.row = 0;
        b.cursor.col = 13; // end of line
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "func main() {");
        assert_eq!(b.lines[1], "");
        assert_eq!(b.cursor.row, 1);
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn newline_before_opener_keeps_base_indent() {
        // `func main |{` — splitting before `{` should leave the new
        // line (which starts with `{`) at the function header's base
        // indent, not one level deeper.
        let mut b = Buffer::new();
        b.lines = vec!["func main {".into()];
        b.cursor.row = 0;
        b.cursor.col = 10; // before '{'
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "func main ");
        assert_eq!(b.lines[1], "{");
        assert_eq!(b.cursor.row, 1);
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn newline_before_opener_preserves_outer_indent() {
        let mut b = Buffer::new();
        b.lines = vec!["    if cond ".into(), "(x) {}".into()];
        // Place cursor before `(` on line 1 to exercise the
        // opener-at-start rule with a non-empty base indent.
        b.lines = vec!["    if cond (x".into()];
        b.cursor.row = 0;
        b.cursor.col = 12; // before '('
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "    if cond ");
        assert_eq!(b.lines[1], "    (x");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn newline_splits_and_carries_indent() {
        let mut b = Buffer::new();
        b.lines = vec!["    let x = foo + bar;".into()];
        b.cursor.row = 0;
        b.cursor.col = 16; // between '+' and ' bar'
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "    let x = foo ");
        assert_eq!(b.lines[1], "    + bar;");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn close_bracket_dedents_when_line_is_blank() {
        // Typed `}` on a line that's all whitespace: dedent one
        // level, then insert.
        let mut b = Buffer::new();
        b.lines = vec!["fn foo() {".into(), "        ".into()];
        b.cursor.row = 1;
        b.cursor.col = 8;
        b.insert_char_smart('}', settings());
        assert_eq!(b.lines[1], "    }");
        assert_eq!(b.cursor.col, 5);
    }

    #[test]
    fn close_bracket_no_dedent_when_text_precedes() {
        // `}` after real code stays where the user typed it.
        let mut b = Buffer::new();
        b.lines = vec!["    let x = HashMap::new(".into()];
        b.cursor.row = 0;
        b.cursor.col = 25;
        b.insert_char_smart(')', settings());
        assert_eq!(b.lines[0], "    let x = HashMap::new()");
        assert_eq!(b.cursor.col, 26);
    }

    #[test]
    fn close_bracket_dedents_partial_indent() {
        // 7 spaces with width 4 → drop 3 to land on 4.
        let mut b = Buffer::new();
        b.lines = vec!["       ".into()];
        b.cursor.row = 0;
        b.cursor.col = 7;
        b.insert_char_smart(']', settings());
        assert_eq!(b.lines[0], "    ]");
        assert_eq!(b.cursor.col, 5);
    }

    #[test]
    fn close_bracket_dedents_tab_indent() {
        let mut b = Buffer::new();
        b.lines = vec!["\t\t".into()];
        b.cursor.row = 0;
        b.cursor.col = 2;
        b.insert_char_smart('}', settings());
        assert_eq!(b.lines[0], "\t}");
        assert_eq!(b.cursor.col, 2);
    }

    #[test]
    fn close_bracket_clears_indent_when_already_at_one_level() {
        let mut b = Buffer::new();
        b.lines = vec!["    ".into()];
        b.cursor.row = 0;
        b.cursor.col = 4;
        b.insert_char_smart('}', settings());
        assert_eq!(b.lines[0], "}");
        assert_eq!(b.cursor.col, 1);
    }

    #[test]
    fn auto_pair_inserts_matching_closer_and_keeps_cursor_between() {
        let mut b = Buffer::new();
        b.lines = vec!["foo".into()];
        b.cursor.row = 0;
        b.cursor.col = 3;
        b.insert_char_smart('(', settings());
        assert_eq!(b.lines[0], "foo()");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn auto_pair_skip_over_closer_when_next_char_matches() {
        let mut b = Buffer::new();
        b.lines = vec!["()".into()];
        b.cursor.row = 0;
        b.cursor.col = 1; // between '(' and ')'
        b.insert_char_smart(')', settings());
        assert_eq!(b.lines[0], "()");
        assert_eq!(b.cursor.col, 2);
    }

    #[test]
    fn auto_pair_suppressed_when_next_char_is_word() {
        let mut b = Buffer::new();
        b.lines = vec!["foo".into()];
        b.cursor.row = 0;
        b.cursor.col = 0; // about to type `(` before the `f`
        b.insert_char_smart('(', settings());
        assert_eq!(b.lines[0], "(foo");
        assert_eq!(b.cursor.col, 1);
    }

    #[test]
    fn auto_pair_quote_suppressed_after_word_char() {
        // Apostrophe in `it's` shouldn't grow into `it''`.
        let mut b = Buffer::new();
        b.lines = vec!["it".into()];
        b.cursor.row = 0;
        b.cursor.col = 2;
        b.insert_char_smart('\'', settings());
        assert_eq!(b.lines[0], "it'");
        assert_eq!(b.cursor.col, 3);
    }

    #[test]
    fn auto_pair_quote_pairs_after_punctuation() {
        let mut b = Buffer::new();
        b.lines = vec!["print(".into()];
        b.cursor.row = 0;
        b.cursor.col = 6;
        b.insert_char_smart('"', settings());
        assert_eq!(b.lines[0], "print(\"\"");
        assert_eq!(b.cursor.col, 7);
    }

    #[test]
    fn delete_char_before_smart_removes_empty_pair() {
        let mut b = Buffer::new();
        b.lines = vec!["foo()".into()];
        b.cursor.row = 0;
        b.cursor.col = 4; // between '(' and ')'
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "foo");
        assert_eq!(b.cursor.col, 3);
    }

    #[test]
    fn delete_char_before_smart_falls_through_when_not_empty_pair() {
        // Backspace inside `(x)` between `(` and `x` only removes `(`.
        let mut b = Buffer::new();
        b.lines = vec!["(x)".into()];
        b.cursor.row = 0;
        b.cursor.col = 1;
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "x)");
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn newline_inside_empty_braces_spreads_three_lines() {
        // Mid-line Enter between `{` and `}` (auto-paired or hand-typed)
        // drops the closer onto its own row at the base indent, with a
        // blank +1-indented row between for the cursor. Without the
        // 3-line spread the closer rides the inner indent.
        let mut b = Buffer::new();
        b.lines = vec!["fn foo() {}".into()];
        b.cursor.row = 0;
        b.cursor.col = 10; // between '{' and '}'
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "fn foo() {");
        assert_eq!(b.lines[1], "    ");
        assert_eq!(b.lines[2], "}");
        assert_eq!(b.cursor.row, 1);
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn newline_inside_empty_braces_preserves_outer_indent() {
        let mut b = Buffer::new();
        b.lines = vec!["    if cond {}".into()];
        b.cursor.row = 0;
        b.cursor.col = 13; // between '{' and '}'
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "    if cond {");
        assert_eq!(b.lines[1], "        ");
        assert_eq!(b.lines[2], "    }");
        assert_eq!(b.cursor.row, 1);
        assert_eq!(b.cursor.col, 8);
    }

    #[test]
    fn newline_inside_empty_parens_also_spreads() {
        let mut b = Buffer::new();
        b.lines = vec!["foo()".into()];
        b.cursor.row = 0;
        b.cursor.col = 4; // between '(' and ')'
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "foo(");
        assert_eq!(b.lines[1], "    ");
        assert_eq!(b.lines[2], ")");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn newline_before_closer_strips_one_indent_level() {
        // Cursor sits before the `}` after a real statement: the new
        // line carries the closer, so it should land one indent level
        // out from the body — `}` aligned to the block's opener, not
        // riding the body indent.
        let mut b = Buffer::new();
        b.lines = vec!["        bar();}".into()];
        b.cursor.row = 0;
        b.cursor.col = 14; // before '}'
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "        bar();");
        assert_eq!(b.lines[1], "    }");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn newline_before_closer_clears_indent_at_one_level() {
        let mut b = Buffer::new();
        b.lines = vec!["    bar();]".into()];
        b.cursor.row = 0;
        b.cursor.col = 10; // before ']'
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "    bar();");
        assert_eq!(b.lines[1], "]");
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn newline_before_closer_strips_one_tab() {
        let mut b = Buffer::new();
        b.lines = vec!["\t\tbar();)".into()];
        b.cursor.row = 0;
        b.cursor.col = 8; // before ')'
        b.insert_newline(settings());
        assert_eq!(b.lines[0], "\t\tbar();");
        assert_eq!(b.lines[1], "\t)");
    }

    #[test]
    fn backspace_before_closer_collapses_one_indent_level() {
        // Cursor on a blank-before-closer line: backspace pulls the
        // closer back one full indent level instead of nibbling a
        // single space at a time.
        let mut b = Buffer::new();
        b.lines = vec!["        }".into()];
        b.cursor.row = 0;
        b.cursor.col = 8; // before '}'
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "    }");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn backspace_before_closer_clears_indent_at_one_level() {
        let mut b = Buffer::new();
        b.lines = vec!["    }".into()];
        b.cursor.row = 0;
        b.cursor.col = 4;
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "}");
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn backspace_before_closer_strips_tab() {
        let mut b = Buffer::new();
        b.lines = vec!["\t\t]".into()];
        b.cursor.row = 0;
        b.cursor.col = 2;
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "\t]");
        assert_eq!(b.cursor.col, 1);
    }

    #[test]
    fn backspace_does_not_dedent_when_text_precedes_closer() {
        // Real content before the closer — normal one-char backspace.
        let mut b = Buffer::new();
        b.lines = vec!["    x)".into()];
        b.cursor.row = 0;
        b.cursor.col = 5; // between 'x' and ')'
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "    )");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn backspace_inside_closer_line_indent_dedents() {
        // Cursor anywhere inside the leading-whitespace run of a line
        // that's "indent + closer" — backspace dedents the whole line
        // by one level, not just one space.
        let mut b = Buffer::new();
        b.lines = vec!["        }".into()];
        b.cursor.row = 0;
        b.cursor.col = 4; // mid-indent, not at the closer
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "    }");
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn backspace_on_closer_line_with_trailing_content_dedents() {
        let mut b = Buffer::new();
        b.lines = vec!["        });".into()];
        b.cursor.row = 0;
        b.cursor.col = 8; // before ')'
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "    });");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn backspace_inside_pure_whitespace_line_dedents() {
        // Empty-but-indented line (e.g., the middle row of the 3-line
        // spread after Enter inside `{}`). Backspace should collapse
        // one indent level, not nibble a single space.
        let mut b = Buffer::new();
        b.lines = vec!["    ".into()];
        b.cursor.row = 0;
        b.cursor.col = 4;
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "");
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn backspace_in_indent_before_content_dedents() {
        // Cursor in leading whitespace before regular content — also
        // dedents (standard smart-backspace behaviour).
        let mut b = Buffer::new();
        b.lines = vec!["        let x = 1;".into()];
        b.cursor.row = 0;
        b.cursor.col = 8; // right before 'let'
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "    let x = 1;");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn backspace_past_first_non_blank_is_normal_one_char() {
        // Once we're into content, backspace is one char as usual.
        let mut b = Buffer::new();
        b.lines = vec!["    let x = 1;".into()];
        b.cursor.row = 0;
        b.cursor.col = 7; // between 't' of 'let' and ' '
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "    le x = 1;");
        assert_eq!(b.cursor.col, 6);
    }

    #[test]
    fn backspace_at_col0_of_closer_line_above_empty_joins_and_dedents() {
        // Closer line orphaned over an empty row: Backspace at col 0
        // should both eat the empty row above and dedent the closer.
        let mut b = Buffer::new();
        b.lines = vec!["".into(), "    }".into()];
        b.cursor.row = 1;
        b.cursor.col = 0;
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines, vec!["}".to_string()]);
        assert_eq!(b.cursor.row, 0);
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn backspace_at_col0_of_closer_line_above_blank_joins_and_dedents() {
        // Whitespace-only row above counts as blank too.
        let mut b = Buffer::new();
        b.lines = vec!["    ".into(), "        }".into()];
        b.cursor.row = 1;
        b.cursor.col = 0;
        b.delete_char_before_smart(settings());
        // Join concatenates leading whitespace, then one level dedent.
        assert_eq!(b.lines, vec!["        }".to_string()]);
        assert_eq!(b.cursor.row, 0);
    }

    #[test]
    fn backspace_at_col0_above_content_does_not_dedent() {
        // Row above has real content — vanilla join, no dedent.
        let mut b = Buffer::new();
        b.lines = vec!["foo".into(), "    }".into()];
        b.cursor.row = 1;
        b.cursor.col = 0;
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines, vec!["foo    }".to_string()]);
        assert_eq!(b.cursor.row, 0);
        assert_eq!(b.cursor.col, 3);
    }

    #[test]
    fn backspace_in_space_indent_under_tab_settings_falls_back_to_one_char() {
        // `use_tabs=true` but the leading run is spaces — the indent
        // character (`\t`) isn't found, so backspace nibbles one space
        // instead of swallowing the whole space run.
        let mut b = Buffer::new();
        b.lines = vec!["    foo".into()];
        b.cursor.row = 0;
        b.cursor.col = 4;
        let indent = IndentSettings {
            width: 4,
            use_tabs: true,
        };
        b.delete_char_before_smart(indent);
        assert_eq!(b.lines[0], "   foo");
        assert_eq!(b.cursor.col, 3);
    }

    #[test]
    fn backspace_in_tab_indent_under_space_settings_falls_back_to_one_char() {
        // `use_tabs=false` but the leading run is a tab — the indent
        // character (` `) isn't found, so backspace deletes one char
        // (the tab) without rounding.
        let mut b = Buffer::new();
        b.lines = vec!["\tfoo".into()];
        b.cursor.row = 0;
        b.cursor.col = 1;
        b.delete_char_before_smart(settings());
        assert_eq!(b.lines[0], "foo");
        assert_eq!(b.cursor.col, 0);
    }

    #[test]
    fn insert_text_raw_single_line_splices_at_cursor() {
        let mut b = Buffer::new();
        b.lines = vec!["abef".into()];
        b.cursor.row = 0;
        b.cursor.col = 2;
        b.insert_text_raw("cd");
        assert_eq!(b.lines[0], "abcdef");
        assert_eq!(b.cursor.col, 4);
    }

    #[test]
    fn insert_text_raw_multiline_preserves_indent_verbatim() {
        // Pasting pre-indented text must not have auto-indent stacked on
        // top — each line lands exactly as it appeared in the payload.
        let mut b = Buffer::new();
        b.lines = vec!["    pub fn foo() {".into(), "    }".into()];
        b.cursor.row = 0;
        b.cursor.col = 18; // end of line 0
        b.insert_text_raw("\n        let x = 1;\n        let y = 2;");
        assert_eq!(b.lines[0], "    pub fn foo() {");
        assert_eq!(b.lines[1], "        let x = 1;");
        assert_eq!(b.lines[2], "        let y = 2;");
        assert_eq!(b.lines[3], "    }");
        assert_eq!(b.cursor.row, 2);
        assert_eq!(b.cursor.col, 18);
    }

    #[test]
    fn insert_text_raw_normalizes_crlf() {
        let mut b = Buffer::new();
        b.lines = vec!["".into()];
        b.insert_text_raw("foo\r\nbar");
        assert_eq!(b.lines, vec!["foo".to_string(), "bar".to_string()]);
    }

    #[test]
    fn insert_text_raw_normalizes_bare_cr() {
        // xterm / macOS Terminal.app deliver bracketed-paste line
        // breaks as bare `\r`. Without normalization the CRs end up
        // embedded in a single line and the renderer surfaces them as
        // `^M`-style artifacts.
        let mut b = Buffer::new();
        b.lines = vec!["".into()];
        b.insert_text_raw("foo\rbar\rbaz");
        assert_eq!(
            b.lines,
            vec!["foo".to_string(), "bar".to_string(), "baz".to_string()]
        );
    }

    #[test]
    fn insert_text_raw_splits_existing_tail_onto_last_line() {
        // Mid-line paste: anything after the cursor on the original row
        // rides the last pasted segment.
        let mut b = Buffer::new();
        b.lines = vec!["abcXYZ".into()];
        b.cursor.row = 0;
        b.cursor.col = 3;
        b.insert_text_raw("1\n2\n3");
        assert_eq!(b.lines, vec!["abc1", "2", "3XYZ"]);
        assert_eq!(b.cursor.row, 2);
        assert_eq!(b.cursor.col, 1);
    }
}
