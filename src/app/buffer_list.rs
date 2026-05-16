//! Buffer-list management: `:bn`/`:bp`/`:bd`. Cycles and deletes
//! against the MRU `opened_paths` and the sleeping snapshot map; the
//! actual file-open work lives in [`super::open`].

use anyhow::Result;

use crate::buffer_ref::BufferRef;
use crate::editor::Buffer;

use super::{App, Toast, root_cause};

impl App {
    /// `:bn` / `:bp` — cycle through `opened_paths`. Same semantics
    /// as vim's `:bnext` / `:bprev`: forward wraps to the start, back
    /// wraps to the end. No-op when there's only one buffer.
    pub fn buffer_cycle(&mut self, forward: bool) -> Result<()> {
        if self.opened_paths.len() <= 1 {
            self.toast = Toast::info("only one buffer");
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
            self.toast = Toast::error("unsaved changes (use :bd!)");
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
                self.toast = Toast::info("deleted, [scratch]");
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
                    self.toast = Toast::info(format!("deleted, restored {}", path.display()));
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
                            self.toast = Toast::error(format!(
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
                    self.toast = Toast::info(format!("deleted, opened {}", path.display()));
                }
                Ok(())
            }
            None => {
                // Nothing left — start a fresh scratch.
                self.install_buffer(Buffer::new());
                self.open_gen = self.open_gen.wrapping_add(1);
                self.record_opened(BufferRef::Scratch);
                self.toast = Toast::info("deleted, [scratch]");
                Ok(())
            }
        }
    }
}
