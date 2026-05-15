//! Tree-sitter syntax highlighting.
//!
//! Two layers live in this module:
//!
//! * [`Loader`] owns the `.so`/`.dylib`/`.dll` grammar libraries and
//!   their resolved `tree_sitter::Language` handles. It also reads the
//!   `highlights.scm` query files from disk. Libraries are cached
//!   indefinitely — `Loader` is meant to live for the whole program so
//!   the loaded `Language` pointers stay valid.
//!
//! * [`Highlighter`] is the per-buffer object: a parser, the last
//!   parsed tree, the compiled query, and a snapshot of the source the
//!   tree was built from. Re-parses lazily when `refresh()` is called
//!   with a newer version than the one already cached.
//!
//! Failures (missing grammar file, broken `.so`, query compile error,
//! ABI mismatch) are returned as `anyhow::Error` so the caller can fall
//! back to plain text gracefully.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use libloading::{Library, Symbol};
use tree_sitter::{
    InputEdit, Language, Parser, Point, Query, QueryCursor, QueryPredicateArg, StreamingIterator,
    Tree,
};

use crate::config::Language as LangSpec;

// ────────────────────────────────────────────────────────────────────────
// Loader
// ────────────────────────────────────────────────────────────────────────

/// Owns `tree-sitter` grammar libraries and resolves them to
/// `tree_sitter::Language` handles on demand. Holds the [`Library`]
/// instances for the lifetime of the program so that the `Language`
/// pointer-into-library remains valid.
pub struct Loader {
    grammar_dir: PathBuf,
    query_dir: PathBuf,
    libs: HashMap<String, Library>,
    languages: HashMap<String, Language>,
}

impl Loader {
    pub fn new(grammar_dir: PathBuf, query_dir: PathBuf) -> Self {
        Self {
            grammar_dir,
            query_dir,
            libs: HashMap::new(),
            languages: HashMap::new(),
        }
    }

    /// Try to build a fresh [`Highlighter`] for `spec`. Loads the
    /// grammar (cached), compiles its highlights query, and — if a
    /// `textobjects.scm` / `indents.scm` is also present — their
    /// queries too. Both are optional; missing files are not an error.
    pub fn highlighter_for(&mut self, spec: &LangSpec) -> Result<Highlighter> {
        let lang = self.load_language(spec)?;
        let highlights_src = self.read_query(spec, "highlights")?;
        let textobjects_src = self.read_query(spec, "textobjects").ok();
        let indents_src = self.read_query(spec, "indents").ok();
        Highlighter::new(
            lang,
            &highlights_src,
            textobjects_src.as_deref(),
            indents_src.as_deref(),
        )
    }

    /// Resolve `spec.grammar` to a `tree_sitter::Language`, loading the
    /// underlying shared library on first request.
    fn load_language(&mut self, spec: &LangSpec) -> Result<Language> {
        if let Some(lang) = self.languages.get(&spec.grammar) {
            return Ok(lang.clone());
        }
        let dir = spec.grammar_dir.as_ref().unwrap_or(&self.grammar_dir);
        let path = library_path(dir, &spec.grammar)
            .with_context(|| format!("locating grammar `{}`", spec.grammar))?;
        let lib = unsafe { Library::new(&path) }
            .with_context(|| format!("loading grammar library {}", path.display()))?;
        let symbol_name = format!("tree_sitter_{}", spec.grammar.replace('-', "_"));
        let language = unsafe {
            let sym: Symbol<unsafe extern "C" fn() -> Language> =
                lib.get(symbol_name.as_bytes()).with_context(|| {
                    format!("symbol `{}` missing in {}", symbol_name, path.display())
                })?;
            sym()
        };
        // Library must outlive `language` (its pointer points into the
        // library). Stash it before we return.
        self.libs.insert(spec.grammar.clone(), lib);
        self.languages
            .insert(spec.grammar.clone(), language.clone());
        Ok(language)
    }

    /// Read `<query_dir>/<name>/<kind>.scm`, honoring a per-language
    /// override on `spec.query_dir` for the directory. Resolves
    /// `; inherits: <lang>[,<lang>…]` headers by recursively loading and
    /// prepending the inherited languages' same-kind files (helix's
    /// convention), so e.g. a 35-line `typescript/highlights.scm` that
    /// inherits `javascript` ends up with the full ecma rule set in
    /// front of the TS-specific delta.
    fn read_query(&self, spec: &LangSpec, kind: &str) -> Result<String> {
        let base = spec.query_dir.as_ref().unwrap_or(&self.query_dir);
        let mut visited = HashSet::new();
        read_query_recursive(base, &spec.name, kind, &mut visited)
    }
}

/// Read `<base>/<lang>/<kind>.scm` and, if its first comment lines
/// contain `; inherits: a,b,…`, prepend each inherited language's
/// same-kind file. Inherited content goes first so the requesting
/// language's patterns appear *later* in the combined source — the
/// query engine and the UI's overlay both treat later patterns as
/// higher-priority, which lets the more-specific language win on
/// conflicts.
///
/// `visited` short-circuits cycles. A missing inherited file is logged
/// and skipped rather than failing the whole load — losing one parent's
/// highlights is better than losing all of them.
fn read_query_recursive(
    base: &Path,
    lang_name: &str,
    kind: &str,
    visited: &mut HashSet<String>,
) -> Result<String> {
    if !visited.insert(lang_name.to_string()) {
        return Ok(String::new());
    }
    let path = base.join(lang_name).join(format!("{}.scm", kind));
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading query {}", path.display()))?;

    let inherits = parse_inherits(&content);
    if inherits.is_empty() {
        return Ok(content);
    }

    let mut combined = String::new();
    for inh in inherits {
        match read_query_recursive(base, &inh, kind, visited) {
            Ok(s) if !s.is_empty() => {
                combined.push_str(&s);
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "inherited query `{}/{}.scm` (from `{}`) skipped: {:#}",
                    inh, kind, lang_name, e
                );
            }
        }
    }
    combined.push_str(&content);
    Ok(combined)
}

