//! File-open orchestration: load the buffer synchronously, fan the
//! expensive work (tree-sitter highlighter build, LSP server spawn) out
//! to worker threads, and reconcile the results when they arrive back
//! on the main loop as `AppEvent::{HighlighterReady, LspReady,
//! PreviewReady}`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::Result;

use crate::editor::Buffer;
use crate::event::AppEvent;
use crate::highlight::Highlighter;
use crate::lsp::{self, LspClient};
use crate::preview::PreviewEntry;

use super::{App, Status, is_command_not_found, root_cause};

impl App {
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
}
