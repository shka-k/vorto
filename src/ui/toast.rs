//! Floating toast that surfaces info / error messages in the
//! bottom-right corner of the buffer viewport.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, Level};

/// Floating toast rendered in the bottom-right of the buffer viewport.
/// Carries the most recent `Toast::{info,warn,error}` message,
/// foreground color picked per level. Skipped when the toast has aged
/// out, when the message is empty, or when a prompt is open (the user
/// is mid-input and shouldn't have an overlay dropped on top of the
/// candidate list / preview).
///
/// Background + border match the `:command` hint panel (see
/// `hints::draw_command_hints`) so floating overlays read as one
/// consistent visual family.
pub(super) fn draw_toast(f: &mut Frame, app: &App, buf_area: Rect) {
    if app.prompt.is_open() || app.toast_remaining().is_none() {
        return;
    }
    let msg = app.toast.text();
    // First line only — multi-line diagnostics would otherwise wreck
    // the overlay. Width caps at half the viewport so the toast never
    // eats the buffer entirely on a narrow terminal.
    let text = msg.lines().next().unwrap_or("").to_string();
    if text.is_empty() {
        return;
    }
    let visible: usize = text.chars().count();
    let max = (buf_area.width / 2).max(1) as usize;
    let body = if visible > max {
        let mut t: String = text.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        text
    };
    let body_width = body.chars().count() as u16;
    // Frame: 1-cell border on each side, plus 1-cell horizontal pad
    // between the border and the text so it doesn't kiss the edges.
    let toast_w = body_width + 4;
    let toast_h = 3;
    if toast_w > buf_area.width || buf_area.height < toast_h {
        return;
    }
    let area = Rect {
        x: buf_area.x + buf_area.width - toast_w,
        y: buf_area.y + buf_area.height - toast_h,
        width: toast_w,
        height: toast_h,
    };
    let bg = Style::default().bg(super::PANEL_BG);
    let fg = match app.toast.level() {
        Level::Info => Color::Reset,
        Level::Warn => Color::Yellow,
        Level::Error => Color::Red,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(bg.fg(Color::DarkGray))
        .style(bg);
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {} ", body),
            bg.fg(fg).add_modifier(Modifier::BOLD),
        ))),
        inner,
    );
}
