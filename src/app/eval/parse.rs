//! Pure parsing — `KeyEvent` → `Token`, `&[Token]` → `Expr`.
//!
//! Two stages live here:
//!
//! 1. [`tokenize`] resolves a single `KeyEvent` to an `Option<Token>` in
//!    the current parse context, looking at the trailing tokens to decide
//!    whether the next key is a count, an operator's argument, a text
//!    object follower, etc.
//! 2. [`classify`] inspects the running token list and decides if it's a
//!    completed command ([`Parse::Complete`]), a valid prefix that should
//!    keep accumulating ([`Parse::Incomplete`]), or junk to drop
//!    ([`Parse::Invalid`]).
//!
//! Both are free functions of the token slice + `Keymap` — no `App`
//! borrow, no side effects. The evaluator in `super` consumes the
//! `Expr` they produce.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{DirectKind, Expr, MotionExpr, MotionKind, Operator, Target, Token};
use crate::config::{
    CTRL_W_BINDINGS, GOTO_BINDINGS, KeySig, Keymap, OBJECT_BINDINGS, OP_PENDING_BINDINGS,
    WINDOW_BINDINGS, Z_BINDINGS,
};
use crate::mode::Mode;

/// Result of [`classify`].
#[derive(Debug)]
pub(in crate::app) enum Parse {
    Complete(Expr),
    Incomplete,
    Invalid,
}

/// Tokenization context — what the parser is "expecting" next, derived
/// from the trailing tokens of the current command.
#[derive(Debug, Clone, Copy)]
enum ParseCtx {
    /// Top of a fresh command, or right after one or more Count tokens.
    Initial,
    /// Right after `<space>` — looking for a leader-bound action.
    LeaderPending,
    /// Right after an operator (or `<count><op>`). Now expecting
    /// a motion, a Scope marker, a Count, or the operator key itself
    /// again for the SelfDouble shortcut.
    OpPending,
    /// Right after a Scope marker (`i` / `a`). Expecting an object.
    ObjectExpected,
    /// Right after `g`. Expecting the second `g` for goto-file-start.
    GotoPending,
    /// Right after `f`/`F`/`t`/`T` (or `r`). Expecting the literal
    /// target/replacement character — the next key (whatever it is)
    /// becomes the argument. The emitted token depends on which
    /// prefix is on the stack (see [`char_arg_token`]).
    CharArgPending,
    /// Right after `z`. Expecting one of `z`/`t`/`b` for the viewport
    /// scroll-to family.
    ZPending,
    /// Right after `<space>w` (the window sub-leader). Expecting one
    /// of the keys in `WINDOW_BINDINGS` (split / focus / close /
    /// cycle).
    WindowPending,
    /// Right after `Ctrl-W`. Expecting one of the keys in
    /// `CTRL_W_BINDINGS` (vim's window-prefix chord — h/j/k/l move
    /// focus, v / s split, c close, w cycle).
    CtrlWPending,
}

/// Decide which tokenization context the next key falls into by looking
/// at the trailing tokens. Pure function of the token slice.
fn context_of(prev: &[Token]) -> ParseCtx {
    use Token::*;
    // Skip trailing Counts when deciding context — counts don't change
    // what kind of token is expected next, only the magnitude.
    let mut last: Option<&Token> = None;
    for t in prev.iter().rev() {
        if !matches!(t, Count(_)) {
            last = Some(t);
            break;
        }
    }
    match last {
        None => ParseCtx::Initial,
        Some(LeaderPrefix) => ParseCtx::LeaderPending,
        Some(Op(_)) => ParseCtx::OpPending,
        Some(Scope(_)) => ParseCtx::ObjectExpected,
        Some(GotoPrefix) => ParseCtx::GotoPending,
        Some(FindCharPrefix { .. } | ReplaceCharPrefix) => ParseCtx::CharArgPending,
        Some(ZPrefix) => ParseCtx::ZPending,
        Some(WindowPrefix) => ParseCtx::WindowPending,
        Some(CtrlWPrefix) => ParseCtx::CtrlWPending,
        // After Motion/Direct/Object/SelfDouble the command is already
        // Complete; we shouldn't be tokenizing in those contexts.
        _ => ParseCtx::Initial,
    }
}

