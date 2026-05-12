//! User configuration loaded from `~/.config/vorto/config.toml`.
//!
//! Schema:
//!
//! ```toml
//! [[bind]]
//! keys   = "<C-s>"      # vim-style key notation; see `parse_sequence`
//! action = "save"        # named action; see `action_to_token`
//!
//! [[bind]]
//! keys   = "<space>w"   # 2-key sequence — installed in the Leader context
//! action = "save"
//! ```
//!
//! Bindings either **override** an existing default (same key sequence)
//! or **add** new ones. Only single keys (Initial context) and
//! `<space>X` two-key sequences (Leader context) are supported in v1.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;

use crate::action::{DirectKind, MotionKind, Operator, PromptKind, Token};
use crate::fuzzy::FuzzyKind;
use crate::keymap::{KeySig, Keymap, LEADER};
use crate::mode::Mode;

// ────────────────────────────────────────────────────────────────────────
// TOML schema
// ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub bind: Vec<Binding>,
}

#[derive(Debug, Deserialize)]
pub struct Binding {
    pub keys: String,
    pub action: String,
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
    let config: Config = toml::from_str(&text)
        .with_context(|| format!("parsing config {}", path.display()))?;
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

// ────────────────────────────────────────────────────────────────────────
// Key sequence parsing
// ────────────────────────────────────────────────────────────────────────

/// Parse a vim-style key string into a sequence of `KeySig`s.
///
/// Each entry is either a single character (`a`, `G`, `?`) or a named
/// key in angle brackets (`<C-s>`, `<space>`, `<esc>`). Modifiers come
/// dash-separated before the key name: `<C-S-x>` = Ctrl+Shift+x.
fn parse_sequence(s: &str) -> Result<Vec<KeySig>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut name = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '>' {
                    closed = true;
                    break;
                }
                name.push(c2);
            }
            if !closed {
                bail!("unterminated <...> in `{}`", s);
            }
            out.push(parse_named_key(&name)?);
        } else {
            out.push(KeySig::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }
    if out.is_empty() {
        bail!("empty key sequence");
    }
    Ok(out)
}

fn parse_named_key(name: &str) -> Result<KeySig> {
    let lower = name.to_lowercase();
    let parts: Vec<&str> = lower.split('-').collect();
    let mut mods = KeyModifiers::NONE;
    for m in &parts[..parts.len() - 1] {
        mods |= match *m {
            "c" | "ctrl" => KeyModifiers::CONTROL,
            "s" | "shift" => KeyModifiers::SHIFT,
            "a" | "alt" | "m" | "meta" => KeyModifiers::ALT,
            other => bail!("unknown modifier: {}", other),
        };
    }
    let key_part = parts[parts.len() - 1];
    let code = match key_part {
        "space" => KeyCode::Char(' '),
        "esc" => KeyCode::Esc,
        "cr" | "enter" | "return" => KeyCode::Enter,
        "bs" | "backspace" => KeyCode::Backspace,
        "tab" => KeyCode::Tab,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        other => bail!("unknown key: <{}>", other),
    };
    Ok(KeySig::new(code, mods))
}

// ────────────────────────────────────────────────────────────────────────
// Action name → Token
// ────────────────────────────────────────────────────────────────────────

/// Look up the Token a config-named action resolves to. Returns `None`
/// when the name isn't recognized.
pub fn action_to_token(name: &str) -> Option<Token> {
    use DirectKind as D;
    use MotionKind as M;
    use Token::*;
    let t = match name {
        // ── motions ────────────────────────────────────────────────────
        "left" => Motion(M::Left),
        "right" => Motion(M::Right),
        "up" => Motion(M::Up),
        "down" => Motion(M::Down),
        "line-start" => Motion(M::LineStart),
        "line-end" => Motion(M::LineEnd),
        "word-forward" => Motion(M::WordForward),
        "word-back" => Motion(M::WordBack),
        "file-start" => Motion(M::FileStart),
        "file-end" => Motion(M::FileEnd),
        "search-next" => Motion(M::SearchNext),
        "search-prev" => Motion(M::SearchPrev),

        // ── direct commands ────────────────────────────────────────────
        "save" => Direct(D::Save),
        "open" => Direct(D::Open),
        "quit" => Direct(D::Quit),
        "quit-force" => Direct(D::QuitForce),
        "save-and-quit" => Direct(D::SaveAndQuit),
        "goto-line" => Direct(D::GotoLine),
        "enter-insert" => Direct(D::EnterMode(Mode::Insert)),
        "enter-normal" => Direct(D::EnterMode(Mode::Normal)),
        "enter-visual" => Direct(D::EnterMode(Mode::Visual)),
        "open-line-below" => Direct(D::OpenLineBelow),
        "open-line-above" => Direct(D::OpenLineAbove),
        "paste" => Direct(D::Paste),
        "undo" => Direct(D::Undo),
        "redo" => Direct(D::Redo),
        "delete-char" => Direct(D::DeleteCharUnderCursor),
        "command-prompt" => Direct(D::OpenPrompt(PromptKind::Command)),
        "search-forward" => Direct(D::OpenPrompt(PromptKind::Search { forward: true })),
        "search-backward" => Direct(D::OpenPrompt(PromptKind::Search { forward: false })),
        "fuzzy-files" => Direct(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Files))),
        "fuzzy-lines" => Direct(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Lines))),

        // ── operators (when bound at top level) ────────────────────────
        "delete-operator" => Op(Operator::Delete),
        "yank-operator" => Op(Operator::Yank),
        "change-operator" => Op(Operator::Change),

        // ── prefix transitions ────────────────────────────────────────
        "leader" => LeaderPrefix,
        "goto-prefix" => GotoPrefix,

        _ => return None,
    };
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_char() {
        let sig = parse_sequence("a").unwrap();
        assert_eq!(sig.len(), 1);
        assert_eq!(sig[0].code, KeyCode::Char('a'));
    }

    #[test]
    fn ctrl_modified() {
        let sig = parse_sequence("<C-s>").unwrap();
        assert_eq!(sig[0].code, KeyCode::Char('s'));
        assert!(sig[0].modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn space_leader_seq() {
        let sig = parse_sequence("<space>w").unwrap();
        assert_eq!(sig.len(), 2);
        assert_eq!(sig[0].code, KeyCode::Char(' '));
        assert_eq!(sig[1].code, KeyCode::Char('w'));
    }

    #[test]
    fn unknown_key() {
        assert!(parse_sequence("<wat>").is_err());
    }

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
}
