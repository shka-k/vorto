//! Small app-state value types that live alongside `App` but are
//! distinct enough to keep out of `mod.rs`.
//!
//! - [`BufferRef`] identifies a buffer in the MRU / sleeping map.
//! - [`Selection`] is a derived view over the active visual range,
//!   computed from the current mode + anchor + cursor.
//!
//! `LastFind` used to live here, but it's a pure motion-grammar value
//! (mirrors `MotionKind::FindChar`'s fields) — it belongs in `action`
//! so lower layers like `effect::Cmd::SetLastFind` can reference it
//! without an upward import.

use std::path::PathBuf;

use crate::editor::Cursor;
use crate::mode::Mode;

/// One entry in the buffer-picker MRU. `Scratch` is the unnamed empty
/// buffer vorto starts with (and that the user can return to); `File`
/// is a previously-opened path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BufferRef {
    Scratch,
    File(PathBuf),
}

/// Resolved visual-mode selection bounds, derived from the anchor and
/// the cursor according to the current visual sub-mode.
#[derive(Debug, Clone, Copy)]
pub enum Selection {
    /// Character-wise, inclusive of both endpoints (vim semantics).
    Char { from: Cursor, to: Cursor },
    /// Whole rows `[from_row..=to_row]`.
    Line { from_row: usize, to_row: usize },
    /// Column rectangle `[r0..=r1] × [c0..=c1]`.
    Block {
        r0: usize,
        c0: usize,
        r1: usize,
        c1: usize,
    },
}

/// Compute the active visual selection from raw inputs. Returns `None`
/// when the editor isn't in any visual mode or the anchor is unset.
pub fn selection(mode: Mode, anchor: Option<Cursor>, cursor: Cursor) -> Option<Selection> {
    let anchor = anchor?;
    Some(match mode {
        Mode::Visual => {
            let (from, to) = if (anchor.row, anchor.col) <= (cursor.row, cursor.col) {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            Selection::Char { from, to }
        }
        Mode::VisualLine => Selection::Line {
            from_row: anchor.row.min(cursor.row),
            to_row: anchor.row.max(cursor.row),
        },
        Mode::VisualBlock => Selection::Block {
            r0: anchor.row.min(cursor.row),
            c0: anchor.col.min(cursor.col),
            r1: anchor.row.max(cursor.row),
            c1: anchor.col.max(cursor.col),
        },
        _ => return None,
    })
}
