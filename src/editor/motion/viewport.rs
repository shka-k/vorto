//! Viewport-relative motions: `H` / `M` / `L` and `<C-d>` / `<C-u>` /
//! `<C-f>` / `<C-b>`.

use crate::action::MotionKind;
use crate::editor::{Buffer, Cursor};

/// Resolve `H` / `M` / `L` against the buffer's current viewport. Reads
/// the topmost-visible row from `scroll` and the row count from
/// `viewport_height`, both updated by the UI on each draw. Falls back
/// to `from` while the viewport is unknown (height == 0).
pub(super) fn viewport_target(buf: &Buffer, from: Cursor, motion: MotionKind) -> Cursor {
    let height = buf.viewport_height.get();
    if height == 0 {
        return from;
    }
    let top = buf.scroll.get();
    let last_row = buf.lines.len().saturating_sub(1);
    let bottom = (top + height).saturating_sub(1).min(last_row);
    let row = match motion {
        MotionKind::ViewportTop => top.min(last_row),
        MotionKind::ViewportBottom => bottom,
        MotionKind::ViewportMiddle => {
            let mid = top + (bottom - top) / 2;
            mid.min(last_row)
        }
        _ => return from,
    };
    let max_col = buf.lines[row].chars().count().saturating_sub(1);
    Cursor {
        row,
        col: from.col.min(max_col),
    }
}

/// Resolve `<C-d>` / `<C-u>` / `<C-f>` / `<C-b>` against the viewport
/// height. The cursor moves; the UI's existing `compute_scroll` keeps
/// the viewport pinned to the cursor on the next draw.
///
/// We use a sensible minimum step (1) so the motion never silently
/// stalls when the viewport hasn't been measured yet — that's how vim
/// behaves in `--clean -e` mode when no window is available.
pub(super) fn page_target(buf: &Buffer, from: Cursor, motion: MotionKind) -> Cursor {
    let height = buf.viewport_height.get().max(1);
    let half = (height / 2).max(1);
    let last_row = buf.lines.len().saturating_sub(1);
    let row = match motion {
        MotionKind::HalfPageDown => (from.row + half).min(last_row),
        MotionKind::HalfPageUp => from.row.saturating_sub(half),
        MotionKind::PageDown => (from.row + height).min(last_row),
        MotionKind::PageUp => from.row.saturating_sub(height),
        _ => return from,
    };
    let max_col = buf.lines[row].chars().count().saturating_sub(1);
    Cursor {
        row,
        col: from.col.min(max_col),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(|s| s.to_string()).collect()
    }

    fn buf_with(lines_in: &str, scroll: usize, height: usize) -> Buffer {
        let mut b = Buffer::new();
        b.lines = lines(lines_in);
        b.scroll.set(scroll);
        b.viewport_height.set(height);
        b
    }

    #[test]
    fn viewport_h_m_l_with_full_window() {
        // 10 lines, viewport rows 0..10. H=0, M=4 (mid floor), L=9.
        let b = buf_with("a\nb\nc\nd\ne\nf\ng\nh\ni\nj", 0, 10);
        let from = Cursor { row: 5, col: 0 };
        assert_eq!(viewport_target(&b, from, MotionKind::ViewportTop).row, 0);
        assert_eq!(viewport_target(&b, from, MotionKind::ViewportMiddle).row, 4);
        assert_eq!(viewport_target(&b, from, MotionKind::ViewportBottom).row, 9);
    }

    #[test]
    fn viewport_clamps_to_file_end() {
        // Viewport says rows 5..15 but file only has 8 lines.
        // L should clamp to last row (7) rather than overshoot.
        let b = buf_with("a\nb\nc\nd\ne\nf\ng\nh", 5, 10);
        let from = Cursor { row: 6, col: 0 };
        assert_eq!(viewport_target(&b, from, MotionKind::ViewportBottom).row, 7);
        assert_eq!(viewport_target(&b, from, MotionKind::ViewportTop).row, 5);
    }

    #[test]
    fn page_motions_step_by_height_and_half() {
        // 20 rows, viewport height = 10. From row 0:
        //   <C-d> → row 5 (half), <C-f> → row 10 (full), and back.
        let lines_str = (0..20).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let b = buf_with(&lines_str, 0, 10);
        let from = Cursor { row: 0, col: 0 };
        assert_eq!(page_target(&b, from, MotionKind::HalfPageDown).row, 5);
        assert_eq!(page_target(&b, from, MotionKind::PageDown).row, 10);
        let mid = Cursor { row: 15, col: 0 };
        assert_eq!(page_target(&b, mid, MotionKind::HalfPageUp).row, 10);
        assert_eq!(page_target(&b, mid, MotionKind::PageUp).row, 5);
    }

    #[test]
    fn page_motions_clamp_to_file_bounds() {
        let b = buf_with("a\nb\nc", 0, 10);
        // From last row, <C-d> stays put; from first row, <C-u> stays put.
        let last = Cursor { row: 2, col: 0 };
        assert_eq!(page_target(&b, last, MotionKind::HalfPageDown).row, 2);
        let first = Cursor { row: 0, col: 0 };
        assert_eq!(page_target(&b, first, MotionKind::HalfPageUp).row, 0);
    }
}
