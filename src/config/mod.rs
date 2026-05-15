//! Resolved user configuration loaded from `~/.config/vorto/config.toml`.
//!
//! The public type [`Config`] is a pure data struct holding the final,
//! ready-to-use settings (`keymap`, `cursor_shapes`, `languages`,
//! `grammar_dir`, `query_dir`). [`Config::load`] consumes a TOML file
//! (when present) and produces a `Config`; everything else in this
//! module is internal plumbing.
//!
//! Schema:
//!
//! ```toml
//! [[bind]]
//! keys   = "<C-s>"      # vim-style key notation; see `keys::parse_sequence`
//! action = "save"        # named action; see `keys::action_to_token`
//!
//! [[bind]]
//! keys   = "<space>w"   # 2-key sequence — installed in the Leader context
//! action = "save"
//! ```
//!
//! Bindings either **override** an existing default (same key sequence)
//! or **add** new ones. Only single keys (Initial context) and
//! `<space>X` two-key sequences (Leader context) are supported in v1.

mod command;
mod cursor;
mod editor;
mod keymap;
mod keys;
mod languages;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::KeyCode;
use serde::Deserialize;

pub use command::{COMMAND_BINDS, CommandBind};
pub use cursor::{CursorShape, CursorShapes};
pub use editor::{EditorConfig, EditorToml};
pub use keymap::{
    GOTO_BINDINGS, KeySig, Keymap, LEADER_DEFAULTS, OBJECT_BINDINGS, OP_PENDING_BINDINGS,
    Z_BINDINGS,
};
pub use languages::{Language, LanguageConfig, LanguageRegistry, LspConfig, LspToml};

use cursor::{CursorConfig, resolve_cursor_shapes};
use keymap::LEADER;
use keys::{action_to_token, parse_sequence};

/// Resolved configuration — the runtime state of "what settings is the
/// app currently using". Pure data: every field is filled in by
/// [`Config::load`] and never mutated afterward.
pub struct Config {
    pub keymap: Keymap,
    pub cursor_shapes: CursorShapes,
    pub languages: LanguageRegistry,
    /// Global editor settings, applied to every buffer that doesn't get
    /// a more specific override from a `[languages.<name>]` block.
    pub editor: EditorConfig,
    /// Absolute path to the grammar directory (`<grammar>.{so,dylib,dll}`).
    pub grammar_dir: PathBuf,
    /// Absolute path to the query directory (`<lang>/highlights.scm`).
    pub query_dir: PathBuf,
}

impl Config {
    /// Load and resolve the user config from `path` (if it exists).
    /// Missing file or `None` path yields a Config seeded entirely from
    /// built-in defaults.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let toml = Toml::load(path)?;
        Self::resolve(toml)
    }

    fn resolve(toml: Toml) -> Result<Self> {
        let mut keymap = Keymap::vim_default();
        for (i, b) in toml.bind.iter().enumerate() {
            install_binding(&mut keymap, &b.keys, &b.action)
                .with_context(|| format!("bind[{}] ({} → {})", i, b.keys, b.action))?;
        }

        let cursor_shapes = resolve_cursor_shapes(&toml.cursor)?;
        let editor = EditorConfig::default().overlay(&toml.editor);
        let languages = LanguageRegistry::build(toml.languages, toml.lsp)?;
        let grammar_dir = toml
            .grammar_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| default_subdir("grammars"));
        let query_dir = toml
            .query_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| default_subdir("queries"));

        Ok(Self {
            keymap,
            cursor_shapes,
            languages,
            editor,
            grammar_dir,
            query_dir,
        })
    }
}

// ────────────────────────────────────────────────────────────────────────
// TOML schema (private — implementation detail of `Config::load`).
// ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct Toml {
    #[serde(default)]
    bind: Vec<BindEntry>,
    #[serde(default)]
    cursor: CursorConfig,
    /// Global `[editor]` table. Per-language overrides flatten the same
    /// fields directly into each `[languages.<name>]` table.
    #[serde(default)]
    editor: EditorToml,
    /// `[languages.<name>]` blocks. Resolved against built-in defaults
    /// by [`LanguageRegistry::build`].
    #[serde(default)]
    languages: std::collections::HashMap<String, LanguageConfig>,
    /// `[lsp.<server-name>]` blocks. Built-in servers can be partially
    /// overlaid (e.g. just `args`); entirely new servers must include
    /// `command`. Referenced from `[languages.<lang>].lsp = ["<name>"]`.
    #[serde(default)]
    lsp: std::collections::HashMap<String, LspToml>,
    /// Directory holding `<grammar>.{so,dylib,dll}`. Defaults to
    /// `<config>/grammars`.
    grammar_dir: Option<String>,
    /// Directory holding `<lang>/highlights.scm`. Defaults to
    /// `<config>/queries`.
    query_dir: Option<String>,
}

impl Toml {
    fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }
}

