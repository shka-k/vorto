//! Small app-state value types that live alongside `App` but are
//! distinct enough to keep out of `mod.rs`.
//!
//! - [`Selection`] is a derived view over the active visual range,
//!   computed from the current mode + anchor + cursor.
//!
//! `BufferRef` used to live here too, but it's a pure value identifier
//! that `prompt` and `ui` also need — it now lives at the crate root
//! ([`crate::buffer_ref`]) so those lower layers don't have to import
//! upward into `app`. `LastFind` is in `action` for the same reason.

use crate::editor::Cursor;
use crate::mode::Mode;

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
