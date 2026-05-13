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
//! - The `handle_*` methods take `&mut App` purely for ergonomic
//!   access to `self.buffer` (and a few cheap reads like
//!   `self.search.last_forward`). They MUST NOT mutate any other
//!   field of `App` — every non-buffer state change is emitted as a
//!   `Cmd` for `App::run_cmds` to apply.
//! - The buffer mutations stay inline because `Buffer` already has a
//!   clean method surface; wrapping every cursor move in a `Cmd`
//!   variant would balloon the enum without buying anything.

use std::path::PathBuf;

use super::{format_dirty_list, is_inclusive_motion, word_under_cursor};
use crate::action::{
    Ctx, DirectKind, Expr, MotionExpr, MotionKind, Operator, PromptKind, Target,
};
use crate::app::{App, BufferRef, LastFind};
use crate::effect::{Cmd, ScrollAnchor};
use crate::mode::Mode;

impl App {
    /// Top-level entry point. Snapshot for undo if the expression
    /// modifies the buffer, then dispatch to the kind-specific
    /// handler.
    pub(super) fn handle_expr(&mut self, expr: Expr, ctx: Ctx) -> Vec<Cmd> {
        if expr_modifies_buffer(&expr) {
            self.buffer.snapshot();
        }
        match expr {
            Expr::Direct { kind, count } => self.handle_direct(kind, count, ctx),
            Expr::Motion(m) => self.handle_motion(m),
            Expr::Op {
                op,
                target,
                outer_count,
            } => self.handle_op(op, target, outer_count),
        }
    }

