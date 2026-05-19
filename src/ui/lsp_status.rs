//! Screen-centered modal for `:lsp`.
//!
//! Read-only counterpart to [`super::hover::draw_hover`]: shows a
//! pre-formatted listing of every configured LSP server and its
//! current running state. Unlike Hover (cursor-anchored), this popup
//! is centered on the frame because it isn't tied to a cursor
//! position — it's a global status view.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, Prompt};
use crate::text_width::str_cell_width;

const MAX_WIDTH: u16 = 90;
const MAX_HEIGHT: u16 = 30;

pub(super) fn draw_lsp_status(f: &mut Frame, app: &App, area: Rect) {
    let Prompt::LspStatus { content, scroll } = &app.prompt.state else {
        return;
    };
    if area.width < 8 || area.height < 4 {
        return;
    }

    // Body width tracks the longest line, capped to MAX_WIDTH and the
    // available frame width.
    let longest = content
        .lines()
        .map(|l| str_cell_width(l) as u16)
        .max()
        .unwrap_or(0);
    let inner_w = longest.clamp(20, MAX_WIDTH);
    let popup_w = (inner_w + 2).min(area.width.saturating_sub(2));

    let line_count = content.lines().count() as u16;
    let popup_h = (line_count + 2).min(MAX_HEIGHT + 2).min(area.height);

    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" lsp status ")
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let body_h = inner.height as usize;
    let max_scroll = (line_count as usize).saturating_sub(body_h);
    let scroll_v = (*scroll).min(max_scroll) as u16;

    let lines: Vec<Line> = content.lines().map(Line::from).collect();
    let para = Paragraph::new(lines).scroll((scroll_v, 0));
    f.render_widget(para, inner);
}
