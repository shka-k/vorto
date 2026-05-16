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
///
/// Errors render multi-line (the message is wrapped to half the
/// viewport width) with a `[Esc] dismiss` hint at the bottom — they
/// don't auto-expire, so the user needs a visible way out. Non-error
/// toasts keep their single-line look since they vanish on their own.
pub(super) fn draw_toast(f: &mut Frame, app: &App, buf_area: Rect) {
    if app.prompt.is_open() || app.toast_remaining().is_none() {
        return;
    }
    let msg = app.toast.text();
    if msg.is_empty() {
        return;
    }
    let level = app.toast.level();
    let is_error = level == Level::Error;

    // Width budget: cap content width at half the viewport so the
    // toast never eats the whole buffer. The 4 covers 1-cell borders
    // and 1-cell internal padding on each side.
    let max_w = (buf_area.width / 2).max(10) as usize;
    let content_max = max_w.saturating_sub(4).max(1);

    let mut lines: Vec<String> = if is_error {
        // Wrap each source line to `content_max`. Hard wrap (by char)
        // since diagnostic messages often have no spaces near the cap
        // (`Query error at 25:4...`).
        let mut out = Vec::new();
        for raw in msg.lines() {
            if raw.is_empty() {
                out.push(String::new());
                continue;
            }
            let mut buf = String::new();
            let mut count = 0usize;
            for ch in raw.chars() {
                buf.push(ch);
                count += 1;
                if count >= content_max {
                    out.push(std::mem::take(&mut buf));
                    count = 0;
                }
            }
            if !buf.is_empty() {
                out.push(buf);
            }
        }
        out
    } else {
        // Non-error: first line only, ellipsised — same as before.
        let first = msg.lines().next().unwrap_or("");
        let s = if first.chars().count() > content_max {
            let mut t: String = first.chars().take(content_max.saturating_sub(1)).collect();
            t.push('…');
            t
        } else {
            first.to_string()
        };
        vec![s]
    };

    // Cap height to half the viewport so a giant message can't push
    // the buffer off-screen; trailing lines past the cap get an
    // ellipsis marker.
    let max_h = (buf_area.height / 2).max(3) as usize;
    let hint_rows: usize = if is_error { 1 } else { 0 };
    let body_cap = max_h.saturating_sub(2 + hint_rows).max(1);
    if lines.len() > body_cap {
        lines.truncate(body_cap);
        if let Some(last) = lines.last_mut() {
            // Replace tail with ellipsis to signal truncation.
            let truncated: String = last
                .chars()
                .take(content_max.saturating_sub(1))
                .collect::<String>();
            *last = format!("{}…", truncated);
        }
    }

    let body_w = lines.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let hint_text = "[Esc] dismiss";
    let inner_w = if is_error {
        body_w.max(hint_text.chars().count())
    } else {
        body_w
    };
    let toast_w = (inner_w + 4) as u16;
    let toast_h = (lines.len() + 2 + hint_rows) as u16;
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
    let fg = match level {
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

    let mut rendered: Vec<Line> = lines
        .into_iter()
        .map(|l| {
            Line::from(Span::styled(
                format!(" {} ", l),
                bg.fg(fg).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    if is_error {
        rendered.push(Line::from(Span::styled(
            format!(" {} ", hint_text),
            bg.fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(rendered), inner);
}
