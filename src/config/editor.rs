//! Editor-wide settings — currently just indent geometry.
//!
//! The same field set surfaces in two places in the TOML schema:
//!
//! * `[editor]` at the top level — the global default.
//! * Flattened into each `[languages.<name>]` table — per-language
//!   overrides. Any field unset there falls through to the global
//!   default; any field set wins. Field-level merge.

use serde::Deserialize;

const DEFAULT_INDENT_WIDTH: usize = 2;
const DEFAULT_TAB_WIDTH: usize = 4;

/// Raw, optional fields as parsed from TOML. Used both for the global
/// `[editor]` table and (via `#[serde(flatten)]`) inside each
/// `[languages.<name>]` table for per-language overrides.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct EditorToml {
    /// Width of one indent level — number of columns the editor uses
    /// when it indents a line. Falls back to `2` when unset.
    pub indent_width: Option<usize>,
    /// Visual width of a literal `\t` character. Falls back to `4` when
    /// unset; Go-style codebases typically want `8`.
    pub tab_width: Option<usize>,
    /// When `true`, auto-inserted indents (newline carry, opener-bracket
    /// level bump) use `\t`; when `false`, they use `indent_width`
    /// spaces. Falls back to `false`. Per-language override is the usual
    /// way to flip this on (e.g. Go).
    pub use_tabs: Option<bool>,
    /// When `true`, spaces and tabs in the buffer are rendered with
    /// visible marker glyphs (middle-dot / arrow) drawn in a dim
    /// foreground. Falls back to `false`.
    pub show_whitespace: Option<bool>,
    /// When `true`, save runs the configured formatter (external command
    /// if `formatter = {…}` is set, otherwise `textDocument/formatting`
    /// against the first attached LSP) before writing to disk. Falls back
    /// to `true`. Per-language overrides flatten the same field into the
    /// `[languages.<name>]` table.
    pub format_on_save: Option<bool>,
}

/// Fully-resolved editor settings — what the runtime actually reads
/// after the user TOML has been overlaid on the built-in defaults.
#[derive(Debug, Clone, Copy)]
pub struct EditorConfig {
    pub indent_width: usize,
    pub tab_width: usize,
    pub use_tabs: bool,
    pub show_whitespace: bool,
    pub format_on_save: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            indent_width: DEFAULT_INDENT_WIDTH,
            tab_width: DEFAULT_TAB_WIDTH,
            use_tabs: false,
            show_whitespace: false,
            format_on_save: true,
        }
    }
}

impl EditorConfig {
    /// Field-level overlay: any `Some(_)` in `user` wins, the rest of
    /// `self` survives. Used to layer per-language overrides on top of
    /// the global default.
    pub fn overlay(self, user: &EditorToml) -> Self {
        Self {
            indent_width: user.indent_width.unwrap_or(self.indent_width),
            tab_width: user.tab_width.unwrap_or(self.tab_width),
            use_tabs: user.use_tabs.unwrap_or(self.use_tabs),
            show_whitespace: user.show_whitespace.unwrap_or(self.show_whitespace),
            format_on_save: user.format_on_save.unwrap_or(self.format_on_save),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_user_omits_fields() {
        let eff = EditorConfig::default().overlay(&EditorToml::default());
        assert_eq!(eff.indent_width, 2);
        assert_eq!(eff.tab_width, 4);
    }

    #[test]
    fn overlay_replaces_only_provided_fields() {
        let base = EditorConfig {
            indent_width: 4,
            tab_width: 4,
            use_tabs: false,
            show_whitespace: false,
            format_on_save: true,
        };
        let eff = base.overlay(&EditorToml {
            tab_width: Some(8),
            ..Default::default()
        });
        assert_eq!(eff.indent_width, 4);
        assert_eq!(eff.tab_width, 8);
    }
}
