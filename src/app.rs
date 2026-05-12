use std::path::Path;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Ctx, DirectKind, Expr, MotionExpr, MotionKind, Operator, PromptKind, Target, Token};
use crate::editor::Buffer;
use crate::fuzzy::{Finder, FuzzyKind};
use crate::keymap;
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
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            buffer: Buffer::new(),
            mode: Mode::Normal,
            prompt: Prompt::None,
            search: SearchState::default(),
            status: Status::info("vorto — :q quit, :w save, <space>f files, <space>l lines"),
            tokens: Vec::new(),
            should_quit: false,
        }
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
            Mode::Visual => return self.handle_visual_key(key),
            Mode::Normal => {}
        }

        // Normal mode: tokenize → classify → evaluate.
        match keymap::tokenize(&self.tokens, self.mode, key) {
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
        match key.code {
            KeyCode::Esc => self.enter_mode(Mode::Normal),
            KeyCode::Char('h') | KeyCode::Left => self.buffer.move_left(),
            KeyCode::Char('l') | KeyCode::Right => self.buffer.move_right(false),
            KeyCode::Char('j') | KeyCode::Down => self.buffer.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.buffer.move_up(),
            KeyCode::Char('y') => {
                self.buffer.yank_line();
                self.status = Status::info("yanked");
                self.enter_mode(Mode::Normal);
            }
            _ => {}
        }
        Ok(())
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
                self.status = Status::error("undo not implemented yet");
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
            M::Left => for _ in 0..n { self.buffer.move_left(); },
            M::Right => for _ in 0..n { self.buffer.move_right(allow_after); },
            M::Up => for _ in 0..n { self.buffer.move_up(); },
            M::Down => for _ in 0..n { self.buffer.move_down(); },
            M::LineStart => self.buffer.move_line_start(),
            M::LineEnd => self.buffer.move_line_end(),
            M::WordForward => for _ in 0..n { self.buffer.move_word_forward(); },
            M::WordBack => for _ in 0..n { self.buffer.move_word_backward(); },
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
            M::SearchNext => for _ in 0..n { self.jump_search(self.search.last_forward); },
            M::SearchPrev => for _ in 0..n { self.jump_search(!self.search.last_forward); },
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
            Target::Motion(_) => {
                self.status = Status::error("operator + motion not implemented yet (Stage 2)");
                Ok(())
            }
            Target::TextObject { .. } => {
                self.status = Status::error("text objects not implemented yet (Stage 3)");
                Ok(())
            }
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Helpers
    // ────────────────────────────────────────────────────────────────────

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
    CommandBind { name: "q",    description: "quit",                kind: DirectKind::Quit },
    CommandBind { name: "q!",   description: "force quit",          kind: DirectKind::QuitForce },
    CommandBind { name: "w",    description: "save (or :w <path>)", kind: DirectKind::Save },
    CommandBind { name: "wq",   description: "save & quit",         kind: DirectKind::SaveAndQuit },
    CommandBind { name: "x",    description: "save & quit",         kind: DirectKind::SaveAndQuit },
    CommandBind { name: "e",    description: "open <path>",         kind: DirectKind::Open },
    CommandBind { name: "goto", description: "go to line <n>",      kind: DirectKind::GotoLine },
];
