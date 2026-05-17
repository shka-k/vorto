//! Pure parsers from `serde_json::Value` into the normalised types in
//! [`super::types`]. Everything here is total — invalid/missing input
//! collapses to `None` / empty rather than panicking.
//!
//! Split by response family so each file stays at one job:
//!
//! - [`navigation`] — definition / declaration / references location
//!   shapes.
//! - [`edit`] — `WorkspaceEdit` / `TextEdit` / `CodeAction` family.
//! - [`completion`] — hover, completion, signature help (popup shapes).
//!
//! The two shared primitives — `parse_range` and `parse_text_edit` —
//! live here because every family pulls them in.

mod completion;
mod edit;
mod navigation;

pub use completion::{
    parse_completion, parse_completion_resolve, parse_hover, parse_signature_help,
};
pub use edit::{parse_code_action, parse_code_actions, parse_text_edits, parse_workspace_edit};
pub use navigation::parse_locations;

use serde_json::Value;

use super::types::{Position, Range, TextEdit};

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

pub(super) fn parse_text_edit(v: &Value) -> Option<TextEdit> {
    let range = parse_range(v.get("range")?)?;
    let new_text = v.get("newText")?.as_str()?.to_string();
    Some(TextEdit { range, new_text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::types::ParameterLabel;
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
    fn parse_signature_help_handles_offsets_and_text_labels() {
        // Modern shape: parameters identified by [start, end] offsets
        // into the signature label.
        let v = json!({
            "signatures": [{
                "label": "fn push(&mut self, x: T)",
                "parameters": [
                    { "label": [8, 17] },   // &mut self
                    { "label": [19, 23] }   // x: T
                ]
            }],
            "activeSignature": 0,
            "activeParameter": 1
        });
        let h = parse_signature_help(&v).unwrap();
        assert_eq!(h.signatures.len(), 1);
        assert_eq!(h.active_signature, 0);
        assert_eq!(h.active_parameter, Some(1));
        assert_eq!(h.signatures[0].parameters.len(), 2);
        match &h.signatures[0].parameters[1].label {
            ParameterLabel::Offsets(s, e) => {
                assert_eq!((*s, *e), (19, 23));
            }
            _ => panic!("expected Offsets"),
        }

        // Legacy shape: parameter label is a substring.
        let v = json!({
            "signatures": [{
                "label": "foo(x, y)",
                "parameters": [{ "label": "x" }, { "label": "y" }]
            }]
        });
        let h = parse_signature_help(&v).unwrap();
        // Missing `activeParameter` defaults to first.
        assert_eq!(h.active_parameter, Some(0));
        match &h.signatures[0].parameters[0].label {
            ParameterLabel::Text(s) => assert_eq!(s, "x"),
            _ => panic!("expected Text"),
        }

        // Explicit null activeParameter — no highlight.
        let v = json!({
            "signatures": [{ "label": "noop()" }],
            "activeParameter": null
        });
        let h = parse_signature_help(&v).unwrap();
        assert_eq!(h.active_parameter, None);

        // Out-of-range activeSignature clamps to last valid index.
        let v = json!({
            "signatures": [{ "label": "a()" }, { "label": "b()" }],
            "activeSignature": 99
        });
        let h = parse_signature_help(&v).unwrap();
        assert_eq!(h.active_signature, 1);

        // Null / empty signatures collapse to None.
        assert!(parse_signature_help(&Value::Null).is_none());
        assert!(parse_signature_help(&json!({ "signatures": [] })).is_none());
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