/// Scan leading comment lines for `; inherits: a,b,c` (single or
/// double semicolons, any whitespace). Returns the parsed language
/// names in declaration order. Stops at the first non-comment / non-
/// blank line — the header has to live at the top of the file.
fn parse_inherits(content: &str) -> Vec<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with(';') {
            return Vec::new();
        }
        let stripped = trimmed.trim_start_matches(';').trim();
        if let Some(rest) = stripped.strip_prefix("inherits:") {
            return rest
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    Vec::new()
}

/// Try each platform-appropriate extension under `dir/<name>.*` and
/// return the first hit. Without this, we'd force users to spell out
/// the extension on every platform.
fn library_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let candidates = [
        format!("{}.so", name),
        format!("{}.dylib", name),
        format!("{}.dll", name),
        format!("lib{}.so", name),
        format!("lib{}.dylib", name),
    ];
    for c in &candidates {
        let p = dir.join(c);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "no grammar library for `{}` in {}",
        name,
        dir.display()
    ))
}

// ────────────────────────────────────────────────────────────────────────
// Highlighter
// ────────────────────────────────────────────────────────────────────────

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
}

impl Highlighter {
    fn new(
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
        let (indents, indent_capture_names) = match indents_src {
            Some(src) => match Query::new(&language, src) {
                Ok(q) => {
                    let names = q.capture_names().iter().map(|s| s.to_string()).collect();
                    (Some(q), names)
                }
                Err(e) => {
                    eprintln!("indents.scm compile failed, auto-indent disabled: {e}");
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
    /// whose node *opens* on `row` (i.e. the node's start row equals
    /// `row` and the node spans more than that single row). Used by
    /// the auto-indent path to decide whether a new line inserted
    /// after `row` should pick up one extra indent level beyond the
    /// row's existing leading whitespace.
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
                if start_row == row && end_row > row {
                    return true;
                }
            }
        }
        false
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
    fn library_path_reports_missing() {
        let dir = std::env::temp_dir();
        let result = library_path(&dir, "definitely-not-a-real-grammar");
        assert!(result.is_err());
    }

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
    fn parse_inherits_picks_up_single_lang() {
        let src = "; inherits: javascript\n\n(identifier) @variable\n";
        assert_eq!(parse_inherits(src), vec!["javascript".to_string()]);
    }

    #[test]
    fn parse_inherits_handles_double_semicolon_and_multiple_langs() {
        let src = ";; inherits: ecma, jsx\n";
        assert_eq!(
            parse_inherits(src),
            vec!["ecma".to_string(), "jsx".to_string()]
        );
    }

    #[test]
    fn parse_inherits_skips_blank_leading_lines() {
        let src = "\n\n; inherits: rust\n";
        assert_eq!(parse_inherits(src), vec!["rust".to_string()]);
    }

    #[test]
    fn parse_inherits_returns_empty_when_no_header() {
        let src = "; Types\n(identifier) @variable\n";
        assert!(parse_inherits(src).is_empty());
    }

    #[test]
    fn parse_inherits_stops_at_first_non_comment() {
        // A code line before the header means there's no header.
        let src = "(identifier) @variable\n; inherits: foo\n";
        assert!(parse_inherits(src).is_empty());
    }

    #[test]
    fn read_query_recursive_prepends_inherited() {
        let dir = std::env::temp_dir().join("vorto_inherits_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("parent")).unwrap();
        std::fs::create_dir_all(dir.join("child")).unwrap();
        std::fs::write(dir.join("parent/highlights.scm"), "PARENT\n").unwrap();
        std::fs::write(
            dir.join("child/highlights.scm"),
            "; inherits: parent\nCHILD\n",
        )
        .unwrap();

        let mut visited = HashSet::new();
        let out = read_query_recursive(&dir, "child", "highlights", &mut visited).unwrap();
        assert!(out.contains("PARENT"));
        assert!(out.contains("CHILD"));
        // Parent appears before child so child patterns win.
        assert!(out.find("PARENT").unwrap() < out.find("CHILD").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_query_recursive_breaks_cycles() {
        let dir = std::env::temp_dir().join("vorto_inherits_cycle");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("a/highlights.scm"), "; inherits: b\nA\n").unwrap();
        std::fs::write(dir.join("b/highlights.scm"), "; inherits: a\nB\n").unwrap();

        let mut visited = HashSet::new();
        let out = read_query_recursive(&dir, "a", "highlights", &mut visited).unwrap();
        // Both files load once, the second visit short-circuits.
        assert!(out.contains("A"));
        assert!(out.contains("B"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn input_edit_full_replacement() {
        let edit = compute_input_edit("abc", "xyz");
        assert_eq!(edit.start_byte, 0);
        assert_eq!(edit.old_end_byte, 3);
        assert_eq!(edit.new_end_byte, 3);
    }
}
