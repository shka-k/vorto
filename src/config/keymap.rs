//! Binding data: `Keymap` (the runtime-mutable Initial/Leader tables)
//! plus the static `OP_PENDING_BINDINGS` / `OBJECT_BINDINGS` reference
//! tables used both by the input parser and by the which-key hint
//! renderer.
//!
//! The actual parser (tokenize/classify/build_expr) lives in
//! [`app/eval.rs`](crate::app::eval).

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{DirectKind, MotionKind, Operator, PromptKind, Token};
use crate::mode::Mode;

pub const LEADER: char = ' ';

// ────────────────────────────────────────────────────────────────────────
// Binding tables — single source of truth for keys + which-key hints
// ────────────────────────────────────────────────────────────────────────

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

/// Keys valid in the OpPending context (right after `d`/`y`/`c`,
/// possibly with an inner count). Operator-repeat (`dd`/`yy`/`cc`) is
/// handled separately — it depends on the active operator.
pub const OP_PENDING_BINDINGS: &[Binding] = {
    use crate::action::Scope;
    use MotionKind::*;
    use Token::Motion as M;
    use Token::Scope as S;
    &[
        Binding {
            key: KeyCode::Char('i'),
            aliases: &[],
            token: S(Scope::Inner),
            label: "inner …",
        },
        Binding {
            key: KeyCode::Char('a'),
            aliases: &[],
            token: S(Scope::Around),
            label: "around …",
        },
        Binding {
            key: KeyCode::Char('w'),
            aliases: &[],
            token: M(WordForward),
            label: "word",
        },
        Binding {
            key: KeyCode::Char('b'),
            aliases: &[],
            token: M(WordBack),
            label: "back",
        },
        Binding {
            key: KeyCode::Char('e'),
            aliases: &[],
            token: M(WordEnd),
            label: "word end",
        },
        Binding {
            key: KeyCode::Char('W'),
            aliases: &[],
            token: M(BigWordForward),
            label: "WORD",
        },
        Binding {
            key: KeyCode::Char('B'),
            aliases: &[],
            token: M(BigWordBack),
            label: "WORD back",
        },
        Binding {
            key: KeyCode::Char('E'),
            aliases: &[],
            token: M(BigWordEnd),
            label: "WORD end",
        },
        Binding {
            key: KeyCode::Char('f'),
            aliases: &[],
            token: Token::FindCharPrefix { forward: true, till: false },
            label: "find char →",
        },
        Binding {
            key: KeyCode::Char('F'),
            aliases: &[],
            token: Token::FindCharPrefix { forward: false, till: false },
            label: "find char ←",
        },
        Binding {
            key: KeyCode::Char('t'),
            aliases: &[],
            token: Token::FindCharPrefix { forward: true, till: true },
            label: "till char →",
        },
        Binding {
            key: KeyCode::Char('T'),
            aliases: &[],
            token: Token::FindCharPrefix { forward: false, till: true },
            label: "till char ←",
        },
        Binding {
            key: KeyCode::Char(';'),
            aliases: &[],
            token: M(RepeatFind { reverse: false }),
            label: "repeat find",
        },
        Binding {
            key: KeyCode::Char(','),
            aliases: &[],
            token: M(RepeatFind { reverse: true }),
            label: "repeat find ↺",
        },
        Binding {
            key: KeyCode::Char('}'),
            aliases: &[],
            token: M(ParagraphForward),
            label: "paragraph fwd",
        },
        Binding {
            key: KeyCode::Char('{'),
            aliases: &[],
            token: M(ParagraphBack),
            label: "paragraph back",
        },
        Binding {
            key: KeyCode::Char('$'),
            aliases: &[KeyCode::End],
            token: M(LineEnd),
            label: "line end",
        },
        Binding {
            key: KeyCode::Char('0'),
            aliases: &[KeyCode::Home],
            token: M(LineStart),
            label: "line start",
        },
        Binding {
            key: KeyCode::Char('^'),
            aliases: &[],
            token: M(LineFirstNonBlank),
            label: "first non-blank",
        },
        Binding {
            key: KeyCode::Char('%'),
            aliases: &[],
            token: M(BracketMatch),
            label: "match bracket",
        },
        Binding {
            key: KeyCode::Char('h'),
            aliases: &[KeyCode::Left],
            token: M(Left),
            label: "left",
        },
        Binding {
            key: KeyCode::Char('l'),
            aliases: &[KeyCode::Right],
            token: M(Right),
            label: "right",
        },
        Binding {
            key: KeyCode::Char('j'),
            aliases: &[KeyCode::Down],
            token: M(Down),
            label: "down",
        },
        Binding {
            key: KeyCode::Char('k'),
            aliases: &[KeyCode::Up],
            token: M(Up),
            label: "up",
        },
        Binding {
            key: KeyCode::Char('G'),
            aliases: &[],
            token: M(FileEnd),
            label: "file end",
        },
    ]
};

