//! Top-level application state.
//!
//! `App` owns the buffer, mode, prompt, configuration, LSP coordinator,
//! highlighter loader, and fuzzy-preview cache + worker channel. The
//! behavioral surface (input handling, LSP orchestration, file-open
//! orchestration, Normal-mode evaluation) is split across sibling
//! `impl App { ... }` blocks in the submodules below.

mod buffer_list;
mod completion;
mod eval;
mod input;
mod jump;
mod lsp_apply;
mod lsp_coordinator;
mod lsp_request;
mod open;
mod runtime;
mod sleeping;
mod toast;
mod types;
mod workers;

pub use completion::CompletionState;

pub use jump::JumpState;
pub use lsp_coordinator::{LspCoordinator, LspEventOutcome};
pub use sleeping::SleepingBuffer;
pub use toast::{Level, Toast, ToastQueue};
pub use types::Selection;

use crate::buffer_ref::BufferRef;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use crate::action::{InsertKey, LastChange, LastFind, Token};

/// Active insert-session recording. Lives on `App` so `handle_insert_key`
/// can append the keystrokes the user types, and finalize on Esc.
#[derive(Debug)]
pub struct InsertRecording {
    pub trigger: crate::action::Expr,
    pub keys: Vec<InsertKey>,
}
use crate::config::{Config, EditorConfig};
use crate::editor::{Buffer, Cursor};
use crate::event::AppEvent;
use crate::editor::SearchState;
use crate::finder::{self, PreviewLru};
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
    pub toasts: ToastQueue,
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
    /// Last buffer-modifying change — what `.` replays. Updated when a
    /// change finishes (immediately for one-shot Exprs, on Esc for
    /// Insert-mode sessions).
    pub last_change: Option<LastChange>,
    /// Active Insert-session recording. `Some` while the user is in an
    /// Insert mode entered through a recordable trigger; finalized into
    /// `last_change` when Esc returns us to Normal.
    pub recording: Option<InsertRecording>,
    /// True when a `g` prefix is pending in Visual mode. Normal mode
    /// uses its token stream for this; Visual mode bypasses the token
    /// pipeline so it tracks the one prefix it cares about here.
    pub visual_g_pending: bool,
    /// Active `gw` jump-label overlay, if any. `Some` between the user
    /// pressing `gw` and either picking a label or cancelling. While
    /// it's `Some`, the input dispatcher routes every key to
    /// [`App::handle_jump_key`] and the UI renders the label overlay.
    pub jump_state: Option<JumpState>,
    /// Active LSP completion popup, if any. `Some` between a successful
    /// `textDocument/completion` response and the user accepting,
    /// dismissing, or invalidating it (cursor row change / backspace
    /// past the prefix start).
    pub completion: Option<CompletionState>,
    /// System clipboard handle, initialized lazily on first yank.
    /// `None` means we haven't tried yet *or* the platform refused to
    /// give us one (Wayland without a compositor, headless CI, …); the
    /// internal `Buffer.yank` register keeps working either way, so a
    /// failed init silently degrades to vorto-local yank.
    pub clipboard: Option<arboard::Clipboard>,
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
            toasts: ToastQueue::new(),
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
            last_change: None,
            recording: None,
            visual_g_pending: false,
            jump_state: None,
            completion: None,
            clipboard: None,
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

    /// Advance the toast queue: drop expired non-fatal toasts and
    /// promote pending ones into the freed slots. The main loop calls
    /// this once per iteration before draw + [`toast_remaining`] so
    /// both see a fresh view.
    pub fn tick_toasts(&mut self) {
        self.toasts.tick();
    }

    /// Time until the next toast-queue state change. `None` means
    /// nothing is on screen and the main loop can block without a
    /// timeout; otherwise the value is the soonest non-fatal TTL
    /// expiry (or a long placeholder if only fatal toasts are live),
    /// so the loop wakes up to advance the queue.
    pub fn toast_remaining(&self) -> Option<std::time::Duration> {
        self.toasts.remaining()
    }

    /// Queue a toast for display. Goes straight to the visible stack
    /// while there's room (cap of 3); otherwise waits behind the
    /// already-visible toasts and is promoted as they expire.
    pub fn push_toast(&mut self, t: Toast) {
        self.toasts.push(t);
    }

    /// Wipe all toasts — visible and queued. Exposed for callers that
    /// want to take over the toast slot wholesale; not currently used
    /// in-tree.
    #[allow(dead_code)]
    pub fn clear_toast(&mut self) {
        self.toasts.clear();
    }

    /// Visual column (0-based cell offset, not char index) of the
    /// primary cursor on its current line, after tabs are expanded
    /// using the buffer's effective `tab_width`. Mirrors what
    /// [`ui::buffer::place_cursor`] places on screen, so the status
    /// bar and any other consumer can show a position that matches
    /// where the cursor actually sits.
    pub fn cursor_visual_col(&self) -> usize {
        let tab_width = self.effective_editor().tab_width.max(1);
        let line = &self.buffer.lines[self.buffer.cursor.row];
        let mut v = 0usize;
        for ch in line.chars().take(self.buffer.cursor.col) {
            if ch == '\t' {
                v += tab_width - (v % tab_width);
            } else {
                v += 1;
            }
        }
        v
    }

    /// Visual y (within the buffer viewport) of `row`, given the
    /// current scroll. Accounts for inline diagnostic lines that push
    /// subsequent source rows down. Returns `None` when `row` is
    /// scrolled off the top.
    ///
    /// Cursor-anchored overlays (hover, completion, code-action menu)
    /// use this so they sit below the right visual line — `cursor.row -
    /// scroll` undercounts whenever any earlier visible row carries a
    /// diagnostic.
    pub fn visual_row_offset(&self, row: usize) -> Option<u16> {
        let scroll = self.buffer.scroll.get();
        if row < scroll {
            return None;
        }
        // One extra visual row per source row whose diagnostics are
        // surfaced inline. Mirrors `ui::buffer`'s filter: the cursor's
        // row shows any severity, every other row only shows `Error`s.
        let cursor_row = self.buffer.cursor.row;
        let mut diag_rows: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        if let Some(diags) = self.current_diagnostics() {
            for d in diags {
                let r = d.range.start.line as usize;
                if r != cursor_row && d.severity != crate::lsp::Severity::Error {
                    continue;
                }
                diag_rows.insert(r);
            }
        }
        let mut y: u16 = 0;
        for r in scroll..row {
            y = y.saturating_add(1);
            if diag_rows.contains(&r) {
                y = y.saturating_add(1);
            }
        }
        Some(y)
    }

    /// `IndentSettings` derived from the active buffer's effective
    /// editor config. Convenience wrapper so the input + eval layers
    /// don't have to redo the `EditorConfig → IndentSettings`
    /// conversion at every call site that inserts a new line.
    pub(super) fn indent_settings(&self) -> crate::editor::IndentSettings {
        let eff = self.effective_editor();
        crate::editor::IndentSettings {
            width: eff.indent_width.max(1),
            use_tabs: eff.use_tabs,
        }
    }

    /// Effective editor settings for the active buffer: the global
    /// `[editor]` defaults with the buffer-language's per-language
    /// overrides layered on top. When the buffer has no path or its
    /// extension doesn't resolve to a known language, the global
    /// defaults are returned as-is.
    pub fn effective_editor(&self) -> EditorConfig {
        let base = self.config.editor;
        let Some(path) = self.buffer.path.as_ref() else {
            return base;
        };
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return base;
        };
        let Some(lang) = self.config.languages.by_extension(ext) else {
            return base;
        };
        base.overlay(&lang.editor)
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
