//! Three-stage input parser:
//!
//! 1. `tokenize(prev_tokens, key, mode)` — `KeyEvent` to `Option<Token>`.
//!    Context-aware: the same key can produce different tokens depending
//!    on what came before (e.g. `i` is `EnterInsert` initially but
//!    `Scope(Inner)` after an operator).
//!
//! 2. `classify(tokens)` — `&[Token]` to `Parse`. Decide whether the
//!    accumulated token list is a complete command, a valid prefix of
//!    one, or invalid garbage.
//!
//! 3. `build_expr(tokens)` — `&[Token]` to `Expr`. Turn a Complete token
//!    list into an AST that the evaluator can walk.
//!
//! Operator + motion / text-object grammar lives entirely in these three
//! functions — no separate state machine or KeyBind table.

use crossterm::event::{KeyCode, KeyEvent};

use crate::action::{
    DirectKind, Expr, MotionExpr, MotionKind, Object, Operator, PromptKind, Scope, Target, Token,
};
use crate::fuzzy::FuzzyKind;
use crate::mode::Mode;

pub const LEADER: char = ' ';

// ────────────────────────────────────────────────────────────────────────
// Parse result
// ────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Parse {
    Complete(Expr),
    Incomplete,
    Invalid,
}

// ────────────────────────────────────────────────────────────────────────
// Tokenize
// ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum Ctx {
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
}

/// Decide which tokenization context the next key falls into by looking
/// at the trailing tokens. Pure function of the token slice.
fn context_of(prev: &[Token]) -> Ctx {
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
        None => Ctx::Initial,
        Some(LeaderPrefix) => Ctx::LeaderPending,
        Some(Op(_)) => Ctx::OpPending,
        Some(Scope(_)) => Ctx::ObjectExpected,
        Some(GotoPrefix) => Ctx::GotoPending,
        // After Motion/Direct/Object/SelfDouble the command is already
        // Complete; we shouldn't be tokenizing in those contexts.
        _ => Ctx::Initial,
    }
}

/// Resolve a key to its token in the current parse context.
///
/// Returns `None` when the key has no meaning in the current context —
/// the caller should treat this as a parse abort (clear the token list).
///
/// Only called for Normal mode (Insert/Visual handle keys directly in
/// `App::handle_key`).
pub fn tokenize(prev: &[Token], mode: Mode, key: KeyEvent) -> Option<Token> {
    debug_assert_eq!(mode, Mode::Normal);
    let ctx = context_of(prev);
    tokenize_normal(ctx, prev, key)
}

fn tokenize_normal(ctx: Ctx, prev: &[Token], key: KeyEvent) -> Option<Token> {
    // Ctrl-r is redo, vim's convention. Works in any context.
    if key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('r')
    {
        return Some(Token::Direct(DirectKind::Redo));
    }

    let code = key.code;

    // Digits: always count-extending if a Count is already in flight,
    // otherwise 1-9 starts a count and 0 is the MoveLineStart motion.
    if let KeyCode::Char(c) = code
        && c.is_ascii_digit()
    {
        let already_counting = matches!(prev.last(), Some(Token::Count(_)));
        let d = c.to_digit(10).unwrap();
        return match (ctx, c, already_counting) {
            // 0 alone in Initial is the line-start motion, not a count.
            (Ctx::Initial, '0', false) => Some(Token::Motion(MotionKind::LineStart)),
            // 0 inside a running count extends it.
            (_, '0', true) => Some(Token::Count(0)),
            // 1-9 always starts/extends a count (in Initial or OpPending).
            (Ctx::Initial | Ctx::OpPending, '1'..='9', _) => Some(Token::Count(d)),
            // In LeaderPending or ObjectExpected, digits don't make sense.
            _ => None,
        };
    }

    match ctx {
        Ctx::Initial => initial_token(code),
        Ctx::LeaderPending => leader_token(code),
        Ctx::OpPending => op_pending_token(code, prev),
        Ctx::ObjectExpected => object_token(code),
        Ctx::GotoPending => goto_pending_token(code),
    }
}

fn goto_pending_token(code: KeyCode) -> Option<Token> {
    match code {
        // gg → second g closes the sequence
        KeyCode::Char('g') => Some(Token::GotoPrefix),
        _ => None,
    }
}

