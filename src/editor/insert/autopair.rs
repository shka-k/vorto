//! Auto-pair classification: which characters open / close a pair, and
//! whether typing an opener at the cursor should also drop in its closer.
//!
//! Pure helpers — no `Buffer` access. The buffer-side wrappers in
//! `super` (`insert_char_smart`, `insert_newline`, `delete_char_before_smart`)
//! drive these by passing the surrounding chars they read off the cursor row.

/// Maps an auto-pair opener to its closer. Quotes are self-paired
/// (closer == opener). Returns `None` for any non-opener.
pub(super) fn auto_pair_closer(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        _ => None,
    }
}

/// True when `c` is a closer that participates in skip-over (typing it
/// where the same char already sits just advances the cursor).
pub(super) fn is_auto_pair_closer(c: char) -> bool {
    matches!(c, ')' | ']' | '}' | '"' | '\'' | '`')
}

/// Decide whether typing `opener` should also insert its closer, given
/// the chars to either side of the cursor. Brackets only check the
/// right side (don't capture an existing identifier); quotes also gate
/// on the left side to dodge apostrophes inside words and the inner
/// edge of an existing quoted region.
pub(super) fn should_auto_pair(opener: char, prev: Option<char>, next: Option<char>) -> bool {
    if let Some(n) = next
        && (n.is_alphanumeric() || n == '_')
    {
        return false;
    }
    if matches!(opener, '"' | '\'' | '`')
        && let Some(p) = prev
        && (p.is_alphanumeric() || p == '_' || p == opener)
    {
        return false;
    }
    true
}

/// File extensions where typing `>` should close the open tag with a
/// matching `</tag>`. Tag autopair is gated by extension rather than by
/// tree-sitter context because the user-facing trigger fires *before*
/// the parser has seen the new `>` — we can't ask the tree what kind
/// of node we're inside.
pub(super) fn supports_tag_autopair(ext: Option<&str>) -> bool {
    matches!(
        ext,
        Some("tsx" | "jsx" | "html" | "htm" | "xml" | "vue" | "svelte" | "astro" | "md" | "mdx")
    )
}

/// True when JSX-style empty fragments (`<>` / `</>`) are valid in this
/// file. Plain HTML/XML have no fragment syntax — for those, a bare
/// `<>` shouldn't auto-close.
pub(super) fn supports_fragment(ext: Option<&str>) -> bool {
    matches!(ext, Some("tsx" | "jsx"))
}

/// HTML void elements that never get a closing tag. Lowercased; the
/// caller folds case before lookup.
fn is_html_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Outcome of scanning back from a freshly-typed `>` for an opening
/// tag that should get an auto-inserted closer. The empty-name variant
/// represents a JSX fragment (`<>` → `<></>`).
pub(super) enum TagAutopair {
    /// Insert `</{0}>`, with `{0}` empty for fragments.
    Close(String),
}

