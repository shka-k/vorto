//! Off-main-thread workers for file open: tree-sitter highlighter
//! build, LSP server spawn + initialize, and the fuzzy-preview producer
//! handoff. Each worker fires an [`AppEvent`] when done; the matching
//! `handle_*_ready` reconciles the result against the current
//! `open_gen` (so a stale result from a previous file open is dropped
//! instead of clobbering the active buffer).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::Result;

use crate::event::AppEvent;
use crate::finder::PreviewEntry;
use crate::lsp::{self, LspClient};
use crate::syntax::Highlighter;

use super::lsp_coordinator::client_key;
use super::{App, Toast, is_command_not_found, root_cause};

impl App {
    /// Build a tree-sitter `Highlighter` for `path` off the main thread
    /// (grammar dlopen + query compile + initial full parse). The result
    /// arrives via [`AppEvent::HighlighterReady`] and is installed on the
    /// buffer in [`Self::handle_highlighter_ready`] when the generation
    /// still matches.
    pub(super) fn spawn_highlighter_worker(&mut self, path: &Path) {
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

    /// Spawn one LSP server per `[[languages.<lang>.lsp]]` entry off
    /// the main thread. Servers that are already running for this
    /// language get an inline `didOpen` (cheap — no process spawn);
    /// new ones fire `initialize` on a worker thread and arrive back
    /// via [`AppEvent::LspReady`].
    pub(super) fn spawn_lsp_worker(&mut self, path: &Path) {
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
        if spec.lsp.is_empty() {
            return;
        }
        let lang_name = spec.name.clone();

        for lsp_cfg in spec.lsp {
            let key = client_key(&lang_name, &lsp_cfg.name);

            if self.lsp.has_client(&key) {
                let text = self.buffer.lines.join("\n");
                if let Err(e) = self.lsp.did_open(&key, &lang_name, path, &text) {
                    self.push_toast(Toast::fatal(format!(
                        "lsp didOpen ({}): {}",
                        key,
                        root_cause(&e)
                    )));
                }
                continue;
            }

            let tx = self.event_tx.clone();
            let emit = self.lsp.make_emit();
            let startup_cwd = self.lsp.startup_cwd().to_path_buf();
            let generation = self.open_gen;
            let path_buf = path.to_path_buf();
            let lang_for_thread = lang_name.clone();
            let key_for_thread = key.clone();
            let cfg = lsp_cfg;

            thread::spawn(move || {
                let root_dir = lsp::discover_root(&startup_cwd, Some(&path_buf), &cfg.root_markers);
                let root_uri = lsp::path_to_uri(&root_dir);
                let result =
                    LspClient::spawn(&key_for_thread, &lang_for_thread, &cfg, &root_uri, emit);
                let _ = tx.send(AppEvent::LspReady {
                    generation,
                    client_key: key_for_thread,
                    lang: lang_for_thread,
                    path: path_buf,
                    result,
                });
            });
        }
    }

    /// Install a freshly-built highlighter on the active buffer. Dropped
    /// when `generation` doesn't match — the user opened another file
    /// while the worker was running.
    pub fn handle_highlighter_ready(&mut self, generation: u64, result: Result<Highlighter>) {
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
                if let Some(msg) = h.warnings.drain(..).next() {
                    self.push_toast(Toast::error(msg));
                }
                self.buffer.highlighter = Some(h);
            }
            Err(e) => {
                self.push_toast(Toast::fatal(format!("highlight: {}", root_cause(&e))));
            }
        }
    }

    /// Adopt a freshly-spawned LSP client and send the deferred
    /// `didOpen`. Dropped when `generation` doesn't match — the freshly
    /// spawned client gets dropped here, which closes its stdin and
    /// shuts the server down. `client_key` is the unique identifier
    /// the coordinator stores the client under (typically
    /// `"<lang>::<server-name>"`); a single `<lang>` may produce
    /// multiple `LspReady` events when several servers are configured.
    pub fn handle_lsp_ready(
        &mut self,
        generation: u64,
        client_key: String,
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
                    self.push_toast(Toast::fatal(format!(
                        "lsp ({}): {}",
                        client_key,
                        root_cause(&e)
                    )));
                }
                return;
            }
        };
        if !self.lsp.attach_client(&client_key, client) {
            // A client for this key was attached between spawn and
            // now (parallel open of another file with the same
            // language). The freshly-spawned one is dropped here.
            return;
        }
        // Re-snapshot the buffer — the user may have edited while the
        // server was initializing.
        let text = self.buffer.lines.join("\n");
        if let Err(e) = self.lsp.did_open(&client_key, &lang, &path, &text) {
            self.push_toast(Toast::fatal(format!(
                "lsp didOpen ({}): {}",
                client_key,
                root_cause(&e)
            )));
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