/// Keys valid in the GotoPending context (right after `g`). Lives
/// here as the single source of truth for both the parser
/// (`goto_pending_token` in `app/eval.rs`) and the which-key hint
/// renderer (`pending_hints` in `ui/hints.rs`).
pub const GOTO_BINDINGS: &[Binding] = {
    use crate::action::DirectKind as D;
    use MotionKind::*;
    use Token::Direct as Dir;
    use Token::Motion as M;
    &[
        // `gg` re-emits the prefix so `[GotoPrefix, GotoPrefix]` closes
        // to a motion in `build_expr`.
        Binding {
            key: KeyCode::Char('g'),
            aliases: &[],
            token: Token::GotoPrefix,
            label: "file start (gg)",
        },
        Binding {
            key: KeyCode::Char('_'),
            aliases: &[],
            token: M(LineLastNonBlank),
            label: "line last non-blank",
        },
        Binding {
            key: KeyCode::Char('e'),
            aliases: &[],
            token: M(WordEndBack),
            label: "word end back",
        },
        Binding {
            key: KeyCode::Char('E'),
            aliases: &[],
            token: M(BigWordEndBack),
            label: "WORD end back",
        },
        Binding {
            key: KeyCode::Char('s'),
            aliases: &[],
            token: M(LineFirstNonBlank),
            label: "line start (= ^)",
        },
        Binding {
            key: KeyCode::Char('l'),
            aliases: &[],
            token: M(LineEnd),
            label: "line end (= $)",
        },
        Binding {
            key: KeyCode::Char('c'),
            aliases: &[],
            token: M(ViewportMiddle),
            label: "viewport mid (= M)",
        },
        Binding {
            key: KeyCode::Char('b'),
            aliases: &[],
            token: M(ViewportBottom),
            label: "viewport bot (= L)",
        },
        Binding {
            key: KeyCode::Char('d'),
            aliases: &[],
            token: Dir(D::GotoDefinition),
            label: "definition (lsp)",
        },
        Binding {
            key: KeyCode::Char('D'),
            aliases: &[],
            token: Dir(D::GotoDeclaration),
            label: "declaration (lsp)",
        },
        Binding {
            key: KeyCode::Char('i'),
            aliases: &[],
            token: Dir(D::GotoImplementation),
            label: "implementation (lsp)",
        },
        Binding {
            key: KeyCode::Char('r'),
            aliases: &[],
            token: Dir(D::FindReferences),
            label: "references (lsp)",
        },
    ]
};

/// Keys valid in the ZPending context (right after `z`). Same
/// pattern as [`GOTO_BINDINGS`].
pub const Z_BINDINGS: &[Binding] = {
    use crate::action::DirectKind as D;
    use Token::Direct as Dir;
    &[
        Binding {
            key: KeyCode::Char('z'),
            aliases: &[],
            token: Dir(D::ViewportCenter),
            label: "center cursor",
        },
        Binding {
            key: KeyCode::Char('t'),
            aliases: &[],
            token: Dir(D::ViewportTopAtCursor),
            label: "scroll cursor to top",
        },
        Binding {
            key: KeyCode::Char('b'),
            aliases: &[],
            token: Dir(D::ViewportBottomAtCursor),
            label: "scroll cursor to bottom",
        },
    ]
};

/// Default `<space>` leader bindings. Lives here as the single source
/// of truth for both `Keymap::install_vim_defaults` (which copies
/// them into the runtime-mutable Leader HashMap) and the which-key
/// hint renderer.
pub const LEADER_DEFAULTS: &[Binding] = {
    use crate::action::{DirectKind as D, PromptKind};
    use crate::finder::FuzzyKind;
    use Token::Direct as Dir;
    &[
        Binding {
            key: KeyCode::Char('f'),
            aliases: &[],
            token: Dir(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Files))),
            label: "fuzzy files",
        },
        Binding {
            key: KeyCode::Char('l'),
            aliases: &[],
            token: Dir(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Lines))),
            label: "fuzzy lines",
        },
        Binding {
            key: KeyCode::Char('b'),
            aliases: &[],
            token: Dir(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Buffers))),
            label: "buffer picker",
        },
        Binding {
            key: KeyCode::Char('r'),
            aliases: &[],
            token: Dir(D::Rename),
            label: "rename (lsp)",
        },
        Binding {
            key: KeyCode::Char('a'),
            aliases: &[],
            token: Dir(D::CodeAction),
            label: "code action (lsp)",
        },
    ]
};

