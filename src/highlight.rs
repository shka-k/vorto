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
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator, Tree};

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
    /// grammar (cached) and compiles its highlights query.
    pub fn highlighter_for(&mut self, spec: &LangSpec) -> Result<Highlighter> {
        let lang = self.load_language(spec)?;
        let query_src = self.read_query(spec, "highlights")?;
        Highlighter::new(lang, &query_src)
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

/// Per-buffer state: parser, tree, query, and the source the tree was
/// built from. Refreshes the tree only when `refresh()` is called with
/// a version newer than the one already cached, so callers can poke at
/// it freely from a hot draw loop.
pub struct Highlighter {
    parser: Parser,
    query: Query,
    tree: Option<Tree>,
    source: String,
    parsed_version: Option<u64>,
    capture_names: Vec<String>,
}

impl Highlighter {
    fn new(language: Language, query_src: &str) -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .context("setting parser language (ABI mismatch?)")?;
        let query = Query::new(&language, query_src).context("compiling highlights query")?;
        let capture_names = query.capture_names().iter().map(|s| s.to_string()).collect();
        Ok(Self {
            parser,
            query,
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
}

/// Translate a byte column on `row` into a character column. Tree-sitter
/// reports byte columns; the UI wants char columns to match how the
/// rest of the editor indexes into lines.
fn byte_to_char_col(source: &str, row: usize, byte_col: usize) -> usize {
    let line = source.lines().nth(row).unwrap_or("");
    let take = byte_col.min(line.len());
    line[..take].chars().count()
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
