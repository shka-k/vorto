//! Cursor-anchored popup for `K` (LSP hover).
//!
//! Read-only counterpart to `code_action::draw_code_action_menu`: shows
//! the hover content in a bordered box, anchored just below the cursor
//! and flipped above when there isn't room below. Long content is
//! word-wrapped to the popup width; the user scrolls with j/k or arrow
//! keys.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, Prompt};

/// Width budget for the popup. Wider than the code-action menu because
/// hover content typically includes function signatures.
const MAX_WIDTH: u16 = 80;
/// Maximum visible rows. Beyond this the content scrolls inside the
/// popup so the menu still fits in one screen.
const MAX_HEIGHT: u16 = 20;

pub(super) fn draw_hover(f: &mut Frame, app: &App, buf_area: Rect) {
    let Prompt::Hover { content, scroll } = &app.prompt.state else {
        return;
    };
    if content.is_empty() {
        return;
    }

    let buffer_scroll = app.buffer.scroll.get();
    let cursor_row = app.buffer.cursor.row;
    if cursor_row < buffer_scroll {
        return;
    }
    // Mirror buffer::place_cursor: 1-char severity sign + 5-char line
    // number column. Same trick as code_action.rs.
    let gutter_width: u16 = 1 + 5;
    let cursor_x = buf_area.x + gutter_width + app.buffer.cursor.col as u16;
    let cursor_y = buf_area.y + (cursor_row - buffer_scroll) as u16;

    // The longest line caps the inner width, but never beyond MAX_WIDTH.
    // Inner width also bounds the available content area on narrow
    // terminals.
    let longest = content
        .lines()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let inner_w = longest.min(MAX_WIDTH);
    let popup_w = (inner_w + 2).min(buf_area.width); // +2 for borders
    if popup_w < 4 {
        return;
    }
    let inner_text_w = popup_w.saturating_sub(2) as usize;

    // Estimate wrapped line count so the popup height tracks the actual
    // rendered content. Rough — counts each source line as
    // `ceil(len / inner_w)`. Empty lines count as 1.
    let wrapped_lines: usize = content
        .lines()
        .map(|l| {
            let n = l.chars().count();
            if n == 0 {
                1
            } else {
                n.div_ceil(inner_text_w.max(1))
            }
        })
        .sum();
    let popup_h = (wrapped_lines as u16 + 2).min(MAX_HEIGHT + 2);

    // Prefer below the cursor, flip above when the bottom would clip.
    let below_y = cursor_y.saturating_add(1);
    let space_below = buf_area.bottom().saturating_sub(below_y);
    let y = if space_below >= popup_h {
        below_y
    } else if cursor_y >= buf_area.y + popup_h {
        cursor_y - popup_h
    } else {
        below_y.min(buf_area.bottom().saturating_sub(1))
    };

    let max_x = buf_area.right().saturating_sub(popup_w);
    let x = cursor_x.min(max_x).max(buf_area.x);

    let area = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h.min(buf_area.bottom().saturating_sub(y)),
    };
    if area.width <= 2 || area.height <= 2 {
        return;
    }

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" hover ")
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let body_h = inner.height as usize;
    // Clamp scroll so we never page past the end. Wrapped-line count is
    // an estimate, so allow a small overshoot rather than fight it.
    let max_scroll = wrapped_lines.saturating_sub(body_h);
    let scroll_v = (*scroll).min(max_scroll) as u16;

    let lines: Vec<Line> = content.lines().map(Line::from).collect();
    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_v, 0));
    f.render_widget(para, inner);
}
