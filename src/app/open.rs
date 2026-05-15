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
use crate::finder::PreviewEntry;
use crate::lsp::{self, LspClient};
use crate::syntax::Highlighter;

use crate::buffer_ref::BufferRef;

use super::{App, SleepingBuffer, Status, is_command_not_found, root_cause};

impl App {
    /// Dispatch a buffer-picker selection. Scratch and File both go
    /// through the same stash-and-restore flow as
    /// [`Self::open_path`] — if the target has a sleeping snapshot
    /// (with preserved unsaved edits, cursor, undo history), we
    /// restore it; otherwise we fall through to a fresh load.
    pub fn switch_to_buffer(&mut self, r: BufferRef) -> Result<()> {
        match r {
            BufferRef::Scratch => {
                if self.buffer.path.is_none() {
                    return Ok(());
                }
                self.lsp.detach_current();
                // `Buffer::new` (one empty line) ≠ `Buffer::default`
                // (zero lines), so we can't use `unwrap_or_default`
                // here — the wrong default would leave the buffer
                // with an empty `lines` Vec and crash motions.
                let next = match self.sleeping.remove(&BufferRef::Scratch) {
                    Some(b) => b.thaw(),
                    None => Buffer::new(),
                };
                self.stash_and_install(next);
                self.open_gen = self.open_gen.wrapping_add(1);
                self.lsp.set_last_synced_version(self.buffer.version);
                self.record_opened(BufferRef::Scratch);
                self.status = Status::info("scratch");
                Ok(())
            }
            BufferRef::File(path) => {
                // Already on this file? Leave cursor/unsaved state alone.
                let current = self
                    .buffer
                    .path
                    .as_ref()
                    .and_then(|p| p.canonicalize().ok());
                if current.as_ref() == Some(&path) {
                    return Ok(());
                }
                self.open_path(&path)
            }
        }
    }

    /// Move the currently-active buffer into the sleeping map
    /// (keyed by its [`BufferRef`]) and install `next` as the new
    /// active buffer. The outgoing buffer is freeze-compressed; its
    /// highlighter is dropped (rebuilt on restore). The version
    /// counter is preserved so LSP `didChange` sequencing re-anchors
    /// cleanly when the buffer wakes up again.
    fn stash_and_install(&mut self, next: Buffer) {
        let key = self.active_ref();
        let mut prev = std::mem::replace(&mut self.buffer, next);
        prev.highlighter = None;
        self.sleeping.insert(key, SleepingBuffer::freeze(prev));
    }

    /// Install `next` as the active buffer without stashing the
    /// previous one. Used by `:bd` where the deleted buffer is
    /// supposed to vanish entirely. Callers must have already cleaned
    /// up any MRU / sleeping entries that refer to the outgoing
    /// buffer.
    fn install_buffer(&mut self, next: Buffer) {
        let _ = std::mem::replace(&mut self.buffer, next);
    }

    /// [`BufferRef`] for the currently-active buffer.
    pub(super) fn active_ref(&self) -> BufferRef {
        match &self.buffer.path {
            Some(p) => BufferRef::File(p.canonicalize().unwrap_or_else(|_| p.clone())),
            None => BufferRef::Scratch,
        }
    }

    /// Open `path`. If the buffer for this path is sleeping (i.e. the
    /// user previously visited it and switched away), wake it up
    /// instead of re-reading from disk — that's what preserves the
    /// unsaved edits, undo stack, and cursor position across a
    /// `<space>b` round-trip. Otherwise load fresh from disk.
    pub fn open_path(&mut self, path: &Path) -> Result<()> {
        let path = self.absolutize(path);
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        let key = BufferRef::File(canon);
        if let Some(restored) = self.sleeping.remove(&key) {
            self.lsp.detach_current();
            self.stash_and_install(restored.thaw());
            self.record_opened(key);
            self.open_gen = self.open_gen.wrapping_add(1);
            self.lsp.set_last_synced_version(self.buffer.version);
            self.status = Status::info(format!("restored {}", path.display()));
            self.spawn_highlighter_worker(&path);
            self.spawn_lsp_worker(&path);
            return Ok(());
        }
        self.open_path_force(&path)
    }