/// Keys valid in the ObjectExpected context (right after `i`/`a` as
/// the scope marker).
pub const OBJECT_BINDINGS: &[Binding] = {
    use crate::action::Object::*;
    use Token::Object as O;
    &[
        Binding {
            key: KeyCode::Char('w'),
            aliases: &[],
            token: O(Word),
            label: "word",
        },
        Binding {
            key: KeyCode::Char('p'),
            aliases: &[],
            token: O(Paragraph),
            label: "paragraph",
        },
        Binding {
            key: KeyCode::Char('"'),
            aliases: &[],
            token: O(DoubleQuote),
            label: "double-quotes",
        },
        Binding {
            key: KeyCode::Char('\''),
            aliases: &[],
            token: O(SingleQuote),
            label: "single-quotes",
        },
        Binding {
            key: KeyCode::Char('('),
            aliases: &[KeyCode::Char(')'), KeyCode::Char('b')],
            token: O(Paren),
            label: "parens",
        },
        Binding {
            key: KeyCode::Char('{'),
            aliases: &[KeyCode::Char('}'), KeyCode::Char('B')],
            token: O(Brace),
            label: "braces",
        },
        Binding {
            key: KeyCode::Char('['),
            aliases: &[KeyCode::Char(']')],
            token: O(Bracket),
            label: "brackets",
        },
        Binding {
            key: KeyCode::Char('f'),
            aliases: &[],
            token: O(Function),
            label: "function (ts)",
        },
        Binding {
            key: KeyCode::Char('c'),
            aliases: &[],
            token: O(Class),
            label: "class (ts)",
        },
        Binding {
            key: KeyCode::Char('a'),
            aliases: &[],
            token: O(Parameter),
            label: "argument (ts)",
        },
    ]
};

