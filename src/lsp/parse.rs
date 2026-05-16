//! Pure parsers from `serde_json::Value` into the normalised types in
//! [`super::types`]. Everything here is total — invalid/missing input
//! collapses to `None` / empty rather than panicking.

use std::collections::HashMap;

use serde_json::Value;

use super::types::{
    CodeAction, CompletionItem, Hover, Location, Position, Range, TextEdit, WorkspaceEdit,
};

/// Parse a `Location` (LSP shape). Returns `None` on schema mismatch.
fn parse_location(v: &Value) -> Option<Location> {
    let uri = v.get("uri").and_then(|x| x.as_str())?.to_string();
    let range = parse_range(v.get("range")?)?;
    Some(Location { uri, range })
}

/// Parse a `LocationLink` and reduce it to the same shape as `Location`
/// (taking `targetUri` + `targetSelectionRange`).
fn parse_location_link(v: &Value) -> Option<Location> {
    let uri = v.get("targetUri").and_then(|x| x.as_str())?.to_string();
    let range = parse_range(
        v.get("targetSelectionRange")
            .or_else(|| v.get("targetRange"))?,
    )?;
    Some(Location { uri, range })
}

/// `textDocument/definition` may answer with a single `Location`, a
/// single `LocationLink`, an array of either, or `null`. Normalise to a
/// flat `Vec<Location>`.
pub fn parse_locations(v: &Value) -> Vec<Location> {
    if v.is_null() {
        return Vec::new();
    }
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|x| parse_location(x).or_else(|| parse_location_link(x)))
            .collect();
    }
    if let Some(loc) = parse_location(v).or_else(|| parse_location_link(v)) {
        return vec![loc];
    }
    Vec::new()
}

pub(super) fn parse_range(v: &Value) -> Option<Range> {
    let start = v.get("start")?;
    let end = v.get("end")?;
    Some(Range {
        start: Position {
            line: start.get("line")?.as_u64().unwrap_or(0) as u32,
            character: start.get("character")?.as_u64().unwrap_or(0) as u32,
        },
        end: Position {
            line: end.get("line")?.as_u64().unwrap_or(0) as u32,
            character: end.get("character")?.as_u64().unwrap_or(0) as u32,
        },
    })
}

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

/// Parse a `textDocument/hover` response. `contents` may arrive as
/// `MarkupContent { kind, value }`, a bare `MarkedString` (string or
/// `{ language, value }`), or an array of `MarkedString`s — collapse all
/// shapes into a single joined string. Returns `None` when `contents`
/// is missing/empty or when the whole response is `null` (servers send
/// `null` to mean "no hover info here").
pub fn parse_hover(v: &Value) -> Option<Hover> {
    if v.is_null() {
        return None;
    }
    let contents = v.get("contents")?;
    let joined = collect_hover_contents(contents);
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(Hover {
        contents: trimmed.to_string(),
    })
}

fn collect_hover_contents(v: &Value) -> String {
    // `MarkupContent` — the modern shape.
    if let Some(obj) = v.as_object()
        && let Some(value) = obj.get("value").and_then(|x| x.as_str())
        && obj.get("kind").is_some()
    {
        return value.to_string();
    }
    // Legacy `MarkedString` — either a plain string or
    // `{ language, value }` (a code block).
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(obj) = v.as_object()
        && let Some(value) = obj.get("value").and_then(|x| x.as_str())
    {
        let lang = obj.get("language").and_then(|x| x.as_str()).unwrap_or("");
        return format!("```{}\n{}\n```", lang, value);
    }
    // Array of `MarkedString`s — join with blank lines so distinct
    // entries (signature, doc, examples) stay visually separated.
    if let Some(arr) = v.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .map(collect_hover_contents)
            .filter(|s| !s.trim().is_empty())
            .collect();
        return parts.join("\n\n");
    }
    String::new()
}

