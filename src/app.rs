use std::path::Path;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Action, BufferAction, Ctx, PromptKind, WorkspaceAction};
use crate::editor::Buffer;
use crate::fuzzy::{Finder, FuzzyKind};
use crate::keymap;
use crate::mode::Mode;
use crate::search::SearchState;

/// A modal overlay over the editor. While `Prompt` is anything other than
/// `None`, `App::handle_key` routes keys to the prompt rather than the
/// underlying mode — so the mode (Normal/Insert/Visual) stays focused on
/// editor key-interpretation and prompts are just transient UI state.
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
/// variants in red so the user can tell feedback apart from problems.
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
    pub pending: Option<char>,
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
            pending: None,
            should_quit: false,
        }
    }

    pub fn open_path(&mut self, path: &Path) -> Result<()> {
        self.buffer = Buffer::load(path)?;
        self.status = Status::info(format!("opened {}", path.display()));
        Ok(())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        if self.prompt.is_open() {
            return self.handle_prompt_key(key);
        }

        // Insert mode: bare char input goes straight to the buffer — no
        // Action wrapping for what is essentially raw text data.
        if matches!(self.mode, Mode::Insert) && !key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c) = key.code {
                self.buffer.insert_char(c);
                return Ok(());
            }
        }

        let lead = keymap::is_pending_lead(self.mode, key, self.pending);
        let actions = keymap::translate(self.mode, key, self.pending.take());

        if actions.is_empty() {
            if let Some(c) = lead {
                self.pending = Some(c);
            }
            return Ok(());
        }

        for action in actions {
            self.dispatch(action, Ctx::default())?;
        }
        Ok(())
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.prompt = Prompt::None;
                return Ok(());
            }
            KeyCode::Enter => return self.submit_prompt(),
            _ => {}
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
        let (head, rest) = match cmd.split_once(' ') {
            Some((h, r)) => (h, r.trim()),
            None => (cmd, ""),
        };
        if head.is_empty() {
            return Ok(());
        }
        match CommandBind::find(head) {
            Some(b) => self.dispatch(b.action, Ctx::with_rest(rest)),
            None => {
                self.status = Status::error(format!("unknown command: {}", head));
                Ok(())
            }
        }
    }

    /// Run an Action with the given context. This is the single entry point
    /// for all semantic operations — both keyboard- and command-driven
    /// inputs funnel through here.
    pub fn dispatch(&mut self, action: Action, ctx: Ctx) -> Result<()> {
        match action {
            Action::Buffer(a) => self.apply_buffer(a),
            Action::Workspace(a) => self.apply_workspace(a, ctx)?,
            Action::EnterMode(m) => self.enter_mode(m),
            Action::OpenPrompt(kind) => self.open_prompt(kind),
            Action::Quit => {
                if self.buffer.dirty {
                    self.status = Status::error("unsaved changes (use :q!)");
                } else {
                    self.should_quit = true;
                }
            }
            Action::QuitForce => self.should_quit = true,
            Action::SaveAndQuit => {
                self.apply_workspace(WorkspaceAction::Save, Ctx::default())?;
                self.should_quit = true;
            }
            Action::OpenLineBelow => {
                self.buffer.insert_line_below();
                self.enter_mode(Mode::Insert);
            }
            Action::OpenLineAbove => {
                self.buffer.insert_line_above();
                self.enter_mode(Mode::Insert);
            }
        }
        Ok(())
    }

    fn apply_buffer(&mut self, action: BufferAction) {
        use BufferAction as B;
        let allow_after = matches!(self.mode, Mode::Insert);
        match action {
            B::MoveLeft => self.buffer.move_left(),
            B::MoveRight => self.buffer.move_right(allow_after),
            B::MoveUp => self.buffer.move_up(),
            B::MoveDown => self.buffer.move_down(),
            B::MoveLineStart => self.buffer.move_line_start(),
            B::MoveLineEnd => self.buffer.move_line_end(),
            B::MoveFileStart => self.buffer.move_file_start(),
            B::MoveFileEnd => self.buffer.move_file_end(),
            B::MoveWordForward => self.buffer.move_word_forward(),
            B::MoveWordBackward => self.buffer.move_word_backward(),
            B::InsertNewline => self.buffer.insert_newline(),
            B::DeleteCharUnderCursor => self.buffer.delete_char_under_cursor(),
            B::DeleteCharBefore => self.buffer.delete_char_before(),
            B::DeleteLine => self.buffer.delete_line(),
            B::Yank => {
                self.buffer.yank_line();
                self.status = Status::info("yanked");
            }
            B::Paste => self.buffer.paste_after(),
            B::Undo => {
                self.status = Status::error("undo not implemented yet");
            }
            B::SearchNext => self.jump_search(self.search.last_forward),
            B::SearchPrev => self.jump_search(!self.search.last_forward),
        }
    }

    fn jump_search(&mut self, forward: bool) {
        if let Some(c) = self.search.find_next(&self.buffer, forward) {
            self.buffer.cursor = c;
        } else {
            self.status = Status::error("pattern not found");
        }
    }

    fn apply_workspace(&mut self, action: WorkspaceAction, ctx: Ctx) -> Result<()> {
        match action {
            WorkspaceAction::Save => {
                if ctx.rest.is_empty() {
                    if self.buffer.path.is_some() {
                        self.buffer.save()?;
                        self.status = Status::info("written");
                    } else {
                        self.status = Status::error("no file name (use :w <path>)");
                    }
                } else {
                    let p = Path::new(ctx.rest);
                    self.buffer.save_as(p)?;
                    self.status = Status::info(format!("written to {}", p.display()));
                }
            }
            WorkspaceAction::Open => {
                if ctx.rest.is_empty() {
                    self.status = Status::error("missing path");
                } else {
                    self.open_path(Path::new(ctx.rest))?;
                }
            }
        }
        Ok(())
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

/// A `:` command binding. Pure data: name + description (for hint UI) + the
/// `Action` it dispatches. Path arguments and the like flow through `Ctx`
/// at dispatch time, not through this table.
pub struct CommandBind {
    pub name: &'static str,
    pub description: &'static str,
    pub action: Action,
}

impl CommandBind {
    pub fn find(name: &str) -> Option<&'static CommandBind> {
        COMMAND_BINDS.iter().find(|b| b.name == name)
    }
}

pub const COMMAND_BINDS: &[CommandBind] = &[
    CommandBind { name: "q",  description: "quit",                action: Action::Quit },
    CommandBind { name: "q!", description: "force quit",          action: Action::QuitForce },
    CommandBind { name: "w",  description: "save (or :w <path>)", action: Action::Workspace(WorkspaceAction::Save) },
    CommandBind { name: "wq", description: "save & quit",         action: Action::SaveAndQuit },
    CommandBind { name: "x",  description: "save & quit",         action: Action::SaveAndQuit },
    CommandBind { name: "e",  description: "open <path>",         action: Action::Workspace(WorkspaceAction::Open) },
];
