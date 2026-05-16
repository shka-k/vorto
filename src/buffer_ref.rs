//! Buffer identifier — names a buffer for the MRU list, the sleeping
//! map, and `PromptOutcome::OpenBuffer`. Pure value type with no
//! behaviour; lives at the crate root so lower layers (`prompt`, `ui`)
//! can reference it without an upward import into `app`.

use std::path::PathBuf;

/// One entry in the buffer-picker MRU. `Scratch(id)` is an unnamed
/// empty buffer: `Scratch(0)` is the one vorto starts with, and every
/// `:new` mints a fresh id so multiple scratch buffers coexist. `File`
/// is a previously-opened path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BufferRef {
    Scratch(u32),
    File(PathBuf),
}

impl BufferRef {
    /// Human-readable label for scratch buffers in pickers and status
    /// messages. `Scratch(0)` is the original anonymous buffer (just
    /// `[scratch]`); subsequent ones get a numeric suffix.
    pub fn scratch_label(id: u32) -> String {
        if id == 0 {
            "[scratch]".to_string()
        } else {
            format!("[scratch {}]", id)
        }
    }
}