/// Parse a `textDocument/completion` response. The result can be:
/// - `null` (no completions),
/// - `CompletionItem[]` (the simple case), or
/// - `{ isIncomplete, items: CompletionItem[] }`.
///
/// All three collapse to a flat `Vec<CompletionItem>`. We don't surface
/// `isIncomplete` — the popup doesn't re-request on every keystroke,
/// so the distinction doesn't pay rent.
pub fn parse_completion(v: &Value) -> Vec<CompletionItem> {
    if v.is_null() {
        return Vec::new();
    }
    let arr = if let Some(a) = v.as_array() {
        a.as_slice()
    } else if let Some(a) = v.get("items").and_then(|x| x.as_array()) {
        a.as_slice()
    } else {
        return Vec::new();
    };
    arr.iter().filter_map(parse_completion_item).collect()
}

fn parse_completion_item(v: &Value) -> Option<CompletionItem> {
    let label = v.get("label")?.as_str()?.to_string();
    let kind = v.get("kind").and_then(|x| x.as_u64()).unwrap_or(0) as u8;
    let text_edit = v
        .get("textEdit")
        .and_then(|te| {
            // Modern servers may send `InsertReplaceEdit { insert, replace, newText }`
            // instead of `TextEdit { range, newText }`. Prefer the replace
            // range — that's the one we'd want when the user accepts.
            let new_text = te.get("newText")?.as_str()?.to_string();
            let range = te
                .get("range")
                .or_else(|| te.get("replace"))
                .or_else(|| te.get("insert"))?;
            let range = parse_range(range)?;
            Some(TextEdit { range, new_text })
        });
    let insert_text = v
        .get("insertText")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let filter_text = v
        .get("filterText")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let sort_text = v
        .get("sortText")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    // `detail` is the canonical field. Some servers
    // (typescript-language-server in particular) defer it to
    // `completionItem/resolve` and instead populate `labelDetails`
    // on the initial response — `{ detail: "(...args): T", description:
    // "Foo.bar" }`. Fall back to that so the popup isn't blank for TS,
    // Vue's Volar, etc. We stitch the two `labelDetails` halves with a
    // space so the right-column shows everything the server has.
    let detail = v
        .get("detail")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            let ld = v.get("labelDetails")?;
            let suffix = ld.get("detail").and_then(|x| x.as_str()).unwrap_or("");
            let desc = ld.get("description").and_then(|x| x.as_str()).unwrap_or("");
            let combined = match (suffix.is_empty(), desc.is_empty()) {
                (true, true) => return None,
                (false, true) => suffix.to_string(),
                (true, false) => desc.to_string(),
                (false, false) => format!("{} {}", suffix, desc),
            };
            Some(combined)
        })
        // Final fallback: turn the LSP `kind` enum into a Go-style
        // short word ("func", "var", "type", …). TS / Pyright / others
        // routinely send completion items without `detail` or
        // `labelDetails` and only fill them on `completionItem/resolve`,
        // which we issue lazily — without this the right column would
        // be blank for those languages until the user accepts an item.
        .or_else(|| kind_word(kind).map(|s| s.to_string()));
    let additional_text_edits = v
        .get("additionalTextEdits")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(parse_text_edit).collect())
        .unwrap_or_default();
    Some(CompletionItem {
        label,
        kind,
        text_edit,
        insert_text,
        filter_text,
        sort_text,
        detail,
        additional_text_edits,
        raw: v.clone(),
        source: String::new(),
    })
}

/// Parse a `completionItem/resolve` response. The result is a single
/// CompletionItem object — same shape as one element of the array
/// returned by `textDocument/completion`. Returns `None` when the
/// server hands back something we can't make sense of (no `label`).
pub fn parse_completion_resolve(v: &Value) -> Option<CompletionItem> {
    parse_completion_item(v)
}

