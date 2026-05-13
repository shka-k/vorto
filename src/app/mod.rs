//! Top-level application state.
//!
//! `App` owns the buffer, mode, prompt, configuration, LSP coordinator,
//! highlighter loader, and fuzzy-preview cache + worker channel. The
//! behavioral surface (input handling, LSP orchestration, file-open
//! orchestration, Normal-mode evaluation) is split across sibling
//! `impl App { ... }` blocks in the submodules below.

mod eval;
mod input;
mod lsp_ops;
mod open;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use crate::action::Token;
use crate::config::Config;
use crate::editor::{Buffer, Cursor};
use crate::event::AppEvent;
use crate::highlight::Loader;
use crate::lsp::LspCoordinator;
use crate::mode::Mode;
use crate::preview::{self, PreviewLru};
use crate::prompt::PromptController;
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
}

/// Walk an anyhow error chain to its innermost cause — keeps the
/// status-bar message focused on the actual filesystem / parser error
/// rather than the wrapping context.
pub(super) fn root_cause(e: &anyhow::Error) -> String {
    e.chain()
        .last()
        .map(|x| x.to_string())
        .unwrap_or_else(|| e.to_string())
}

/// True if the error chain contains an `io::Error` with `NotFound` kind —
/// i.e. the LSP server binary isn't on `PATH`. Lets us silently skip
/// built-in defaults the user hasn't installed.
pub(super) fn is_command_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}
