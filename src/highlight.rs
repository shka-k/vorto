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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use libloading::{Library, Symbol};
use tree_sitter::{
    Language, Parser, Query, QueryCursor, QueryPredicateArg, StreamingIterator, Tree,
};

use crate::languages::Language as LangSpec;

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
    /// `textobjects.scm` is also present — its text-object query. The
    /// text-object query is optional; missing files are not an error.
    pub fn highlighter_for(&mut self, spec: &LangSpec) -> Result<Highlighter> {
        let lang = self.load_language(spec)?;
        let highlights_src = self.read_query(spec, "highlights")?;
        let textobjects_src = self.read_query(spec, "textobjects").ok();
        Highlighter::new(lang, &highlights_src, textobjects_src.as_deref())
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
    /// override on `spec.query_dir` for the directory.
    fn read_query(&self, spec: &LangSpec, kind: &str) -> Result<String> {
        let base = spec.query_dir.as_ref().unwrap_or(&self.query_dir);
        let path = base.join(&spec.name).join(format!("{}.scm", kind));
        std::fs::read_to_string(&path)
            .with_context(|| format!("reading query {}", path.display()))
    }
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
    ) -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .context("setting parser language (ABI mismatch?)")?;
        let query =
            Query::new(&language, highlights_src).context("compiling highlights query")?;
        let capture_names = query.capture_names().iter().map(|s| s.to_string()).collect();
        let (textobjects, textobject_capture_names) = match textobjects_src {
            Some(src) => {
                let q = Query::new(&language, src).context("compiling textobjects query")?;
                let names = q.capture_names().iter().map(|s| s.to_string()).collect();
                (Some(q), names)
            }
            None => (None, Vec::new()),
        };
        Ok(Self {
            parser,
            query,
            textobjects,
            textobject_capture_names,
            tree: None,
            source: String::new(),
            parsed_version: None,
            capture_names,
        })
    }

    /// Re-parse `source` if it's newer than the cached tree. Cheap when
    /// the version hasn't changed.
    pub fn refresh(&mut self, source: &str, version: u64) {
        if self.parsed_version == Some(version) {
            return;
        }
        self.tree = self.parser.parse(source, None);
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
        let cursor_pt = (cursor_row, char_to_byte_col(&self.source, cursor_row, cursor_col_chars));

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
                    [QueryPredicateArg::String(n), QueryPredicateArg::Capture(s), QueryPredicateArg::Capture(e)] => {
                        (n.as_ref(), *s, *e)
                    }
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
}
