//! Range-level and line-level edits, plus the yank register.
//!
//! Buffer mutations that operate on a span (line, char range, column
//! block) and stash the deleted/copied text into `Buffer.yank`. Single-
//! character edits sit in [`super::insert`].

use super::{Buffer, Cursor, char_to_byte};

impl Buffer {
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
}

impl Buffer {
    /// Apply a per-character transform across the half-open range
    /// `[from, to)`. The two endpoints may sit on different rows.
    /// Backs the visual-mode `~` / `u` / `U` family.
    pub fn transform_case_range(&mut self, from: Cursor, to: Cursor, f: fn(char) -> char) {
        let (from, to) = order(from, to);
        if from == to {
            return;
        }
        for row in from.row..=to.row {
            let chars: Vec<char> = self.lines[row].chars().collect();
            let lo = if row == from.row { from.col } else { 0 };
            let hi = if row == to.row {
                to.col.min(chars.len())
            } else {
                chars.len()
            };
            if lo >= hi {
                continue;
            }
            self.lines[row] = chars
                .iter()
                .enumerate()
                .map(|(i, c)| if i >= lo && i < hi { f(*c) } else { *c })
                .collect();
        }
        self.touch();
    }

    /// Apply a per-character transform to every char on rows
    /// `[from_row..=to_row]`.
    pub fn transform_case_lines(&mut self, from_row: usize, to_row: usize, f: fn(char) -> char) {
        let (a, b) = (from_row.min(to_row), from_row.max(to_row));
        let b = b.min(self.lines.len().saturating_sub(1));
        for row in a..=b {
            self.lines[row] = self.lines[row].chars().map(f).collect();
        }
        self.touch();
    }

    /// Apply a per-character transform across a column rectangle.
    pub fn transform_case_block(
        &mut self,
        r0: usize,
        c0: usize,
        r1: usize,
        c1: usize,
        f: fn(char) -> char,
    ) {
        let (r0, r1) = (r0.min(r1), r0.max(r1));
        let (c0, c1) = (c0.min(c1), c0.max(c1));
        let r1 = r1.min(self.lines.len().saturating_sub(1));
        for row in r0..=r1 {
            let chars: Vec<char> = self.lines[row].chars().collect();
            self.lines[row] = chars
                .iter()
                .enumerate()
                .map(|(i, c)| if i >= c0 && i <= c1 { f(*c) } else { *c })
                .collect();
        }
        self.touch();
    }
}

/// Lowercase a char, keeping its column width. Multi-char expansions
/// (eg. Turkish `İ` → two codepoints) fall back to the original so
/// column counts stay stable.
pub fn to_lower_keep_width(c: char) -> char {
    if c.is_uppercase() {
        let mut it = c.to_lowercase();
        let first = it.next().unwrap_or(c);
        if it.next().is_some() { c } else { first }
    } else {
        c
    }
}

/// Uppercase a char, keeping its column width. See [`to_lower_keep_width`].
pub fn to_upper_keep_width(c: char) -> char {
    if c.is_lowercase() {
        let mut it = c.to_uppercase();
        let first = it.next().unwrap_or(c);
        if it.next().is_some() { c } else { first }
    } else {
        c
    }
}

/// Flip a single character's case: upper→lower, lower→upper, others
/// unchanged. For chars whose case expansion is multi-char (a tiny
/// minority — eg. German `ß` → `SS`) we fall back to the original
/// char to keep column counts stable.
pub fn flip_case_char_keep_width(c: char) -> char {
    if c.is_uppercase() {
        let mut it = c.to_lowercase();
        let first = it.next().unwrap_or(c);
        if it.next().is_some() { c } else { first }
    } else if c.is_lowercase() {
        let mut it = c.to_uppercase();
        let first = it.next().unwrap_or(c);
        if it.next().is_some() { c } else { first }
    } else {
        c
    }
}

fn order(a: Cursor, b: Cursor) -> (Cursor, Cursor) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

// ────────────────────────────────────────────────────────────────────────
// Line-level edits.
// ────────────────────────────────────────────────────────────────────────

