//! Static binding tables (the single source of truth for both the
//! parser and the which-key hint renderer) plus the bulk vim-default
//! initializer that copies them — and the inline Initial-context
//! defaults — into a fresh `Keymap`.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::action::{DirectKind, MotionKind, Operator, PromptKind, Token};
use crate::mode::Mode;

use super::{Binding, KeySig, Keymap, LEADER};

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
        // `g` after an operator opens the goto-prefix sub-grammar
        // (`dge`, `dg_`, `dgn`, ...). The follow-up key is then
        // resolved through `GOTO_BINDINGS` in `goto_pending_token`.
        Binding {
            key: KeyCode::Char('g'),
            aliases: &[],
            token: Token::GotoPrefix,
            label: "goto …",
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
        Binding {
            key: KeyCode::Char('w'),
            aliases: &[],
            token: Dir(D::JumpLabel),
            label: "jump label (2-char)",
        },
        Binding {
            key: KeyCode::Char('n'),
            aliases: &[],
            token: Dir(D::SearchSelectNext { reverse: false }),
            label: "select next match",
        },
        Binding {
            key: KeyCode::Char('N'),
            aliases: &[],
            token: Dir(D::SearchSelectNext { reverse: true }),
            label: "select prev match",
        },
        Binding {
            key: KeyCode::Char('A'),
            aliases: &[],
            token: Dir(D::SelectWholeBuffer),
            label: "select whole buffer (= ggVG)",
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
    use crate::finder::{FuzzyKind, IgnoreOpts};
    use Token::Direct as Dir;
    &[
        Binding {
            key: KeyCode::Char('f'),
            aliases: &[],
            token: Dir(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Files {
                ignore: IgnoreOpts::DEFAULT,
            }))),
            label: "fuzzy files",
        },
        // `<space>F` — same picker, but dotfile segments are kept so
        // `.env`, `.github/...` etc. become visible. `.gitignore` is
        // still honored.
        Binding {
            key: KeyCode::Char('F'),
            aliases: &[],
            token: Dir(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Files {
                ignore: IgnoreOpts::SHOW_HIDDEN,
            }))),
            label: "fuzzy files (+hidden)",
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
        Binding {
            key: KeyCode::Char('K'),
            aliases: &[KeyCode::Char('k')],
            token: Dir(D::Hover),
            label: "hover (lsp)",
        },
        Binding {
            key: KeyCode::Char('c'),
            aliases: &[],
            token: Dir(D::ToggleComment),
            label: "toggle line comment",
        },
        Binding {
            key: KeyCode::Char('*'),
            aliases: &[],
            token: Dir(D::SearchWordKeep { forward: true }),
            label: "search word (keep cursor)",
        },
        Binding {
            key: KeyCode::Char('#'),
            aliases: &[],
            token: Dir(D::SearchWordKeep { forward: false }),
            label: "search word ← (keep cursor)",
        },
        Binding {
            key: KeyCode::Char(','),
            aliases: &[],
            token: Dir(D::MultiCursorClear),
            label: "clear extra cursors",
        },
        // `<space>w` — sub-leader for window/pane operations. The
        // follow-up key resolves through `WINDOW_BINDINGS`.
        Binding {
            key: KeyCode::Char('w'),
            aliases: &[],
            token: Token::WindowPrefix,
            label: "window …",
        },
    ]
};

/// Keys valid in the `CtrlWPending` context (right after `Ctrl-W`).
/// Mirrors vim's window-prefix chord: h/j/k/l are focus-move, v / s
/// are vertical / horizontal splits, c / q close the active pane, w
/// cycles. Different from [`WINDOW_BINDINGS`] (the `<space>w` sub-
/// leader) because user-facing semantics differ: `<space>w h` is
/// horizontal split, but vim's `Ctrl-W h` is focus-left.
pub const CTRL_W_BINDINGS: &[Binding] = {
    use crate::action::DirectKind as D;
    use crate::action::FocusDir;
    use Token::Direct as Dir;
    &[
        Binding {
            key: KeyCode::Char('h'),
            aliases: &[KeyCode::Left],
            token: Dir(D::FocusWindow {
                dir: FocusDir::Left,
            }),
            label: "focus left",
        },
        Binding {
            key: KeyCode::Char('l'),
            aliases: &[KeyCode::Right],
            token: Dir(D::FocusWindow {
                dir: FocusDir::Right,
            }),
            label: "focus right",
        },
        Binding {
            key: KeyCode::Char('j'),
            aliases: &[KeyCode::Down],
            token: Dir(D::FocusWindow {
                dir: FocusDir::Down,
            }),
            label: "focus down",
        },
        Binding {
            key: KeyCode::Char('k'),
            aliases: &[KeyCode::Up],
            token: Dir(D::FocusWindow {
                dir: FocusDir::Up,
            }),
            label: "focus up",
        },
        Binding {
            key: KeyCode::Char('w'),
            aliases: &[],
            token: Dir(D::CycleWindow),
            label: "next pane",
        },
        Binding {
            key: KeyCode::Char('v'),
            aliases: &[],
            token: Dir(D::SplitWindowVertical),
            label: "split right (vertical)",
        },
        Binding {
            key: KeyCode::Char('s'),
            aliases: &[],
            token: Dir(D::SplitWindowHorizontal),
            label: "split below (horizontal)",
        },
        Binding {
            key: KeyCode::Char('c'),
            aliases: &[KeyCode::Char('q')],
            token: Dir(D::CloseWindow),
            label: "close pane",
        },
    ]
};

