//! Per-buffer tree-sitter highlighter.
//!
//! [`Highlighter`] holds a parser, the last parsed tree, the compiled
//! query, and a snapshot of the source the tree was built from. Re-parses
//! lazily when [`Highlighter::refresh`] is called with a newer version
//! than the one already cached.
//!
//! Failures (broken `.so`, query compile error, ABI mismatch) bubble up
//! as `anyhow::Error` from [`Highlighter::new`] so the caller can fall
//! back to plain text gracefully.

use anyhow::{Context, Result};
use tree_sitter::{
    InputEdit, Language, Parser, Point, Query, QueryCursor, QueryPredicateArg, StreamingIterator,
    Tree,
};

/// Per-buffer state: parser, tree, queries, and the source the tree
/// was built from. Refreshes the tree only when `refresh()` is called
/// with a version newer than the one already cached, so callers can
/// poke at it freely from a hot draw loop.
pub struct Highlighter {
    parser: Parser,
    query: Query,
    /// Optional secondary query for `textobjects.scm`. Drives the
    /// tree-sitter-aware text objects (`if`/`af` for function, etc.).
    /// `None` when the language has no textobjects file installed.
    textobjects: Option<Query>,
    textobject_capture_names: Vec<String>,
    /// Optional `indents.scm` query. Drives the auto-indent on newline
    /// / `o` / `O`. `None` when the language ships no indents file.
    indents: Option<Query>,
    indent_capture_names: Vec<String>,
    tree: Option<Tree>,
    source: String,
    parsed_version: Option<u64>,
    capture_names: Vec<String>,
    /// Non-fatal warnings collected during construction (e.g. an
    /// `indents.scm` that failed to compile). The TUI drains these into
    /// toasts; writing to stderr from a worker thread would corrupt the
    /// alt-screen display.
    pub warnings: Vec<String>,
}

