//! Document model: `Buffer` (lines + cursor + undo + yank) and the
//! basic state lifecycle (load / save / version bump).
//!
//! Behaviour is split across siblings so this file stays focused on
//! state:
//!
//! - [`cursor`] — single-step cursor primitives (`h`/`j`/`k`/`l`,
//!   line/file edges, column clamp, `advance_one`).
//! - [`motion`] — word/paragraph/find/viewport motions and the shared
//!   [`Buffer::motion_target`] entry point.
//! - [`text_object`] — `iw`/`ip`/`i(` etc. resolution.
//! - [`ops`] — range/line/block delete + yank + paste, plus line-level
//!   edits (`J`, `D`, `S`, `~`, comment toggle).
//! - [`insert`] — typing, newline, opener/closer auto-pair, dedent,
//!   single-char delete primitives (`x`, backspace).
//! - [`search`] — `/`/`?` find-next state and lookup over the buffer.
//! - [`history`] — undo / redo snapshot stacks.
//! - [`vcs_link`] — HEAD-blob diff bridge driving the gutter VCS bars.

mod cursor;
mod history;
mod insert;
mod motion;
mod ops;
mod search;
mod text_object;
mod vcs_link;

pub use search::SearchState;

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::syntax::Highlighter;
use crate::vcs::{self, LineStatus};

#[derive(Default)]
pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    /// Additional cursor positions for multi-cursor editing. The primary
    /// cursor lives in `cursor`; extras are *only* the non-primary ones,
    /// stored in insertion order so a pop semantic ("remove last added")
    /// is a simple `pop()`. Empty in the single-cursor common case.
    pub extra_cursors: Vec<Cursor>,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub yank: String,
    /// Monotonically increases on every content-modifying call. Used by
    /// the highlighter to decide whether its cached tree is stale.
    pub version: u64,
    /// Per-buffer tree-sitter state, attached at file-open time when a
    /// matching grammar + query are available. `None` means "no syntax
    /// highlighting for this buffer", which is the safe fallback.
    pub highlighter: Option<Highlighter>,
    /// Topmost line currently visible in the viewport. Sticky — only
    /// moved when the cursor would otherwise leave the viewport (the
    /// UI layer updates it during `draw_buffer`, so it's wrapped in
    /// `Cell` to stay reachable through a shared `&Buffer`).
    pub scroll: Cell<usize>,
    /// Leftmost visual column currently visible. Sticky like `scroll`:
    /// the UI shifts it during `draw_buffer` only when the cursor would
    /// otherwise leave the horizontal viewport.
    pub col_scroll: Cell<usize>,
    /// Height (in rows) of the buffer viewport at the last draw. The
    /// UI writes this during `compute_scroll`; motion code reads it
    /// for `H`/`M`/`L` and `<C-d>`/`<C-u>`/`<C-f>`/`<C-b>`. `0` until
    /// the first frame is drawn — motions guard against that.
    pub viewport_height: Cell<usize>,
    /// Visual y (within the buffer viewport) of the row the cursor sits
    /// on at the last draw. Differs from `cursor.row - scroll` when
    /// inline diagnostics push subsequent rows down. The UI writes this
    /// in `draw_buffer`; `place_cursor` and cursor-anchored overlays
    /// read it.
    pub cursor_visual_y: Cell<u16>,
    // `pub` so the sleeping-buffer freezer can take the stacks
    // by move (and reinstall them on thaw) without going through
    // accessor boilerplate. Editor-internal mutations still go
    // through the `snapshot` / `undo` / `redo` methods.
    pub undo_stack: Vec<Snapshot>,
    pub redo_stack: Vec<Snapshot>,
    /// HEAD blob lines captured at file-open time. `None` when the
    /// buffer isn't backed by a file inside a git repo. `Some(empty)`
    /// when the file is in a repo but not yet tracked at HEAD — every
    /// current line will diff as `Added`.
    pub vcs_base: Option<Vec<String>>,
    /// Cached `(version, per-line status)` produced by diffing
    /// `vcs_base` against `lines`. Recomputed lazily when `version`
    /// moves; wrapped in `RefCell` so the UI can refresh it through
    /// the shared `&Buffer` it gets at draw time.
    pub vcs_diff: RefCell<Option<(u64, Vec<Option<LineStatus>>)>>,
}

/// Frozen buffer state for the undo/redo history. Exposed at the
/// crate boundary so the sleeping-buffer compressor can destructure
/// individual snapshots when it freezes a buffer; the editor module
/// itself still owns all the read/write logic.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub lines: Vec<String>,
    pub cursor: Cursor,
    /// Multi-cursor extras at snapshot time. Empty when there are no
    /// extras (the common case). Undo restores them along with the
    /// primary cursor so the multi-cursor state round-trips.
    pub extra_cursors: Vec<Cursor>,
    pub dirty: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
}

/// Knobs the buffer needs to produce an indent string for a freshly
/// inserted line. `width` is the spaces-per-level fallback when a level
/// is added in spaces. `use_tabs` is the tie-breaker when the reference
/// line carries no indent of its own (empty file, top-level statement);
/// when the reference line *does* have leading whitespace, that style is
/// preserved so we don't mix tabs and spaces within a file.
#[derive(Debug, Clone, Copy)]
pub struct IndentSettings {
    pub width: usize,
    pub use_tabs: bool,
}

impl Default for IndentSettings {
    fn default() -> Self {
        Self {
            width: 4,
            use_tabs: false,
        }
    }
}

impl Buffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Default::default()
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let vcs_base = vcs::head_blob_lines(path);
        Ok(Self {
            lines,
            cursor: Cursor::default(),
            extra_cursors: Vec::new(),
            path: Some(path.to_path_buf()),
            dirty: false,
            yank: String::new(),
            version: 0,
            highlighter: None,
            scroll: Cell::new(0),
            col_scroll: Cell::new(0),
            viewport_height: Cell::new(0),
            cursor_visual_y: Cell::new(0),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            vcs_base,
            vcs_diff: RefCell::new(None),
        })
    }

    pub fn save(&mut self) -> Result<()> {
        if let Some(p) = &self.path {
            fs::write(p, self.lines.join("\n"))?;
            self.dirty = false;
        }
        Ok(())
    }

    pub fn save_as(&mut self, path: &Path) -> Result<()> {
        fs::write(path, self.lines.join("\n"))?;
        self.path = Some(path.to_path_buf());
        self.dirty = false;
        Ok(())
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.version = self.version.wrapping_add(1);
    }

    /// Bump the version counter without touching `dirty`. Used when an
    /// external rewriter (e.g. LSP workspace edit application) wants to
    /// invalidate cached highlights without otherwise altering state.
    pub fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub fn refresh_highlights(&mut self) {
        let Some(h) = self.highlighter.as_mut() else {
            return;
        };
        let source = self.lines.join("\n");
        h.refresh(&source, self.version);
    }
}

// ────────────────────────────────────────────────────────────────────────
// Shared helpers, available to all editor submodules.
// ────────────────────────────────────────────────────────────────────────

/// Convert a 0-based character index into the corresponding byte offset
/// in `s`. Past-the-end indices clamp to `s.len()` so callers can use
/// the result as an exclusive end without bounds checking.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Punct,
    Space,
}

fn classify(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

fn is_blank_line(line: &str) -> bool {
    line.chars().all(|c| c.is_whitespace())
}
