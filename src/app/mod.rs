mod eval;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Result, anyhow};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Ctx, DirectKind, Operator, PromptKind, Token};
use crate::config::CursorShapes;
use crate::editor::{Buffer, Cursor};
use crate::fuzzy::FuzzyKind;
use crate::highlight::Loader;
use crate::keymap::{self, Keymap};
use crate::languages::Language;
use crate::lsp::{
    self, Diagnostic, Location, LspCoordinator, LspEvent, LspEventOutcome, WorkspaceEdit,
};
use crate::mode::Mode;
use crate::prompt::{PromptController, PromptOutcome};
use crate::search::SearchState;

pub use crate::prompt::Prompt;

/// Unified event flowing into the main loop. Terminal input and LSP
/// reader threads both feed into the same `mpsc::Sender`.
pub enum AppEvent {
    Term(Event),
    Lsp(LspEvent),
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
    pub prompt: PromptController,
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
    /// Tree-sitter grammar loader. Lives for the whole program so the
    /// loaded `Language` pointers stay valid.
    pub loader: Loader,
    /// Resolved language registry (built-in defaults overlaid with
    /// `[languages.<name>]` from the user's config).
    pub languages: HashMap<String, Language>,
    /// `ext -> language name` lookup, built once from `languages`.
    pub extension_index: HashMap<String, String>,
    /// Working directory captured once at process startup. All workspace
    /// root discovery anchors here — `:e` opened mid-session still uses
    /// the same anchor as the file passed on the command line.
    pub startup_cwd: PathBuf,
    /// All LSP state — clients, current document, diagnostics, pending
    /// requests, sync version, root anchor. See [`LspCoordinator`].
    pub lsp: LspCoordinator,
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
    pub fn new(
        keymap: Keymap,
        loader: Loader,
        languages: HashMap<String, Language>,
        extension_index: HashMap<String, String>,
        event_tx: Sender<AppEvent>,
        startup_cwd: PathBuf,
    ) -> Self {
        let lsp = LspCoordinator::new(event_tx, startup_cwd.clone());
        Self {
            buffer: Buffer::new(),
            mode: Mode::Normal,
            prompt: PromptController::new(),
            search: SearchState::default(),
            status: Status::info("vorto — :q quit, :w save, <space>f files, <space>l lines"),
            tokens: Vec::new(),
            keymap,
            cursor_shapes: CursorShapes::default(),
            visual_anchor: None,
            loader,
            languages,
            extension_index,
            startup_cwd,
            lsp,
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
        // Tell the previous LSP client we're done with that document so
        // it can drop diagnostics and stop watching it.
        self.lsp.detach_current();
        self.buffer = Buffer::load(path)?;
        self.attach_highlighter();
        self.attach_lsp();
        self.lsp.set_last_synced_version(self.buffer.version);
        self.status = Status::info(format!("opened {}", path.display()));
        Ok(())
    }

    /// Resolve the language for the current buffer, spawn an `LspClient`
    /// if one isn't already running, and send `didOpen`. Failures are
    /// surfaced via the status bar — the buffer keeps working without LSP.
    fn attach_lsp(&mut self) {
        let Some(path) = self.buffer.path.clone() else {
            return;
        };
        let Some(ext) = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            return;
        };
        let Some(lang_name) = self.extension_index.get(&ext).cloned() else {
            return;
        };
        let Some(spec) = self.languages.get(&lang_name).cloned() else {
            return;
        };
        let Some(lsp_cfg) = spec.lsp else { return };

        if let Err(e) = self.lsp.ensure_client(&lang_name, &lsp_cfg, &path) {
            // Built-in defaults reference servers most users won't have
            // installed. If the binary isn't on PATH, stay quiet — the
            // buffer keeps working without LSP. Other failures
            // (handshake errors, etc.) are still surfaced.
            if !is_command_not_found(&e) {
                self.status = Status::error(format!("lsp ({}): {}", lang_name, root_cause(&e)));
            }
            return;
        }
        let text = self.buffer.lines.join("\n");
        if let Err(e) = self.lsp.did_open(&lang_name, &path, &text) {
            self.status = Status::error(format!("lsp didOpen ({}): {}", lang_name, root_cause(&e)));
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // LSP actions
    // ────────────────────────────────────────────────────────────────────

    /// Send a request whose result is a list of `Location`s and whose
    /// expected handling is "jump to the first one". Covers
    /// `definition`, `declaration`, and `implementation` — all three
    /// answer with the same shape.
    fn lsp_jump(&mut self, method: &str, label: &'static str) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        if let Err(e) = self.lsp.request_jump(method, label, self.buffer.cursor) {
            self.status = Status::error(format!("lsp {}: {}", method, root_cause(&e)));
        }
    }

    fn lsp_find_references(&mut self) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        if let Err(e) = self.lsp.request_references(self.buffer.cursor) {
            self.status = Status::error(format!("lsp references: {}", root_cause(&e)));
        }
    }