impl Highlighter {
    pub(super) fn new(
        language: Language,
        highlights_src: &str,
        textobjects_src: Option<&str>,
        indents_src: Option<&str>,
    ) -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .context("setting parser language (ABI mismatch?)")?;
        let query = Query::new(&language, highlights_src).context("compiling highlights query")?;
        let capture_names = query
            .capture_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (textobjects, textobject_capture_names) = match textobjects_src {
            Some(src) => {
                let q = Query::new(&language, src).context("compiling textobjects query")?;
                let names = q.capture_names().iter().map(|s| s.to_string()).collect();
                (Some(q), names)
            }
            None => (None, Vec::new()),
        };
        // Indents query failures are non-fatal: a bad node name in
        // indents.scm just disables auto-indent for the language —
        // we still want highlighting/textobjects to work. The
        // trailing-bracket fallback in `compute_new_line_indent`
        // keeps newline/o/O usable in this degraded state.
        let mut warnings = Vec::new();
        let (indents, indent_capture_names) = match indents_src {
            Some(src) => match Query::new(&language, src) {
                Ok(q) => {
                    let names = q.capture_names().iter().map(|s| s.to_string()).collect();
                    (Some(q), names)
                }
                Err(e) => {
                    warnings.push(format!(
                        "indents.scm compile failed, auto-indent disabled: {e}"
                    ));
                    (None, Vec::new())
                }
            },
            None => (None, Vec::new()),
        };
        Ok(Self {
            parser,
            query,
            textobjects,
            textobject_capture_names,
            indents,
            indent_capture_names,
            tree: None,
            source: String::new(),
            parsed_version: None,
            capture_names,
            warnings,
        })
    }

    /// Re-parse `source` if it's newer than the cached tree.
    ///
    /// When a previous tree is cached, computes the byte-range diff
    /// against the old source, applies it via `Tree::edit`, and asks
    /// tree-sitter to reuse the existing tree — incremental parsing is
    /// the whole point of `tree-sitter`, and the previous code was
    /// passing `None` here, so every keystroke re-parsed the entire
    /// file. With incremental, edits past the affected node are O(1).
    pub fn refresh(&mut self, source: &str, version: u64) {
        if self.parsed_version == Some(version) {
            return;
        }
        let old_tree = match self.tree.as_mut() {
            Some(tree) if !self.source.is_empty() => {
                let edit = compute_input_edit(&self.source, source);
                tree.edit(&edit);
                Some(&*tree)
            }
            _ => None,
        };
        self.tree = self.parser.parse(source, old_tree);
        self.source = source.to_string();
        self.parsed_version = Some(version);
    }

    /// Return all captures intersecting rows `[start_row..=end_row]`
    /// of the last-parsed source, with column values already converted
    /// from byte offsets to character offsets so callers can directly
    /// index into character-based line strings.
    pub fn captures_in_rows(&self, start_row: usize, end_row: usize) -> Vec<Capture> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&self.query, tree.root_node(), self.source.as_bytes());

        let mut out = Vec::new();
        // `QueryMatches` is a streaming iterator in tree-sitter 0.25+,
        // so we drive it with an explicit `.next()` loop rather than
        // `for ... in`.
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let node = cap.node;
                let start = node.start_position();
                let end = node.end_position();
                if end.row < start_row || start.row > end_row {
                    continue;
                }
                let name = self
                    .capture_names
                    .get(cap.index as usize)
                    .cloned()
                    .unwrap_or_default();
                out.push(Capture {
                    start_row: start.row,
                    start_col: byte_to_char_col(&self.source, start.row, start.column),
                    end_row: end.row,
                    end_col: byte_to_char_col(&self.source, end.row, end.column),
                    name,
                });
            }
        }
        // Stable, document-order sort. Captures coming *later* in the
        // query file are usually more specific (vim's vimscript-ish
        // priority by source order), so a "last write wins" overlay in
        // the UI layer picks the more specific styling.
        out.sort_by_key(|c| (c.start_row, c.start_col));
        out
    }

    /// True when the `indents.scm` query has an `@indent.begin` capture
    /// whose node *opens* on `row`. Used by the auto-indent path to
    /// decide whether a new line inserted after `row` should pick up
    /// one extra indent level beyond the row's existing leading
    /// whitespace.
    ///
    /// Two shapes are accepted:
    /// - `start_row == row && end_row > row`: the node already has a
    ///   body that wraps the following lines (e.g. `def f():\n    x`).
    /// - `start_row == row && end_row == row` with an empty `body`
    ///   child: the node is mid-construction — the user just typed
    ///   `def f():` and hasn't filled in the body yet, so tree-sitter
    ///   reports a zero-width body block on the same row. Without this
    ///   branch Python auto-indent never fires while typing, only
    ///   after-the-fact when there's already body content below.
    ///
    /// Returns `false` when no indents query is installed, the tree
    /// hasn't been built yet, or no matching capture starts on `row`.
    pub fn indent_begins_at(&self, row: usize) -> bool {
        let Some(tree) = self.tree.as_ref() else {
            return false;
        };
        let Some(query) = self.indents.as_ref() else {
            return false;
        };
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), self.source.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let name = self
                    .indent_capture_names
                    .get(cap.index as usize)
                    .map(String::as_str)
                    .unwrap_or("");
                if name != "indent.begin" {
                    continue;
                }
                let node = cap.node;
                let start_row = node.start_position().row;
                let end_row = node.end_position().row;
                if start_row != row {
                    continue;
                }
                if end_row > row {
                    return true;
                }
                // Same-row span: distinguish an incomplete header
                // (empty body, user about to type it) from a true
                // one-liner (`if x: y` — body has content, no
                // auto-indent wanted).
                if let Some(body) = node.child_by_field_name("body")
                    && body.start_byte() == body.end_byte()
                {
                    return true;
                }
            }
        }
        false
    }

    /// Indent scopes intersecting the visible window `[start_row, end_row]`.
    ///
    /// Each scope corresponds to an `@indent.begin` node from the
    /// language's `indents.scm`. The returned tuple is
    /// `(scope_start_row, scope_end_row)` in source-row coordinates,
    /// inclusive on both ends. Same-row scopes (an unfilled header like
    /// `def f():` with an empty body) are dropped — they contribute no
    /// body rows to draw a guide on.
    ///
    /// Callers convert each scope's start-row leading whitespace into a
    /// visual column to position the guide line.
    pub fn indent_scopes_in_rows(&self, start_row: usize, end_row: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let Some(tree) = self.tree.as_ref() else {
            return out;
        };
        let Some(query) = self.indents.as_ref() else {
            return out;
        };
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), self.source.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let name = self
                    .indent_capture_names
                    .get(cap.index as usize)
                    .map(String::as_str)
                    .unwrap_or("");
                if name != "indent.begin" {
                    continue;
                }
                let node = cap.node;
                let s = node.start_position().row;
                let e = node.end_position().row;
                if e <= s {
                    continue;
                }
                if e < start_row || s > end_row {
                    continue;
                }
                out.push((s, e));
            }
        }
        // Sort by start row, then by *descending* end row so an enclosing
        // scope precedes its children — the renderer iterates in order
        // and the cursor's "innermost containing" pick is just the last
        // scope that contains it.
        out.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        out
    }

    /// Find the smallest text-object range matching `target` (a query
    /// capture name like `"function.outer"`) that contains the cursor.
    /// Returns `None` when no `textobjects.scm` is loaded, the tree
    /// hasn't been built yet, or no match contains the cursor.
    ///
    /// Both direct captures and ranges synthesized via the
    /// `(#make-range! "name" @start @end)` predicate are considered —
    /// the latter is how `nvim-treesitter-textobjects` defines most
    /// `.inner` ranges (function/class body excluding braces, etc.).
    /// Returned coordinates are `(start_row, start_col_chars,
    /// end_row, end_col_chars)`, with `end` exclusive — ready to feed
    /// into `Buffer::delete_range` / `yank_range`.
    pub fn find_text_object(
        &self,
        target: &str,
        cursor_row: usize,
        cursor_col_chars: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        let tree = self.tree.as_ref()?;
        let query = self.textobjects.as_ref()?;

        // Cursor as a tree-sitter Point: row is line index, column is
        // byte offset within that line.
        let cursor_pt = (
            cursor_row,
            char_to_byte_col(&self.source, cursor_row, cursor_col_chars),
        );

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), self.source.as_bytes());

        // Best candidate so far, tracked by byte-length so we pick the
        // innermost. (Multiple matches can contain the cursor when
        // text objects nest, e.g. inner function inside outer impl.)
        let mut best: Option<Candidate> = None;

        while let Some(m) = matches.next() {
            // 1. Direct captures with the target name.
            for cap in m.captures {
                let name = self
                    .textobject_capture_names
                    .get(cap.index as usize)
                    .map(String::as_str)
                    .unwrap_or("");
                if name != target {
                    continue;
                }
                let node = cap.node;
                consider(
                    &mut best,
                    node.start_byte()..node.end_byte(),
                    point(node.start_position()),
                    point(node.end_position()),
                    cursor_pt,
                );
            }

            // 2. Ranges synthesized via `#make-range!` predicates on
            //    this pattern.
            for pred in query.general_predicates(m.pattern_index) {
                if pred.operator.as_ref() != "make-range!" {
                    continue;
                }
                let (name, start_idx, end_idx) = match pred.args.as_ref() {
                    [
                        QueryPredicateArg::String(n),
                        QueryPredicateArg::Capture(s),
                        QueryPredicateArg::Capture(e),
                    ] => (n.as_ref(), *s, *e),
                    _ => continue,
                };
                if name != target {
                    continue;
                }
                // Span = (min start across all `_start` captures) ..
                //        (max end across all `_end` captures).
                let mut span_start: Option<tree_sitter::Node> = None;
                let mut span_end: Option<tree_sitter::Node> = None;
                for cap in m.captures {
                    if cap.index == start_idx {
                        span_start = match span_start {
                            None => Some(cap.node),
                            Some(prev) if cap.node.start_byte() < prev.start_byte() => {
                                Some(cap.node)
                            }
                            other => other,
                        };
                    }
                    if cap.index == end_idx {
                        span_end = match span_end {
                            None => Some(cap.node),
                            Some(prev) if cap.node.end_byte() > prev.end_byte() => Some(cap.node),
                            other => other,
                        };
                    }
                }
                if let (Some(s), Some(e)) = (span_start, span_end) {
                    consider(
                        &mut best,
                        s.start_byte()..e.end_byte(),
                        point(s.start_position()),
                        point(e.end_position()),
                        cursor_pt,
                    );
                }
            }
        }

        let c = best?;
        Some((
            c.start.0,
            byte_to_char_col(&self.source, c.start.0, c.start.1),
            c.end.0,
            byte_to_char_col(&self.source, c.end.0, c.end.1),
        ))
    }

    /// Find the bracket-pair mate of the character at `(row, col_chars)`.
    ///
    /// Returns `Some((row, char_col))` of the matching bracket when the
    /// cursor sits on one of `()[]{}` *as a syntactic token* — i.e.
    /// tree-sitter resolved it to a bracket node, not to a containing
    /// `string`/`comment`/etc. literal. That naturally excludes
    /// brackets inside string and comment text without any extra
    /// bookkeeping.
    ///
    /// Returns `None` when no tree is parsed yet, when the character is
    /// not a bracket token, or when the parent node has no matching
    /// counterpart (broken syntax, parse error recovery).
    pub fn matching_bracket(&self, row: usize, col_chars: usize) -> Option<(usize, usize)> {
        let tree = self.tree.as_ref()?;
        let line = self.source.lines().nth(row)?;
        let byte_col = char_to_byte_col(&self.source, row, col_chars);
        let ch_byte_len = line
            .get(byte_col..)
            .and_then(|s| s.chars().next())
            .map(char::len_utf8)?;
        let start = Point {
            row,
            column: byte_col,
        };
        let end = Point {
            row,
            column: byte_col + ch_byte_len,
        };
        let node = tree.root_node().descendant_for_point_range(start, end)?;
        let (target, want_last) = match node.kind() {
            "(" => (")", true),
            "[" => ("]", true),
            "{" => ("}", true),
            ")" => ("(", false),
            "]" => ("[", false),
            "}" => ("{", false),
            _ => return None,
        };
        let parent = node.parent()?;
        // Opener → matching close is the *last* matching-kind child of
        // the parent (in case of nested same-kind tokens within one
        // parent, which is unusual but cheap to guard against). Closer
        // → first matching-kind child (the opener).
        let mut found: Option<tree_sitter::Node> = None;
        let mut walk = parent.walk();
        for child in parent.children(&mut walk) {
            if child.kind() == target {
                found = Some(child);
                if !want_last {
                    break;
                }
            }
        }
        let m = found?;
        let pos = m.start_position();
        Some((
            pos.row,
            byte_to_char_col(&self.source, pos.row, pos.column),
        ))
    }
}

