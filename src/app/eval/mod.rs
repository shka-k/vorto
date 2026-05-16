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

use super::{App, InsertRecording, Toast};
use crate::action::{Ctx, DirectKind, Expr, InsertKey, LastChange, MotionExpr, MotionKind};
use crate::config::CommandBind;
use crate::editor::Cursor;
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
                self.push_toast(Toast::error(format!("unknown command: {}", head)));
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
        let cmds = if should_fan_out(&expr) && !self.buffer.extra_cursors.is_empty() {
            // Multi-cursor fan-out. Buffer-modifying exprs take one
            // shared snapshot up front so a single `u` undoes the
            // whole batch; pure motions skip the snapshot (they
            // wouldn't go on the undo stack in the single-cursor
            // path either). The fan-out then applies the expr at
            // every cursor with diff-based bookkeeping.
            if modifies {
                self.buffer.snapshot();
            }
            self.fan_out_op(expr, ctx)
        } else {
            self.handle_expr(expr, ctx)
        };
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
            self.push_toast(Toast::error("nothing to repeat".to_string()));
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
                let indent = self.indent_settings();
                for k in keys {
                    match k {
                        InsertKey::Char(c) => self.buffer.insert_char_smart(c, indent),
                        InsertKey::Newline => self.buffer.insert_newline(indent),
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

// ────────────────────────────────────────────────────────────────────────
// Helpers shared by `handle` and `runtime`.
// ────────────────────────────────────────────────────────────────────────

/// Human-readable list of dirty sleeping buffers for the `:q` refusal
/// message. Trims long lists with "+N more" so the status bar stays
/// readable.
pub(super) fn format_dirty_list(refs: &[&crate::buffer_ref::BufferRef]) -> String {
    const SHOW: usize = 3;
    let names: Vec<String> = refs
        .iter()
        .take(SHOW)
        .map(|r| match r {
            crate::buffer_ref::BufferRef::Scratch => "[scratch]".to_string(),
            crate::buffer_ref::BufferRef::File(p) => p
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

/// True for expressions that should be applied at every cursor in
/// multi-cursor mode. Covers:
///
/// - `c` / `d` operators against any target (motion, text-object,
///   line-wise, search-match);
/// - Pure motions (`j` / `w` / `$` / `f<c>` / etc.) — each cursor
///   moves independently. Search-jumping motions are excluded
///   because they emit `Cmd::JumpSearch` whose buffer mutation
///   happens at runtime time, after the fan-out loop has already
///   captured cursor positions;
/// - Single-cursor-natural Directs that touch one character or the
///   current line (`x`, `r`, `~`, `s`, `S`, `D`, `C`);
/// - Line-count-changing edits (`o`, `O`, `J`, paste, `Y`) — handled
///   by the row-delta branch of `adjust_already_processed`.
///
/// Yank is intentionally left in (each cursor's yank still overwrites
/// the shared register, so the last-processed wins — same as the
/// current single-cursor `y` overwriting from earlier yanks). Undo /
/// redo / mode-switches / search are primary-only.
fn should_fan_out(expr: &Expr) -> bool {
    use DirectKind as D;
    use MotionKind as M;
    match expr {
        Expr::Op { .. } => true,
        Expr::Motion(m) => !matches!(
            m.motion,
            M::SearchNext | M::SearchPrev | M::SearchWordForward | M::SearchWordBack
        ),
        Expr::Direct { kind, .. } => matches!(
            kind,
            D::DeleteCharUnderCursor
                | D::ReplaceChar { .. }
                | D::ToggleCase
                | D::SubstituteChar
                | D::SubstituteLine
                | D::DeleteToEol
                | D::ChangeToEol
                | D::OpenLineBelow
                | D::OpenLineAbove
                | D::Paste
                | D::JoinLines
                | D::YankLine
        ),
    }
}

impl App {
    /// Run `expr` at the primary cursor and at every extra cursor,
    /// keeping a single shared undo entry. Cursors are processed in
    /// descending `(row, col)` order so a later (lower-position) edit
    /// doesn't shift any already-processed cursor's saved position;
    /// after each edit we compare the buffer's line lengths and shift
    /// already-saved positions on the affected row to account for
    /// chars added/removed at lower columns.
    ///
    /// `Cmd`s produced by individual cursor runs are deduped so the
    /// runtime applies things like `EnterMode(Insert)` once.
    pub(super) fn fan_out_op(&mut self, expr: Expr, ctx: Ctx) -> Vec<Cmd> {
        let mut all: Vec<(usize, Cursor)> = std::iter::once((0usize, self.buffer.cursor))
            .chain(
                self.buffer
                    .extra_cursors
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (i + 1, *c)),
            )
            .collect();
        all.sort_by_key(|(_, c)| std::cmp::Reverse((c.row, c.col)));

        let mut new_positions = vec![Cursor::default(); all.len()];
        let mut cmds: Vec<Cmd> = Vec::new();
        let mut seen_mode = false;
        let mut seen_status = false;
        let mut seen_last_find = false;

        for i in 0..all.len() {
            let (orig_idx, pos) = all[i];
            self.buffer.cursor = pos;
            let before = line_chars(self);
            let new_cmds = self.handle_expr_no_snapshot(expr.clone(), ctx);
            let after = line_chars(self);
            new_positions[orig_idx] = self.buffer.cursor;
            adjust_already_processed(
                &mut new_positions,
                &all[..i],
                &before,
                &after,
                pos,
            );
            for cmd in new_cmds {
                match &cmd {
                    Cmd::EnterMode(_) if seen_mode => continue,
                    Cmd::EnterMode(_) => seen_mode = true,
                    Cmd::ToastInfo(_) | Cmd::ToastError(_) if seen_status => continue,
                    Cmd::ToastInfo(_) | Cmd::ToastError(_) => seen_status = true,
                    Cmd::SetLastFind(_) if seen_last_find => continue,
                    Cmd::SetLastFind(_) => seen_last_find = true,
                    _ => {}
                }
                cmds.push(cmd);
            }
        }

        // Write back: primary = positions[0], extras = positions[1..],
        // deduping coincident positions.
        self.buffer.cursor = new_positions[0];
        let primary = new_positions[0];
        let mut extras: Vec<Cursor> = Vec::with_capacity(new_positions.len() - 1);
        for c in new_positions.into_iter().skip(1) {
            if c == primary || extras.contains(&c) {
                continue;
            }
            extras.push(c);
        }
        self.buffer.extra_cursors = extras;
        cmds
    }
}

/// Per-row char-count snapshot, used to spot what a single fan-out
/// step did to the buffer.
fn line_chars(app: &App) -> Vec<usize> {
    app.buffer
        .lines
        .iter()
        .map(|l| l.chars().count())
        .collect()
}

/// Apply the buffer diff between `before` and `after` to the cursors
/// already in `new_positions` for the indices listed in `already`.
///
/// Two regimes:
///
/// 1. **Same row count.** Per-row delta tells us how many chars the
///    edit added or removed on each row. For each row with a non-zero
///    delta we shift the cols of all already-processed cursors on
///    that row that sit at or past `edit_origin.col` — anything to
///    the left of a forward edit is untouched.
///
/// 2. **Row count changed.** We locate the first row where the line
///    lengths diverge and treat it as the edit row. Cursors strictly
///    past that row are shifted by the row delta. Cursors on the
///    edit row are left alone — that's correct for `dd` / `J` (the
///    row was either removed or merged with the next, and same-row
///    edits don't cleanly translate). Final row indices are clamped
///    to the new buffer bounds.
fn adjust_already_processed(
    new_positions: &mut [Cursor],
    already: &[(usize, Cursor)],
    before: &[usize],
    after: &[usize],
    edit_origin: Cursor,
) {
    if before.len() == after.len() {
        for (row, (b, a)) in before.iter().zip(after.iter()).enumerate() {
            if a == b {
                continue;
            }
            let delta = *a as i64 - *b as i64;
            for (orig_idx, _) in already {
                let p = &mut new_positions[*orig_idx];
                if p.row != row {
                    continue;
                }
                if p.col < edit_origin.col {
                    continue;
                }
                let new_col = p.col as i64 + delta;
                p.col = new_col.max(edit_origin.col as i64) as usize;
            }
        }
    } else {
        // Row count changed — find where and shift the tail.
        let edit_row = first_diverging_row(before, after);
        let row_delta = after.len() as i64 - before.len() as i64;
        for (orig_idx, _) in already {
            let p = &mut new_positions[*orig_idx];
            if p.row > edit_row {
                let new_row = (p.row as i64 + row_delta).max(0) as usize;
                p.row = new_row;
            }
        }
    }
    // Unconditional final clamp. Covers the case where a row that an
    // already-processed cursor sat on was removed entirely — e.g.
    // multiple `dd`s descending through the buffer leave saved
    // positions whose `row` index no longer exists. Without this, the
    // first render after the operation panics on an out-of-range line
    // lookup.
    let last_row = after.len().saturating_sub(1);
    for (orig_idx, _) in already {
        let p = &mut new_positions[*orig_idx];
        if p.row > last_row {
            p.row = last_row;
        }
        let line_len = after.get(p.row).copied().unwrap_or(0);
        if p.col > line_len {
            p.col = line_len;
        }
    }
}

/// First row index where `before` and `after` line-length vectors
/// differ. When one is a strict prefix of the other, returns the
/// length of the shorter side — i.e. the first row that exists only
/// in the longer side, which is the edit row for pure-append edits.
fn first_diverging_row(before: &[usize], after: &[usize]) -> usize {
    for i in 0..before.len().min(after.len()) {
        if before[i] != after[i] {
            return i;
        }
    }
    before.len().min(after.len())
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
