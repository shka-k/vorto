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
    Ctx, DirectKind, Expr, LastFind, MotionExpr, MotionKind, Operator, PromptKind, Target,
};
use crate::app::App;
use crate::buffer_ref::BufferRef;
use crate::effect::{Cmd, ScrollAnchor};
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
                let indent = self.indent_settings();
                self.buffer.insert_line_below(indent);
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            D::OpenLineAbove => {
                let indent = self.indent_settings();
                self.buffer.insert_line_above(indent);
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
                let col = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);
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
                cmds.push(Cmd::SyncYank);
                cmds.push(Cmd::ToastInfo("yanked".into()));
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
                    cmds.push(Cmd::ToastError("already at oldest change".into()));
                }
            }
            D::Redo => {
                if !self.buffer.redo() {
                    cmds.push(Cmd::ToastError("already at newest change".into()));
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
                    crate::finder::FuzzyKind::Buffers,
                )));
            }
            D::NewScratchBuffer => cmds.push(Cmd::NewScratchBuffer),
            D::SaveAndQuit => cmds.push(Cmd::Save {
                path: parse_save_path(ctx.rest),
                then_quit: true,
                force: false,
            }),
            D::Save => cmds.push(Cmd::Save {
                path: parse_save_path(ctx.rest),
                then_quit: false,
                force: false,
            }),
            D::SaveForce => cmds.push(Cmd::Save {
                path: parse_save_path(ctx.rest),
                then_quit: false,
                force: true,
            }),
            D::Open => {
                if ctx.rest.is_empty() {
                    cmds.push(Cmd::ToastError("missing path".into()));
                } else {
                    cmds.push(Cmd::OpenPath(PathBuf::from(ctx.rest)));
                }
            }
            D::OpenLog => match crate::log::default_path() {
                Some(p) => cmds.push(Cmd::OpenPath(p)),
                None => cmds.push(Cmd::ToastError("log path unresolved".into())),
            },
            D::Reload => cmds.push(Cmd::Reload),
            D::ReloadAll => cmds.push(Cmd::ReloadAll),
            D::GotoLine => match ctx.rest.parse::<usize>() {
                Ok(n) if n >= 1 => self.goto_line_n_pure(n),
                _ => cmds.push(Cmd::ToastError("usage: :goto <line>".into())),
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
            D::CodeAction => cmds.push(Cmd::LspCodeAction),
            D::Hover => cmds.push(Cmd::LspHover),
            // Intercepted by `App::evaluate` before reaching here.
            D::RepeatLast => unreachable!("RepeatLast handled in App::evaluate"),
            D::SearchSelectNext { reverse } => {
                cmds.push(Cmd::SearchSelectMatch { reverse });
            }
            D::SearchWordKeep { forward } => {
                push_word_search(self, &mut cmds, forward, false);
            }
            D::ClearSearch => {
                cmds.push(Cmd::SetSearch {
                    pattern: String::new(),
                    forward: true,
                });
            }
            D::Substitute => run_substitute(self, ctx.rest, &mut cmds),
            D::MultiCursorAddNext => add_next_cursor(self, &mut cmds),
            D::MultiCursorPop => {
                if let Some(c) = self.buffer.extra_cursors.pop() {
                    self.buffer.cursor = c;
                } else {
                    cmds.push(Cmd::ToastInfo("no extra cursor to remove".into()));
                }
            }
            D::MultiCursorClear => {
                if self.buffer.extra_cursors.is_empty() {
                    cmds.push(Cmd::ToastInfo("no extra cursors".into()));
                } else {
                    let n = self.buffer.extra_cursors.len();
                    self.buffer.extra_cursors.clear();
                    cmds.push(Cmd::ToastInfo(format!("cleared {n} extra cursors")));
                }
            }
            D::JumpLabel => cmds.push(Cmd::StartJumpLabel),
            D::SelectWholeBuffer => cmds.push(Cmd::SelectWholeBuffer),
            D::ToggleComment => match buffer_comment_token(self) {
                Some(token) => {
                    let start_row = self.buffer.cursor.row;
                    let start_col = self.buffer.cursor.col;
                    let max = self.buffer.lines.len();
                    for i in 0..count {
                        self.buffer.toggle_line_comment(&token);
                        if i + 1 < count && self.buffer.cursor.row + 1 < max {
                            self.buffer.cursor.row += 1;
                        }
                    }
                    self.buffer.cursor.row = start_row;
                    self.buffer.cursor.col = start_col;
                    self.buffer.clamp_col(false);
                }
                None => cmds.push(Cmd::ToastError("no comment token for this buffer".into())),
            },
            D::SplitWindowHorizontal => cmds.push(Cmd::SplitWindow {
                dir: crate::app::SplitDir::Horizontal,
            }),
            D::SplitWindowVertical => cmds.push(Cmd::SplitWindow {
                dir: crate::app::SplitDir::Vertical,
            }),
            D::CloseWindow => cmds.push(Cmd::CloseWindow),
            D::FocusWindow { dir } => cmds.push(Cmd::FocusWindow { dir }),
            D::CycleWindow => cmds.push(Cmd::CycleWindow),
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
            cmds.push(Cmd::ToastError("no previous find".into()));
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
            M::SearchWordForward => push_word_search(self, &mut cmds, true, true),
            M::SearchWordBack => push_word_search(self, &mut cmds, false, true),
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
                if matches!(op, Operator::Indent | Operator::Dedent) {
                    let indent = self.indent_settings();
                    let start_row = self.buffer.cursor.row;
                    let last = self.buffer.lines.len().saturating_sub(1);
                    let span = outer_count.max(1) as usize - 1;
                    let end_row = start_row.saturating_add(span).min(last);
                    for r in start_row..=end_row {
                        if matches!(op, Operator::Indent) {
                            self.buffer.indent_line(r, indent);
                        } else {
                            self.buffer.dedent_line(r, indent);
                        }
                    }
                    self.buffer.cursor.row = start_row;
                    cursor_to_first_non_blank(&mut self.buffer);
                } else {
                    for _ in 0..outer_count {
                        match op {
                            Operator::Delete => self.buffer.delete_line(),
                            Operator::Yank => {
                                self.buffer.yank_line();
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
                let (resolved, last_find_update) = resolve_motion_pure(m.motion, self.last_find);
                if let Some(lf) = last_find_update {
                    cmds.push(Cmd::SetLastFind(lf));
                }
                let Some(resolved) = resolved else {
                    cmds.push(Cmd::ToastError("no previous find".into()));
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
            Target::SearchMatch { reverse } => {
                // The match range starts at the pattern hit, not at
                // the cursor — that's the whole point of having a
                // dedicated target. We read `self.search` (allowed by
                // the handler's discipline) and apply the op to each
                // match found in sequence; `outer_count > 1` walks
                // forward through successive matches (e.g. `2dgn`).
                let forward = self.search.last_forward ^ reverse;
                for _ in 0..outer_count {
                    let Some((start, end_incl)) =
                        self.search.find_match_range(&self.buffer, forward)
                    else {
                        cmds.push(Cmd::ToastError("pattern not found".into()));
                        break;
                    };
                    let end = self.buffer.advance_one(end_incl);
                    self.apply_op_range_handle(op, start, end, &mut cmds);
                }
            }
            Target::TextObject { scope, object } => {
                for _ in 0..outer_count {
                    match self.buffer.text_object_range(scope, object) {
                        Some((start, end)) => self.apply_op_range_handle(op, start, end, &mut cmds),
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
                cmds.push(Cmd::SyncYank);
                cmds.push(Cmd::ToastInfo("yanked".into()));
            }
            Operator::Change => {
                self.buffer.delete_range(start, end);
                cmds.push(Cmd::EnterMode(Mode::Insert));
            }
            Operator::Indent | Operator::Dedent => {
                // `>` and `<` are line-wise even with a non-line target —
                // every row spanned by the motion gets one indent step.
                let indent = self.indent_settings();
                let (lo, hi) = if (start.row, start.col) <= (end.row, end.col) {
                    (start.row, end.row)
                } else {
                    (end.row, start.row)
                };
                for r in lo..=hi {
                    if matches!(op, Operator::Indent) {
                        self.buffer.indent_line(r, indent);
                    } else {
                        self.buffer.dedent_line(r, indent);
                    }
                }
                self.buffer.cursor.row = lo;
                cursor_to_first_non_blank(&mut self.buffer);
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

/// Plan the response to a bare `:q`. Refuses with an error status
/// while there are unsaved edits in the active or any sleeping
/// buffer; otherwise emits the actual quit command.
fn plan_quit(app: &App) -> Cmd {
    if app.buffer.dirty {
        return Cmd::ToastError("unsaved changes (use :q!)".into());
    }
    let sleeping_dirty: Vec<&BufferRef> = app
        .sleeping
        .iter()
        .filter(|(_, b)| b.dirty)
        .map(|(r, _)| r)
        .collect();
    if !sleeping_dirty.is_empty() {
        return Cmd::ToastError(format!(
            "unsaved changes in {} (use :q!)",
            format_dirty_list(&sleeping_dirty)
        ));
    }
    Cmd::Quit
}

/// `<C-n>` body. Pulls the word under the cursor, finds its next
/// occurrence forward from primary (wrapping around the buffer), and
/// pushes primary as a new extra cursor before jumping primary to the
/// match. Also seeds `App.search` via `Cmd::SetSearch` so `n` / `N`
/// keep working on the same pattern after the user is done adding
/// cursors. When the cursor isn't on a word (e.g. sitting on `[`),
/// drops into Visual mode at the current position instead of erroring
/// — the user can extend the selection and try again, or just operate
/// on the highlighted char. No-ops with a status message when the
/// next match would land on a cursor that's already tracked.
fn add_next_cursor(app: &mut App, cmds: &mut Vec<Cmd>) {
    let Some(word) = word_under_cursor(&app.buffer) else {
        app.enter_mode(Mode::Visual);
        return;
    };
    // Use a throwaway SearchState for the lookup so we can act on the
    // result this turn — `Cmd::SetSearch` is only applied after
    // `handle_expr` returns, so reading `app.search` here would see
    // the pre-Ctrl-N pattern.
    let mut tmp = crate::editor::SearchState::default();
    tmp.set(word.clone(), true);
    let Some(next) = tmp.find_next(&app.buffer, true) else {
        cmds.push(Cmd::ToastError("no further match".into()));
        return;
    };
    let primary = app.buffer.cursor;
    if next == primary || app.buffer.extra_cursors.contains(&next) {
        cmds.push(Cmd::ToastInfo("no further match".into()));
        return;
    }
    app.buffer.extra_cursors.push(primary);
    app.buffer.cursor = next;
    cmds.push(Cmd::SetSearch {
        pattern: word,
        forward: true,
    });
    let n = app.buffer.extra_cursors.len() + 1;
    cmds.push(Cmd::ToastInfo(format!("{n} cursors")));
}

/// Move the cursor to the first non-whitespace column on its current
/// row, falling back to col 0 on an all-blank line. Used after
/// indent/dedent operators to match vim's landing position.
fn cursor_to_first_non_blank(buf: &mut crate::editor::Buffer) {
    let line = buf.current_line();
    let col = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);
    buf.cursor.col = col;
}

fn parse_save_path(rest: &str) -> Option<PathBuf> {
    if rest.is_empty() {
        None
    } else {
        Some(PathBuf::from(rest))
    }
}

pub(super) fn expr_modifies_buffer(expr: &Expr) -> bool {
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
                | D::ToggleComment
                | D::Substitute
        ),
        Expr::Motion(_) => false,
        Expr::Op { op, .. } => !matches!(op, Operator::Yank),
    }
}

/// Body of `:s/pat/repl/[g]` and `:%s/...`. Parses the raw command
/// string off `ctx.rest`, falls back to the active search pattern when
/// the user passed an empty pattern (vim convention: `:%s//new/g`
/// after a `/old`), applies the substitution against the buffer, and
/// pushes a status toast plus a `SetSearch` so `n`/`hlsearch` track
/// what was replaced.
fn run_substitute(app: &mut App, raw: &str, cmds: &mut Vec<Cmd>) {
    let Some(parsed) = crate::editor::parse_substitute(raw) else {
        cmds.push(Cmd::ToastError("usage: :s/pat/repl/[g]".into()));
        return;
    };
    let args = match parsed {
        Ok(a) => a,
        Err(msg) => {
            cmds.push(Cmd::ToastError(msg.into()));
            return;
        }
    };

    // Empty pattern → reuse the last search pattern. Saves typing
    // after `/foo<CR>:%s//bar/g`.
    let fallback;
    let pattern = if args.pattern.is_empty() {
        if app.search.query.is_empty() {
            cmds.push(Cmd::ToastError("no previous search pattern".into()));
            return;
        }
        fallback = app.search.query.clone();
        fallback.as_str()
    } else {
        args.pattern
    };

    let resolved = crate::editor::SubsArgs {
        range: args.range,
        pattern,
        replacement: args.replacement,
        global: args.global,
    };
    let outcome = app.buffer.substitute(&resolved);

    if outcome.matches == 0 {
        cmds.push(Cmd::ToastError(format!("pattern not found: {}", pattern)));
        return;
    }
    cmds.push(Cmd::SetSearch {
        pattern: pattern.to_string(),
        forward: true,
    });
    cmds.push(Cmd::ToastInfo(format!(
        "{} substitution{} on {} line{}",
        outcome.matches,
        if outcome.matches == 1 { "" } else { "s" },
        outcome.lines_changed,
        if outcome.lines_changed == 1 { "" } else { "s" },
    )));
}

/// Look up the active buffer's language comment token. Returns `None`
/// when the buffer has no file path, the extension is unknown, or the
/// language has no `comment_token` configured.
fn buffer_comment_token(app: &App) -> Option<String> {
    let path = app.buffer.path.as_ref()?;
    let ext = path.extension()?.to_str()?;
    let lang = app.config.languages.by_extension(ext)?;
    lang.comment_token.clone()
}