    fn open_rename_prompt(&mut self) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        self.prompt.open_rename();
    }

    fn submit_rename(&mut self, new_name: String) {
        if new_name.is_empty() {
            self.status = Status::error("rename: empty name");
            return;
        }
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        if let Err(e) = self.lsp.request_rename(new_name, self.buffer.cursor) {
            self.status = Status::error(format!("lsp rename: {}", root_cause(&e)));
        }
    }

    fn apply_jump_outcome(&mut self, label: &'static str, locations: Vec<Location>) {
        let Some(first) = locations.into_iter().next() else {
            self.status = Status::info(format!("no {}", label));
            return;
        };
        if let Err(e) = self.jump_to_location(&first) {
            self.status = Status::error(format!("jump: {}", root_cause(&e)));
        }
    }

    fn apply_references_outcome(&mut self, locations: Vec<Location>) {
        if locations.is_empty() {
            self.status = Status::info("no references");
            return;
        }
        if locations.len() == 1 {
            if let Err(e) = self.jump_to_location(&locations[0]) {
                self.status = Status::error(format!("jump: {}", root_cause(&e)));
            }
            return;
        }
        let items: Vec<String> = locations
            .iter()
            .map(|loc| format_location_label(loc, &self.startup_cwd))
            .collect();
        self.prompt.open_locations(items, locations);
    }

    fn apply_rename_outcome(&mut self, new_name: String, edit: Option<WorkspaceEdit>) {
        let Some(edit) = edit else {
            self.status = Status::info("rename: nothing to change");
            return;
        };
        match self.lsp.apply_workspace_edit(edit) {
            Ok(result) => {
                if !result.current_buffer_edits.is_empty() {
                    self.buffer.snapshot();
                    let mut lines = std::mem::take(&mut self.buffer.lines);
                    lsp::apply_text_edits(&mut lines, result.current_buffer_edits);
                    self.buffer.lines = lines;
                    self.buffer.bump_version();
                    self.buffer.dirty = true;
                }
                self.status = Status::info(format!(
                    "renamed to {} ({} occurrences in {} files)",
                    new_name, result.total_edits, result.files_touched
                ));
            }
            Err(e) => {
                self.status = Status::error(format!("rename: {}", root_cause(&e)));
            }
        }
    }

    fn jump_to_location(&mut self, loc: &Location) -> Result<()> {
        let path = lsp::uri_to_path(&loc.uri)
            .ok_or_else(|| anyhow!("unsupported uri scheme: {}", loc.uri))?;
        let need_open = match &self.buffer.path {
            Some(p) => p.canonicalize().ok() != path.canonicalize().ok(),
            None => true,
        };
        if need_open {
            self.open_path(&path)?;
        }
        let row = loc.range.start.line as usize;
        let col = loc.range.start.character as usize;
        let last = self.buffer.lines.len().saturating_sub(1);
        self.buffer.cursor.row = row.min(last);
        self.buffer.cursor.col = col;
        self.buffer.clamp_col(false);
        Ok(())
    }

    /// Send `didChange` if the buffer has been mutated since the last
    /// sync. Called from the main loop after every key handled.
    pub fn sync_buffer_if_dirty(&mut self) {
        if self.buffer.version == self.lsp.last_synced_version() {
            return;
        }
        self.lsp.set_last_synced_version(self.buffer.version);
        let text = self.buffer.lines.join("\n");
        if let Err(e) = self.lsp.did_change(&text) {
            self.status = Status::error(format!("lsp didChange: {}", root_cause(&e)));
        }
    }

    /// Apply an event from an LSP reader thread. Diagnostics replace
    /// whatever we had stored for that URI; messages are surfaced as
    /// non-error status; reader errors do the same.
    pub fn handle_lsp_event(&mut self, ev: LspEvent) {
        match self.lsp.handle_event(ev) {
            LspEventOutcome::Nothing => {}
            LspEventOutcome::InfoMessage(s) => self.status = Status::info(s),
            LspEventOutcome::ErrorMessage(s) => self.status = Status::error(s),
            LspEventOutcome::Jump { label, locations } => self.apply_jump_outcome(label, locations),
            LspEventOutcome::References(locations) => self.apply_references_outcome(locations),
            LspEventOutcome::Rename { new_name, edit } => self.apply_rename_outcome(new_name, edit),
        }
    }

    /// Diagnostics for the current buffer's URI, if any. Convenience for
    /// the UI layer.
    pub fn current_diagnostics(&self) -> Option<&[Diagnostic]> {
        self.lsp.current_diagnostics()
    }

    /// First diagnostic that covers the cursor row, prioritising errors.
    pub fn diagnostic_on_cursor(&self) -> Option<&Diagnostic> {
        self.lsp.diagnostic_on_cursor(self.buffer.cursor.row as u32)
    }

    /// Look up a language for the current buffer's file extension and,
    /// when one matches, attach a fresh [`Highlighter`]. Failures are
    /// surfaced as a non-fatal status message; the buffer keeps working
    /// without syntax highlighting.
    fn attach_highlighter(&mut self) {
        self.buffer.highlighter = None;
        let ext = self
            .buffer
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        let Some(ext) = ext else { return };
        let Some(name) = self.extension_index.get(&ext).cloned() else {
            return;
        };
        let Some(spec) = self.languages.get(&name).cloned() else {
            return;
        };
        match self.loader.highlighter_for(&spec) {
            Ok(h) => self.buffer.highlighter = Some(h),
            Err(e) => {
                self.status = Status::error(format!("highlight ({}): {}", name, root_cause(&e)));
            }
        }
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
        let outcome = self.prompt.handle_key(key);
        self.apply_prompt_outcome(outcome)
    }

    fn apply_prompt_outcome(&mut self, outcome: PromptOutcome) -> Result<()> {
        match outcome {
            PromptOutcome::Nothing | PromptOutcome::Cancelled => Ok(()),
            PromptOutcome::RunCommand(line) => self.execute_command(&line),
            PromptOutcome::Search { forward, query } => {
                self.search.set(query, forward);
                if let Some(c) = self.search.find_next(&self.buffer, forward) {
                    self.buffer.cursor = c;
                } else {
                    self.status = Status::error("pattern not found");
                }
                Ok(())
            }
            PromptOutcome::OpenRelativeFile(rel) => {
                // Items are root-relative paths (see `collect_files`). Re-
                // anchor against `startup_cwd` so the resulting buffer
                // path doesn't depend on whatever `current_dir()` is now.
                let path = self.startup_cwd.join(rel);
                self.open_path(&path)
            }
            PromptOutcome::GotoLine(row) => {
                self.buffer.cursor.row = row;
                self.buffer.cursor.col = 0;
                self.buffer.clamp_col(false);
                Ok(())
            }
            PromptOutcome::JumpToLocation(loc) => {
                if let Err(e) = self.jump_to_location(&loc) {
                    self.status = Status::error(format!("jump: {}", root_cause(&e)));
                }
                Ok(())
            }
            PromptOutcome::SubmitRename(new_name) => {
                self.submit_rename(new_name);
                Ok(())
            }
        }
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
        match kind {
            PromptKind::Command => self.prompt.open_command(),
            PromptKind::Search { forward } => self.prompt.open_search(forward),
            PromptKind::Fuzzy(FuzzyKind::Files) => self.prompt.open_files(&self.startup_cwd),
            PromptKind::Fuzzy(FuzzyKind::Lines) => self.prompt.open_lines(&self.buffer.lines),
            // `Locations` pickers are built from server results, not opened
            // from a keymap — fall through to a no-op rather than a fresh
            // empty picker that would do nothing useful on submit.
            PromptKind::Fuzzy(FuzzyKind::Locations) => {}
        }
    }
}

