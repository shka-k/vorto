use std::path::Path;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{
    Ctx, DirectKind, Expr, MotionExpr, MotionKind, Operator, PromptKind, Target, Token,
};
use crate::config::CursorShapes;
use crate::editor::{Buffer, Cursor};
use crate::fuzzy::{Finder, FuzzyKind};
use crate::keymap::{self, Keymap};
use crate::mode::Mode;
use crate::search::SearchState;

pub enum Prompt {
    None,
    Command(String),
    Search { forward: bool, query: String },
    Fuzzy(Finder),
}

impl Prompt {
    pub fn is_open(&self) -> bool {
        !matches!(self, Prompt::None)
    }
}

/// Status-bar message paired with its severity. The UI renders `Error`
/// variants in red.
pub enum Status {
    Info(String),
    Error(String),
}

impl Status {
    pub fn info(s: impl Into<String>) -> Self {
        Status::Info(s.into())
    }
    pub fn error(s: impl Into<String>) -> Self {
        Status::Error(s.into())
    }
    pub fn text(&self) -> &str {
        match self {
            Status::Info(s) | Status::Error(s) => s,
        }
    }
    pub fn is_error(&self) -> bool {
        matches!(self, Status::Error(_))
    }
}

pub struct App {
    pub buffer: Buffer,
    pub mode: Mode,
    pub prompt: Prompt,
    pub search: SearchState,
    pub status: Status,
    /// Accumulated tokens since the last command fired. Cleared on
    /// Complete dispatch or Invalid parse.
    pub tokens: Vec<Token>,
    /// User-customisable binding tables (defaults to the vim mapping
    /// and gets overridden by `~/.config/vorto/config.toml` at startup).
    pub keymap: Keymap,
    /// Per-mode cursor shapes (Block/Bar/Underbar) — applied by the main
    /// loop via `SetCursorStyle` after every draw.
    pub cursor_shapes: CursorShapes,
    /// Anchor cursor for visual modes — the position the selection was
    /// started from. `None` outside of any visual mode.
    pub visual_anchor: Option<Cursor>,
    pub should_quit: bool,
}

/// Resolved visual-mode selection bounds, derived from the anchor and
/// the cursor according to the current visual sub-mode.
#[derive(Debug, Clone, Copy)]
pub enum Selection {
    /// Character-wise, inclusive of both endpoints (vim semantics).
    Char { from: Cursor, to: Cursor },
    /// Whole rows `[from_row..=to_row]`.
    Line { from_row: usize, to_row: usize },
    /// Column rectangle `[r0..=r1] × [c0..=c1]`.
    Block {
        r0: usize,
        c0: usize,
        r1: usize,
        c1: usize,
    },
}

impl App {
    pub fn with_keymap(keymap: Keymap) -> Self {
        Self {
            buffer: Buffer::new(),
            mode: Mode::Normal,
            prompt: Prompt::None,
            search: SearchState::default(),
            status: Status::info("vorto — :q quit, :w save, <space>f files, <space>l lines"),
            tokens: Vec::new(),
            keymap,
            cursor_shapes: CursorShapes::default(),
            visual_anchor: None,
            should_quit: false,
        }
    }

    /// Current selection range, if the editor is in any visual mode and
    /// an anchor is set. Returns `None` otherwise.
    pub fn selection(&self) -> Option<Selection> {
        let anchor = self.visual_anchor?;
        let cursor = self.buffer.cursor;
        Some(match self.mode {
            Mode::Visual => {
                let (from, to) = if (anchor.row, anchor.col) <= (cursor.row, cursor.col) {
                    (anchor, cursor)
                } else {
                    (cursor, anchor)
                };
                Selection::Char { from, to }
            }
            Mode::VisualLine => Selection::Line {
                from_row: anchor.row.min(cursor.row),
                to_row: anchor.row.max(cursor.row),
            },
            Mode::VisualBlock => Selection::Block {
                r0: anchor.row.min(cursor.row),
                c0: anchor.col.min(cursor.col),
                r1: anchor.row.max(cursor.row),
                c1: anchor.col.max(cursor.col),
            },
            _ => return None,
        })
    }

    pub fn open_path(&mut self, path: &Path) -> Result<()> {
        self.buffer = Buffer::load(path)?;
        self.status = Status::info(format!("opened {}", path.display()));
        Ok(())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.prompt.is_open() {
            return self.handle_prompt_key(key);
        }

        // Global panic button.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        // Insert & Visual modes have small enough surfaces that they're
        // handled directly. The token pipeline is Normal-mode only — that
        // is where the rich operator/motion/text-object grammar lives.
        match self.mode {
            Mode::Insert => return self.handle_insert_key(key),
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                return self.handle_visual_key(key);
            }
            Mode::Normal => {}
        }

