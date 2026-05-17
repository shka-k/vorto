//! Hover, completion, and signature-help: the response shapes that
//! drive popup-style UI. Each entry point collapses the LSP spec's
//! "you may receive this, or this, or null" variants into a single
//! normalised type.

use serde_json::Value;

use super::{parse_range, parse_text_edit};
use crate::lsp::types::{
    CompletionItem, Hover, ParameterInformation, ParameterLabel, SignatureHelp,
    SignatureInformation, TextEdit,
};

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
    let text_edit = v.get("textEdit").and_then(|te| {
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
        resolved_detail: None,
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

/// Parse a `textDocument/signatureHelp` response. Returns `None` for
/// `null`, an explicitly empty `signatures` array, or any shape we can't
/// make sense of — all three collapse to "no popup". `active_signature`
/// is clamped into the valid range so consumers can index without
/// bounds-checking.
pub fn parse_signature_help(v: &Value) -> Option<SignatureHelp> {
    if v.is_null() {
        return None;
    }
    let sigs_raw = v.get("signatures").and_then(|x| x.as_array())?;
    let signatures: Vec<SignatureInformation> =
        sigs_raw.iter().filter_map(parse_signature_info).collect();
    if signatures.is_empty() {
        return None;
    }
    let active_signature = v
        .get("activeSignature")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as usize;
    let active_signature = active_signature.min(signatures.len() - 1);
    // `activeParameter` is `uinteger | null` per spec. A literal `null`
    // means "no current parameter" — explicitly distinct from "missing,
    // fall back to first". `serde_json::Value::Null.as_u64()` returns
    // `None`, so the absence-vs-null distinction here is whether the
    // field is present at all.
    let active_parameter = match v.get("activeParameter") {
        Some(Value::Null) => None,
        Some(x) => x.as_u64().map(|n| n as usize),
        None => Some(0),
    };
    Some(SignatureHelp {
        signatures,
        active_signature,
        active_parameter,
    })
}

fn parse_signature_info(v: &Value) -> Option<SignatureInformation> {
    let label = v.get("label")?.as_str()?.to_string();
    let parameters = v
        .get("parameters")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(parse_parameter_info).collect())
        .unwrap_or_default();
    let active_parameter = match v.get("activeParameter") {
        Some(Value::Null) => None,
        Some(x) => x.as_u64().map(|n| n as usize),
        None => None,
    };
    Some(SignatureInformation {
        label,
        parameters,
        active_parameter,
    })
}

fn parse_parameter_info(v: &Value) -> Option<ParameterInformation> {
    let label_v = v.get("label")?;
    let label = if let Some(s) = label_v.as_str() {
        ParameterLabel::Text(s.to_string())
    } else if let Some(arr) = label_v.as_array() {
        let start = arr.first()?.as_u64()? as u32;
        let end = arr.get(1)?.as_u64()? as u32;
        ParameterLabel::Offsets(start, end)
    } else {
        return None;
    };
    Some(ParameterInformation { label })
}
