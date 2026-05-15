//! Unified event type for the main loop.
//!
//! Terminal input and LSP reader threads both feed into a single
//! `mpsc::Sender<AppEvent>` so the main loop can block on one channel
//! and drain bursts of either kind.

use std::path::PathBuf;

use crossterm::event::Event;

use crate::finder::PreviewEntry;
use crate::lsp::{LspClient, LspEvent};
use crate::syntax::Highlighter;

pub enum AppEvent {
    Term(Event),
    Lsp(LspEvent),
    /// A worker thread spawned by `open_path` finished building a
    /// tree-sitter highlighter (grammar dlopen + query compile + initial
    /// parse). `gen` is the generation the worker was spawned for — the
    /// main loop drops the event when `app.open_gen != gen` so a stale
    /// result from a previous file doesn't clobber the current buffer.
    HighlighterReady {
        generation: u64,
        result: anyhow::Result<Highlighter>,
    },
    /// A worker thread finished spawning an LSP client and running its
    /// `initialize` handshake. The main loop adopts the client and
    /// fires `didOpen` with a fresh snapshot of the current buffer.
    /// `client_key` is the per-server identifier (`"<lang>::<server>"`)
    /// the coordinator stores the client under; one of these events is
    /// emitted per `[[languages.<lang>.lsp]]` entry configured for the
    /// opened file.
    LspReady {
        generation: u64,
        client_key: String,
        lang: String,
        path: PathBuf,
        result: anyhow::Result<LspClient>,
    },
    /// The fuzzy-finder preview worker finished building a highlighted
    /// snapshot for a file. The main loop drops it into the preview LRU.
    /// Stale results are kept anyway — the LRU is keyed by path, so a
    /// late-arriving response just becomes a cheap cache entry for next
    /// time the user navigates back to that file.
    PreviewReady(PreviewEntry),
}