    fn handle_direct(&mut self, kind: DirectKind, count: u32, ctx: Ctx) -> Vec<Cmd> {
        use DirectKind as D;
        let mut cmds = Vec::new();
        match kind {
            D::EnterMode(m) => cmds.push(Cmd::EnterMode(m)),
            D::OpenPrompt(k) => cmds.push(Cmd::OpenPrompt(k)),
            D::OpenLineBelow => {
                self.buffer.insert_line_below();
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::OpenLineAbove => {
                self.buffer.insert_line_above();
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::AppendAfterCursor => {
                // Past-the-end is allowed in Insert, so step right with
                // that permission rather than the Normal-mode clamp.
                self.buffer.move_right(true);
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::AppendAtLineEnd => {
                self.buffer.cursor.col = self.buffer.current_line_len();
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::InsertAtLineStart => {
                let line = self.buffer.current_line();
                let col = line
                    .chars()
                    .position(|c| !c.is_whitespace())
                    .unwrap_or(0);
                self.buffer.cursor.col = col;
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::ChangeToEol => {
                self.buffer.delete_to_eol();
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::DeleteToEol => self.buffer.delete_to_eol(),
            D::YankLine => {
                for _ in 0..count {
                    self.buffer.yank_line();
                }
                cmds.push(Cmd::StatusInfo("yanked".into()));
            }
            D::JoinLines => {
                for _ in 0..count {
                    self.buffer.join_next_line();
                }
            }
            D::ToggleCase => {
                for _ in 0..count {
                    self.buffer.toggle_case_under_cursor();
                }
            }
            D::SubstituteChar => {
                for _ in 0..count {
                    self.buffer.delete_char_under_cursor();
                }
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::SubstituteLine => {
                self.buffer.clear_current_line();
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::ReplaceChar { ch } => {
                for _ in 0..count {
                    self.buffer.replace_char(ch);
                    // After each replacement, vim leaves the cursor on
                    // the replaced char; a count > 1 walks forward one
                    // step per replacement.
                    self.buffer.move_right(false);
                }
                // Final cursor: vim leaves it on the LAST replaced
                // char, not past it.
                self.buffer.move_left();
            }
            D::ViewportCenter => cmds.push(Cmd::Scroll(ScrollAnchor::Center)),
            D::ViewportTopAtCursor => cmds.push(Cmd::Scroll(ScrollAnchor::Top)),
            D::ViewportBottomAtCursor => cmds.push(Cmd::Scroll(ScrollAnchor::Bottom)),
            D::Paste => {
                for _ in 0..count {
                    self.buffer.paste_after();
                }
            }
            D::Undo => {
                if !self.buffer.undo() {
                    cmds.push(Cmd::StatusError("already at oldest change".into()));
                }
            }
            D::Redo => {
                if !self.buffer.redo() {
                    cmds.push(Cmd::StatusError("already at newest change".into()));
                }
            }
            D::DeleteCharUnderCursor => {
                for _ in 0..count {
                    self.buffer.delete_char_under_cursor();
                }
            }
            D::Quit => cmds.push(plan_quit(self)),
            D::QuitForce => cmds.push(Cmd::Quit),
            D::BufferNext => cmds.push(Cmd::BufferCycle { forward: true }),
            D::BufferPrev => cmds.push(Cmd::BufferCycle { forward: false }),
            D::BufferDelete => cmds.push(Cmd::BufferDelete { force: false }),
            D::BufferDeleteForce => cmds.push(Cmd::BufferDelete { force: true }),
            D::BufferList => {
                cmds.push(Cmd::OpenPrompt(PromptKind::Fuzzy(
                    crate::fuzzy::FuzzyKind::Buffers,
                )));
            }
            D::SaveAndQuit => cmds.push(Cmd::Save {
                path: parse_save_path(ctx.rest),
                then_quit: true,
            }),
            D::Save => cmds.push(Cmd::Save {
                path: parse_save_path(ctx.rest),
                then_quit: false,
            }),
            D::Open => {
                if ctx.rest.is_empty() {
                    cmds.push(Cmd::StatusError("missing path".into()));
                } else {
                    cmds.push(Cmd::OpenPath(PathBuf::from(ctx.rest)));
                }
            }
            D::GotoLine => match ctx.rest.parse::<usize>() {
                Ok(n) if n >= 1 => self.goto_line_n_pure(n),
                _ => cmds.push(Cmd::StatusError("usage: :goto <line>".into())),
            },
            D::GotoDefinition => cmds.push(Cmd::LspJump {
                method: "textDocument/definition",
                label: "definition",
            }),
            D::GotoDeclaration => cmds.push(Cmd::LspJump {
                method: "textDocument/declaration",
                label: "declaration",
            }),
            D::GotoImplementation => cmds.push(Cmd::LspJump {
                method: "textDocument/implementation",
                label: "implementation",
            }),
            D::FindReferences => cmds.push(Cmd::LspFindReferences),
            D::Rename => cmds.push(Cmd::OpenRenamePrompt),
        }
        cmds
    }

    fn handle_motion(&mut self, m: MotionExpr) -> Vec<Cmd> {
        use MotionKind as M;
        let mut cmds = Vec::new();
        let allow_after = matches!(self.mode, Mode::Insert);
        let n = m.count;

        let (resolved, last_find_update) = resolve_motion_pure(m.motion, self.last_find);
        if let Some(lf) = last_find_update {
            cmds.push(Cmd::SetLastFind(lf));
        }
        let Some(resolved) = resolved else {
            cmds.push(Cmd::StatusError("no previous find".into()));
            return cmds;
        };

        match resolved {
            M::Left => {
                for _ in 0..n {
                    self.buffer.move_left();
                }
            }
            M::Right => {
                for _ in 0..n {
                    self.buffer.move_right(allow_after);
                }
            }
            M::Up => {
                for _ in 0..n {
                    self.buffer.move_up();
                }
            }
            M::Down => {
                for _ in 0..n {
                    self.buffer.move_down();
                }
            }
            M::LineStart => self.buffer.move_line_start(),
            M::LineEnd => self.buffer.move_line_end(),
            // `*` / `#` extract the word under the cursor (buffer
            // read), then ask the runtime to seed the search state
            // and jump. The buffer mutation for the cursor jump
            // happens during `JumpSearch` because it depends on the
            // updated search pattern.
            M::SearchWordForward => push_word_search(self, &mut cmds, true),
            M::SearchWordBack => push_word_search(self, &mut cmds, false),
            M::WordForward => {
                for _ in 0..n {
                    self.buffer.move_word_forward();
                }
            }
            M::WordBack => {
                for _ in 0..n {
                    self.buffer.move_word_backward();
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
                let target = self.buffer.motion_target(self.buffer.cursor, resolved, n);
                self.buffer.cursor = target;
            }
            // Resolved away by `resolve_motion_pure` — should never
            // reach the match arm.
            M::RepeatFind { .. } => {}
            // `gg` with no count goes to line 1; `5gg` to line 5.
            M::FileStart => {
                if n > 1 {
                    self.goto_line_n_pure(n as usize);
                } else {
                    self.buffer.move_file_start();
                }
            }
            // `G` with no count goes to file end; `20G` to line 20.
            M::FileEnd => {
                if n > 1 {
                    self.goto_line_n_pure(n as usize);
                } else {
                    self.buffer.move_file_end();
                }
            }
            M::SearchNext => {
                for _ in 0..n {
                    cmds.push(Cmd::JumpSearch {
                        forward: self.search.last_forward,
                    });
                }
            }
            M::SearchPrev => {
                for _ in 0..n {
                    cmds.push(Cmd::JumpSearch {
                        forward: !self.search.last_forward,
                    });
                }
            }
            M::ParagraphForward => {
                for _ in 0..n {
                    self.buffer.move_paragraph_forward();
                }
            }
            M::ParagraphBack => {
                for _ in 0..n {
                    self.buffer.move_paragraph_backward();
                }
            }
        }
        cmds
    }

    fn handle_op(&mut self, op: Operator, target: Target, outer_count: u32) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        match target {
            Target::LineWise => {
                for _ in 0..outer_count {
                    match op {
                        Operator::Delete => self.buffer.delete_line(),
                        Operator::Yank => {
                            self.buffer.yank_line();
                            cmds.push(Cmd::StatusInfo("yanked".into()));
                        }
                        Operator::Change => {
                            cmds.push(Cmd::StatusError("change not implemented yet".into()));
                        }
                    }
                }
            }
            Target::Motion(m) => {
                let (resolved, last_find_update) = resolve_motion_pure(m.motion, self.last_find);
                if let Some(lf) = last_find_update {
                    cmds.push(Cmd::SetLastFind(lf));
                }
                let Some(resolved) = resolved else {
                    cmds.push(Cmd::StatusError("no previous find".into()));
                    return cmds;
                };
                let inclusive = is_inclusive_motion(resolved);
                for _ in 0..outer_count {
                    let start = self.buffer.cursor;
                    let target = self.buffer.motion_target(start, resolved, m.count);
                    // Vim's inclusive motions (`e`, `f<c>`, `t<c>`, …)
                    // include the landing char in the operator range;
                    // `apply_op_range_handle` takes an exclusive end,
                    // so push one past for these.
                    let end = if inclusive {
                        self.buffer.advance_one(target)
                    } else {
                        target
                    };
                    self.apply_op_range_handle(op, start, end, &mut cmds);
                }
            }
            Target::TextObject { scope, object } => {
                for _ in 0..outer_count {
                    match self.buffer.text_object_range(scope, object) {
                        Some((start, end)) => self.apply_op_range_handle(op, start, end, &mut cmds),
                        None => {
                            cmds.push(Cmd::StatusError("no matching object".into()));
                            break;
                        }
                    }
                }
            }
        }
        cmds
    }

    /// Apply an operator over the range [start, end). Shared by
    /// motion-target and text-object dispatch, and by visual-mode
    /// operator application (which calls the `App`-mutating sibling
    /// `apply_op_range` for now).
    fn apply_op_range_handle(
        &mut self,
        op: Operator,
        start: crate::editor::Cursor,
        end: crate::editor::Cursor,
        cmds: &mut Vec<Cmd>,
    ) {
        match op {
            Operator::Delete => self.buffer.delete_range(start, end),
            Operator::Yank => {
                self.buffer.yank_range(start, end);
                cmds.push(Cmd::StatusInfo("yanked".into()));
            }
            Operator::Change => {
                self.buffer.delete_range(start, end);
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
        }
    }

    /// Buffer-only goto-line implementation. The original
    /// `goto_line_n` lives on the legacy evaluator path; this copy
    /// keeps `handle_*` from depending on it so we can drop the old
    /// path cleanly in the rewire step.
    fn goto_line_n_pure(&mut self, n: usize) {
        let last = self.buffer.lines.len().saturating_sub(1);
        self.buffer.cursor.row = n.saturating_sub(1).min(last);
        self.buffer.cursor.col = 0;
        self.buffer.clamp_col(false);
    }
}

// ────────────────────────────────────────────────────────────────────────
// Pure helpers
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
        M::FindChar { ch, forward, till } => (
            Some(motion),
            Some(LastFind { ch, forward, till }),
        ),
        _ => (Some(motion), None),
    }
}

/// Pull the word at the cursor, push the matching `SetSearch` +
/// `JumpSearch` pair. Factored out so `*` and `#` share the path.
fn push_word_search(app: &App, cmds: &mut Vec<Cmd>, forward: bool) {
    match word_under_cursor(&app.buffer) {
        Some(word) => {
            cmds.push(Cmd::SetSearch {
                pattern: word,
                forward,
            });
            cmds.push(Cmd::JumpSearch { forward });
        }
        None => cmds.push(Cmd::StatusError("no word under cursor".into())),
    }
}

/// Plan the response to a bare `:q`. Refuses with an error status
/// while there are unsaved edits in the active or any sleeping
/// buffer; otherwise emits the actual quit command.
fn plan_quit(app: &App) -> Cmd {
    if app.buffer.dirty {
        return Cmd::StatusError("unsaved changes (use :q!)".into());
    }
    let sleeping_dirty: Vec<&BufferRef> = app
        .sleeping
        .iter()
        .filter(|(_, b)| b.dirty)
        .map(|(r, _)| r)
        .collect();
    if !sleeping_dirty.is_empty() {
        return Cmd::StatusError(format!(
            "unsaved changes in {} (use :q!)",
            format_dirty_list(&sleeping_dirty)
        ));
    }
    Cmd::Quit
}

fn parse_save_path(rest: &str) -> Option<PathBuf> {
    if rest.is_empty() {
        None
    } else {
        Some(PathBuf::from(rest))
    }
}

fn expr_modifies_buffer(expr: &Expr) -> bool {
    use DirectKind as D;
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
        ),
        Expr::Motion(_) => false,
        Expr::Op { op, .. } => !matches!(op, Operator::Yank),
    }
}
