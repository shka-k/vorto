//! Command / motion / operator evaluation.
//!
//! `App::handle_key` turns a token stream into an [`Expr`]; this module
//! turns the `Expr` into buffer mutations and side effects. Lives in
//! its own file so `app/mod.rs` doesn't have to bundle the input
//! pipeline and the evaluation pipeline together.

use std::path::Path;

use anyhow::Result;

use super::{App, CommandBind, Status, root_cause};
use crate::action::{Ctx, DirectKind, Expr, MotionExpr, MotionKind, Operator, Target};
use crate::editor::Cursor;
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
                    self.should_quit = true;
                }
            }
            D::QuitForce => self.should_quit = true,
            D::SaveAndQuit => {
                self.do_save(ctx.rest)?;
                self.should_quit = true;
            }
            D::Save => self.do_save(ctx.rest)?,
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

    fn eval_motion(&mut self, m: MotionExpr) {
        use MotionKind as M;
        let allow_after = matches!(self.mode, Mode::Insert);
        let n = m.count;
        match m.motion {
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
                for _ in 0..outer_count {
                    let start = self.buffer.cursor;
                    let end = self.buffer.motion_target(start, m.motion, m.count);
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

    fn jump_search(&mut self, forward: bool) {
        if let Some(c) = self.search.find_next(&self.buffer, forward) {
            self.buffer.cursor = c;
        } else {
            self.status = Status::error("pattern not found");
        }
    }

    fn do_save(&mut self, rest: &str) -> Result<()> {
        if rest.is_empty() {
            if self.buffer.path.is_some() {
                self.buffer.save()?;
                self.status = Status::info("written");
            } else {
                self.status = Status::error("no file name (use :w <path>)");
            }
        } else {
            let p = Path::new(rest);
            self.buffer.save_as(p)?;
            self.status = Status::info(format!("written to {}", p.display()));
        }
        // Notify the LSP server that the buffer is now on disk — many
        // servers (rust-analyzer in particular) only run their full
        // checker on save, so without this nothing fresh would arrive.
        self.notify_lsp_save();
        Ok(())
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
        ),
        Expr::Motion(_) => false,
        Expr::Op { op, .. } => !matches!(op, Operator::Yank),
    }
}