        // Normal mode: tokenize → classify → evaluate.
        match self.keymap.tokenize(&self.tokens, self.mode, key) {
            Some(t) => self.tokens.push(t),
            None => {
                self.tokens.clear();
                return Ok(());
            }
        }
        match keymap::classify(&self.tokens) {
            keymap::Parse::Complete(expr) => {
                self.tokens.clear();
                self.evaluate(expr, Ctx::default())?;
            }
            keymap::Parse::Incomplete => {}
            keymap::Parse::Invalid => self.tokens.clear(),
        }
        Ok(())
    }

    fn handle_insert_key(&mut self, key: KeyEvent) -> Result<()> {
        if !key.modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = key.code
        {
            self.buffer.insert_char(c);
            return Ok(());
        }
        match key.code {
            KeyCode::Esc => self.enter_mode(Mode::Normal),
            KeyCode::Enter => self.buffer.insert_newline(),
            KeyCode::Backspace => self.buffer.delete_char_before(),
            KeyCode::Left => self.buffer.move_left(),
            KeyCode::Right => self.buffer.move_right(true),
            KeyCode::Up => self.buffer.move_up(),
            KeyCode::Down => self.buffer.move_down(),
            _ => {}
        }
        Ok(())
    }

    fn handle_visual_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.enter_mode(Mode::Normal),
            KeyCode::Char('h') | KeyCode::Left => self.buffer.move_left(),
            KeyCode::Char('l') | KeyCode::Right => self.buffer.move_right(false),
            KeyCode::Char('j') | KeyCode::Down => self.buffer.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.buffer.move_up(),
            KeyCode::Char('w') => self.buffer.move_word_forward(),
            KeyCode::Char('b') => self.buffer.move_word_backward(),
            KeyCode::Char('0') | KeyCode::Home => self.buffer.move_line_start(),
            KeyCode::Char('$') | KeyCode::End => self.buffer.move_line_end(),
            KeyCode::Char('G') => self.buffer.move_file_end(),
            // Toggle visual sub-modes: pressing the same trigger again
            // exits, a different one switches without losing the anchor.
            KeyCode::Char('v') if !ctrl => self.toggle_visual(Mode::Visual),
            KeyCode::Char('v') if ctrl => self.toggle_visual(Mode::VisualBlock),
            KeyCode::Char('V') => self.toggle_visual(Mode::VisualLine),
            KeyCode::Char('y') => {
                self.apply_visual_op(Operator::Yank);
                self.enter_mode(Mode::Normal);
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                self.buffer.snapshot();
                self.apply_visual_op(Operator::Delete);
                self.enter_mode(Mode::Normal);
            }
            KeyCode::Char('c') => {
                self.buffer.snapshot();
                self.apply_visual_op(Operator::Change);
            }
            _ => {}
        }
        Ok(())
    }

    fn toggle_visual(&mut self, target: Mode) {
        if self.mode == target {
            self.enter_mode(Mode::Normal);
        } else {
            // Switch sub-mode but keep the anchor — pressing `V` from
            // charwise visual should extend the selection line-wise.
            self.mode = target;
        }
    }

    fn apply_visual_op(&mut self, op: Operator) {
        let Some(sel) = self.selection() else { return };
        match sel {
            Selection::Char { from, to } => {
                let end = self.buffer.advance_one(to);
                match op {
                    Operator::Yank => {
                        self.buffer.yank_range(from, end);
                        self.status = Status::info("yanked");
                        self.buffer.cursor = from;
                    }
                    Operator::Delete => self.buffer.delete_range(from, end),
                    Operator::Change => {
                        self.buffer.delete_range(from, end);
                        self.enter_mode(Mode::Insert);
                    }
                }
            }
            Selection::Line { from_row, to_row } => match op {
                Operator::Yank => {
                    self.buffer.yank_lines(from_row, to_row);
                    self.status = Status::info("yanked");
                    self.buffer.cursor.row = from_row;
                    self.buffer.cursor.col = 0;
                }
                Operator::Delete => self.buffer.delete_lines(from_row, to_row),
                Operator::Change => {
                    self.buffer.delete_lines(from_row, to_row);
                    self.enter_mode(Mode::Insert);
                }
            },
            Selection::Block { r0, c0, r1, c1 } => match op {
                Operator::Yank => {
                    self.buffer.yank_block(r0, c0, r1, c1);
                    self.status = Status::info("yanked");
                    self.buffer.cursor = Cursor { row: r0, col: c0 };
                }
                Operator::Delete => self.buffer.delete_block(r0, c0, r1, c1),
                Operator::Change => {
                    self.buffer.delete_block(r0, c0, r1, c1);
                    self.enter_mode(Mode::Insert);
                }
            },
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if key.code == KeyCode::Esc || ctrl_c {
            self.prompt = Prompt::None;
            return Ok(());
        }
        if key.code == KeyCode::Enter {
            return self.submit_prompt();
        }

        match &mut self.prompt {
            Prompt::None => unreachable!("prompt routing checked is_open"),
            Prompt::Command(buf) => match key.code {
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            Prompt::Search { query, .. } => match key.code {
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Char(c) => query.push(c),
                _ => {}
            },
            Prompt::Fuzzy(finder) => match key.code {
                KeyCode::Backspace => finder.pop(),
                KeyCode::Up => finder.prev(),
                KeyCode::Down => finder.next(),
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    finder.next()
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    finder.prev()
                }
                KeyCode::Char(c) => finder.push(c),
                _ => {}
            },
        }
        Ok(())
    }

    fn submit_prompt(&mut self) -> Result<()> {
        let prompt = std::mem::replace(&mut self.prompt, Prompt::None);
        match prompt {
            Prompt::None => Ok(()),
            Prompt::Command(line) => self.execute_command(line.trim()),
            Prompt::Search { forward, query } => {
                self.search.set(query, forward);
                if let Some(c) = self.search.find_next(&self.buffer, forward) {
                    self.buffer.cursor = c;
                } else {
                    self.status = Status::error("pattern not found");
                }
                Ok(())
            }
            Prompt::Fuzzy(finder) => self.submit_fuzzy(finder),
        }
    }

    fn submit_fuzzy(&mut self, finder: Finder) -> Result<()> {
        let sel = match finder.selection() {
            Some(s) => s.clone(),
            None => return Ok(()),
        };
        match finder.kind {
            FuzzyKind::Files => {
                let path = finder.items[sel.idx].clone();
                self.open_path(Path::new(&path))?;
            }
            FuzzyKind::Lines => {
                self.buffer.cursor.row = sel.idx;
                self.buffer.cursor.col = 0;
                self.buffer.clamp_col(false);
            }
        }
        Ok(())
    }

    fn execute_command(&mut self, cmd: &str) -> Result<()> {
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

    // ────────────────────────────────────────────────────────────────────
    // Evaluate
    // ────────────────────────────────────────────────────────────────────

    fn evaluate(&mut self, expr: Expr, ctx: Ctx) -> Result<()> {
        // Take an undo snapshot before any Expr that's going to change
        // the buffer (or kick off an Insert-mode session). Pure cursor
        // moves and yanks intentionally don't snapshot.
        if Self::expr_modifies_buffer(&expr) {
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

    // ────────────────────────────────────────────────────────────────────
    // Helpers
    // ────────────────────────────────────────────────────────────────────

    /// Apply an operator over the range [start, end). Used by Op + Motion
    /// targets — the motion already produced the endpoint cursor.
    fn apply_op_range(
        &mut self,
        op: Operator,
        start: crate::editor::Cursor,
        end: crate::editor::Cursor,
    ) {
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
        Ok(())
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

    fn enter_mode(&mut self, mode: Mode) {
        // Set or clear the visual anchor at the mode boundary. Entering
        // any visual mode pins the anchor to the current cursor;
        // entering Normal/Insert drops it.
        if mode.is_visual() && !self.mode.is_visual() {
            self.visual_anchor = Some(self.buffer.cursor);
        } else if !mode.is_visual() {
            self.visual_anchor = None;
        }
        if mode == Mode::Normal {
            self.buffer.clamp_col(false);
        }
        self.mode = mode;
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        self.prompt = match kind {
            PromptKind::Command => Prompt::Command(String::new()),
            PromptKind::Search { forward } => Prompt::Search {
                forward,
                query: String::new(),
            },
            PromptKind::Fuzzy(FuzzyKind::Files) => Prompt::Fuzzy(Finder::files(Path::new("."))),
            PromptKind::Fuzzy(FuzzyKind::Lines) => Prompt::Fuzzy(Finder::lines(&self.buffer.lines)),
        };
    }
}

// ════════════════════════════════════════════════════════════════════════
// `:` command table
// ════════════════════════════════════════════════════════════════════════

pub struct CommandBind {
    pub name: &'static str,
    pub description: &'static str,
    pub kind: DirectKind,
}

impl CommandBind {
    pub fn find(name: &str) -> Option<&'static CommandBind> {
        COMMAND_BINDS.iter().find(|b| b.name == name)
    }
}

pub const COMMAND_BINDS: &[CommandBind] = &[
    CommandBind {
        name: "q",
        description: "quit",
        kind: DirectKind::Quit,
    },
    CommandBind {
        name: "q!",
        description: "force quit",
        kind: DirectKind::QuitForce,
    },
    CommandBind {
        name: "w",
        description: "save (or :w <path>)",
        kind: DirectKind::Save,
    },
    CommandBind {
        name: "wq",
        description: "save & quit",
        kind: DirectKind::SaveAndQuit,
    },
    CommandBind {
        name: "x",
        description: "save & quit",
        kind: DirectKind::SaveAndQuit,
    },
    CommandBind {
        name: "e",
        description: "open <path>",
        kind: DirectKind::Open,
    },
    CommandBind {
        name: "goto",
        description: "go to line <n>",
        kind: DirectKind::GotoLine,
    },
];
