//! `WorkspaceEdit` / `TextEdit` parsing and the `CodeAction` shapes
//! that wrap them. Pure JSON → typed conversion; document
//! `create`/`rename`/`delete` operations are dropped on the floor
//! (rename refactors that need them are out of scope).

use std::collections::HashMap;

use serde_json::Value;

use super::parse_text_edit;
use crate::lsp::types::{CodeAction, TextEdit, WorkspaceEdit};

/// Parse a `WorkspaceEdit`. Both legacy `changes: { uri: [TextEdit] }`
/// and modern `documentChanges: [TextDocumentEdit]` shapes are
/// flattened into the same map. We ignore document `create`/`rename`/
/// `delete` operations — rename refactors that need them are out of
/// scope for now.
pub fn parse_workspace_edit(v: &Value) -> Option<WorkspaceEdit> {
    if v.is_null() {
        return None;
    }
    let mut out: HashMap<String, Vec<TextEdit>> = HashMap::new();
    if let Some(changes) = v.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in changes {
            if let Some(arr) = edits.as_array() {
                let edits = arr.iter().filter_map(parse_text_edit).collect();
                out.insert(uri.clone(), edits);
            }
        }
    }
    if let Some(doc_changes) = v.get("documentChanges").and_then(|c| c.as_array()) {
        for dc in doc_changes {
            // A `TextDocumentEdit` has `textDocument.uri` + `edits[]`. Other
            // operations (`CreateFile`, `RenameFile`, `DeleteFile`) have
            // their own `kind` field — skip those.
            if dc.get("kind").is_some() {
                continue;
            }
            let Some(uri) = dc
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let Some(edits) = dc.get("edits").and_then(|e| e.as_array()) else {
                continue;
            };
            let edits: Vec<TextEdit> = edits.iter().filter_map(parse_text_edit).collect();
            out.entry(uri.to_string()).or_default().extend(edits);
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(WorkspaceEdit { changes: out })
}

/// Parse the result of `textDocument/codeAction`. The server returns
/// `null`, a single object (rare), or an array mixing `CodeAction`
/// objects and legacy `Command` objects. We treat everything with a
/// `title` as a candidate; `Command`-only entries (no `edit` and no
/// `data` for resolve) still appear in the picker but do nothing on
/// submit because we don't run `workspace/executeCommand` yet.
pub fn parse_code_actions(v: &Value) -> Vec<CodeAction> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<CodeAction>, item: &Value| {
        let Some(title) = item.get("title").and_then(|t| t.as_str()) else {
            return;
        };
        let edit = item.get("edit").and_then(parse_workspace_edit);
        out.push(CodeAction {
            title: title.to_string(),
            edit,
            raw: item.clone(),
            // Filled in by the coordinator (which knows the originating
            // client). Parse stage is source-agnostic.
            source: String::new(),
        });
    };
    if let Some(arr) = v.as_array() {
        for item in arr {
            push(&mut out, item);
        }
    } else if v.is_object() {
        push(&mut out, v);
    }
    out
}

/// Parse the result of `codeAction/resolve` — same shape as a single
/// `CodeAction` from the list response, just with the previously-missing
/// `edit` filled in (in the typical case).
pub fn parse_code_action(v: &Value) -> Option<CodeAction> {
    let title = v.get("title").and_then(|t| t.as_str())?.to_string();
    let edit = v.get("edit").and_then(parse_workspace_edit);
    Some(CodeAction {
        title,
        edit,
        raw: v.clone(),
        source: String::new(),
    })
}

/// Parse a `textDocument/formatting` (or `rangeFormatting`) response.
/// The result is `TextEdit[]` or `null`; both collapse to a flat vector.
/// Empty when the server returned nothing or every entry was malformed.
pub fn parse_text_edits(v: &Value) -> Vec<TextEdit> {
    if v.is_null() {
        return Vec::new();
    }
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter().filter_map(parse_text_edit).collect()
}