// ────────────────────────────────────────────────────────────────────────
// Keymap — runtime-mutable binding table per context
// ────────────────────────────────────────────────────────────────────────

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

    fn install_vim_defaults(&mut self) {
        use DirectKind as D;
        use MotionKind as M;
        use Token::*;

        let none = KeyModifiers::NONE;
        let initial = [
            // ── movement ─────────────────────────────────────────────
            (KeyCode::Char('h'), Motion(M::Left)),
            (KeyCode::Left, Motion(M::Left)),
            (KeyCode::Char('l'), Motion(M::Right)),
            (KeyCode::Right, Motion(M::Right)),
            (KeyCode::Char('j'), Motion(M::Down)),
            (KeyCode::Down, Motion(M::Down)),
            (KeyCode::Char('k'), Motion(M::Up)),
            (KeyCode::Up, Motion(M::Up)),
            (KeyCode::Char('$'), Motion(M::LineEnd)),
            (KeyCode::End, Motion(M::LineEnd)),
            (KeyCode::Home, Motion(M::LineStart)),
            (KeyCode::Char('^'), Motion(M::LineFirstNonBlank)),
            (KeyCode::Char('w'), Motion(M::WordForward)),
            (KeyCode::Char('b'), Motion(M::WordBack)),
            (KeyCode::Char('e'), Motion(M::WordEnd)),
            (KeyCode::Char('W'), Motion(M::BigWordForward)),
            (KeyCode::Char('B'), Motion(M::BigWordBack)),
            (KeyCode::Char('E'), Motion(M::BigWordEnd)),
            (KeyCode::Char('{'), Motion(M::ParagraphBack)),
            (KeyCode::Char('}'), Motion(M::ParagraphForward)),
            (KeyCode::Char('G'), Motion(M::FileEnd)),
            (KeyCode::Char('H'), Motion(M::ViewportTop)),
            (KeyCode::Char('M'), Motion(M::ViewportMiddle)),
            (KeyCode::Char('L'), Motion(M::ViewportBottom)),
            (KeyCode::Char('%'), Motion(M::BracketMatch)),
            (KeyCode::Char('*'), Motion(M::SearchWordForward)),
            (KeyCode::Char('#'), Motion(M::SearchWordBack)),
            (KeyCode::Char('n'), Motion(M::SearchNext)),
            (KeyCode::Char('N'), Motion(M::SearchPrev)),
            // ── char-find prefixes (next keystroke is the literal target) ─
            (
                KeyCode::Char('f'),
                FindCharPrefix { forward: true, till: false },
            ),
            (
                KeyCode::Char('F'),
                FindCharPrefix { forward: false, till: false },
            ),
            (
                KeyCode::Char('t'),
                FindCharPrefix { forward: true, till: true },
            ),
            (
                KeyCode::Char('T'),
                FindCharPrefix { forward: false, till: true },
            ),
            (KeyCode::Char(';'), Motion(M::RepeatFind { reverse: false })),
            (KeyCode::Char(','), Motion(M::RepeatFind { reverse: true })),
            // ── operators ────────────────────────────────────────────
            (KeyCode::Char('d'), Op(Operator::Delete)),
            (KeyCode::Char('y'), Op(Operator::Yank)),
            (KeyCode::Char('c'), Op(Operator::Change)),
            // ── standalone commands ──────────────────────────────────
            (KeyCode::Char('i'), Direct(D::EnterMode(Mode::Insert))),
            (KeyCode::Char('I'), Direct(D::InsertAtLineStart)),
            (KeyCode::Char('a'), Direct(D::AppendAfterCursor)),
            (KeyCode::Char('A'), Direct(D::AppendAtLineEnd)),
            (KeyCode::Char('v'), Direct(D::EnterMode(Mode::Visual))),
            (KeyCode::Char('V'), Direct(D::EnterMode(Mode::VisualLine))),
            (KeyCode::Char('o'), Direct(D::OpenLineBelow)),
            (KeyCode::Char('O'), Direct(D::OpenLineAbove)),
            (KeyCode::Char('x'), Direct(D::DeleteCharUnderCursor)),
            (KeyCode::Char('p'), Direct(D::Paste)),
            (KeyCode::Char('u'), Direct(D::Undo)),
            (KeyCode::Char('C'), Direct(D::ChangeToEol)),
            (KeyCode::Char('D'), Direct(D::DeleteToEol)),
            (KeyCode::Char('Y'), Direct(D::YankLine)),
            (KeyCode::Char('J'), Direct(D::JoinLines)),
            (KeyCode::Char('~'), Direct(D::ToggleCase)),
            (KeyCode::Char('s'), Direct(D::SubstituteChar)),
            (KeyCode::Char('S'), Direct(D::SubstituteLine)),
            (KeyCode::Char('r'), ReplaceCharPrefix),
            (KeyCode::Char('z'), ZPrefix),
            (
                KeyCode::Char(':'),
                Direct(D::OpenPrompt(PromptKind::Command)),
            ),
            (
                KeyCode::Char('/'),
                Direct(D::OpenPrompt(PromptKind::Search { forward: true })),
            ),
            (
                KeyCode::Char('?'),
                Direct(D::OpenPrompt(PromptKind::Search { forward: false })),
            ),
            (KeyCode::Char('g'), GotoPrefix),
            (KeyCode::Char(LEADER), LeaderPrefix),
        ];
        for (code, token) in initial {
            self.bind_initial(KeySig::new(code, none), token);
        }

        // Ctrl-V → visual-block. (Plain `v` and `V` are bound above; the
        // SHIFT modifier on `V` is stripped by `KeySig::new`.)
        self.bind_initial(
            KeySig::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            Direct(D::EnterMode(Mode::VisualBlock)),
        );
        // Page motions.
        let ctrl = KeyModifiers::CONTROL;
        for (ch, m) in [
            ('d', M::HalfPageDown),
            ('u', M::HalfPageUp),
            ('f', M::PageDown),
            ('b', M::PageUp),
        ] {
            self.bind_initial(KeySig::new(KeyCode::Char(ch), ctrl), Motion(m));
        }

        // Leader bindings — single source of truth in LEADER_DEFAULTS.
        for b in LEADER_DEFAULTS {
            self.bind_leader(KeySig::new(b.key, none), b.token);
            for &alias in b.aliases {
                self.bind_leader(KeySig::new(alias, none), b.token);
            }
        }
    }
}
