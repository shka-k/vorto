//! File-open orchestration: load the buffer synchronously and stash
//! the previous one. The expensive follow-up work (tree-sitter
//! highlighter build, LSP server spawn) is fanned out via
//! [`super::workers`]; multi-buffer cycling and deletion live in
//! [`super::buffer_list`].

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::editor::Buffer;

use crate::buffer_ref::BufferRef;

use super::{App, SleepingBuffer, Toast};

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
                self.toast = Toast::info("scratch");
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
    pub(super) fn stash_and_install(&mut self, next: Buffer) {
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
    pub(super) fn install_buffer(&mut self, next: Buffer) {
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
            self.toast = Toast::info(format!("restored {}", path.display()));
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
        self.toast = if is_new {
            Toast::info(format!("{} [new file]", path.display()))
        } else {
            Toast::info(format!("opened {}", path.display()))
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
}
