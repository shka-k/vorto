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

use handle::expr_modifies_buffer;
pub(super) use parse::{Parse, classify, tokenize};

use anyhow::Result;

use super::{App, InsertRecording, Status};
use crate::action::{Ctx, DirectKind, Expr, InsertKey, LastChange, MotionExpr, MotionKind};
use crate::config::CommandBind;
use crate::effect::Cmd;
use crate::mode::Mode;

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
        // `.` intercepts before normal dispatch so we don't recurse into
        // the recording path while replaying.
        if let Expr::Direct {
            kind: DirectKind::RepeatLast,
            count,
        } = expr
        {
            return self.replay_last_change(count, ctx);
        }

        let modifies = expr_modifies_buffer(&expr);
        let snapshot = if modifies { Some(expr.clone()) } else { None };
        let cmds = self.handle_expr(expr, ctx);
        let enters_insert = cmds
            .iter()
            .any(|c| matches!(c, Cmd::EnterMode(Mode::Insert)));

        if let Some(expr) = snapshot {
            if enters_insert {
                // Start a fresh Insert recording — the trigger is the
                // Expr that got us here, the keys arrive via
                // `handle_insert_key` and finalize on Esc.
                self.recording = Some(InsertRecording {
                    trigger: expr,
                    keys: Vec::new(),
                });
            } else {
                self.last_change = Some(LastChange::Expr(expr));
                self.recording = None;
            }
        }

        self.run_cmds(cmds)
    }

    /// `.` — replay the last recorded change. With a count prefix, the
    /// count overrides the recorded one (vim's behaviour for `5.`).
    fn replay_last_change(&mut self, count: u32, ctx: Ctx) -> Result<()> {
        let Some(change) = self.last_change.clone() else {
            self.status = Status::error("nothing to repeat".to_string());
            return Ok(());
        };
        match change {
            LastChange::Expr(e) => {
                let e = override_count(e, count);
                let cmds = self.handle_expr(e, ctx);
                self.run_cmds(cmds)
            }
            LastChange::Insert { trigger, keys } => {
                let trigger = override_count(trigger, count);
                let cmds = self.handle_expr(trigger, ctx);
                self.run_cmds(cmds)?;
                for k in keys {
                    match k {
                        InsertKey::Char(c) => self.buffer.insert_char(c),
                        InsertKey::Newline => self.buffer.insert_newline(),
                        InsertKey::Backspace => self.buffer.delete_char_before(),
                    }
                }
                self.enter_mode(Mode::Normal);
                Ok(())
            }
        }
    }
}

/// Replace the count carried by an `Expr` when the user supplied one
/// explicitly via `N.`. A count of 0 or 1 leaves the original count
/// alone — `.` with no prefix replays exactly what was recorded.
fn override_count(expr: Expr, count: u32) -> Expr {
    if count <= 1 {
        return expr;
    }
    match expr {
        Expr::Direct { kind, .. } => Expr::Direct { kind, count },
        Expr::Motion(m) => Expr::Motion(MotionExpr { count, ..m }),
        Expr::Op {
            op,
            target,
            outer_count: _,
        } => Expr::Op {
            op,
            target,
            outer_count: count,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Operator, Target};

    #[test]
    fn override_count_leaves_expr_alone_when_no_prefix() {
        let e = Expr::Direct {
            kind: DirectKind::DeleteCharUnderCursor,
            count: 3,
        };
        // `.` with no count prefix → recorded count survives.
        let got = override_count(e.clone(), 1);
        assert_eq!(got, e);
        let got_zero = override_count(e.clone(), 0);
        assert_eq!(got_zero, e);
    }

    #[test]
    fn override_count_replaces_each_expr_shape() {
        let direct = Expr::Direct {
            kind: DirectKind::Paste,
            count: 1,
        };
        let got = override_count(direct, 5);
        assert!(matches!(
            got,
            Expr::Direct {
                kind: DirectKind::Paste,
                count: 5
            }
        ));

        let op = Expr::Op {
            op: Operator::Delete,
            target: Target::Motion(MotionExpr {
                motion: MotionKind::WordForward,
                count: 1,
            }),
            outer_count: 1,
        };
        let got = override_count(op, 4);
        match got {
            Expr::Op { outer_count, .. } => assert_eq!(outer_count, 4),
            _ => panic!("expected Op"),
        }

        let m = Expr::Motion(MotionExpr {
            motion: MotionKind::Down,
            count: 1,
        });
        let got = override_count(m, 7);
        match got {
            Expr::Motion(mx) => assert_eq!(mx.count, 7),
            _ => panic!("expected Motion"),
        }
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
