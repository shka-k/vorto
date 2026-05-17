//! Pure indent-string arithmetic: extend, trim, and compute the indent
//! string for a freshly inserted line. Callers in `super` apply the
//! results to buffer rows; this module never touches `Buffer` state.

use crate::editor::IndentSettings;
use crate::syntax::Highlighter;

/// Remove one indent level from the end of `indent` (which must be
/// pure leading whitespace). Tab-terminated runs drop one `\t`;
/// space-terminated runs round *down* to the nearest multiple of
/// `settings.width` strictly below the current count — same rules
/// `Buffer::dedent_current_line` applies to a buffer row.
pub(super) fn strip_one_indent_level(indent: &str, settings: IndentSettings) -> String {
    if indent.is_empty() {
        return String::new();
    }
    if indent.ends_with('\t') {
        let mut out = indent.to_string();
        out.pop();
        return out;
    }
    let trailing_spaces = indent.chars().rev().take_while(|c| *c == ' ').count();
    if trailing_spaces == 0 {
        return indent.to_string();
    }
    let w = settings.width.max(1);
    let target = (trailing_spaces.saturating_sub(1) / w) * w;
    let remove = trailing_spaces - target;
    indent[..indent.len() - remove].to_string()
}

/// Leading-whitespace prefix of `line`, copied verbatim so the new
/// line preserves whatever tabs-vs-spaces mix the reference uses.
pub(super) fn copy_leading_indent(line: &str, _settings: IndentSettings) -> String {
    line.chars()
        .take_while(|c| c.is_whitespace() && *c != '\n')
        .collect()
}

/// Build the indent string for a brand-new line that sits *after*
/// `ref_row` in the buffer (vim's `o` / `O`). Strategy:
///
/// 1. Copy `reference_line`'s existing leading whitespace — the basic
///    vim `autoindent` behaviour, used when nothing else fires.
/// 2. Add one extra indent level when either signal fires:
///    - tree-sitter `indents.scm` reports an `@indent.begin` node
///      opening on `ref_row` (and spanning past it), or
///    - the reference line's last non-whitespace char is `{` / `(`
///      / `[`. Universal fallback for languages without indents.scm.
///
/// `Buffer::insert_newline` deliberately uses a narrower rule (see inline)
/// — pressing Enter on `func main() {` shouldn't auto-indent, but
/// `o` on the same line should land in the body.
pub(super) fn compute_new_line_indent(
    reference_line: &str,
    ref_row: usize,
    highlighter: &Option<Highlighter>,
    settings: IndentSettings,
) -> String {
    let base = copy_leading_indent(reference_line, settings);
    let ts_begin = highlighter
        .as_ref()
        .is_some_and(|h| h.indent_begins_at(ref_row));
    let trailing_opener = reference_line
        .trim_end()
        .chars()
        .last()
        .is_some_and(|c| matches!(c, '{' | '(' | '['));
    if ts_begin || trailing_opener {
        add_one_indent_level(&base, settings)
    } else {
        base
    }
}

/// Append one indent level to `base`. Tab-indented bases get an extra
/// `\t`; space-indented (or empty) bases get `settings.width` spaces,
/// honoring `settings.use_tabs` only when there's nothing to mimic.
pub(super) fn add_one_indent_level(base: &str, settings: IndentSettings) -> String {
    let use_tabs = if base.is_empty() {
        settings.use_tabs
    } else {
        base.contains('\t')
    };
    let mut out = base.to_string();
    if use_tabs {
        out.push('\t');
    } else {
        for _ in 0..settings.width.max(1) {
            out.push(' ');
        }
    }
    out
}
