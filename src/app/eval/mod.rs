//! Command evaluation entry point.
//!
//! Three concerns split across siblings:
//!
//! - [`parse`] — pure `KeyEvent` → `Token` → `Expr` parsing.
//! - [`handle`] — `Expr` → buffer mutations + a `Vec<Cmd>` for every
//!   non-buffer state change.
//! - `super::runtime` — applies the `Cmd`s back to `App`.
//!
//! This module ties them together: [`App::evaluate`] is the one entry
//! the input layer calls per finished command, and `App::execute_command`
//! is the `:`-prompt counterpart.

mod handle;
mod parse;

pub(super) use parse::{Parse, classify, tokenize};

use anyhow::Result;

use super::{App, Status};
use crate::action::{Ctx, DirectKind, Expr, MotionKind};
use crate::config::CommandBind;

impl App {
    pub(super) fn execute_command(&mut self, cmd: &str) -> Result<()> {
        // `:42` shortcut for `:goto 42`.
        if cmd.parse::<usize>().is_ok() {
            return self.evaluate(
                Expr::Direct {
                    kind: DirectKind::GotoLine,
                    count: 1,
                },
                Ctx::with_rest(cmd),
            );
        }

        let (head, rest) = match cmd.split_once(' ') {
            Some((h, r)) => (h, r.trim()),
            None => (cmd, ""),
        };
        if head.is_empty() {
            return Ok(());
        }
        match CommandBind::find(head) {
            Some(b) => self.evaluate(
                Expr::Direct {
                    kind: b.kind,
                    count: 1,
                },
                Ctx::with_rest(rest),
            ),
            None => {
                self.status = Status::error(format!("unknown command: {}", head));
                Ok(())
            }
        }
    }

    pub(super) fn evaluate(&mut self, expr: Expr, ctx: Ctx) -> Result<()> {
        let cmds = self.handle_expr(expr, ctx);
        self.run_cmds(cmds)
    }
}

// ────────────────────────────────────────────────────────────────────────
// Helpers shared by `handle` and `runtime`.
// ────────────────────────────────────────────────────────────────────────

/// Human-readable list of dirty sleeping buffers for the `:q` refusal
/// message. Trims long lists with "+N more" so the status bar stays
/// readable.
pub(super) fn format_dirty_list(refs: &[&super::BufferRef]) -> String {
    const SHOW: usize = 3;
    let names: Vec<String> = refs
        .iter()
        .take(SHOW)
        .map(|r| match r {
            super::BufferRef::Scratch => "[scratch]".to_string(),
            super::BufferRef::File(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
        })
        .collect();
    let mut s = names.join(", ");
    if refs.len() > SHOW {
        s.push_str(&format!(" +{} more", refs.len() - SHOW));
    }
    s
}

/// Vim's "inclusive vs exclusive" classification for motions used as
/// operator targets. Inclusive motions include their landing character
/// in the range; exclusive ones don't.
pub(super) fn is_inclusive_motion(motion: MotionKind) -> bool {
    use MotionKind as M;
    matches!(
        motion,
        M::WordEnd
            | M::BigWordEnd
            | M::WordEndBack
            | M::BigWordEndBack
            | M::FindChar { .. }
            | M::LineEnd
            | M::LineLastNonBlank
            | M::FileEnd
            | M::BracketMatch
    )
}

/// Extract the word under the cursor (char-class `Word`) as a plain
/// string. Returns `None` when the cursor is on whitespace or the
/// line is empty.
pub(super) fn word_under_cursor(buf: &crate::editor::Buffer) -> Option<String> {
    let line: Vec<char> = buf.lines[buf.cursor.row].chars().collect();
    if buf.cursor.col >= line.len() {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    if !is_word(line[buf.cursor.col]) {
        return None;
    }
    let mut lo = buf.cursor.col;
    while lo > 0 && is_word(line[lo - 1]) {
        lo -= 1;
    }
    let mut hi = buf.cursor.col;
    while hi + 1 < line.len() && is_word(line[hi + 1]) {
        hi += 1;
    }
    Some(line[lo..=hi].iter().collect())
}
