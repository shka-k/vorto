//! Floating toasts that surface info / error messages in the
//! bottom-right corner of the buffer viewport. Up to three toasts can
//! be live simultaneously; they stack upward from the bottom-right
//! with the oldest at the bottom (next to expire) and the newest on
//! top.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, Level};
use crate::text_width::{char_cell_width, prefix_byte_len_for_width, str_cell_width};

/// Render the active toast stack into the bottom-right corner of
/// `buf_area`. Toasts are skipped entirely while a prompt is open —
/// the user is mid-input and shouldn't have an overlay dropped on top
/// of the candidate list / preview.
///
/// Fatal toasts render multi-line (the message is wrapped to half the
/// viewport width) with a `[Esc] dismiss` hint at the bottom — they
/// don't auto-expire, so the user needs a visible way out. Other
/// toasts (including regular errors) keep their single-line look
/// since they vanish on their own.
///
/// Background + border match the `:command` hint panel (see
/// `hints::draw_command_hints`) so floating overlays read as one
/// consistent visual family.
pub(super) fn draw_toast(f: &mut Frame, app: &App, buf_area: Rect) {
    if app.prompt.is_open() {
        return;
    }
    let active = app.toasts.active();
    if active.is_empty() {
        return;
    }

    let max_w = (buf_area.width / 2).max(10) as usize;
    let content_max = max_w.saturating_sub(4).max(1);
    let max_h = (buf_area.height / 2).max(3) as usize;

    // Render oldest first at the bottom; each subsequent toast stacks
    // upward. `next_y_bottom` tracks the bottom of the next slot.
    let mut next_y_bottom = buf_area.y + buf_area.height;
    for toast in active {
        let level = toast.level();
        let rect = layout_toast(
            toast.text(),
            level,
            buf_area,
            content_max,
            max_h,
            next_y_bottom,
        );
        let Some((area, lines, hint)) = rect else {
            continue;
        };
        draw_one(f, area, level, &lines, hint);
        next_y_bottom = area.y;
        if next_y_bottom <= buf_area.y {
            break;
        }
    }
}

/// Build the wrapped body + outer rect for a single toast. Returns
/// `None` when the toast would exceed the available space (truncated
/// out of view at the top of the stack).
fn layout_toast(
    msg: &str,
    level: Level,
    buf_area: Rect,
    content_max: usize,
    max_h: usize,
    next_y_bottom: u16,
) -> Option<(Rect, Vec<String>, Option<&'static str>)> {
    let is_fatal = level == Level::Fatal;
    let mut lines: Vec<String> = if is_fatal {
        let mut out = Vec::new();
        for raw in msg.lines() {
            if raw.is_empty() {
                out.push(String::new());
                continue;
            }
            let mut buf = String::new();
            let mut width = 0usize;
            for ch in raw.chars() {
                let w = char_cell_width(ch);
                if width + w > content_max && !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                    width = 0;
                }
                buf.push(ch);
                width += w;
            }
            if !buf.is_empty() {
                out.push(buf);
            }
        }
        out
    } else {
        let first = msg.lines().next().unwrap_or("");
        let s = if str_cell_width(first) > content_max {
            let cut = prefix_byte_len_for_width(first, content_max.saturating_sub(1));
            let mut t = String::with_capacity(cut + 3);
            t.push_str(&first[..cut]);
            t.push('…');
            t
        } else {
            first.to_string()
        };
        vec![s]
    };

    let hint_rows: usize = if is_fatal { 1 } else { 0 };
    let body_cap = max_h.saturating_sub(2 + hint_rows).max(1);
    if lines.len() > body_cap {
        lines.truncate(body_cap);
        if let Some(last) = lines.last_mut() {
            let cut = prefix_byte_len_for_width(last, content_max.saturating_sub(1));
            let mut t = String::with_capacity(cut + 3);
            t.push_str(&last[..cut]);
            t.push('…');
            *last = t;
        }
    }

    let body_w = lines.iter().map(|s| str_cell_width(s)).max().unwrap_or(0);
    let hint_text = "[Esc] dismiss";
    let inner_w = if is_fatal {
        body_w.max(str_cell_width(hint_text))
    } else {
        body_w
    };
    let toast_w = (inner_w + 4) as u16;
    let toast_h = (lines.len() + 2 + hint_rows) as u16;
    if toast_w > buf_area.width || next_y_bottom < buf_area.y + toast_h {
        return None;
    }
    let area = Rect {
        x: buf_area.x + buf_area.width - toast_w,
        y: next_y_bottom - toast_h,
        width: toast_w,
        height: toast_h,
    };
    let hint = if is_fatal { Some(hint_text) } else { None };
    Some((area, lines, hint))
}

fn draw_one(f: &mut Frame, area: Rect, level: Level, lines: &[String], hint: Option<&'static str>) {
    let bg = Style::default().bg(super::PANEL_BG);
    let fg = match level {
        Level::Info => Color::Reset,
        Level::Warn => Color::Yellow,
        Level::Error => Color::Red,
        Level::Fatal => Color::Red,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(bg.fg(Color::DarkGray))
        .style(bg);
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let mut rendered: Vec<Line> = lines
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                format!(" {} ", l),
                bg.fg(fg).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    if let Some(hint_text) = hint {
        rendered.push(Line::from(Span::styled(
            format!(" {} ", hint_text),
            bg.fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(rendered), inner);
}
