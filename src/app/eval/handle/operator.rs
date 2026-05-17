//! `Expr::Op` evaluation — apply an operator (`d`/`y`/`c`/`>`/`<`) over a
//! target derived from a motion, a search match, a text object, or the
//! line-wise repeat (`dd`/`yy`/…). The shared [`apply_op_range`]
//! collapses motion + text-object + search-match dispatch onto one
//! range-based primitive.

use super::{cursor_to_first_non_blank, resolve_motion_pure};
use crate::action::{Operator, Target};
use crate::app::App;
use crate::app::eval::is_inclusive_motion;
use crate::editor::Cursor;
use crate::effect::Cmd;
use crate::mode::Mode;

pub(super) fn handle_op(app: &mut App, op: Operator, target: Target, outer_count: u32) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    match target {
        Target::LineWise => {
            if matches!(op, Operator::Indent | Operator::Dedent) {
                let indent = app.indent_settings();
                let start_row = app.buffer.cursor.row;
                let last = app.buffer.lines.len().saturating_sub(1);
                let span = outer_count.max(1) as usize - 1;
                let end_row = start_row.saturating_add(span).min(last);
                for r in start_row..=end_row {
                    if matches!(op, Operator::Indent) {
                        app.buffer.indent_line(r, indent);
                    } else {
                        app.buffer.dedent_line(r, indent);
                    }
                }
                app.buffer.cursor.row = start_row;
                cursor_to_first_non_blank(&mut app.buffer);
            } else {
                for _ in 0..outer_count {
                    match op {
                        Operator::Delete => app.buffer.delete_line(),
                        Operator::Yank => {
                            app.buffer.yank_line();
                            cmds.push(Cmd::SyncYank);
                            cmds.push(Cmd::ToastInfo("yanked".into()));
                        }
                        Operator::Change => {
                            cmds.push(Cmd::ToastError("change not implemented yet".into()));
                        }
                        Operator::Indent | Operator::Dedent => unreachable!(),
                    }
                }
            }
        }
        Target::Motion(m) => {
            let (resolved, last_find_update) = resolve_motion_pure(m.motion, app.last_find);
            if let Some(lf) = last_find_update {
                cmds.push(Cmd::SetLastFind(lf));
            }
            let Some(resolved) = resolved else {
                cmds.push(Cmd::ToastError("no previous find".into()));
                return cmds;
            };
            let inclusive = is_inclusive_motion(resolved);
            for _ in 0..outer_count {
                let start = app.buffer.cursor;
                let target = app.buffer.motion_target(start, resolved, m.count);
                // Vim's inclusive motions (`e`, `f<c>`, `t<c>`, …)
                // include the landing char in the operator range;
                // `apply_op_range` takes an exclusive end, so push
                // one past for these.
                let end = if inclusive {
                    app.buffer.advance_one(target)
                } else {
                    target
                };
                apply_op_range(app, op, start, end, &mut cmds);
            }
        }
        Target::SearchMatch { reverse } => {
            // The match range starts at the pattern hit, not at
            // the cursor — that's the whole point of having a
            // dedicated target. We read `app.search` and apply the
            // op to each match found in sequence; `outer_count > 1`
            // walks forward through successive matches (e.g. `2dgn`).
            let forward = app.search.last_forward ^ reverse;
            for _ in 0..outer_count {
                let Some((start, end_incl)) = app.search.find_match_range(&app.buffer, forward)
                else {
                    cmds.push(Cmd::ToastError("pattern not found".into()));
                    break;
                };
                let end = app.buffer.advance_one(end_incl);
                apply_op_range(app, op, start, end, &mut cmds);
            }
        }
        Target::TextObject { scope, object } => {
            for _ in 0..outer_count {
                match app.buffer.text_object_range(scope, object) {
                    Some((start, end)) => apply_op_range(app, op, start, end, &mut cmds),
                    None => {
                        cmds.push(Cmd::ToastError("no matching object".into()));
                        break;
                    }
                }
            }
        }
    }
    cmds
}

/// Apply an operator over the range [start, end). Shared by
/// motion-target, search-match, and text-object dispatch.
fn apply_op_range(app: &mut App, op: Operator, start: Cursor, end: Cursor, cmds: &mut Vec<Cmd>) {
    match op {
        Operator::Delete => app.buffer.delete_range(start, end),
        Operator::Yank => {
            app.buffer.yank_range(start, end);
            cmds.push(Cmd::SyncYank);
            cmds.push(Cmd::ToastInfo("yanked".into()));
        }
        Operator::Change => {
            app.buffer.delete_range(start, end);
            cmds.push(Cmd::EnterMode(Mode::Insert));
        }
        Operator::Indent | Operator::Dedent => {
            // `>` and `<` are line-wise even with a non-line target —
            // every row spanned by the motion gets one indent step.
            let indent = app.indent_settings();
            let (lo, hi) = if (start.row, start.col) <= (end.row, end.col) {
                (start.row, end.row)
            } else {
                (end.row, start.row)
            };
            for r in lo..=hi {
                if matches!(op, Operator::Indent) {
                    app.buffer.indent_line(r, indent);
                } else {
                    app.buffer.dedent_line(r, indent);
                }
            }
            app.buffer.cursor.row = lo;
            cursor_to_first_non_blank(&mut app.buffer);
        }
    }
}
