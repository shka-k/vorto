//! Active completion popup state.
//!
//! Lives on `App` rather than in `Prompt` because the popup is
//! non-modal: the user keeps typing in insert mode while it's open,
//! and the menu just re-filters in-place. The prompt machinery is for
//! modal inputs (`:` / `/` / fuzzy / rename / code-action), which
//! suspend insert-mode entirely.
//!
//! Lifecycle:
//! 1. `<C-Space>` in insert mode triggers `App::lsp_completion`, which
//!    snapshots `prefix_start` (the column where the identifier under
//!    the cursor begins) and fires `textDocument/completion`.
//! 2. The response arrives via the LSP event channel; if `prefix_start`
//!    still matches, the items are stashed here and the popup opens.
//! 3. Insert-mode keystrokes pass through the existing buffer
//!    mutations; `App::update_completion_filter` re-runs after each one
//!    to refresh `filtered` against the live prefix.
//! 4. Accept (Enter/Tab) applies the server's `textEdit` if it has one,
//!    otherwise replaces `[prefix_start..cursor]` with the item's
//!    `insertText`/`label`. Esc / cursor jump / row change closes.

use crate::editor::Cursor;
use crate::lsp::CompletionItem;

/// Active completion popup. `None` on `App` when nothing is showing.
pub struct CompletionState {
    /// Where the identifier being completed starts. Filtering compares
    /// the text from this column to the live cursor column against
    /// each item's `filter_text` / `label`.
    pub prefix_start: Cursor,
    /// Raw items as the server returned them. Never mutated after
    /// install; `filtered` is the view that gets re-derived.
    pub items: Vec<CompletionItem>,
    /// Indices into `items` that match the live prefix, in the order
    /// they should appear in the popup.
    pub filtered: Vec<usize>,
    /// Selected row inside `filtered`. Clamped to `filtered.len() - 1`
    /// on every re-filter so it never points past the end.
    pub selected: usize,
}

impl CompletionState {
    /// Build state for a fresh server response. `prefix` is the text the
    /// user has already typed (from `prefix_start` to cursor) — used to
    /// pre-filter so the popup opens already narrowed instead of
    /// flashing the full list and then collapsing on the next keystroke.
    pub fn new(prefix_start: Cursor, items: Vec<CompletionItem>, prefix: &str) -> Self {
        let mut s = Self {
            prefix_start,
            items,
            filtered: Vec::new(),
            selected: 0,
        };
        s.refilter(prefix);
        s
    }

    /// Re-derive `filtered` against `prefix`. Empty prefix shows every
    /// item; otherwise we keep only items whose `filter_text` (or
    /// `label`, when `filter_text` is absent) contains `prefix` as a
    /// case-insensitive substring. Within the survivors, items whose
    /// match starts at position 0 sort ahead of mid-string matches —
    /// that keeps `vec` ranking ahead of `IntoIterator::vec_into` for
    /// the prefix `"vec"`. Server-supplied `sort_text` breaks ties.
    pub fn refilter(&mut self, prefix: &str) {
        let needle = prefix.to_lowercase();
        let mut scored: Vec<(usize, usize, &str)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let hay = it.filter_text.as_deref().unwrap_or(&it.label).to_lowercase();
                let pos = if needle.is_empty() {
                    Some(0)
                } else {
                    hay.find(&needle)
                };
                pos.map(|p| {
                    let sort = it.sort_text.as_deref().unwrap_or(&it.label);
                    (i, p, sort)
                })
            })
            .collect();
        // Primary: prefix matches before substring matches.
        // Secondary: server's `sort_text` (lexicographic).
        scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(b.2)));
        self.filtered = scored.into_iter().map(|(i, _, _)| i).collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    pub fn current(&self) -> Option<&CompletionItem> {
        self.filtered
            .get(self.selected)
            .and_then(|i| self.items.get(*i))
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
    }
}

/// Walk back from `cursor` over identifier-continuation chars (letters,
/// digits, `_`) and return the column where that run started. When the
/// cursor isn't sitting on a word, returns the cursor column unchanged
/// — that's the "completing from scratch" case.
pub fn identifier_prefix_start(line: &str, cursor_col: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut col = cursor_col.min(chars.len());
    while col > 0 {
        let c = chars[col - 1];
        if is_ident_continue(c) {
            col -= 1;
        } else {
            break;
        }
    }
    col
}

/// True for chars that should extend an identifier (and therefore
/// auto-trigger completion when typed). Letters, digits, and `_` —
/// matches our `identifier_prefix_start` walk.
pub fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Extract `[prefix_start_col..cursor_col]` from `line` as a `String`.
/// Returns empty when the cursor is at or before `prefix_start_col`
/// (the "we've already backspaced past where completion started" case
/// — caller should close the popup).
pub fn prefix_slice(line: &str, prefix_start_col: usize, cursor_col: usize) -> String {
    if cursor_col <= prefix_start_col {
        return String::new();
    }
    line.chars()
        .skip(prefix_start_col)
        .take(cursor_col - prefix_start_col)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            kind: 0,
            text_edit: None,
            insert_text: None,
            filter_text: None,
            sort_text: None,
            detail: None,
            additional_text_edits: Vec::new(),
        }
    }

    #[test]
    fn prefix_start_walks_back_over_ident_chars() {
        // `let foo_bar` with cursor after the `r` (col 11) should start at col 4.
        let line = "let foo_bar";
        assert_eq!(identifier_prefix_start(line, line.chars().count()), 4);
        // Cursor on whitespace → starts where it is.
        assert_eq!(identifier_prefix_start("let  x", 4), 4);
        // Cursor at start of line.
        assert_eq!(identifier_prefix_start("abc", 0), 0);
    }

    #[test]
    fn refilter_prefers_prefix_matches() {
        let items = vec![item("into_vec"), item("vec"), item("vector")];
        let mut s = CompletionState::new(Cursor { row: 0, col: 0 }, items, "vec");
        // "vec" and "vector" start with the needle (pos 0), "into_vec" doesn't (pos 5).
        let order: Vec<&str> = s.filtered.iter().map(|i| s.items[*i].label.as_str()).collect();
        assert_eq!(order, vec!["vec", "vector", "into_vec"]);
        // Empty prefix → every item, original order.
        s.refilter("");
        assert_eq!(s.filtered.len(), 3);
    }

    #[test]
    fn refilter_clamps_selection() {
        let items = vec![item("aaa"), item("aab"), item("aac")];
        let mut s = CompletionState::new(Cursor { row: 0, col: 0 }, items, "");
        s.selected = 2;
        s.refilter("aac");
        assert_eq!(s.filtered.len(), 1);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn move_selection_wraps() {
        let items = vec![item("a"), item("b"), item("c")];
        let mut s = CompletionState::new(Cursor { row: 0, col: 0 }, items, "");
        s.move_selection(1);
        assert_eq!(s.selected, 1);
        s.move_selection(-2);
        assert_eq!(s.selected, 2); // wrapped
        s.move_selection(1);
        assert_eq!(s.selected, 0); // wrapped
    }

    #[test]
    fn prefix_slice_extracts_typed_chars() {
        assert_eq!(prefix_slice("let foo_bar", 4, 7), "foo");
        // Cursor at or before prefix_start → empty.
        assert_eq!(prefix_slice("xy", 2, 1), "");
        assert_eq!(prefix_slice("xy", 2, 2), "");
    }
}
