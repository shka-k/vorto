//! LSP wire-protocol types, normalised for the client's internal use.
//!
//! These are not 1:1 with the spec — fields the client doesn't act on are
//! dropped, and a few shape choices (`WorkspaceEdit` as a flat map,
//! `Hover` as a single joined string) bake in decisions the parsers make.

use std::collections::HashMap;

use serde_json::Value;

/// Event delivered from a reader thread back to the App. Keyed by the
/// document URI the event applies to (when relevant) so the App can
/// route to the right buffer without knowing which client sent it.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// Server replaced the diagnostics for a document. An empty `items`
    /// vector means "clear".
    Diagnostics { uri: String, items: Vec<Diagnostic> },
    /// `window/showMessage` — surface in the status bar.
    Message { level: u8, text: String },
    /// Response to an earlier request we sent. `id` matches what
    /// [`super::LspClient::request`] returned; the App keeps a `(lang, id) →
    /// kind` map so it knows how to interpret `result`. `lang` is
    /// stamped by the reader thread so the App can disambiguate
    /// responses arriving from multiple servers on the same channel.
    Response {
        lang: String,
        id: u64,
        /// `None` when the server returned an error or a null result.
        result: Option<Value>,
        /// Server error message, if any.
        error: Option<String>,
    },
    /// Reader hit a fatal error and is exiting.
    Error(String),
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Copy)]
pub struct Position {
    /// 0-based line.
    pub line: u32,
    /// 0-based UTF-16 character offset per spec. We treat it as a char
    /// index — close enough for ASCII source which is the common case.
    pub character: u32,
}

/// LSP `Location` — a span inside a single file. Used for definition /
/// references results.
#[derive(Debug, Clone)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// `textDocument/hover` response, normalised. `contents` is whatever
/// markdown/plaintext the server produced, already joined to a single
/// string. The optional source `range` the server returns is dropped —
/// we don't paint a highlight over the symbol while the popup is up.
#[derive(Debug, Clone)]
pub struct Hover {
    pub contents: String,
}

/// LSP `TextEdit` — replace `range` with `new_text`.
#[derive(Debug, Clone)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// A single completion candidate. Lossily reduced from the LSP shape
/// to the subset the popup actually needs.
///
/// `text_edit` wins over `insert_text` wins over `label` for insertion:
/// the spec lets servers send any one of them and clients are required
/// to fall back in that order. We don't currently support snippet
/// placeholders (`$1`, `${1:foo}`) — they're inserted literally.
#[derive(Debug, Clone)]
pub struct CompletionItem {
    /// What the user sees in the popup list.
    pub label: String,
    /// LSP `CompletionItemKind` (1-25). Used purely for the abbreviated
    /// badge ("Fn", "Var", "Mod", …) on each row.
    pub kind: u8,
    /// Set when the server supplies a precise edit — that range may
    /// extend further than our notion of "the prefix being typed"
    /// (e.g. completing inside a partial path replaces the whole path).
    pub text_edit: Option<TextEdit>,
    /// Plain replacement text. Used when `text_edit` is absent.
    pub insert_text: Option<String>,
    /// Used for filter ranking when the server overrides what the user's
    /// typed prefix should be matched against (rust-analyzer leans on
    /// this for `::` and method-chain completions). Falls back to
    /// `label` when absent.
    pub filter_text: Option<String>,
    /// Sort key. We honor it when present so rust-analyzer's "this
    /// crate first" ordering survives client-side filtering.
    pub sort_text: Option<String>,
    /// Free-form details — usually a short type signature ("fn(u32) -> u32")
    /// shown beside the label. May be empty.
    pub detail: Option<String>,
}

/// Simplified LSP `WorkspaceEdit` — a flat map from document URI to the
/// edits to apply there. We accept both `changes` and `documentChanges`
/// shapes server-side and normalise into this view.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceEdit {
    pub changes: HashMap<String, Vec<TextEdit>>,
}

/// LSP `CodeAction` (or `Command`) returned by `textDocument/codeAction`.
/// We keep the raw JSON alongside the parsed `title`/`edit` so the
/// caller can echo it back to `codeAction/resolve` for actions that
/// arrive without an embedded edit (rust-analyzer's "Extract …"
/// refactors are the typical case).
#[derive(Debug, Clone)]
pub struct CodeAction {
    pub title: String,
    pub edit: Option<WorkspaceEdit>,
    /// Raw JSON value as the server sent it. Required for
    /// `codeAction/resolve`, which the spec defines as round-tripping
    /// the whole CodeAction object back.
    pub raw: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    pub(super) fn from_code(c: i64) -> Severity {
        match c {
            1 => Severity::Error,
            2 => Severity::Warning,
            3 => Severity::Info,
            _ => Severity::Hint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_from_code_defaults_to_hint() {
        assert_eq!(Severity::from_code(1), Severity::Error);
        assert_eq!(Severity::from_code(4), Severity::Hint);
        assert_eq!(Severity::from_code(99), Severity::Hint);
    }
}