/// Render a `path:line:col` label for an LSP `Location`. Used to
/// populate the references picker. Falls back to the URI when the path
/// can't be made relative.
fn format_location_label(loc: &Location, root: &Path) -> String {
    let path = match lsp::uri_to_path(&loc.uri) {
        Some(p) => p,
        None => return loc.uri.clone(),
    };
    // Canonicalize both sides so symlinked or /private-prefixed paths
    // still compare equal — otherwise nothing strips and every label
    // shows an absolute path.
    let path_c = path.canonicalize().unwrap_or_else(|_| path.clone());
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let shown = path_c
        .strip_prefix(&root_c)
        .unwrap_or(&path_c)
        .to_string_lossy()
        .into_owned();
    format!(
        "{}:{}:{}",
        shown,
        loc.range.start.line + 1,
        loc.range.start.character + 1
    )
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

/// Walk an anyhow error chain to its innermost cause — keeps the
/// status-bar message focused on the actual filesystem / parser error
/// rather than the wrapping context.
fn root_cause(e: &anyhow::Error) -> String {
    e.chain()
        .last()
        .map(|x| x.to_string())
        .unwrap_or_else(|| e.to_string())
}

/// True if the error chain contains an `io::Error` with `NotFound` kind —
/// i.e. the LSP server binary isn't on `PATH`. Lets us silently skip
/// built-in defaults the user hasn't installed.
fn is_command_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
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
