mod eval;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Ctx, Operator, PromptKind, Token};
use crate::config::Config;
use crate::editor::{Buffer, Cursor};
use crate::event::AppEvent;
use crate::fuzzy::FuzzyKind;
use crate::highlight::{Highlighter, Loader};
use crate::preview::{self, PreviewEntry, PreviewLru};
use crate::lsp::{
    self, Diagnostic, Location, LspClient, LspCoordinator, LspEvent, LspEventOutcome,
    WorkspaceEdit,
};
use crate::mode::Mode;
use crate::prompt::{PromptController, PromptOutcome};
use crate::search::SearchState;

pub use crate::prompt::Prompt;

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
    /// Anchor cursor for visual modes — the position the selection was
    /// started from. `None` outside of any visual mode.
    pub visual_anchor: Option<Cursor>,
    /// Resolved user configuration (keymap, cursor shapes, language
    /// registry, grammar/query dirs). Frozen at startup.
    pub config: Config,
    /// Tree-sitter grammar loader. Lives for the whole program so the
    /// loaded `Language` pointers stay valid. Wrapped in `Arc<Mutex>` so
    /// the file-open worker thread can build a fresh highlighter off the
    /// main thread, and the fuzzy-finder preview can still lazily build
    /// a separate highlighter for the file under the cursor during the
    /// (otherwise `&App`) draw pass.
    pub loader: Arc<Mutex<Loader>>,
    /// Bounded LRU of fuzzy-finder source previews. The worker thread
    /// fills this asynchronously through `AppEvent::PreviewReady`; the
    /// draw path looks here first and falls back to plain text on miss
    /// (while enqueueing a worker request). Living on `App` so back-
    /// to-back navigation to the same file is instant.
    pub preview_lru: RefCell<PreviewLru>,
    /// Request channel feeding the preview worker. Cloned on draw to
    /// dispatch "build preview for path X" jobs.
    pub preview_tx: std::sync::mpsc::Sender<PathBuf>,
    /// Last path we asked the worker about. Prevents the draw loop from
    /// flooding the channel with duplicate requests for the same
    /// selection while the worker is still busy.
    pub last_preview_request: RefCell<Option<PathBuf>>,
    /// Working directory captured once at process startup. All workspace
    /// root discovery anchors here — `:e` opened mid-session still uses
    /// the same anchor as the file passed on the command line.
    pub startup_cwd: PathBuf,
    /// All LSP state — clients, current document, diagnostics, pending
    /// requests, sync version, root anchor. See [`LspCoordinator`].
    pub lsp: LspCoordinator,
    /// Shared event channel — kept on `App` so `open_path` can spawn
    /// worker threads that report `HighlighterReady` / `LspReady` back
    /// to the main loop without going through the LSP coordinator.
    pub event_tx: Sender<AppEvent>,
    /// Monotonic counter bumped on every `open_path`. Worker threads
    /// stamp their result with the generation they were spawned for; a
    /// stale result (user opened another file in the meantime) gets
    /// dropped instead of clobbering the current buffer.
    pub open_gen: u64,
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
        config: Config,
        loader: Loader,
        event_tx: Sender<AppEvent>,
        startup_cwd: PathBuf,
    ) -> Self {
        let lsp = LspCoordinator::new(event_tx.clone(), startup_cwd.clone());
        let loader = Arc::new(Mutex::new(loader));
        let (preview_tx, preview_rx) = std::sync::mpsc::channel::<PathBuf>();
        // Spawn the fuzzy-finder preview worker. It owns the receiver,
        // clones of `loader` and the language registry, and an `emit`
        // closure that wraps results in `AppEvent::PreviewReady` so the
        // main loop just inserts them into the LRU on dispatch.
        let preview_emit_tx = event_tx.clone();
        preview::spawn_preview_worker(
            Arc::clone(&loader),
            config.languages.clone(),
            preview_rx,
            Box::new(move |entry| {
                let _ = preview_emit_tx.send(AppEvent::PreviewReady(entry));
            }),
        );
        Self {
            buffer: Buffer::new(),
            mode: Mode::Normal,
            prompt: PromptController::new(),
            search: SearchState::default(),
            status: Status::info("vorto — :q quit, :w save, <space>f files, <space>l lines"),
            tokens: Vec::new(),
            visual_anchor: None,
            config,
            loader,
            preview_lru: RefCell::new(PreviewLru::new(16)),
            preview_tx,
            last_preview_request: RefCell::new(None),
            startup_cwd,
            lsp,
            event_tx,
            open_gen: 0,
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
        // Bump the generation: any in-flight worker thread from a
        // previous `open_path` is now stale. Its result will be dropped
        // when it lands instead of clobbering this buffer.
        self.open_gen = self.open_gen.wrapping_add(1);
        // Pre-seed the LSP sync version so the first `didChange` after
        // open is a no-op when nothing has changed since load.
        self.lsp.set_last_synced_version(self.buffer.version);
        self.status = Status::info(format!("opened {}", path.display()));
        // If the fuzzy preview worker already built a highlighter for
        // this path, steal it: we're about to render the buffer and the
        // tree is ready right now. Saves a worker round-trip and the
        // "plain text → highlighted" flash. Re-`refresh` against the
        // buffer's source/version so the cached tree's incremental diff
        // re-anchors on whatever `Buffer::load` just read (usually a
        // no-op because the file hasn't changed since the preview ran).
        if let Some(entry) = self.preview_lru.borrow_mut().take(path) {
            self.buffer.highlighter = None;
            let mut h = entry.highlighter;
            let source = self.buffer.lines.join("\n");
            h.refresh(&source, self.buffer.version);
            self.buffer.highlighter = Some(h);
        } else {
            self.spawn_highlighter_worker(path);
        }
        self.spawn_lsp_worker(path);
        Ok(())
    }

    /// Build a tree-sitter `Highlighter` for `path` off the main thread
    /// (grammar dlopen + query compile + initial full parse). The result
    /// arrives via [`AppEvent::HighlighterReady`] and is installed on the
    /// buffer in [`Self::handle_highlighter_ready`] when the generation
    /// still matches.
    fn spawn_highlighter_worker(&mut self, path: &Path) {
        self.buffer.highlighter = None;
        let Some(ext) = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            return;
        };
        let Some(spec) = self.config.languages.by_extension(&ext).cloned() else {
            return;
        };
        let loader = Arc::clone(&self.loader);
        let tx = self.event_tx.clone();
        let generation = self.open_gen;
        // Snapshot the source we'll parse against. The user might edit
        // the buffer while the worker runs; we recover by re-`refresh`-
        // ing on the main thread when the highlighter arrives.
        let source = self.buffer.lines.join("\n");
        let buffer_version = self.buffer.version;
        thread::spawn(move || {
            let result = (|| -> Result<Highlighter> {
                let mut h = loader.lock().unwrap().highlighter_for(&spec)?;
                h.refresh(&source, buffer_version);
                Ok(h)
            })();
            let _ = tx.send(AppEvent::HighlighterReady { generation, result });
        });
    }

    /// Spawn the LSP server + run its `initialize` handshake off the
    /// main thread. When the same language already has a client, just
    /// fire `didOpen` inline (cheap — no process spawn). Otherwise the
    /// finished client arrives via [`AppEvent::LspReady`].
    fn spawn_lsp_worker(&mut self, path: &Path) {
        let Some(ext) = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            return;
        };
        let Some(spec) = self.config.languages.by_extension(&ext).cloned() else {
            return;
        };
        let Some(lsp_cfg) = spec.lsp else { return };
        let lang_name = spec.name;

        if self.lsp.has_client(&lang_name) {
            let text = self.buffer.lines.join("\n");
            if let Err(e) = self.lsp.did_open(&lang_name, path, &text) {
                self.status =
                    Status::error(format!("lsp didOpen ({}): {}", lang_name, root_cause(&e)));
            }
            return;
        }

        let tx = self.event_tx.clone();
        let emit = self.lsp.make_emit();
        let startup_cwd = self.lsp.startup_cwd().to_path_buf();
        let generation = self.open_gen;
        let path_buf = path.to_path_buf();

        thread::spawn(move || {
            let root_dir =
                lsp::discover_root(&startup_cwd, Some(&path_buf), &lsp_cfg.root_markers);
            let root_uri = lsp::path_to_uri(&root_dir);
            let result = LspClient::spawn(&lang_name, &lsp_cfg, &root_uri, emit);
            let _ = tx.send(AppEvent::LspReady {
                generation,
                lang: lang_name,
                path: path_buf,
                result,
            });
        });
    }

    /// Install a freshly-built highlighter on the active buffer. Dropped
    /// when `generation` doesn't match — the user opened another file
    /// while the worker was running.
    pub fn handle_highlighter_ready(
        &mut self,
        generation: u64,
        result: Result<Highlighter>,
    ) {
        if generation != self.open_gen {
            return;
        }
        match result {
            Ok(mut h) => {
                // The user may have edited the buffer while the worker
                // was parsing the snapshot we handed it. Re-`refresh`
                // here so the tree matches the live source.
                if self.buffer.version != 0 {
                    let source = self.buffer.lines.join("\n");
                    h.refresh(&source, self.buffer.version);
                }
                self.buffer.highlighter = Some(h);
            }
            Err(e) => {
                self.status = Status::error(format!("highlight: {}", root_cause(&e)));
            }
        }
    }

    /// Adopt a freshly-spawned LSP client and send the deferred
    /// `didOpen`. Dropped when `generation` doesn't match — the freshly
    /// spawned client gets dropped here, which closes its stdin and
    /// shuts the server down.
    pub fn handle_lsp_ready(
        &mut self,
        generation: u64,
        lang: String,
        path: PathBuf,
        result: Result<LspClient>,
    ) {
        if generation != self.open_gen {
            return;
        }
        let client = match result {
            Ok(c) => c,
            Err(e) => {
                // Built-in defaults reference servers most users won't
                // have installed. Stay quiet when the binary isn't on
                // PATH; surface every other failure.
                if !is_command_not_found(&e) {
                    self.status =
                        Status::error(format!("lsp ({}): {}", lang, root_cause(&e)));
                }
                return;
            }
        };
        if !self.lsp.attach_client(&lang, client) {
            // A client for this language was attached between spawn
            // and now (parallel open of another file with the same
            // language). The freshly-spawned one is dropped here.
            return;
        }
        // Re-snapshot the buffer — the user may have edited while the
        // server was initializing.
        let text = self.buffer.lines.join("\n");
        if let Err(e) = self.lsp.did_open(&lang, &path, &text) {
            self.status =
                Status::error(format!("lsp didOpen ({}): {}", lang, root_cause(&e)));
        }
        self.lsp.set_last_synced_version(self.buffer.version);
    }

    /// Insert a freshly-built fuzzy preview into the LRU. `last_preview_
    /// request` is cleared when the arriving path matches it so the
    /// draw path will re-enqueue if the user has already moved on.
    pub fn handle_preview_ready(&mut self, entry: PreviewEntry) {
        let mut pending = self.last_preview_request.borrow_mut();
        if pending.as_deref() == Some(entry.path.as_path()) {
            *pending = None;
        }
        self.preview_lru.borrow_mut().insert(entry);
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
        match eval::tokenize(&self.config.keymap, &self.tokens, self.mode, key) {
            Some(t) => self.tokens.push(t),
            None => {
                self.tokens.clear();
                return Ok(());
            }
        }
        match eval::classify(&self.tokens) {
            eval::Parse::Complete(expr) => {
                self.tokens.clear();
                self.evaluate(expr, Ctx::default())?;
            }
            eval::Parse::Incomplete => {}
            eval::Parse::Invalid => self.tokens.clear(),
        }
        Ok(())
    }

    fn handle_insert_key(&mut self, key: KeyEvent) -> Result<()> {
        let no_ctrl = !key.modifiers.contains(KeyModifiers::CONTROL);
        if no_ctrl && let KeyCode::Char(c) = key.code {
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