    /// Resolve a user-supplied path to an absolute path against
    /// `startup_cwd`. Doesn't touch the filesystem — works for files
    /// that don't exist yet, which `canonicalize()` rejects. Critical
    /// for `:e new_file.rs`: without absolutizing, the relative path
    /// flows into [`crate::lsp::path_to_uri`] which produces a broken
    /// `file:///new_file.rs` URI (no directory), and the LSP server
    /// silently ignores the document.
    fn absolutize(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.startup_cwd.join(path)
        }
    }

    /// Open `path` from disk, discarding any sleeping copy. Used on
    /// the initial command-line load and as the fall-through for
    /// `open_path` when there's no sleeping snapshot to restore.
    pub fn open_path_force(&mut self, path: &Path) -> Result<()> {
        // Load up front — if this fails we want to leave the active
        // buffer alone. Missing files are treated as a new, unsaved
        // buffer attached to `path` so `:w` materializes the file.
        let (loaded, is_new) = match Buffer::load(path) {
            Ok(b) => (b, false),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                let mut b = Buffer::new();
                b.path = Some(path.to_path_buf());
                (b, true)
            }
            Err(e) => return Err(e),
        };
        let canon = path
            .canonicalize()
            .unwrap_or_else(|_| path.to_path_buf());
        // Tell the previous LSP client we're done with that document so
        // it can drop diagnostics and stop watching it.
        self.lsp.detach_current();
        self.stash_and_install(loaded);
        // Re-loading a path drops any previously-sleeping copy of it
        // — the user explicitly asked for the disk version.
        self.sleeping.remove(&BufferRef::File(canon.clone()));
        self.record_opened(BufferRef::File(canon));
        // Bump the generation: any in-flight worker thread from a
        // previous `open_path` is now stale. Its result will be dropped
        // when it lands instead of clobbering this buffer.
        self.open_gen = self.open_gen.wrapping_add(1);
        // Pre-seed the LSP sync version so the first `didChange` after
        // open is a no-op when nothing has changed since load.
        self.lsp.set_last_synced_version(self.buffer.version);
        self.status = if is_new {
            Status::info(format!("{} [new file]", path.display()))
        } else {
            Status::info(format!("opened {}", path.display()))
        };
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

    /// `:bn` / `:bp` — cycle through `opened_paths`. Same semantics
    /// as vim's `:bnext` / `:bprev`: forward wraps to the start, back
    /// wraps to the end. No-op when there's only one buffer.
    pub fn buffer_cycle(&mut self, forward: bool) -> Result<()> {
        if self.opened_paths.len() <= 1 {
            self.status = Status::info("only one buffer");
            return Ok(());
        }
        let current_ref = self.active_ref();
        let len = self.opened_paths.len();
        let idx = self
            .opened_paths
            .iter()
            .position(|r| r == &current_ref)
            .unwrap_or(0);
        let target_idx = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        let target = self.opened_paths[target_idx].clone();
        self.switch_to_buffer(target)
    }

    /// `:bd` / `:bd!` — drop the current buffer from MRU and
    /// sleeping, then switch to the most-recent remaining buffer
    /// (falling back to a fresh scratch). Refuses on dirty without
    /// `force`. The deleted buffer is *not* stashed — its content
    /// is gone, same as vim's `:bd`.
    pub fn buffer_delete(&mut self, force: bool) -> Result<()> {
        if !force && self.buffer.dirty {
            self.status = Status::error("unsaved changes (use :bd!)");
            return Ok(());
        }
        let current_ref = self.active_ref();
        // Pick a successor before mutating state — the most-recent
        // entry that *isn't* the one we're deleting.
        let target = self
            .opened_paths
            .iter()
            .rev()
            .find(|r| *r != &current_ref)
            .cloned();
        // Drop the deleted buffer from all bookkeeping.
        self.opened_paths.retain(|r| r != &current_ref);
        self.sleeping.remove(&current_ref);
        self.lsp.detach_current();

        match target {
            Some(BufferRef::Scratch) => {
                let restored = match self.sleeping.remove(&BufferRef::Scratch) {
                    Some(b) => b.thaw(),
                    None => Buffer::new(),
                };
                self.install_buffer(restored);
                self.open_gen = self.open_gen.wrapping_add(1);
                self.lsp.set_last_synced_version(self.buffer.version);
                self.record_opened(BufferRef::Scratch);
                self.status = Status::info("deleted, [scratch]");
                Ok(())
            }
            Some(BufferRef::File(path)) => {
                // Restore from sleeping when available; otherwise
                // re-read disk. Both paths set up LSP/highlighter.
                if let Some(b) = self.sleeping.remove(&BufferRef::File(path.clone())) {
                    self.install_buffer(b.thaw());
                    self.open_gen = self.open_gen.wrapping_add(1);
                    self.lsp.set_last_synced_version(self.buffer.version);
                    self.record_opened(BufferRef::File(path.clone()));
                    self.spawn_highlighter_worker(&path);
                    self.spawn_lsp_worker(&path);
                    self.status = Status::info(format!("deleted, restored {}", path.display()));
                } else {
                    // Successor isn't in sleeping (rare — would mean
                    // it was evicted by MRU cap while being in the
                    // picker). Fresh-load from disk.
                    let loaded = match Buffer::load(&path) {
                        Ok(b) => b,
                        Err(e) => {
                            self.install_buffer(Buffer::new());
                            self.open_gen = self.open_gen.wrapping_add(1);
                            self.record_opened(BufferRef::Scratch);
                            self.status = Status::error(format!(
                                "deleted; failed to open {}: {} — using scratch",
                                path.display(),
                                root_cause(&e)
                            ));
                            return Ok(());
                        }
                    };
                    self.install_buffer(loaded);
                    self.record_opened(BufferRef::File(path.clone()));
                    self.open_gen = self.open_gen.wrapping_add(1);
                    self.lsp.set_last_synced_version(self.buffer.version);
                    self.spawn_highlighter_worker(&path);
                    self.spawn_lsp_worker(&path);
                    self.status = Status::info(format!("deleted, opened {}", path.display()));
                }
                Ok(())
            }
            None => {
                // Nothing left — start a fresh scratch.
                self.install_buffer(Buffer::new());
                self.open_gen = self.open_gen.wrapping_add(1);
                self.record_opened(BufferRef::Scratch);
                self.status = Status::info("deleted, [scratch]");
                Ok(())
            }
        }
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