/// Resolve a key to its token in the current parse context.
///
/// Returns `None` when the key has no meaning in the current context —
/// the caller should treat this as a parse abort (clear the token
/// list). Only called for Normal mode.
pub(in crate::app) fn tokenize(
    km: &Keymap,
    prev: &[Token],
    mode: Mode,
    key: KeyEvent,
) -> Option<Token> {
    debug_assert_eq!(mode, Mode::Normal);

    // Ctrl-r is redo (vim convention). Works in any context.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
        return Some(Token::Direct(DirectKind::Redo));
    }
    // Ctrl-w opens the window-prefix sub-grammar; the next key
    // resolves through `CTRL_W_BINDINGS`. Only fires at the start of a
    // fresh command — using `<C-w>` mid-sequence would clobber a
    // pending operator/scope state.
    if prev.is_empty()
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('w')
    {
        return Some(Token::CtrlWPrefix);
    }

    let ctx = context_of(prev);
    let code = key.code;

    // Digit handling stays special: count parsing is a parser
    // primitive, not a user-rebindable shortcut.
    if let Some(c) = ascii_digit(code) {
        let already_counting = matches!(prev.last(), Some(Token::Count(_)));
        let d = c.to_digit(10).unwrap();
        return match (ctx, c, already_counting) {
            // 0 alone in Initial is the line-start motion, not a count.
            (ParseCtx::Initial, '0', false) => Some(Token::Motion(MotionKind::LineStart)),
            // 0 inside a running count extends it.
            (_, '0', true) => Some(Token::Count(0)),
            // 1-9 always starts/extends a count (Initial or OpPending).
            (ParseCtx::Initial | ParseCtx::OpPending, '1'..='9', _) => Some(Token::Count(d)),
            // In LeaderPending / ObjectExpected, digits don't make sense.
            _ => None,
        };
    }

    let sig = KeySig::from_event(key);
    match ctx {
        ParseCtx::Initial => km.initial.get(&sig).copied(),
        ParseCtx::LeaderPending => km.leader.get(&sig).copied(),
        ParseCtx::OpPending => op_pending_token(code, prev),
        ParseCtx::ObjectExpected => object_token(code),
        ParseCtx::GotoPending => goto_pending_token(code),
        ParseCtx::CharArgPending => char_arg_token(code, prev),
        ParseCtx::ZPending => z_pending_token(code),
        ParseCtx::WindowPending => window_pending_token(code),
        ParseCtx::CtrlWPending => ctrl_w_pending_token(code),
    }
}

fn window_pending_token(code: crossterm::event::KeyCode) -> Option<Token> {
    WINDOW_BINDINGS
        .iter()
        .find(|b| b.matches(code))
        .map(|b| b.token)
}

fn ctrl_w_pending_token(code: crossterm::event::KeyCode) -> Option<Token> {
    CTRL_W_BINDINGS
        .iter()
        .find(|b| b.matches(code))
        .map(|b| b.token)
}

/// In CharArgPending, any printable character becomes the literal
/// argument. The output token depends on the most recent pending
/// prefix — `f`/`F`/`t`/`T` produce a `FindChar` motion, `r`
/// produces a `ReplaceChar` direct.
fn char_arg_token(code: KeyCode, prev: &[Token]) -> Option<Token> {
    let prefix = prev.iter().rev().find(|t| {
        matches!(
            t,
            Token::FindCharPrefix { .. } | Token::ReplaceCharPrefix
        )
    })?;
    let KeyCode::Char(ch) = code else {
        // Escape/arrow/etc abort the pending arg — return None so the
        // caller clears the token stack.
        return None;
    };
    match prefix {
        Token::FindCharPrefix { forward, till } => {
            Some(Token::Motion(MotionKind::FindChar {
                ch,
                forward: *forward,
                till: *till,
            }))
        }
        Token::ReplaceCharPrefix => Some(Token::Direct(DirectKind::ReplaceChar { ch })),
        _ => None,
    }
}

fn z_pending_token(code: KeyCode) -> Option<Token> {
    Z_BINDINGS
        .iter()
        .find(|b| b.matches(code))
        .map(|b| b.token)
}

fn goto_pending_token(code: KeyCode) -> Option<Token> {
    GOTO_BINDINGS
        .iter()
        .find(|b| b.matches(code))
        .map(|b| b.token)
}

