//! Command evaluation.
//!
//! `parse` owns the pure side of the pipeline (`KeyEvent` → `Token`,
//! `&[Token]` → `Expr`). This module owns the *behavior* — interpreting
//! a finished `Expr` against the App and producing buffer mutations,
//! status messages, LSP requests, and so on.
//!
//! As the refactor proceeds, the body of `eval_direct` / `eval_motion`
//! will move to a sibling `handle` module that returns `Vec<Cmd>`
//! instead of poking `App` directly. The plumbing here stays compatible
//! during the move.

mod parse;

pub(super) use parse::{Parse, classify, tokenize};

use std::path::Path;

use anyhow::Result;

use super::{App, LastFind, Status, root_cause};
use crate::action::{Ctx, DirectKind, Expr, MotionExpr, MotionKind, Operator, Target};
use crate::config::CommandBind;
use crate::editor::Cursor;
use crate::effect::ScrollAnchor;
use crate::mode::Mode;

impl App {
    pub(super) fn execute_command(&mut self, cmd: &str) -> Result<()> {
        // `:42` shortcut for `:goto 42`.
        if cmd.parse::<usize>().is_ok() {
            return self.eval_direct(DirectKind::GotoLine, 1, Ctx::with_rest(cmd));
        }

        let (head, rest) = match cmd.split_once(' ') {
            Some((h, r)) => (h, r.trim()),
            None => (cmd, ""),
        };
        if head.is_empty() {
            return Ok(());
        }
        match CommandBind::find(head) {
            Some(b) => self.eval_direct(b.kind, 1, Ctx::with_rest(rest)),
            None => {
                self.status = Status::error(format!("unknown command: {}", head));
                Ok(())
            }
        }
    }

    pub(super) fn evaluate(&mut self, expr: Expr, ctx: Ctx) -> Result<()> {
        // Take an undo snapshot before any Expr that's going to change
        // the buffer (or kick off an Insert-mode session). Pure cursor
        // moves and yanks intentionally don't snapshot.
        if expr_modifies_buffer(&expr) {
            self.buffer.snapshot();
        }
        match expr {
            Expr::Direct { kind, count } => self.eval_direct(kind, count, ctx),
            Expr::Motion(m) => {
                self.eval_motion(m);
                Ok(())
            }
            Expr::Op {
                op,
                target,
                outer_count,
            } => self.eval_op(op, target, outer_count),
        }
    }

