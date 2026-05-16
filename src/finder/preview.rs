//! Fuzzy-finder source-preview cache and worker thread.
//!
//! The picker's right pane shows a syntax-highlighted view of the file
//! under the cursor. Building that view (grammar dlopen, query compile,
//! file read, full tree-sitter parse) is too expensive to do on the
//! draw path, so it happens on a dedicated worker thread; this module
//! owns the worker and the LRU cache of finished previews.
//!
//! Flow:
//!
//! 1. Draw code asks the LRU for `path`. On hit it renders highlighted.
//! 2. On miss, draw code sends `path` to the worker via `preview_tx`
//!    and renders plain text for that frame.
//! 3. Worker reads the file, builds a [`PreviewEntry`], and hands it
//!    back through the supplied `emit` closure. The main loop installs
//!    it in the LRU and the next frame renders with highlights.
//!
//! The worker drains its channel before each unit of work so a fast
//! `j`/`k` scroll only triggers one parse for the final selection.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::config::LanguageRegistry;
use crate::syntax::{Highlighter, Loader};

/// One fully-built fuzzy-finder preview: pre-split lines plus a tree-
/// sitter highlighter with a parsed tree already attached. Built off
/// the main thread by [`spawn_preview_worker`]; the main loop drops it
/// into [`PreviewLru`] on receipt.
pub struct PreviewEntry {
    pub path: PathBuf,
    pub lines: Vec<String>,
    pub highlighter: Highlighter,
}

/// Small bounded LRU of completed previews keyed by path. Keeps a
/// handful of recently-visited entries so navigating back to a file in
/// the picker (or hitting the same file from `Files` and `Locations`
/// kinds back-to-back) skips the worker round-trip entirely.
pub struct PreviewLru {
    cap: usize,
    /// Most-recently-used at the front. `VecDeque` because pop-from-end
    /// and push-to-front is the access pattern, and the capacity stays
    /// small enough that the linear-scan `get` is fine.
    entries: VecDeque<PreviewEntry>,
}

impl PreviewLru {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: VecDeque::with_capacity(cap),
        }
    }

    /// Bring `path`'s entry to the front and return it. `None` when
    /// not cached.
    pub fn get(&mut self, path: &Path) -> Option<&PreviewEntry> {
        let pos = self.entries.iter().position(|e| e.path == path)?;
        if pos != 0 {
            let entry = self.entries.remove(pos).unwrap();
            self.entries.push_front(entry);
        }
        self.entries.front()
    }

    /// Remove and return `path`'s entry if cached. Used by `open_path`
    /// to "steal" a preview-built highlighter so opening a file the
    /// user has been previewing skips the worker round-trip entirely.
    pub fn take(&mut self, path: &Path) -> Option<PreviewEntry> {
        let pos = self.entries.iter().position(|e| e.path == path)?;
        self.entries.remove(pos)
    }

    /// Insert `entry`, evicting an existing entry for the same path
    /// first, and trimming back to `cap` from the LRU end.
    pub fn insert(&mut self, entry: PreviewEntry) {
        self.entries.retain(|e| e.path != entry.path);
        self.entries.push_front(entry);
        while self.entries.len() > self.cap {
            self.entries.pop_back();
        }
    }
}

/// Spawn the fuzzy-finder preview worker. The worker owns its receive
/// end, drains the request channel to keep only the latest path when
/// the user is scrolling fast, builds a [`PreviewEntry`] off the main
/// thread, and hands it back through `emit`.
///
/// `emit` is a closure rather than an `AppEvent` sender so this module
/// doesn't have to depend on `crate::event`.
pub fn spawn_preview_worker(
    loader: Arc<Mutex<Loader>>,
    languages: LanguageRegistry,
    rx: Receiver<PathBuf>,
    emit: Box<dyn Fn(PreviewEntry) + Send + 'static>,
) {
    thread::spawn(move || {
        // Block on the first request, then drain anything that piled up
        // while we were idle so we only do work for the newest path —
        // mid-burst entries from a fast j/k scroll get silently dropped.
        while let Ok(mut path) = rx.recv() {
            while let Ok(latest) = rx.try_recv() {
                path = latest;
            }
            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(spec) = languages.by_extension(ext).cloned() else {
                continue;
            };
            let mut highlighter = match loader.lock().unwrap().highlighter_for(&spec) {
                Ok(h) => h,
                Err(_) => continue,
            };
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lines: Vec<String> = source.lines().map(|s| s.to_string()).collect();
            highlighter.refresh(&source, 1);
            emit(PreviewEntry {
                path,
                lines,
                highlighter,
            });
        }
    });
}