fn op_pending_token(code: KeyCode, prev: &[Token]) -> Option<Token> {
    // The most recent Op token is the one we're following.
    let pending_op = prev.iter().rev().find_map(|t| match t {
        Token::Op(o) => Some(*o),
        _ => None,
    })?;

    // Operator key pressed again: SelfDouble (dd, yy, cc). Stays inline
    // because the matching key is determined by the active operator
    // rather than by a static table.
    let same_key = matches!(
        (pending_op, code),
        (Operator::Delete, KeyCode::Char('d'))
            | (Operator::Yank, KeyCode::Char('y'))
            | (Operator::Change, KeyCode::Char('c'))
            | (Operator::Indent, KeyCode::Char('>'))
            | (Operator::Dedent, KeyCode::Char('<'))
    );
    if same_key {
        return Some(Token::SelfDouble(pending_op));
    }

    OP_PENDING_BINDINGS
        .iter()
        .find(|b| b.matches(code))
        .map(|b| b.token)
}

fn ascii_digit(code: KeyCode) -> Option<char> {
    match code {
        KeyCode::Char(c) if c.is_ascii_digit() => Some(c),
        _ => None,
    }
}

fn object_token(code: KeyCode) -> Option<Token> {
    OBJECT_BINDINGS
        .iter()
        .find(|b| b.matches(code))
        .map(|b| b.token)
}

// ────────────────────────────────────────────────────────────────────────
// Count helpers
// ────────────────────────────────────────────────────────────────────────

/// Peel leading `Count(_)` tokens off the slice and combine them into one
/// number (with `1` as default when none are present).
fn take_count(tokens: &[Token]) -> (u32, &[Token]) {
    let mut count: u32 = 0;
    let mut i = 0;
    while let Some(Token::Count(d)) = tokens.get(i) {
        count = count.saturating_mul(10).saturating_add(*d);
        i += 1;
    }
    if i == 0 {
        (1, tokens)
    } else {
        (count.max(1), &tokens[i..])
    }
}

// ────────────────────────────────────────────────────────────────────────
// classify + build_expr
// ────────────────────────────────────────────────────────────────────────

/// Try to interpret the current token list. Returns Complete with the
/// resulting Expr when the list is a finished command, Incomplete when
/// it's a valid prefix of one, or Invalid otherwise.
pub(in crate::app) fn classify(tokens: &[Token]) -> Parse {
    if let Some(expr) = build_expr(tokens) {
        return Parse::Complete(expr);
    }
    if is_valid_prefix(tokens) {
        return Parse::Incomplete;
    }
    Parse::Invalid
}

fn build_expr(tokens: &[Token]) -> Option<Expr> {
    use Token::*;
    let (outer_count, rest) = take_count(tokens);

    match rest {
        // Direct standalone — count usually meaningless, kept for parity.
        [Direct(d)] => Some(Expr::Direct {
            kind: *d,
            count: outer_count,
        }),

        // Motion alone or with leading count (already captured).
        [Motion(m)] => Some(Expr::Motion(MotionExpr {
            motion: *m,
            count: outer_count,
        })),

        // `f<c>` / `t<c>` / etc — the prefix is purely a parser
        // shaping token and disappears at the AST level.
        [FindCharPrefix { .. }, Motion(m)] => Some(Expr::Motion(MotionExpr {
            motion: *m,
            count: outer_count,
        })),

        // Leader-style: <space>f, <space>l
        [LeaderPrefix, Direct(d)] => Some(Expr::Direct {
            kind: *d,
            count: outer_count,
        }),

        // Window sub-leader: <space>w v, <space>w h, <space>w <arrow>, ...
        [LeaderPrefix, WindowPrefix, Direct(d)] => Some(Expr::Direct {
            kind: *d,
            count: outer_count,
        }),

        // Vim window-prefix chord: <C-w>h, <C-w>v, <C-w>w, ...
        [CtrlWPrefix, Direct(d)] => Some(Expr::Direct {
            kind: *d,
            count: outer_count,
        }),

        // gg → file start (with optional count: 5gg = goto line 5)
        [GotoPrefix, GotoPrefix] => Some(Expr::Motion(MotionExpr {
            motion: MotionKind::FileStart,
            count: outer_count,
        })),

        // gd / gr — goto-prefix followed by an LSP action
        [GotoPrefix, Direct(d)] => Some(Expr::Direct {
            kind: *d,
            count: outer_count,
        }),

        // g_ / ge / gE / gs / gl / gc / gb — goto-prefix followed by
        // a motion. Drops the prefix at the AST level.
        [GotoPrefix, Motion(m)] => Some(Expr::Motion(MotionExpr {
            motion: *m,
            count: outer_count,
        })),

        // zz / zt / zb — z-prefix followed by a viewport direct.
        [ZPrefix, Direct(d)] => Some(Expr::Direct {
            kind: *d,
            count: outer_count,
        }),

        // `r<c>` — the prefix is purely a parser shaping token; the
        // emitted `ReplaceChar` direct carries the typed character.
        [ReplaceCharPrefix, Direct(d)] => Some(Expr::Direct {
            kind: *d,
            count: outer_count,
        }),

        // Operator + something
        [Op(op), inner @ ..] => build_op_expr(*op, inner, outer_count),

        _ => None,
    }
}