impl Buffer {
    /// Join the next line into the current one with a single space
    /// separator (vim's `J`). Strips leading whitespace on the joined
    /// line; if the current line ends in whitespace or is empty, no
    /// space is inserted. Cursor lands on the join boundary.
    pub fn join_next_line(&mut self) {
        if self.cursor.row + 1 >= self.lines.len() {
            return;
        }
        let next = self.lines.remove(self.cursor.row + 1);
        let next_trimmed = next.trim_start();
        let cur = &mut self.lines[self.cursor.row];
        let needs_space = !cur.is_empty()
            && !cur
                .chars()
                .last()
                .map(|c| c.is_whitespace())
                .unwrap_or(false)
            && !next_trimmed.is_empty();
        let join_col = cur.chars().count();
        if needs_space {
            cur.push(' ');
        }
        cur.push_str(next_trimmed);
        self.cursor.col = join_col;
        self.touch();
    }

    /// Toggle the case of the character under the cursor, then advance
    /// one column (vim's `~`). No-op on an empty line.
    pub fn toggle_case_under_cursor(&mut self) {
        let line = &mut self.lines[self.cursor.row];
        if self.cursor.col >= line.chars().count() {
            return;
        }
        let byte_idx = char_to_byte(line, self.cursor.col);
        let ch = line[byte_idx..].chars().next().unwrap();
        let replacement: String = if ch.is_uppercase() {
            ch.to_lowercase().collect()
        } else if ch.is_lowercase() {
            ch.to_uppercase().collect()
        } else {
            return; // not a cased letter — leave it and don't advance
        };
        line.replace_range(byte_idx..byte_idx + ch.len_utf8(), &replacement);
        self.touch();
        // Advance, allowing past-end only inside Insert (we're in Normal
        // here, so clamp to last col).
        let max = self.current_line_len().saturating_sub(1);
        if self.cursor.col < max {
            self.cursor.col += 1;
        }
    }

    /// Delete from `cursor` to the end of the current line (vim's `D`).
    /// The deleted text goes into the yank register.
    pub fn delete_to_eol(&mut self) {
        let line = self.lines[self.cursor.row].clone();
        let byte_idx = char_to_byte(&line, self.cursor.col);
        self.yank = line[byte_idx..].to_string();
        self.lines[self.cursor.row].truncate(byte_idx);
        self.touch();
        self.clamp_col(false);
    }

    /// Replace the entire current line with an empty string (vim's
    /// `S`). The full line content goes into the yank register.
    pub fn clear_current_line(&mut self) {
        self.yank = self.lines[self.cursor.row].clone();
        self.lines[self.cursor.row].clear();
        self.cursor.col = 0;
        self.touch();
    }

    /// Toggle a single-line comment on the current line using `token`
    /// as the prefix (e.g. `"//"`, `"#"`). If the first non-blank run
    /// of the line already starts with `token`, the prefix (and a
    /// single trailing space, when present) is stripped; otherwise
    /// `token + " "` is inserted at the first non-blank column. Blank
    /// lines are skipped — vim-commentary semantics.
    pub fn toggle_line_comment(&mut self, token: &str) {
        let row = self.cursor.row;
        let line = &self.lines[row];
        let indent_chars = line.chars().take_while(|c| c.is_whitespace()).count();
        let indent_bytes = char_to_byte(line, indent_chars);
        let rest = &line[indent_bytes..];
        if rest.is_empty() {
            return;
        }
        if let Some(after_token) = rest.strip_prefix(token) {
            let trim_len = if after_token.starts_with(' ') {
                token.len() + 1
            } else {
                token.len()
            };
            self.lines[row].replace_range(indent_bytes..indent_bytes + trim_len, "");
            let removed_chars = token.chars().count() + (trim_len - token.len());
            if self.cursor.col > indent_chars {
                self.cursor.col = self.cursor.col.saturating_sub(removed_chars);
            }
        } else {
            let insert = format!("{} ", token);
            self.lines[row].insert_str(indent_bytes, &insert);
            let added_chars = insert.chars().count();
            if self.cursor.col >= indent_chars {
                self.cursor.col += added_chars;
            }
        }
        self.clamp_col(false);
        self.touch();
    }
}
