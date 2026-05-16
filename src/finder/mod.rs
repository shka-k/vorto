//! Fuzzy picker + asynchronous source preview cache.
//!
//! `fuzzy` is the matching engine (query / items / selection); `preview`
//! is the worker-backed LRU that supplies syntax-highlighted snapshots
//! for the file under the picker cursor.

mod fuzzy;
mod preview;

pub use fuzzy::{Finder, FuzzyKind, IgnoreOpts, workspace_files};
pub use preview::{PreviewEntry, PreviewLru, spawn_preview_worker};