/// Single `[[bind]]` row from the TOML schema.
#[derive(Debug, Deserialize)]
struct BindEntry {
    keys: String,
    action: String,
}

// ────────────────────────────────────────────────────────────────────────
// Path resolution
// ────────────────────────────────────────────────────────────────────────

/// Resolve the config-file path. Honors `$XDG_CONFIG_HOME` if set,
/// otherwise falls back to `$HOME/.config/vorto/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg).join("vorto/config.toml");
        if p.exists() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home).join(".config/vorto/config.toml");
    Some(p)
}

fn default_subdir(name: &str) -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("vorto").join(name);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config/vorto").join(name);
    }
    PathBuf::from(name)
}

// ────────────────────────────────────────────────────────────────────────
// Binding application
// ────────────────────────────────────────────────────────────────────────

fn install_binding(keymap: &mut Keymap, keys: &str, action: &str) -> Result<()> {
    let sequence = parse_sequence(keys)?;
    let token = action_to_token(action).ok_or_else(|| anyhow!("unknown action: {}", action))?;
    match sequence.as_slice() {
        [k] => {
            keymap.bind_initial(*k, token);
        }
        [first, second] if first.code == KeyCode::Char(LEADER) && first.modifiers.is_empty() => {
            keymap.bind_leader(*second, token);
        }
        [_, _] => bail!(
            "only `<space>X` two-key sequences are supported; got: {}",
            keys
        ),
        _ => bail!(
            "sequences of more than 2 keys aren't supported yet; got: {}",
            keys
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{DirectKind, Token};
    use crossterm::event::KeyModifiers;

    #[test]
    fn install_leader_binding() {
        let mut km = Keymap::vim_default();
        install_binding(&mut km, "<space>w", "save").unwrap();
        let sig = KeySig::new(KeyCode::Char('w'), KeyModifiers::NONE);
        assert_eq!(km.leader.get(&sig), Some(&Token::Direct(DirectKind::Save)));
    }

    #[test]
    fn install_overrides_existing() {
        let mut km = Keymap::vim_default();
        install_binding(&mut km, "u", "quit").unwrap();
        let sig = KeySig::new(KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(km.initial.get(&sig), Some(&Token::Direct(DirectKind::Quit)));
    }

    #[test]
    fn parse_inline_array_form() {
        let text = r#"
bind = [
  { keys = "<C-s>", action = "save" },
  { keys = "<space>w", action = "save" },
]
"#;
        let toml: Toml = toml::from_str(text).unwrap();
        assert_eq!(toml.bind.len(), 2);
        assert_eq!(toml.bind[0].keys, "<C-s>");
        assert_eq!(toml.bind[1].action, "save");
    }

    #[test]
    fn cursor_defaults_when_unset() {
        let toml: Toml = toml::from_str("").unwrap();
        let shapes = resolve_cursor_shapes(&toml.cursor).unwrap();
        assert!(matches!(shapes.normal, CursorShape::Block));
        assert!(matches!(shapes.insert, CursorShape::Bar));
        assert!(matches!(shapes.visual, CursorShape::Underbar));
    }

    #[test]
    fn cursor_overrides() {
        let text = r#"
[cursor]
normal = "bar"
insert = "underbar"
visual = "block"
"#;
        let toml: Toml = toml::from_str(text).unwrap();
        let shapes = resolve_cursor_shapes(&toml.cursor).unwrap();
        assert!(matches!(shapes.normal, CursorShape::Bar));
        assert!(matches!(shapes.insert, CursorShape::Underbar));
        assert!(matches!(shapes.visual, CursorShape::Block));
    }

    #[test]
    fn cursor_unknown_shape() {
        let text = r#"
[cursor]
normal = "diamond"
"#;
        let toml: Toml = toml::from_str(text).unwrap();
        assert!(resolve_cursor_shapes(&toml.cursor).is_err());
    }

    #[test]
    fn parse_languages_table() {
        let text = r#"
[languages.rust]
extensions = ["rs", "rlib"]

[languages.fish]
extensions = ["fish"]
grammar = "fish-shell"
"#;
        let toml: Toml = toml::from_str(text).unwrap();
        assert_eq!(toml.languages.len(), 2);
        assert_eq!(
            toml.languages["rust"].extensions.as_deref(),
            Some(&["rs".to_string(), "rlib".to_string()][..])
        );
        assert_eq!(
            toml.languages["fish"].grammar.as_deref(),
            Some("fish-shell")
        );
    }

    #[test]
    fn parse_table_array_form() {
        let text = r#"
[[bind]]
keys = "<C-s>"
action = "save"

[[bind]]
keys = "<space>w"
action = "save"
"#;
        let toml: Toml = toml::from_str(text).unwrap();
        assert_eq!(toml.bind.len(), 2);
        assert_eq!(toml.bind[0].keys, "<C-s>");
    }
}
