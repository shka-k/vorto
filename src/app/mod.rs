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
mod runtime;
mod sleeping;
mod status;
mod types;

pub use sleeping::SleepingBuffer;
pub use status::Status;
pub use types::{BufferRef, Selection};

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use crate::action::{LastFind, Token};
use crate::config::Config;
use crate::editor::{Buffer, Cursor};
use crate::event::AppEvent;
use crate::editor::SearchState;
use crate::finder::{self, PreviewLru};
use crate::lsp::LspCoordinator;
use crate::mode::Mode;
use crate::prompt::PromptController;
use crate::syntax::Loader;

pub use crate::prompt::Prompt;

/// Cap on the recently-opened-files MRU. 64 is plenty for normal use
/// and bounds memory without needing a fancy eviction policy.
const MRU_CAP: usize = 64;

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
    /// MRU of recently-touched buffers (newest at the end). Drives the
    /// `<space>b` buffer picker. Capped at [`MRU_CAP`] entries. The
    /// scratch buffer is represented by `BufferRef::Scratch` so it
    /// stays selectable even after the user opens a file over it.
    pub opened_paths: Vec<BufferRef>,
    /// Sleeping (non-active) buffers, keyed by [`BufferRef`]. When the
    /// user switches away from a buffer we move its state in here so
    /// the unsaved edits, undo history, and cursor position are still
    /// around the next time they pick it up. The highlighter isn't
    /// preserved — it's rebuilt by the worker on restore. Lines and
    /// undo/redo content are deflate-compressed when the buffer's
    /// total raw byte count is large enough to be worth it (see
    /// `sleeping::SleepingBuffer::freeze`).
    pub sleeping: HashMap<BufferRef, SleepingBuffer>,
    /// Last `f`/`F`/`t`/`T` so `;` and `,` know what to repeat.
    pub last_find: Option<LastFind>,
    /// True when a `g` prefix is pending in Visual mode. Normal mode
    /// uses its token stream for this; Visual mode bypasses the token
    /// pipeline so it tracks the one prefix it cares about here.
    pub visual_g_pending: bool,
    pub should_quit: bool,
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
        finder::spawn_preview_worker(
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
            // Pre-seed with Scratch so the picker always offers a way
            // back to the unnamed empty buffer, even after opening a
            // real file over it.
            opened_paths: vec![BufferRef::Scratch],
            sleeping: HashMap::new(),
            last_find: None,
            visual_g_pending: false,
            should_quit: false,
        }
    }

    /// Record `r` as the most recent buffer the user touched. Moves
    /// existing entries to the front so the picker stays in MRU order,
    /// caps the list at [`MRU_CAP`] entries, and evicts the matching
    /// sleeping snapshot when one falls off the back of the MRU —
    /// otherwise the in-memory snapshots would grow unbounded.
    pub(super) fn record_opened(&mut self, r: BufferRef) {
        self.opened_paths.retain(|x| x != &r);
        self.opened_paths.push(r);
        while self.opened_paths.len() > MRU_CAP {
            let evicted = self.opened_paths.remove(0);
            self.sleeping.remove(&evicted);
        }
    }

    /// Current selection range, if the editor is in any visual mode and
    /// an anchor is set. Returns `None` otherwise.
    pub fn selection(&self) -> Option<Selection> {
        types::selection(self.mode, self.visual_anchor, self.buffer.cursor)
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
