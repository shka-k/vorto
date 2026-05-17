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
mod inline_suggestion;
mod insert;
mod motion;
mod ops;
mod search;
mod substitute;
mod text_object;
mod vcs_link;

/// Per-buffer indent-guide animation state.
///
/// `started_at = Some(t)` means an animation is in flight from `t`;
/// `None` means the cursor's current scope has already played its
/// animation and is now static (kept so we can detect when the
/// cursor moves into a *different* scope and restart from zero).
/// `scope_key = (start_row, end_row, col)` is enough to detect a
/// scope change without holding a reference to the tree.
/// `anchor_row` is the cursor row at animation start.
pub type IndentAnimState = (
    Option<std::time::Instant>,
    (usize, usize, usize),
    usize,
);

pub use inline_suggestion::{RequestId, Suggestion, SuggestionState};
pub use ops::{flip_case_char_keep_width, to_lower_keep_width, to_upper_keep_width};
pub use search::SearchState;
pub use substitute::{SubsArgs, parse_substitute};

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    /// Set by `run_scroll(Center)` when the viewport height isn't known
    /// yet (e.g. right after switching to a sleeping buffer whose
    /// `viewport_height` thawed back to 0). The next `compute_scroll`
    /// in `draw_buffer` reads-and-clears this and centers the cursor
    /// instead of running the sticky scroll logic — so the user always
    /// lands mid-viewport on a picker-driven jump, even when the
    /// height-aware path couldn't fire yet.
    pub pending_center: Cell<bool>,
    /// Visual y (within the buffer viewport) of the row the cursor sits
    /// on at the last draw. Differs from `cursor.row - scroll` when
    /// inline diagnostics push subsequent rows down. The UI writes this
    /// in `draw_buffer`; `place_cursor` and cursor-anchored overlays
    /// read it.
    pub cursor_visual_y: Cell<u16>,
    /// Active indent-guide animation state. Reset whenever the
    /// cursor enters a different scope; cleared by the renderer
    /// once progress reaches 1.0 so a static frame doesn't keep
    /// waking the loop.
    pub indent_anim: Cell<Option<IndentAnimState>>,
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
    /// Filesystem signature `(mtime, len)` captured the last time
    /// we touched the backing file — at load, after a successful
    /// save, and after `:reload`. `None` for scratch buffers and
    /// for new files that haven't been written yet. The runtime
    /// checks this before `:w` to refuse silently clobbering an
    /// external edit.
    pub disk_meta: Option<FileMeta>,
}

/// Filesystem signature used to detect external edits between
/// load/save and the next save. `len` is what `Metadata::len()`
/// returns; `mtime` is `Metadata::modified()`. Both are cheap to
/// fetch and together catch the overwhelming majority of out-of-band
/// edits — a tool that rewrites a file with the same byte count *and*
/// preserves mtime to nanosecond precision will slip through, but
/// that combination is vanishingly rare in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: SystemTime,
    pub len: u64,
}

impl FileMeta {
    /// Fetch `(mtime, len)` for `path`. Returns `None` when the file
    /// doesn't exist, isn't a regular file the OS will stat, or the
    /// platform refuses to report modification time. Callers treat
    /// `None` as "no baseline to compare against" and skip the drift
    /// check rather than refusing to save.
    pub fn of(path: &Path) -> Option<Self> {
        let md = fs::metadata(path).ok()?;
        let mtime = md.modified().ok()?;
        Some(Self {
            mtime,
            len: md.len(),
        })
    }
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
        let disk_meta = FileMeta::of(path);
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
            pending_center: Cell::new(false),
            cursor_visual_y: Cell::new(0),
            indent_anim: Cell::new(None),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            vcs_base,
            vcs_diff: RefCell::new(None),
            disk_meta,
        })
    }

    pub fn save(&mut self) -> Result<()> {
        if let Some(p) = &self.path {
            fs::write(p, self.lines.join("\n"))?;
            self.dirty = false;
            self.disk_meta = FileMeta::of(p);
        }
        Ok(())
    }

    pub fn save_as(&mut self, path: &Path) -> Result<()> {
        fs::write(path, self.lines.join("\n"))?;
        self.path = Some(path.to_path_buf());
        self.dirty = false;
        self.disk_meta = FileMeta::of(path);
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

    /// Re-read `self.path` from disk and replace the buffer contents
    /// in place. Caller is responsible for the dirty-vs-force decision
    /// — this method always reloads.
    ///
    /// Returns:
    /// - `Ok(true)` when the on-disk content differed and the buffer
    ///   was rewritten (undo snapshot taken, version bumped, cursor
    ///   clamped, highlighter refreshed).
    /// - `Ok(false)` when disk matched the buffer — only `disk_meta`
    ///   is refreshed (mtime alone may have moved), nothing else
    ///   moves so undo history stays intact.
    /// - `Err(_)` when the read failed or no path is attached.
    pub fn reload_from_disk(&mut self) -> Result<bool> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no file name"))?;
        let text = fs::read_to_string(&path)?;
        let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        if lines == self.lines {
            self.disk_meta = FileMeta::of(&path);
            return Ok(false);
        }
        self.snapshot();
        self.lines = lines;
        self.dirty = false;
        self.version = self.version.wrapping_add(1);
        self.vcs_base = vcs::head_blob_lines(&path);
        *self.vcs_diff.borrow_mut() = None;
        self.disk_meta = FileMeta::of(&path);

        // Clamp every cursor (primary + extras) into the possibly-shrunk
        // buffer. Done inline instead of going through `clamp_col` so
        // we can fix `row` first — `clamp_col` reads `current_line` off
        // the primary cursor's row, which would panic if `row` were
        // still past the new end.
        let last_row = self.lines.len().saturating_sub(1);
        let clamp_one = |c: &mut Cursor, lines: &[String]| {
            if c.row > last_row {
                c.row = last_row;
            }
            let row_len = lines.get(c.row).map(|s| s.chars().count()).unwrap_or(0);
            if c.col > row_len {
                c.col = row_len;
            }
        };
        clamp_one(&mut self.cursor, &self.lines);
        for c in &mut self.extra_cursors {
            clamp_one(c, &self.lines);
        }

        self.refresh_highlights();
        Ok(true)
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

/// Inverse of [`char_to_byte`]. Counts the chars up to (but not
/// including) `byte_idx`. The caller is responsible for ensuring
/// `byte_idx` falls on a char boundary.
fn byte_to_char(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx].chars().count()
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