fn initial_token(code: KeyCode) -> Option<Token> {
    use DirectKind as D;
    use MotionKind as M;
    use Token::*;
    let t = match code {
        // movement
        KeyCode::Char('h') | KeyCode::Left => Motion(M::Left),
        KeyCode::Char('l') | KeyCode::Right => Motion(M::Right),
        KeyCode::Char('j') | KeyCode::Down => Motion(M::Down),
        KeyCode::Char('k') | KeyCode::Up => Motion(M::Up),
        KeyCode::Char('$') | KeyCode::End => Motion(M::LineEnd),
        KeyCode::Home => Motion(M::LineStart),
        KeyCode::Char('w') => Motion(M::WordForward),
        KeyCode::Char('b') => Motion(M::WordBack),
        KeyCode::Char('G') => Motion(M::FileEnd),
        KeyCode::Char('n') => Motion(M::SearchNext),
        KeyCode::Char('N') => Motion(M::SearchPrev),

        // operators
        KeyCode::Char('d') => Op(Operator::Delete),
        KeyCode::Char('y') => Op(Operator::Yank),
        KeyCode::Char('c') => Op(Operator::Change),

        // standalone commands
        KeyCode::Char('i') => Direct(D::EnterMode(Mode::Insert)),
        KeyCode::Char('a') => Motion(M::Right), // vim's append: stub as MoveRight for now
        KeyCode::Char('v') => Direct(D::EnterMode(Mode::Visual)),
        KeyCode::Char('o') => Direct(D::OpenLineBelow),
        KeyCode::Char('O') => Direct(D::OpenLineAbove),
        KeyCode::Char('x') => Direct(D::DeleteCharUnderCursor),
        KeyCode::Char('p') => Direct(D::Paste),
        KeyCode::Char('u') => Direct(D::Undo),
        KeyCode::Char(':') => Direct(D::OpenPrompt(PromptKind::Command)),
        KeyCode::Char('/') => Direct(D::OpenPrompt(PromptKind::Search { forward: true })),
        KeyCode::Char('?') => Direct(D::OpenPrompt(PromptKind::Search { forward: false })),
        // `g` opens a 2-key sequence (gg → file start).
        KeyCode::Char('g') => GotoPrefix,

        // leader
        KeyCode::Char(LEADER) => LeaderPrefix,

        _ => return None,
    };
    Some(t)
}

fn leader_token(code: KeyCode) -> Option<Token> {
    use DirectKind as D;
    let t = match code {
        KeyCode::Char('f') => Token::Direct(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Files))),
        KeyCode::Char('l') => Token::Direct(D::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Lines))),
        _ => return None,
    };
    Some(t)
}

fn op_pending_token(code: KeyCode, prev: &[Token]) -> Option<Token> {
    use MotionKind as M;

    // The most recent Op token is the one we're following.
    let pending_op = prev.iter().rev().find_map(|t| match t {
        Token::Op(o) => Some(*o),
        _ => None,
    })?;

    // Operator key pressed again: SelfDouble (dd, yy, cc).
    let same_key = matches!(
        (pending_op, code),
        (Operator::Delete, KeyCode::Char('d'))
            | (Operator::Yank, KeyCode::Char('y'))
            | (Operator::Change, KeyCode::Char('c'))
    );
    if same_key {
        return Some(Token::SelfDouble(pending_op));
    }

    let t = match code {
        // scope markers (text objects)
        KeyCode::Char('i') => Token::Scope(Scope::Inner),
        KeyCode::Char('a') => Token::Scope(Scope::Around),

        // motions — same vocabulary as Initial motions
        KeyCode::Char('h') | KeyCode::Left => Token::Motion(M::Left),
        KeyCode::Char('l') | KeyCode::Right => Token::Motion(M::Right),
        KeyCode::Char('j') | KeyCode::Down => Token::Motion(M::Down),
        KeyCode::Char('k') | KeyCode::Up => Token::Motion(M::Up),
        KeyCode::Char('w') => Token::Motion(M::WordForward),
        KeyCode::Char('b') => Token::Motion(M::WordBack),
        KeyCode::Char('$') | KeyCode::End => Token::Motion(M::LineEnd),
        KeyCode::Char('0') | KeyCode::Home => Token::Motion(M::LineStart),
        KeyCode::Char('G') => Token::Motion(M::FileEnd),

        _ => return None,
    };
    Some(t)
}

fn object_token(code: KeyCode) -> Option<Token> {
    let o = match code {
        KeyCode::Char('w') => Object::Word,
        KeyCode::Char('"') => Object::DoubleQuote,
        KeyCode::Char('\'') => Object::SingleQuote,
        KeyCode::Char('(') | KeyCode::Char(')') | KeyCode::Char('b') => Object::Paren,
        KeyCode::Char('{') | KeyCode::Char('}') | KeyCode::Char('B') => Object::Brace,
        KeyCode::Char('[') | KeyCode::Char(']') => Object::Bracket,
        _ => return None,
    };
    Some(Token::Object(o))
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
pub fn classify(tokens: &[Token]) -> Parse {
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

        // Leader-style: <space>f, <space>l
        [LeaderPrefix, Direct(d)] => Some(Expr::Direct {
            kind: *d,
            count: outer_count,
        }),

        // gg → file start (with optional count: 5gg = goto line 5)
        [GotoPrefix, GotoPrefix] => Some(Expr::Motion(MotionExpr {
            motion: MotionKind::FileStart,
            count: outer_count,
        })),

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
        [] => true,                // just counts so far
        [LeaderPrefix] => true,    // <space> waiting for follower
        [GotoPrefix] => true,      // g waiting for the second g
        [Op(_)] => true,           // d / y / c waiting
        [Op(_), Scope(_)] => true, // di waiting for an object
        [Op(_), Count(_), ..] => {
            // After Op + inner counts, only Scope (heading for text object)
            // is a continuation we can still extend.
            let after_op = &rest[1..];
            let (_, after_inner_count) = take_count(after_op);
            matches!(after_inner_count, [] | [Scope(_)])
        }
        _ => false,
    }
}
