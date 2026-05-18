//! Cursor-anchored popup menu for `<space>a`.
//!
//! Layout: a small list of action titles in a bordered box that sits
//! directly below the cursor. If the popup would overflow the buffer
//! area on the bottom or right, it flips to sit above and/or shifts
//! left to stay inside.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Padding};

use crate::app::{App, Prompt};
use crate::text_width::{prefix_byte_len_for_width, str_cell_width};

/// Width budget for the popup. Long titles are truncated with an
/// ellipsis so a single absurd title can't widen the box past this.
/// The popup is still clamped to the buffer area, so this is only the
/// upper bound on small screens it never reaches.
const MAX_WIDTH: u16 = 120;
/// Maximum number of visible rows. Beyond this we scroll inside the
/// popup so the menu still fits in a single screen.
const MAX_HEIGHT: u16 = 24;

pub(super) fn draw_code_action_menu(f: &mut Frame, app: &App, buf_area: Rect) {
    let Prompt::CodeActionMenu { actions, selected } = &app.prompt.state else {
        return;
    };
    if actions.is_empty() {
        return;
    }

    let cursor_row = app.buffer.cursor.row;
    let Some(rel_y) = app.visual_row_offset(cursor_row) else {
        return;
    };
    // Mirror buffer::place_cursor: 1-char severity sign + 5-char line
    // number column, then the cursor's *visual* column. We don't
    // import the constants from buffer.rs to keep ui submodules
    // self-contained.
    let gutter_width: u16 = 1 + 5;
    let cursor_x = buf_area.x + gutter_width + app.cursor_visual_col() as u16;
    let cursor_y = buf_area.y + rel_y;

    let inner_w = actions
        .iter()
        .map(|a| str_cell_width(&a.title) as u16)
        .max()
        .unwrap_or(0)
        .min(MAX_WIDTH);
    // popup width = inner text + 2 border cols + 2 horizontal padding cols.
    let popup_w = (inner_w + 4).min(buf_area.width);
    let popup_h = (actions.len() as u16 + 2).min(MAX_HEIGHT + 2);

    // Prefer below the cursor; flip above when the popup would clip the
    // bottom edge of the buffer pane.
    let below_y = cursor_y.saturating_add(1);
    let space_below = buf_area.bottom().saturating_sub(below_y);
    let (y, _below) = if space_below >= popup_h {
        (below_y, true)
    } else if cursor_y >= buf_area.y + popup_h {
        (cursor_y - popup_h, false)
    } else {
        // Neither side fits cleanly — clamp to whatever space exists
        // below so the menu still appears (it'll just be shorter).
        (below_y.min(buf_area.bottom().saturating_sub(1)), true)
    };

    // Anchor the left edge to the cursor; shift left when the popup
    // would overflow the right edge of the pane.
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
        .title(" code actions ")
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Vertical scroll: keep the selection visible when the action list
    // is taller than the popup body.
    let body_h = inner.height as usize;
    let scroll = selected.saturating_sub(body_h.saturating_sub(1));
    let inner_w = inner.width as usize;

    let items: Vec<ListItem> = actions
        .iter()
        .enumerate()
        .skip(scroll)
        .take(body_h)
        .map(|(i, a)| {
            let is_sel = i == *selected;
            let title = truncate(&a.title, inner_w);
            let style = if is_sel {
                Style::default()
                    .bg(Color::Rgb(58, 78, 122))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(title, style)))
        })
        .collect();
    f.render_widget(List::new(items), inner);
}

/// Visual-width truncation: keep as many leading chars as fit within
/// `max` terminal cells, replacing the tail with `…` when something
/// was dropped.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if str_cell_width(s) <= max {
        return s.to_string();
    }
    let cut = prefix_byte_len_for_width(s, max.saturating_sub(1));
    let mut out = String::with_capacity(cut + 3);
    out.push_str(&s[..cut]);
    out.push('…');
    out
}