/// Keys valid in the `WindowPending` context (right after `<space>w`).
/// Same source-of-truth pattern as [`GOTO_BINDINGS`] — referenced by
/// both the parser (`window_pending_token` in `app/eval/parse.rs`) and
/// the which-key hint renderer.
pub const WINDOW_BINDINGS: &[Binding] = {
    use crate::action::DirectKind as D;
    use crate::action::FocusDir;
    use Token::Direct as Dir;
    &[
        Binding {
            key: KeyCode::Char('v'),
            aliases: &[],
            token: Dir(D::SplitWindowVertical),
            label: "split right (vertical)",
        },
        Binding {
            key: KeyCode::Char('h'),
            aliases: &[],
            token: Dir(D::SplitWindowHorizontal),
            label: "split below (horizontal)",
        },
        Binding {
            key: KeyCode::Char('c'),
            aliases: &[],
            token: Dir(D::CloseWindow),
            label: "close pane",
        },
        Binding {
            key: KeyCode::Char('o'),
            aliases: &[],
            token: Dir(D::CycleWindow),
            label: "next pane",
        },
        Binding {
            key: KeyCode::Left,
            aliases: &[],
            token: Dir(D::FocusWindow {
                dir: FocusDir::Left,
            }),
            label: "focus left",
        },
        Binding {
            key: KeyCode::Right,
            aliases: &[],
            token: Dir(D::FocusWindow {
                dir: FocusDir::Right,
            }),
            label: "focus right",
        },
        Binding {
            key: KeyCode::Up,
            aliases: &[],
            token: Dir(D::FocusWindow {
                dir: FocusDir::Up,
            }),
            label: "focus up",
        },
        Binding {
            key: KeyCode::Down,
            aliases: &[],
            token: Dir(D::FocusWindow {
                dir: FocusDir::Down,
            }),
            label: "focus down",
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

impl Keymap {
    pub(super) fn install_vim_defaults(&mut self) {
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
            (KeyCode::Char('>'), Op(Operator::Indent)),
            (KeyCode::Char('<'), Op(Operator::Dedent)),
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
            (KeyCode::Char('.'), Direct(D::RepeatLast)),
            (KeyCode::Char('g'), GotoPrefix),
            // `K` — LSP hover popup for the symbol under the cursor.
            // Matches vim's `K` (which runs `man`-style lookups by
            // default) and helix's binding.
            (KeyCode::Char('K'), Direct(D::Hover)),
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

        // Multi-cursor: `+` adds a cursor at the next word match, `-`
        // removes the most recently added one. Bare keys (no Ctrl) so
        // they're trivially reachable inside any terminal multiplexer.
        // Vim's `+` / `-` (first non-blank of next/prev line) are
        // redundant with `j^` / `k^`, so re-purposing them here costs
        // nothing. Clearing all extras is on the leader (`<space>,`)
        // — see LEADER_DEFAULTS.
        self.bind_initial(
            KeySig::new(KeyCode::Char('+'), none),
            Direct(D::MultiCursorAddNext),
        );
        self.bind_initial(
            KeySig::new(KeyCode::Char('-'), none),
            Direct(D::MultiCursorPop),
        );

        // Leader bindings — single source of truth in LEADER_DEFAULTS.
        for b in LEADER_DEFAULTS {
            self.bind_leader(KeySig::new(b.key, none), b.token);
            for &alias in b.aliases {
                self.bind_leader(KeySig::new(alias, none), b.token);
            }
        }
    }
}
