//! Binding types — `Binding`, `KeySig`, `Keymap` — and the leader
//! constant. The static binding tables (and the bulk vim-default
//! initializer) live in [`tables`] so this file stays focused on
//! the runtime types.
//!
//! The actual parser (tokenize / classify / build_expr) lives in
//! [`app/eval`](crate::app::eval).

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::Token;

mod tables;

pub use tables::{
    BRACKET_NEXT_BINDINGS, BRACKET_PREV_BINDINGS, CTRL_W_BINDINGS, GOTO_BINDINGS, LEADER_DEFAULTS,
    OBJECT_BINDINGS, OP_PENDING_BINDINGS, WINDOW_BINDINGS, Z_BINDINGS,
};

pub const LEADER: char = ' ';

/// One entry in the OpPending / ObjectExpected binding tables.
///
/// Lives in this module so the keymap and the which-key hint renderer
/// can read the same definitions. `key` is the primary key (the one
/// shown in hint panels); `aliases` are extra `KeyCode`s that resolve
/// to the same token but don't get their own hint row (e.g. arrow
/// keys mirroring `hjkl`).
pub struct Binding {
    pub key: KeyCode,
    pub aliases: &'static [KeyCode],
    pub token: Token,
    pub label: &'static str,
}

impl Binding {
    pub(crate) fn matches(&self, code: KeyCode) -> bool {
        self.key == code || self.aliases.contains(&code)
    }
}

/// Canonical key signature used for hash-table lookup. SHIFT is stripped
/// for Char keys (since 'G' vs 'g' is already encoded in the character)
/// so terminals that report it explicitly behave the same as ones that
/// don't.
#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy)]
pub struct KeySig {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeySig {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        let modifiers = if matches!(code, KeyCode::Char(_)) {
            modifiers - KeyModifiers::SHIFT
        } else {
            modifiers
        };
        Self { code, modifiers }
    }

    pub fn from_event(key: KeyEvent) -> Self {
        Self::new(key.code, key.modifiers)
    }
}

/// User-customisable binding tables, one per tokenization context that
/// the parser can be in. Each context is a `KeySig → Token` map.
///
/// `Initial` and `Leader` are the two everyday-customisable contexts —
/// they're the ones the TOML config writes into. `OpPending`,
/// `ObjectExpected`, and `GotoPending` use fixed match arms (their
/// grammar is part of the parser, not "user shortcuts"), so they're
/// intentionally absent here.
pub struct Keymap {
    pub initial: HashMap<KeySig, Token>,
    pub leader: HashMap<KeySig, Token>,
}

impl Keymap {
    /// Empty Keymap with no bindings; useful only as a builder base.
    pub fn empty() -> Self {
        Self {
            initial: HashMap::new(),
            leader: HashMap::new(),
        }
    }

    /// All of vim's default Normal-mode bindings.
    pub fn vim_default() -> Self {
        let mut m = Self::empty();
        m.install_vim_defaults();
        m
    }

    /// Insert a binding into the Initial context.
    pub fn bind_initial(&mut self, sig: KeySig, token: Token) {
        self.initial.insert(sig, token);
    }

    /// Insert a binding into the Leader-pending context (keys that fire
    /// after `<space>` has been pressed).
    pub fn bind_leader(&mut self, sig: KeySig, token: Token) {
        self.leader.insert(sig, token);
    }
}
