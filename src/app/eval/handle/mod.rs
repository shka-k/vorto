//! Translate an `Expr` into buffer mutations + a list of `Cmd`s.
//!
//! Why a list of `Cmd`s instead of poking `App` directly: the old
//! `eval_direct` / `eval_motion` / `eval_op` mixed buffer edits, mode
//! transitions, LSP requests, file I/O, status messages, and search
//! bookkeeping in one function. Splitting "what happens to the buffer"
//! from "what happens elsewhere" makes both halves easier to read and
//! to test in isolation.
//!
//! Discipline:
//! - The `handle_*` functions take `&mut App` purely for ergonomic
//!   access to `app.buffer` (and a few cheap reads like
//!   `app.search.last_forward`). They MUST NOT mutate any other
//!   field of `App` — every non-buffer state change is emitted as a
//!   `Cmd` for `App::run_cmds` to apply.
//! - The buffer mutations stay inline because `Buffer` already has a
//!   clean method surface; wrapping every cursor move in a `Cmd`
//!   variant would balloon the enum without buying anything.
//!
//! Split by `Expr` variant so each file owns one dispatch:
//!
//! - [`direct`] — kitchen-sink keystrokes (mode transitions, ex-style
//!   commands, multi-cursor, windows).
//! - [`motion`] — `MotionKind` evaluation (cursor moves).
//! - [`operator`] — `d`/`y`/`c`/`>`/`<` over motion / search match /
//!   text object / line-wise.

mod direct;
mod motion;
mod operator;

use crate::action::{Ctx, Expr, LastFind, MotionKind, Operator};
use crate::app::App;
use crate::app::eval::word_under_cursor;
use crate::effect::Cmd;
use crate::mode::Mode;

impl App {
    /// Top-level entry point. Snapshot for undo if the expression
    /// modifies the buffer, then dispatch to the kind-specific
    /// handler. Callers that need to drive multiple invocations under
    /// a single undo step (multi-cursor fan-out) should use
    /// [`handle_expr_no_snapshot`] directly after taking the snapshot
    /// themselves.
    pub(super) fn handle_expr(&mut self, expr: Expr, ctx: Ctx) -> Vec<Cmd> {
        if expr_modifies_buffer(&expr) {
            self.buffer.snapshot();
        }
        self.handle_expr_no_snapshot(expr, ctx)
    }

    /// Variant of [`handle_expr`] that does not take an undo snapshot.
    /// Exposed so the multi-cursor fan-out can snapshot once for the
    /// whole batch instead of N times.
    pub(super) fn handle_expr_no_snapshot(&mut self, expr: Expr, ctx: Ctx) -> Vec<Cmd> {
        match expr {
            Expr::Direct { kind, count } => direct::handle_direct(self, kind, count, ctx),
            Expr::Motion(m) => motion::handle_motion(self, m),
            Expr::Op {
                op,
                target,
                outer_count,
            } => operator::handle_op(self, op, target, outer_count),
        }
    }

    /// Buffer-only goto-line implementation. Pulled up to `App` so
    /// both `motion::handle_motion` (`gg`/`G` with a count) and
    /// `direct::handle_direct` (`:goto N`) can share it.
    fn goto_line_n_pure(&mut self, n: usize) {
        let last = self.buffer.lines.len().saturating_sub(1);
        self.buffer.cursor.row = n.saturating_sub(1).min(last);
        self.buffer.cursor.col = 0;
        self.buffer.clamp_col(false);
    }
}

// ────────────────────────────────────────────────────────────────────────
// Shared helpers (private to handle::*; visible to descendants)
// ────────────────────────────────────────────────────────────────────────

/// Pure version of the old `App::resolve_find_motion`. Returns the
/// motion to actually evaluate (`None` when `;`/`,` was pressed with
/// no prior find) plus any update to `last_find` that the caller
/// should apply via `Cmd::SetLastFind`.
fn resolve_motion_pure(
    motion: MotionKind,
    last_find: Option<LastFind>,
) -> (Option<MotionKind>, Option<LastFind>) {
    use MotionKind as M;
    match motion {
        M::RepeatFind { reverse } => {
            let Some(lf) = last_find else {
                return (None, None);
            };
            let forward = if reverse { !lf.forward } else { lf.forward };
            (
                Some(M::FindChar {
                    ch: lf.ch,
                    forward,
                    till: lf.till,
                }),
                None,
            )
        }
        M::FindChar { ch, forward, till } => (Some(motion), Some(LastFind { ch, forward, till })),
        _ => (Some(motion), None),
    }
}

/// Pull the word at the cursor, push the matching `SetSearch` (and,
/// when `jump`, a `JumpSearch`). Factored out so `*` / `#` (which
/// jump) and `g*` / `g#` (which only seed the pattern) share the
/// extraction path.
fn push_word_search(app: &App, cmds: &mut Vec<Cmd>, forward: bool, jump: bool) {
    match word_under_cursor(&app.buffer) {
        Some(word) => {
            cmds.push(Cmd::SetSearch {
                pattern: word,
                forward,
            });
            if jump {
                // SetSearch above just set `last_forward` to `forward`,
                // so jumping with `reverse: false` follows that direction.
                cmds.push(Cmd::JumpSearch { reverse: false });
            }
        }
        None => cmds.push(Cmd::ToastError("no word under cursor".into())),
    }
}

/// Move the cursor to the first non-whitespace column on its current
/// row, falling back to col 0 on an all-blank line. Used after
/// indent/dedent operators to match vim's landing position.
fn cursor_to_first_non_blank(buf: &mut crate::editor::Buffer) {
    let line = buf.current_line();
    let col = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);
    buf.cursor.col = col;
}

pub(super) fn expr_modifies_buffer(expr: &Expr) -> bool {
    use crate::action::DirectKind as D;
    match expr {
        Expr::Direct { kind, .. } => matches!(
            kind,
            D::OpenLineBelow
                | D::OpenLineAbove
                | D::Paste
                | D::DeleteCharUnderCursor
                | D::EnterMode(Mode::Insert)
                | D::AppendAfterCursor
                | D::AppendAtLineEnd
                | D::InsertAtLineStart
                | D::ChangeToEol
                | D::DeleteToEol
                | D::JoinLines
                | D::ToggleCase
                | D::SubstituteChar
                | D::SubstituteLine
                | D::ReplaceChar { .. }
                | D::ToggleComment
                | D::Substitute
        ),
        Expr::Motion(_) => false,
        Expr::Op { op, .. } => !matches!(op, Operator::Yank),
    }
}