/// Candidate text-object range during the inner search. Keeps both
/// byte and Point info so we can compare sizes cheaply while still
/// returning row/col coordinates at the end.
struct Candidate {
    bytes: std::ops::Range<usize>,
    start: (usize, usize),
    end: (usize, usize),
}

fn point(p: tree_sitter::Point) -> (usize, usize) {
    (p.row, p.column)
}

/// Byte-level diff of `old` vs `new` packaged as a tree-sitter
/// [`InputEdit`]. Finds the longest shared prefix and suffix and treats
/// everything in between as the changed region. For a one-keystroke
/// insertion this collapses to a zero-or-one-byte edit at the cursor,
/// which is exactly what makes incremental reparse fast.
///
/// Operates on bytes, not chars. `InputEdit.position.column` is itself
/// a byte column in tree-sitter, so this is consistent — and the
/// parser only consults positions to map back to nodes, then re-parses
/// the affected region from the new source either way.
fn compute_input_edit(old: &str, new: &str) -> InputEdit {
    let old_bytes = old.as_bytes();
    let new_bytes = new.as_bytes();
    let common_prefix = old_bytes
        .iter()
        .zip(new_bytes.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = old_bytes
        .len()
        .min(new_bytes.len())
        .saturating_sub(common_prefix);
    let common_suffix = old_bytes
        .iter()
        .rev()
        .zip(new_bytes.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    let start_byte = common_prefix;
    let old_end_byte = old_bytes.len() - common_suffix;
    let new_end_byte = new_bytes.len() - common_suffix;
    InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position: byte_to_point(old_bytes, start_byte),
        old_end_position: byte_to_point(old_bytes, old_end_byte),
        new_end_position: byte_to_point(new_bytes, new_end_byte),
    }
}

/// Convert a byte offset within `bytes` to a `(row, byte-column)`
/// [`Point`]. Linear scan from the start — for typical edits the offset
/// is small (cursor area) so this stays cheap; for whole-file rewrites
/// the parse itself dominates anyway.
fn byte_to_point(bytes: &[u8], offset: usize) -> Point {
    let offset = offset.min(bytes.len());
    let mut row = 0usize;
    let mut line_start = 0usize;
    for (i, &b) in bytes[..offset].iter().enumerate() {
        if b == b'\n' {
            row += 1;
            line_start = i + 1;
        }
    }
    Point {
        row,
        column: offset - line_start,
    }
}

/// Replace `best` with `range` when it contains the cursor and is
/// strictly smaller than what's there. "Smaller" is by byte count, so
/// nested objects (e.g. inner function inside an outer impl) resolve
/// to the innermost one.
fn consider(
    best: &mut Option<Candidate>,
    bytes: std::ops::Range<usize>,
    start: (usize, usize),
    end: (usize, usize),
    cursor: (usize, usize),
) {
    if !(start <= cursor && cursor < end) {
        return;
    }
    let len = bytes.end - bytes.start;
    let take = match best {
        None => true,
        Some(c) => len < c.bytes.end - c.bytes.start,
    };
    if take {
        *best = Some(Candidate { bytes, start, end });
    }
}

/// Translate a byte column on `row` into a character column. Tree-sitter
/// reports byte columns; the UI wants char columns to match how the
/// rest of the editor indexes into lines.
fn byte_to_char_col(source: &str, row: usize, byte_col: usize) -> usize {
    let line = source.lines().nth(row).unwrap_or("");
    let take = byte_col.min(line.len());
    line[..take].chars().count()
}

/// Inverse of [`byte_to_char_col`]: given a character column, return
/// the byte column. Saturates at end-of-line.
fn char_to_byte_col(source: &str, row: usize, char_col: usize) -> usize {
    let line = source.lines().nth(row).unwrap_or("");
    line.char_indices()
        .nth(char_col)
        .map(|(b, _)| b)
        .unwrap_or(line.len())
}

/// One styled range delivered by the query engine. Coordinates are
/// inclusive on `start`, exclusive on `end`, in *characters* (not
/// bytes) — already converted by [`Highlighter`].
#[derive(Debug, Clone)]
pub struct Capture {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_to_char_handles_ascii() {
        let src = "let x = 1\nprintln!(\"hi\")";
        assert_eq!(byte_to_char_col(src, 0, 0), 0);
        assert_eq!(byte_to_char_col(src, 0, 4), 4);
        assert_eq!(byte_to_char_col(src, 1, 9), 9);
    }

    #[test]
    fn byte_to_char_handles_multibyte() {
        // "あ" is 3 bytes in UTF-8 → 1 char.
        let src = "あ x";
        // Byte col 3 = right after 'あ' = char col 1.
        assert_eq!(byte_to_char_col(src, 0, 3), 1);
        // Byte col 5 = after "あ x" = char col 3.
        assert_eq!(byte_to_char_col(src, 0, 5), 3);
    }

    #[test]
    fn input_edit_single_byte_insertion() {
        let edit = compute_input_edit("abc", "abXc");
        assert_eq!(edit.start_byte, 2);
        assert_eq!(edit.old_end_byte, 2);
        assert_eq!(edit.new_end_byte, 3);
        assert_eq!(edit.start_position, Point { row: 0, column: 2 });
        assert_eq!(edit.new_end_position, Point { row: 0, column: 3 });
    }

    #[test]
    fn input_edit_no_change_is_noop_range() {
        let edit = compute_input_edit("hello", "hello");
        assert_eq!(edit.start_byte, 5);
        assert_eq!(edit.old_end_byte, 5);
        assert_eq!(edit.new_end_byte, 5);
    }

    #[test]
    fn input_edit_multi_line_replacement() {
        let edit = compute_input_edit("fn a() {\n  1\n}\n", "fn a() {\n  42\n}\n");
        // Common prefix: "fn a() {\n  " (11 bytes)
        // Common suffix: "\n}\n" (3 bytes)
        assert_eq!(edit.start_byte, 11);
        assert_eq!(edit.old_end_byte, 15 - 3); // "1" → byte 11..12
        assert_eq!(edit.new_end_byte, 16 - 3); // "42" → byte 11..13
        assert_eq!(edit.start_position, Point { row: 1, column: 2 });
    }

    #[test]
    fn input_edit_full_replacement() {
        let edit = compute_input_edit("abc", "xyz");
        assert_eq!(edit.start_byte, 0);
        assert_eq!(edit.old_end_byte, 3);
        assert_eq!(edit.new_end_byte, 3);
    }
}
