//! Buffer identifier — names a buffer for the MRU list, the sleeping
//! map, and `PromptOutcome::OpenBuffer`. Pure value type with no
//! behaviour; lives at the crate root so lower layers (`prompt`, `ui`)
//! can reference it without an upward import into `app`.

use std::path::PathBuf;

/// One entry in the buffer-picker MRU. `Scratch` is the unnamed empty
/// buffer vorto starts with (and that the user can return to); `File`
/// is a previously-opened path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BufferRef {
    Scratch,
    File(PathBuf),
}