    fn eval_direct(&mut self, kind: DirectKind, count: u32, ctx: Ctx) -> Result<()> {
        use DirectKind as D;
        match kind {
            D::EnterMode(m) => self.enter_mode(m),
            D::OpenPrompt(k) => self.open_prompt(k),
            D::OpenLineBelow => {
                self.buffer.insert_line_below();
                self.enter_mode(Mode::Insert);
            }
            D::OpenLineAbove => {
                self.buffer.insert_line_above();
                self.enter_mode(Mode::Insert);
            }
            D::AppendAfterCursor => {
                // Past-the-end is allowed in Insert, so step right with
                // that permission rather than the Normal-mode clamp.
                self.buffer.move_right(true);
                self.enter_mode(Mode::Insert);
            }
            D::AppendAtLineEnd => {
                self.buffer.cursor.col = self.buffer.current_line_len();
                self.enter_mode(Mode::Insert);
            }
            D::InsertAtLineStart => {
                let line = self.buffer.current_line();
                let col = line
                    .chars()
                    .position(|c| !c.is_whitespace())
                    .unwrap_or(0);
                self.buffer.cursor.col = col;
                self.enter_mode(Mode::Insert);
            }
            D::ChangeToEol => {
                self.buffer.delete_to_eol();
                self.enter_mode(Mode::Insert);
            }
            D::DeleteToEol => self.buffer.delete_to_eol(),
            D::YankLine => {
                for _ in 0..count {
                    self.buffer.yank_line();
                }
                self.status = Status::info("yanked");
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
                self.enter_mode(Mode::Insert);
            }
            D::SubstituteLine => {
                self.buffer.clear_current_line();
                self.enter_mode(Mode::Insert);
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
            D::ViewportCenter => self.scroll_to_cursor(ScrollAnchor::Center),
            D::ViewportTopAtCursor => self.scroll_to_cursor(ScrollAnchor::Top),
            D::ViewportBottomAtCursor => self.scroll_to_cursor(ScrollAnchor::Bottom),
            D::Paste => {
                for _ in 0..count {
                    self.buffer.paste_after();
                }
            }
            D::Undo => {
                if !self.buffer.undo() {
                    self.status = Status::error("already at oldest change");
                }
            }
            D::Redo => {
                if !self.buffer.redo() {
                    self.status = Status::error("already at newest change");
                }
            }
            D::DeleteCharUnderCursor => {
                for _ in 0..count {
                    self.buffer.delete_char_under_cursor();
                }
            }
            D::Quit => {
                if self.buffer.dirty {
                    self.status = Status::error("unsaved changes (use :q!)");
                } else {
                    let sleeping_dirty: Vec<&super::BufferRef> = self
                        .sleeping
                        .iter()
                        .filter(|(_, b)| b.dirty)
                        .map(|(r, _)| r)
                        .collect();
                    if !sleeping_dirty.is_empty() {
                        self.status = Status::error(format!(
                            "unsaved changes in {} (use :q!)",
                            format_dirty_list(&sleeping_dirty)
                        ));
                    } else {
                        self.should_quit = true;
                    }
                }
            }
            D::QuitForce => self.should_quit = true,
            D::BufferNext => self.buffer_cycle(true)?,
            D::BufferPrev => self.buffer_cycle(false)?,
            D::BufferDelete => self.buffer_delete(false)?,
            D::BufferDeleteForce => self.buffer_delete(true)?,
            D::BufferList => {
                self.open_prompt(crate::action::PromptKind::Fuzzy(
                    crate::fuzzy::FuzzyKind::Buffers,
                ));
            }
            D::SaveAndQuit => {
                // Only quit when the save actually happened — `:wq` on
                // a no-name buffer must surface the "no file name"
                // error and stay open instead of silently dropping
                // the buffer's contents.
                if self.do_save(ctx.rest)? {
                    self.should_quit = true;
                }
            }
            D::Save => {
                self.do_save(ctx.rest)?;
            }
            D::Open => {
                if ctx.rest.is_empty() {
                    self.status = Status::error("missing path");
                } else {
                    self.open_path(Path::new(ctx.rest))?;
                }
            }
            D::GotoLine => self.goto_line(ctx.rest),
            D::GotoDefinition => self.lsp_jump("textDocument/definition", "definition"),
            D::GotoDeclaration => self.lsp_jump("textDocument/declaration", "declaration"),
            D::GotoImplementation => self.lsp_jump("textDocument/implementation", "implementation"),
            D::FindReferences => self.lsp_find_references(),
            D::Rename => self.open_rename_prompt(),
        }
        Ok(())
    }

    /// Recenter the viewport against the cursor's current row. Driven
    /// by `zz`/`zt`/`zb`. Reads the height that the last frame was
    /// drawn at (published by the UI in `Buffer.viewport_height`).
    fn scroll_to_cursor(&mut self, anchor: ScrollAnchor) {
        let height = self.buffer.viewport_height.get();
        if height == 0 {
            return;
        }
        let cur = self.buffer.cursor.row;
        let last = self.buffer.lines.len().saturating_sub(1);
        let scroll = match anchor {
            ScrollAnchor::Top => cur,
            ScrollAnchor::Center => cur.saturating_sub(height / 2),
            ScrollAnchor::Bottom => cur + 1 - height.min(cur + 1),
        };
        // Clamp so we never scroll past the bottom of the file.
        let max_scroll = last.saturating_sub(height.saturating_sub(1));
        self.buffer.scroll.set(scroll.min(max_scroll));
    }

    /// Resolve `RepeatFind` to a concrete `FindChar` and record any
    /// real `FindChar` as the new `last_find`. Returns `None` when
    /// `;`/`,` is pressed with no prior find — the caller posts the
    /// "no previous find" error and aborts.
    fn resolve_find_motion(&mut self, motion: MotionKind) -> Option<MotionKind> {
        use MotionKind as M;
        match motion {
            M::RepeatFind { reverse } => {
                let lf = self.last_find?;
                let forward = if reverse { !lf.forward } else { lf.forward };
                Some(M::FindChar {
                    ch: lf.ch,
                    forward,
                    till: lf.till,
                })
            }
            M::FindChar { ch, forward, till } => {
                self.last_find = Some(LastFind { ch, forward, till });
                Some(motion)
            }
            _ => Some(motion),
        }
    }

    fn eval_motion(&mut self, m: MotionExpr) {
        use MotionKind as M;
        let allow_after = matches!(self.mode, Mode::Insert);
        let n = m.count;
        let Some(resolved) = self.resolve_find_motion(m.motion) else {
            self.status = Status::error("no previous find");
            return;
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
            // `*` / `#` extract the word under the cursor and seed the
            // search state, then jump. Handled here (not in motion_target)
            // because the buffer doesn't know about `App.search`.
            M::SearchWordForward => self.search_word_under_cursor(true),
            M::SearchWordBack => self.search_word_under_cursor(false),
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
            // (`motion_target` is stateless so we route everything that
            // doesn't need App-side context through here.)
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
            // Resolved away by `resolve_find_motion` — should never
            // reach the match arm.
            M::RepeatFind { .. } => {}
            // `gg` with no count goes to line 1; `5gg` to line 5.
            M::FileStart => {
                if n > 1 {
                    self.goto_line_n(n as usize);
                } else {
                    self.buffer.move_file_start();
                }
            }
            // `G` with no count goes to file end; `20G` to line 20.
            M::FileEnd => {
                if n > 1 {
                    self.goto_line_n(n as usize);
                } else {
                    self.buffer.move_file_end();
                }
            }
            M::SearchNext => {
                for _ in 0..n {
                    self.jump_search(self.search.last_forward);
                }
            }
            M::SearchPrev => {
                for _ in 0..n {
                    self.jump_search(!self.search.last_forward);
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
    }

    fn eval_op(&mut self, op: Operator, target: Target, outer_count: u32) -> Result<()> {
        match target {
            Target::LineWise => {
                for _ in 0..outer_count {
                    match op {
                        Operator::Delete => self.buffer.delete_line(),
                        Operator::Yank => {
                            self.buffer.yank_line();
                            self.status = Status::info("yanked");
                        }
                        Operator::Change => {
                            self.status = Status::error("change not implemented yet");
                        }
                    }
                }
                Ok(())
            }
            Target::Motion(m) => {
                let Some(resolved) = self.resolve_find_motion(m.motion) else {
                    self.status = Status::error("no previous find");
                    return Ok(());
                };
                let inclusive = is_inclusive_motion(resolved);
                for _ in 0..outer_count {
                    let start = self.buffer.cursor;
                    let target = self.buffer.motion_target(start, resolved, m.count);
                    // Vim's inclusive motions (`e`, `f<c>`, `t<c>`, …)
                    // include the landing char in the operator range;
                    // `apply_op_range` takes an exclusive end, so push
                    // one past for these.
                    let end = if inclusive {
                        self.buffer.advance_one(target)
                    } else {
                        target
                    };
                    self.apply_op_range(op, start, end);
                }
                Ok(())
            }
            Target::TextObject { scope, object } => {
                for _ in 0..outer_count {
                    match self.buffer.text_object_range(scope, object) {
                        Some((start, end)) => self.apply_op_range(op, start, end),
                        None => {
                            self.status = Status::error("no matching object");
                            break;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Apply an operator over the range [start, end). Shared by Op +
    /// Motion targets and by visual-mode operator application.
    pub(super) fn apply_op_range(&mut self, op: Operator, start: Cursor, end: Cursor) {
        match op {
            Operator::Delete => self.buffer.delete_range(start, end),
            Operator::Yank => {
                self.buffer.yank_range(start, end);
                self.status = Status::info("yanked");
            }
            Operator::Change => {
                self.buffer.delete_range(start, end);
                self.enter_mode(Mode::Insert);
            }
        }
    }

    /// `*` / `#` — extract the word under the cursor, seed it as the
    /// active search pattern, and jump to the next/prev match.
    /// Word here matches the char-class definition (`[A-Za-z0-9_]+`)
    /// so it works the same with or without a syntax highlighter.
    pub(super) fn search_word_under_cursor(&mut self, forward: bool) {
        let Some(word) = word_under_cursor(&self.buffer) else {
            self.status = Status::error("no word under cursor");
            return;
        };
        self.search.set(word, forward);
        self.jump_search(forward);
    }

    fn jump_search(&mut self, forward: bool) {
        if let Some(c) = self.search.find_next(&self.buffer, forward) {
            self.buffer.cursor = c;
        } else {
            self.status = Status::error("pattern not found");
        }
    }

    /// Persist the active buffer to disk. Returns `true` when a write
    /// actually happened (so `:wq` / `:x` can tell save-failed-but-
    /// status-set apart from real success and refuse to quit in that
    /// case). I/O errors propagate; `Ok(false)` means "no write
    /// performed, status already explains why".
    fn do_save(&mut self, rest: &str) -> Result<bool> {
        if rest.is_empty() {
            if self.buffer.path.is_some() {
                self.buffer.save()?;
                self.status = Status::info("written");
            } else {
                self.status = Status::error("no file name (use :w <path>)");
                return Ok(false);
            }
        } else {
            let p = Path::new(rest);
            self.buffer.save_as(p)?;
            self.status = Status::info(format!("written to {}", p.display()));
        }
        // Buffer is now clean — buffer.save() already cleared the
        // flag; nothing else to do here. Sleeping copies keep their
        // own dirty state and are checked independently.
        // Notify the LSP server that the buffer is now on disk — many
        // servers (rust-analyzer in particular) only run their full
        // checker on save, so without this nothing fresh would arrive.
        self.notify_lsp_save();
        Ok(true)
    }

    fn notify_lsp_save(&mut self) {
        let text = self.buffer.lines.join("\n");
        if let Err(e) = self.lsp.did_save(&text) {
            self.status = Status::error(format!("lsp didSave: {}", root_cause(&e)));
        }
    }

    fn goto_line(&mut self, arg: &str) {
        match arg.parse::<usize>() {
            Ok(n) if n >= 1 => self.goto_line_n(n),
            _ => {
                self.status = Status::error("usage: :goto <line>");
            }
        }
    }

    fn goto_line_n(&mut self, n: usize) {
        let last = self.buffer.lines.len().saturating_sub(1);
        self.buffer.cursor.row = n.saturating_sub(1).min(last);
        self.buffer.cursor.col = 0;
        self.buffer.clamp_col(false);
    }
}

/// Human-readable list of dirty sleeping buffers for the `:q`
/// refusal message. Trims long lists with "+N more" so the status
/// bar stays readable.
fn format_dirty_list(refs: &[&super::BufferRef]) -> String {
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
fn is_inclusive_motion(motion: MotionKind) -> bool {
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
fn word_under_cursor(buf: &crate::editor::Buffer) -> Option<String> {
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
