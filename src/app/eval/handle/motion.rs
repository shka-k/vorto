//! `Expr::Motion` evaluation — pure cursor moves driven by `MotionKind`.
//! Inclusive-vs-exclusive distinctions matter for operators, not for
//! standalone motions; the inclusivity bit only surfaces in
//! [`super::operator::handle_op`].

use super::{push_word_search, resolve_motion_pure};
use crate::action::{MotionExpr, MotionKind};
use crate::app::App;
use crate::effect::Cmd;
use crate::mode::Mode;

pub(super) fn handle_motion(app: &mut App, m: MotionExpr) -> Vec<Cmd> {
    use MotionKind as M;
    let mut cmds = Vec::new();
    let allow_after = matches!(app.mode, Mode::Insert);
    let n = m.count;

    let (resolved, last_find_update) = resolve_motion_pure(m.motion, app.last_find);
    if let Some(lf) = last_find_update {
        cmds.push(Cmd::SetLastFind(lf));
    }
    let Some(resolved) = resolved else {
        cmds.push(Cmd::ToastError("no previous find".into()));
        return cmds;
    };

    match resolved {
        M::Left => {
            for _ in 0..n {
                app.buffer.move_left();
            }
        }
        M::Right => {
            for _ in 0..n {
                app.buffer.move_right(allow_after);
            }
        }
        M::Up => {
            for _ in 0..n {
                app.buffer.move_up();
            }
        }
        M::Down => {
            for _ in 0..n {
                app.buffer.move_down();
            }
        }
        M::LineStart => app.buffer.move_line_start(),
        M::LineEnd => app.buffer.move_line_end(),
        // `*` / `#` extract the word under the cursor (buffer
        // read), then ask the runtime to seed the search state
        // and jump. The buffer mutation for the cursor jump
        // happens during `JumpSearch` because it depends on the
        // updated search pattern.
        M::SearchWordForward => push_word_search(app, &mut cmds, true, true),
        M::SearchWordBack => push_word_search(app, &mut cmds, false, true),
        M::WordForward => {
            for _ in 0..n {
                app.buffer.move_word_forward();
            }
        }
        M::WordBack => {
            for _ in 0..n {
                app.buffer.move_word_backward();
            }
        }
        // Pure motions: ask the buffer for the target and assign.
        M::WordEnd
        | M::BigWordForward
        | M::BigWordBack
        | M::BigWordEnd
        | M::WordEndBack
        | M::BigWordEndBack
        | M::LineFirstNonBlank
        | M::LineLastNonBlank
        | M::BracketMatch
        | M::FindChar { .. }
        | M::ViewportTop
        | M::ViewportMiddle
        | M::ViewportBottom
        | M::HalfPageDown
        | M::HalfPageUp
        | M::PageDown
        | M::PageUp => {
            let target = app.buffer.motion_target(app.buffer.cursor, resolved, n);
            app.buffer.cursor = target;
        }
        // Resolved away by `resolve_motion_pure` — should never
        // reach the match arm.
        M::RepeatFind { .. } => {}
        // `gg` with no count goes to line 1; `5gg` to line 5.
        M::FileStart => {
            if n > 1 {
                app.goto_line_n_pure(n as usize);
            } else {
                app.buffer.move_file_start();
            }
        }
        // `G` with no count goes to file end; `20G` to line 20.
        M::FileEnd => {
            if n > 1 {
                app.goto_line_n_pure(n as usize);
            } else {
                app.buffer.move_file_end();
            }
        }
        M::SearchNext => {
            for _ in 0..n {
                cmds.push(Cmd::JumpSearch { reverse: false });
            }
        }
        M::SearchPrev => {
            for _ in 0..n {
                cmds.push(Cmd::JumpSearch { reverse: true });
            }
        }
        M::ParagraphForward => {
            for _ in 0..n {
                app.buffer.move_paragraph_forward();
            }
        }
        M::ParagraphBack => {
            for _ in 0..n {
                app.buffer.move_paragraph_backward();
            }
        }
    }
    cmds
}