fn build_op_expr(op: Operator, after_op: &[Token], outer_count: u32) -> Option<Expr> {
    use Token::*;
    let (motion_count, body) = take_count(after_op);

    match body {
        // dd / yy / cc
        [SelfDouble(_)] => Some(Expr::Op {
            op,
            target: Target::LineWise,
            outer_count: outer_count.saturating_mul(motion_count),
        }),

        // dw / 3dw / d3w / 3d2w — motion-based
        [Motion(m)] => Some(Expr::Op {
            op,
            target: Target::Motion(MotionExpr {
                motion: *m,
                count: motion_count,
            }),
            outer_count,
        }),

        // `df<c>` / `2dt<c>` — operator followed by a char-find motion.
        // The FindCharPrefix is a parser shaping token and is dropped
        // from the AST.
        [FindCharPrefix { .. }, Motion(m)] => Some(Expr::Op {
            op,
            target: Target::Motion(MotionExpr {
                motion: *m,
                count: motion_count,
            }),
            outer_count,
        }),

        // `dg_` / `dge` / etc — operator followed by a `g`-prefixed
        // motion. Same parser-shaping treatment as the find-char case.
        [GotoPrefix, Motion(m)] => Some(Expr::Op {
            op,
            target: Target::Motion(MotionExpr {
                motion: *m,
                count: motion_count,
            }),
            outer_count,
        }),

        // `cgn` / `dgn` / `ygn` (and the `gN` variants) — operator
        // followed by the gn target. Doesn't fit `Target::Motion`
        // because the range starts at the match (not the cursor); use
        // the dedicated `SearchMatch` target.
        [GotoPrefix, Direct(DirectKind::SearchSelectNext { reverse })] => Some(Expr::Op {
            op,
            target: Target::SearchMatch { reverse: *reverse },
            outer_count: outer_count.saturating_mul(motion_count),
        }),

        // dib / di" — text objects (motion_count must be 1; multi-count
        // on a text object isn't supported yet)
        [Scope(s), Object(o)] if motion_count == 1 => Some(Expr::Op {
            op,
            target: Target::TextObject {
                scope: *s,
                object: *o,
            },
            outer_count,
        }),

        _ => None,
    }
}

/// True if the token slice is the prefix of some buildable command.
/// Used to decide between Incomplete (keep accumulating) and Invalid
/// (clear and beep).
fn is_valid_prefix(tokens: &[Token]) -> bool {
    use Token::*;
    // Strip leading counts — they're transparent to validity.
    let (_, rest) = take_count(tokens);
    match rest {
        [] => true,                                  // just counts so far
        [LeaderPrefix] => true,                      // <space> waiting for follower
        [LeaderPrefix, WindowPrefix] => true,        // <space>w waiting for v/h/c/o/arrow
        [CtrlWPrefix] => true,                       // <C-w> waiting for follower
        [GotoPrefix] => true,                        // g waiting for the second g
        [ZPrefix] => true,                           // z waiting for z/t/b
        [FindCharPrefix { .. }] => true,             // f/F/t/T waiting for the literal char
        [ReplaceCharPrefix] => true,                 // r waiting for the replacement
        [Op(_)] => true,                             // d / y / c waiting
        [Op(_), Scope(_)] => true,                   // di waiting for an object
        [Op(_), FindCharPrefix { .. }] => true,      // df / dt waiting for the char
        [Op(_), GotoPrefix] => true,                 // dg waiting for the follower
        [Op(_), Count(_), ..] => {
            // After Op + inner counts the only continuations we can
            // still extend are Scope (heading for a text object) and
            // FindCharPrefix (heading for an `f<c>` style target).
            let after_op = &rest[1..];
            let (_, after_inner_count) = take_count(after_op);
            matches!(
                after_inner_count,
                [] | [Scope(_)] | [FindCharPrefix { .. }]
            )
        }
        _ => false,
    }
}
