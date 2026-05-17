//! Capture-name → terminal color mapping.
//!
//! Tree-sitter highlight queries tag nodes with dotted names like
//! `function.method` or `keyword.return`. The theme resolves each one
//! by trying progressively shorter prefixes — so `function.method`
//! falls back to `function` when no exact entry exists. Match order:
//! longest prefix wins.

use ratatui::style::{Color, Modifier, Style};

/// Resolve a tree-sitter capture name (e.g. `function.method`) into a
/// ratatui style. Unknown captures return the default (uncolored)
/// style so the UI degrades to plain text.
pub fn style_for(capture: &str) -> Style {
    let mut candidate = capture;
    loop {
        if let Some(style) = lookup(candidate) {
            return style;
        }
        match candidate.rfind('.') {
            Some(i) => candidate = &candidate[..i],
            None => return Style::default(),
        }
    }
}

fn lookup(name: &str) -> Option<Style> {
    let s = match name {
        "keyword" => Style::default().fg(Color::Magenta),
        "string" => Style::default().fg(Color::Green),
        "string.escape" => Style::default().fg(Color::LightGreen),
        "character" => Style::default().fg(Color::Green),
        "number" => Style::default().fg(Color::LightRed),
        "boolean" => Style::default().fg(Color::LightRed),
        "constant" => Style::default().fg(Color::LightRed),
        "constant.builtin" => Style::default()
            .fg(Color::LightRed)
            .add_modifier(Modifier::BOLD),
        "comment" => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        "function" => Style::default().fg(Color::LightBlue),
        "function.macro" => Style::default().fg(Color::LightMagenta),
        "function.builtin" => Style::default().fg(Color::LightBlue),
        "method" => Style::default().fg(Color::LightBlue),
        "type" => Style::default().fg(Color::Yellow),
        "type.builtin" => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        "variable" => Style::default(),
        "variable.parameter" => Style::default().fg(Color::White),
        "variable.builtin" => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        "property" => Style::default().fg(Color::White),
        "field" => Style::default().fg(Color::White),
        "label" => Style::default().fg(Color::Yellow),
        "operator" => Style::default().fg(Color::White),
        "punctuation.bracket" => Style::default().fg(Color::Gray),
        "attribute" => Style::default().fg(Color::LightMagenta),
        "tag" => Style::default().fg(Color::LightBlue),
        _ => return None,
    };
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        let s = style_for("keyword");
        assert_eq!(s.fg, Some(Color::Magenta));
    }

    #[test]
    fn dotted_falls_back_to_prefix() {
        // `function.method` isn't in the table; should fall back to
        // `function`.
        let s = style_for("function.method");
        assert_eq!(s.fg, Some(Color::LightBlue));
    }

    #[test]
    fn unknown_capture_is_default() {
        let s = style_for("completely-unknown-capture");
        assert_eq!(s.fg, None);
    }

    #[test]
    fn deeply_nested_falls_back() {
        // `keyword.foo.bar.baz` → `keyword`.
        let s = style_for("keyword.foo.bar.baz");
        assert_eq!(s.fg, Some(Color::Magenta));
    }
}