/// Short Go-style word for the LSP `CompletionItemKind` enum. Used as
/// the last-resort fallback for `CompletionItem.detail` when the server
/// sends neither `detail` nor `labelDetails`. Mirrors what gopls puts
/// in its `detail` so the right column reads similarly across servers.
fn kind_word(kind: u8) -> Option<&'static str> {
    match kind {
        1 => Some("text"),
        2 => Some("method"),
        3 => Some("func"),
        4 => Some("constructor"),
        5 => Some("field"),
        6 => Some("var"),
        7 => Some("class"),
        8 => Some("interface"),
        9 => Some("module"),
        10 => Some("property"),
        11 => Some("unit"),
        12 => Some("value"),
        13 => Some("enum"),
        14 => Some("keyword"),
        15 => Some("snippet"),
        16 => Some("color"),
        17 => Some("file"),
        18 => Some("reference"),
        19 => Some("folder"),
        20 => Some("enum member"),
        21 => Some("const"),
        22 => Some("struct"),
        23 => Some("event"),
        24 => Some("operator"),
        25 => Some("type param"),
        _ => None,
    }
}

fn parse_text_edit(v: &Value) -> Option<TextEdit> {
    let range = parse_range(v.get("range")?)?;
    let new_text = v.get("newText")?.as_str()?.to_string();
    Some(TextEdit { range, new_text })
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_locations_handles_all_shapes() {
        // Single Location object.
        let single = json!({
            "uri": "file:///a.rs",
            "range": { "start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5} }
        });
        let v = parse_locations(&single);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].uri, "file:///a.rs");

        // Array of Locations.
        let arr = json!([single]);
        assert_eq!(parse_locations(&arr).len(), 1);

        // LocationLink shape.
        let link = json!([{
            "targetUri": "file:///b.rs",
            "targetSelectionRange": {
                "start": {"line": 0, "character": 0},
                "end":   {"line": 0, "character": 3}
            }
        }]);
        let v = parse_locations(&link);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].uri, "file:///b.rs");

        // null → empty.
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn parse_workspace_edit_normalises_both_shapes() {
        // Legacy `changes` map.
        let v = json!({
            "changes": {
                "file:///a.rs": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end":   {"line": 0, "character": 3}
                    },
                    "newText": "X"
                }]
            }
        });
        let edit = parse_workspace_edit(&v).unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes["file:///a.rs"].len(), 1);

        // Modern `documentChanges` array.
        let v = json!({
            "documentChanges": [{
                "textDocument": { "uri": "file:///b.rs", "version": 1 },
                "edits": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end":   {"line": 0, "character": 3}
                    },
                    "newText": "Y"
                }]
            }]
        });
        let edit = parse_workspace_edit(&v).unwrap();
        assert_eq!(edit.changes["file:///b.rs"][0].new_text, "Y");

        assert!(parse_workspace_edit(&Value::Null).is_none());
    }

    #[test]
    fn parse_completion_handles_array_and_list_shapes() {
        // Bare CompletionItem[].
        let v = json!([
            { "label": "push", "kind": 2, "detail": "fn push(&mut self, x: T)" },
            { "label": "pop",  "kind": 2 }
        ]);
        let items = parse_completion(&v);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "push");
        assert_eq!(items[0].kind, 2);
        assert_eq!(items[0].detail.as_deref(), Some("fn push(&mut self, x: T)"));

        // CompletionList { isIncomplete, items }.
        let v = json!({
            "isIncomplete": true,
            "items": [{ "label": "len" }]
        });
        let items = parse_completion(&v);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "len");

        // null → empty.
        assert!(parse_completion(&Value::Null).is_empty());

        // textEdit honored, falling back to InsertReplaceEdit shape.
        let v = json!([{
            "label": "foo",
            "textEdit": {
                "newText": "foo()",
                "replace": {
                    "start": { "line": 1, "character": 2 },
                    "end":   { "line": 1, "character": 5 }
                },
                "insert": {
                    "start": { "line": 1, "character": 2 },
                    "end":   { "line": 1, "character": 4 }
                }
            }
        }]);
        let items = parse_completion(&v);
        let te = items[0].text_edit.as_ref().unwrap();
        assert_eq!(te.new_text, "foo()");
        // We pick `replace`, not `insert`.
        assert_eq!(te.range.end.character, 5);
    }

    #[test]
    fn parse_completion_preserves_raw_for_resolve_round_trip() {
        let v = json!([{
            "label": "HashMap",
            "data": { "opaque": "server-handle" }
        }]);
        let items = parse_completion(&v);
        assert_eq!(items[0].raw["data"]["opaque"], "server-handle");
    }

    #[test]
    fn parse_completion_resolve_pulls_out_additional_edits() {
        // The resolve response is a single CompletionItem object,
        // not an array — that's the shape distinction from the
        // initial completion request.
        let v = json!({
            "label": "HashMap",
            "additionalTextEdits": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end":   { "line": 0, "character": 0 }
                },
                "newText": "use std::collections::HashMap;\n"
            }]
        });
        let item = parse_completion_resolve(&v).unwrap();
        assert_eq!(item.additional_text_edits.len(), 1);
        assert_eq!(
            item.additional_text_edits[0].new_text,
            "use std::collections::HashMap;\n"
        );
    }

    #[test]
    fn parse_completion_picks_up_additional_text_edits() {
        // Auto-import shape: the primary insertion is the symbol name
        // and the `additionalTextEdits` carry the `use …;` line.
        let v = json!([{
            "label": "HashMap",
            "additionalTextEdits": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end":   { "line": 0, "character": 0 }
                },
                "newText": "use std::collections::HashMap;\n"
            }]
        }]);
        let items = parse_completion(&v);
        assert_eq!(items[0].additional_text_edits.len(), 1);
        assert_eq!(
            items[0].additional_text_edits[0].new_text,
            "use std::collections::HashMap;\n"
        );
    }

    #[test]
    fn parse_hover_handles_all_content_shapes() {
        // Modern MarkupContent.
        let v = json!({
            "contents": { "kind": "markdown", "value": "**fn** foo()" }
        });
        let h = parse_hover(&v).unwrap();
        assert_eq!(h.contents, "**fn** foo()");

        // Legacy bare MarkedString string.
        let v = json!({ "contents": "plain text" });
        assert_eq!(parse_hover(&v).unwrap().contents, "plain text");

        // Legacy MarkedString with language fence.
        let v = json!({
            "contents": { "language": "rust", "value": "fn foo()" }
        });
        let h = parse_hover(&v).unwrap();
        assert!(h.contents.contains("```rust"));
        assert!(h.contents.contains("fn foo()"));

        // Array of mixed entries — joined with blank lines.
        let v = json!({
            "contents": [
                { "language": "rust", "value": "fn foo()" },
                "docs go here"
            ]
        });
        let h = parse_hover(&v).unwrap();
        assert!(h.contents.contains("fn foo()"));
        assert!(h.contents.contains("docs go here"));
        assert!(h.contents.contains("\n\n"));

        // Empty / null → None.
        assert!(parse_hover(&Value::Null).is_none());
        assert!(parse_hover(&json!({ "contents": "" })).is_none());
        assert!(parse_hover(&json!({ "contents": [] })).is_none());
    }

    #[test]
    fn parse_code_actions_handles_array_and_unresolved() {
        let v = json!([
            {
                "title": "Quickfix: add semicolon",
                "edit": {
                    "changes": {
                        "file:///a.rs": [{
                            "range": {
                                "start": {"line": 0, "character": 5},
                                "end":   {"line": 0, "character": 5}
                            },
                            "newText": ";"
                        }]
                    }
                }
            },
            {
                "title": "Refactor: extract function",
                "data": "opaque-server-handle"
            }
        ]);
        let actions = parse_code_actions(&v);
        assert_eq!(actions.len(), 2);
        assert!(actions[0].edit.is_some());
        assert!(actions[1].edit.is_none());
        // The raw JSON is preserved for round-tripping through resolve.
        assert_eq!(actions[1].raw["data"], "opaque-server-handle");
    }
}