/// Scan `line` chars `0..cursor_col` to decide whether typing `>` at
/// `cursor_col` should auto-insert a closing tag. Returns the tag name
/// to close (empty for a JSX fragment), or `None` when no auto-close
/// applies.
///
/// Rules:
/// - There must be an unmatched `<` on this line before the cursor.
///   We walk back from the cursor, skipping over `"…"` / `'…'` runs,
///   and bail if we hit a `>` (a previous tag has already closed).
/// - `</…` (closing tag) and `<!…` / `<?…` (comments, doctypes,
///   processing instructions) are skipped.
/// - A `/` immediately before the cursor means self-closing — leave
///   it alone.
/// - HTML void elements (`<br>`, `<img>`, …) in `.html`/`.htm`/`.xml`
///   files get no closer.
/// - Fragments (`<>`) only auto-close in JSX/TSX.
pub(super) fn detect_open_tag(
    line: &str,
    cursor_col: usize,
    ext: Option<&str>,
) -> Option<TagAutopair> {
    if !supports_tag_autopair(ext) {
        return None;
    }
    if cursor_col == 0 {
        return None;
    }
    let chars: Vec<char> = line.chars().collect();
    if cursor_col > chars.len() {
        return None;
    }
    // Self-closing tag: `<Foo />` — bail before scanning back.
    if chars[cursor_col - 1] == '/' {
        return None;
    }

    let mut in_str: Option<char> = None;
    let mut i = cursor_col;
    let open_idx;
    loop {
        if i == 0 {
            return None;
        }
        i -= 1;
        let c = chars[i];
        if let Some(q) = in_str {
            if c == q {
                in_str = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => in_str = Some(c),
            '>' => return None,
            '<' => {
                open_idx = i;
                break;
            }
            _ => {}
        }
    }

    let after_open = open_idx + 1;
    // `</`, `<!`, `<?` — not an opener we should auto-close.
    if let Some(&next_c) = chars.get(after_open)
        && matches!(next_c, '/' | '!' | '?')
    {
        return None;
    }

    // Read the tag name. JSX/TSX allow dotted member names
    // (`<Foo.Bar>`); HTML/XML allow namespaced names (`<svg:rect>`).
    let mut j = after_open;
    let mut name = String::new();
    while j < cursor_col {
        let ch = chars[j];
        if name.is_empty() {
            if ch.is_alphabetic() || ch == '_' {
                name.push(ch);
                j += 1;
            } else {
                break;
            }
        } else if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':') {
            name.push(ch);
            j += 1;
        } else {
            break;
        }
    }

    if name.is_empty() {
        // `<` immediately followed by `>` — fragment, JSX-only.
        if after_open == cursor_col && supports_fragment(ext) {
            return Some(TagAutopair::Close(String::new()));
        }
        return None;
    }

    if matches!(ext, Some("html") | Some("htm") | Some("xml"))
        && is_html_void(&name.to_ascii_lowercase())
    {
        return None;
    }

    Some(TagAutopair::Close(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_jsx_simple_tag() {
        let line = "<div";
        match detect_open_tag(line, line.chars().count(), Some("tsx")) {
            Some(TagAutopair::Close(n)) => assert_eq!(n, "div"),
            _ => panic!("expected Close(div)"),
        }
    }

    #[test]
    fn detects_jsx_with_attributes() {
        let line = "<Button onClick={x} class=\"y\"";
        match detect_open_tag(line, line.chars().count(), Some("tsx")) {
            Some(TagAutopair::Close(n)) => assert_eq!(n, "Button"),
            _ => panic!("expected Close(Button)"),
        }
    }

    #[test]
    fn detects_dotted_member_name() {
        let line = "<Foo.Bar prop";
        match detect_open_tag(line, line.chars().count(), Some("tsx")) {
            Some(TagAutopair::Close(n)) => assert_eq!(n, "Foo.Bar"),
            _ => panic!("expected Close(Foo.Bar)"),
        }
    }

    #[test]
    fn fragment_in_tsx() {
        let line = "<";
        match detect_open_tag(line, line.chars().count(), Some("tsx")) {
            Some(TagAutopair::Close(n)) => assert!(n.is_empty()),
            _ => panic!("expected fragment"),
        }
    }

    #[test]
    fn fragment_not_in_html() {
        let line = "<";
        assert!(detect_open_tag(line, line.chars().count(), Some("html")).is_none());
    }

    #[test]
    fn self_closing_skipped() {
        let line = "<img src=\"a\" /";
        assert!(detect_open_tag(line, line.chars().count(), Some("tsx")).is_none());
    }

    #[test]
    fn closing_tag_skipped() {
        let line = "</div";
        assert!(detect_open_tag(line, line.chars().count(), Some("tsx")).is_none());
    }

    #[test]
    fn comment_open_skipped() {
        let line = "<!-- hi --";
        assert!(detect_open_tag(line, line.chars().count(), Some("html")).is_none());
    }

    #[test]
    fn already_closed_tag_skipped() {
        // `<div>` is already closed by the `>`; the second tag has no name yet.
        let line = "<div>foo";
        assert!(detect_open_tag(line, line.chars().count(), Some("tsx")).is_none());
    }

    #[test]
    fn quoted_lt_inside_attr_ignored() {
        // The `<` inside the attribute string is not a tag opener.
        let line = "<input value=\"a<b\"";
        match detect_open_tag(line, line.chars().count(), Some("html")) {
            Some(_) => panic!("`input` is a void element — should not auto-close"),
            None => {}
        }
    }

    #[test]
    fn html_void_element_skipped() {
        let line = "<br";
        assert!(detect_open_tag(line, line.chars().count(), Some("html")).is_none());
    }

    #[test]
    fn html_void_element_still_closed_in_jsx() {
        // In JSX `<br>` is illegal anyway but we follow the file's
        // ext rule rather than the spec — TSX/JSX get a closer.
        let line = "<br";
        match detect_open_tag(line, line.chars().count(), Some("tsx")) {
            Some(TagAutopair::Close(n)) => assert_eq!(n, "br"),
            _ => panic!("expected Close(br)"),
        }
    }

    #[test]
    fn unsupported_ext_returns_none() {
        let line = "<div";
        assert!(detect_open_tag(line, line.chars().count(), Some("rs")).is_none());
        assert!(detect_open_tag(line, line.chars().count(), None).is_none());
    }
}
