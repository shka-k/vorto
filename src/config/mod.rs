//! User configuration loaded from `~/.config/vorto/config.toml`.
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

mod cursor;
mod keys;

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::KeyCode;
use serde::Deserialize;

use crate::keymap::{Keymap, LEADER};
use crate::languages::LanguageConfig;

pub use cursor::{CursorConfig, CursorShape, CursorShapes, resolve_cursor_shapes};
use keys::{action_to_token, parse_sequence};

// ────────────────────────────────────────────────────────────────────────
// TOML schema
// ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub bind: Vec<Binding>,
    #[serde(default)]
    pub cursor: CursorConfig,
    /// `[languages.<name>]` blocks. Resolved against built-in defaults
    /// at startup — see `languages::resolve`.
    #[serde(default)]
    pub languages: std::collections::HashMap<String, LanguageConfig>,
    /// Directory holding `<grammar>.{so,dylib,dll}`. Defaults to
    /// `<config>/grammars`.
    pub grammar_dir: Option<String>,
    /// Directory holding `<lang>/highlights.scm`. Defaults to
    /// `<config>/queries`.
    pub query_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Binding {
    pub keys: String,
    pub action: String,
}

/// Resolve `grammar_dir` to an absolute path, falling back to
/// `<config>/grammars` when the user hasn't set one.
pub fn grammar_dir(c: &Config) -> PathBuf {
    c.grammar_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_subdir("grammars"))
}

/// Resolve `query_dir` to an absolute path, falling back to
/// `<config>/queries`.
pub fn query_dir(c: &Config) -> PathBuf {
    c.query_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_subdir("queries"))
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

/// Load and parse the user config from `path` (if it exists). Missing
/// file is fine — returns an empty `Config`.
pub fn load_or_default(path: Option<&std::path::Path>) -> Result<Config> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
    Ok(config)
}

/// Apply each `[[bind]]` entry to the keymap. Reports the first failing
/// binding with its index, so the user can find the offending line.
pub fn apply(config: &Config, keymap: &mut Keymap) -> Result<()> {
    for (i, b) in config.bind.iter().enumerate() {
        install_binding(keymap, &b.keys, &b.action)
            .with_context(|| format!("bind[{}] ({} → {})", i, b.keys, b.action))?;
    }
    Ok(())
}

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
    use crate::keymap::KeySig;
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
        let toml = r#"
bind = [
  { keys = "<C-s>", action = "save" },
  { keys = "<space>w", action = "save" },
]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.bind.len(), 2);
        assert_eq!(cfg.bind[0].keys, "<C-s>");
        assert_eq!(cfg.bind[1].action, "save");
    }

    #[test]
    fn cursor_defaults_when_unset() {
        let cfg: Config = toml::from_str("").unwrap();
        let shapes = resolve_cursor_shapes(&cfg.cursor).unwrap();
        assert!(matches!(shapes.normal, CursorShape::Block));
        assert!(matches!(shapes.insert, CursorShape::Bar));
        assert!(matches!(shapes.visual, CursorShape::Underbar));
    }

    #[test]
    fn cursor_overrides() {
        let toml = r#"
[cursor]
normal = "bar"
insert = "underbar"
visual = "block"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let shapes = resolve_cursor_shapes(&cfg.cursor).unwrap();
        assert!(matches!(shapes.normal, CursorShape::Bar));
        assert!(matches!(shapes.insert, CursorShape::Underbar));
        assert!(matches!(shapes.visual, CursorShape::Block));
    }

    #[test]
    fn cursor_unknown_shape() {
        let toml = r#"
[cursor]
normal = "diamond"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(resolve_cursor_shapes(&cfg.cursor).is_err());
    }

    #[test]
    fn parse_languages_table() {
        let toml = r#"
[languages.rust]
extensions = ["rs", "rlib"]

[languages.fish]
extensions = ["fish"]
grammar = "fish-shell"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.languages.len(), 2);
        assert_eq!(
            cfg.languages["rust"].extensions.as_deref(),
            Some(&["rs".to_string(), "rlib".to_string()][..])
        );
        assert_eq!(cfg.languages["fish"].grammar.as_deref(), Some("fish-shell"));
    }

    #[test]
    fn parse_table_array_form() {
        let toml = r#"
[[bind]]
keys = "<C-s>"
action = "save"

[[bind]]
keys = "<space>w"
action = "save"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.bind.len(), 2);
        assert_eq!(cfg.bind[0].keys, "<C-s>");
    }
}
