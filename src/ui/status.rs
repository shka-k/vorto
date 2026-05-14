//! Status bar (mode badge, filename, cursor position), the
//! `:` / `/` / rename line directly under it, and the floating
//! toast that surfaces info / error messages in the top-right
//! corner of the buffer viewport.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::action::{Operator, Token};
use crate::app::{App, Level, Prompt};
use crate::mode::Mode;

const STATUS_LEFT_WIDTH: u16 = 14;
const STATUS_RIGHT_WIDTH: u16 = 24;

pub(super) fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    // Three columns: mode badge on the left, filename centered, pending
    // tokens + cursor position right-aligned.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(STATUS_LEFT_WIDTH),
            Constraint::Min(1),
            Constraint::Length(STATUS_RIGHT_WIDTH),
        ])
        .split(area);

    let (label, color) = status_label(app);
    let mode_span = Span::styled(
        format!(" {} ", label),
        Style::default()
            .bg(color)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(Paragraph::new(Line::from(vec![mode_span])), cols[0]);

    let name = file_label(app);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            name,
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        cols[1],
    );

    // Visual column (tab-expanded), so the displayed `col` matches the
    // cell the cursor visibly sits on. Using `cursor.col` directly would
    // disagree with the on-screen position whenever a tab sits between
    // the line start and the cursor.
    let pos = format!(
        "{}:{} ",
        app.buffer.cursor.row + 1,
        app.cursor_visual_col() + 1
    );
    let pending = format_pending(&app.tokens);
    let mut right_spans = Vec::new();
    if !pending.is_empty() {
        right_spans.push(Span::styled(
            pending,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        right_spans.push(Span::raw(" "));
    }
    right_spans.push(Span::styled(pos, Style::default().fg(Color::Gray)));
    f.render_widget(
        Paragraph::new(Line::from(right_spans)).alignment(Alignment::Right),
        cols[2],
    );
}

/// Floating toast rendered in the top-right of the buffer viewport.
/// Carries the most recent `Status::{info,warn,error}` message,
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
    let msg = app.status.text();
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
        y: buf_area.y,
        width: toast_w,
        height: toast_h,
    };
    let bg = Style::default().bg(super::PANEL_BG);
    let fg = match app.status.level() {
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

fn file_label(app: &App) -> String {
    match &app.buffer.path {
        Some(p) => {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            if app.buffer.dirty {
                format!("{} [+]", name)
            } else {
                name
            }
        }
        None => "[scratch]".to_string(),
    }
}

pub(super) fn draw_command_line(f: &mut Frame, app: &App, area: Rect) {
    let (prefix, content) = match &app.prompt.state {
        Prompt::Command(buf) => (":", buf.as_str()),
        Prompt::Search {
            forward: true,
            query,
        } => ("/", query.as_str()),
        Prompt::Search {
            forward: false,
            query,
        } => ("?", query.as_str()),
        Prompt::Rename(buf) => ("rename ▸ ", buf.as_str()),
        _ => return,
    };
    let text = format!("{}{}", prefix, content);
    f.render_widget(Paragraph::new(text), area);
}

fn status_label(app: &App) -> (String, Color) {
    match &app.prompt.state {
        Prompt::None => (app.mode.to_string(), mode_color(app.mode)),
        Prompt::Command(_) => ("COMMAND".into(), Color::Yellow),
        Prompt::Search { forward: true, .. } => ("SEARCH/".into(), Color::LightBlue),
        Prompt::Search { forward: false, .. } => ("SEARCH?".into(), Color::LightBlue),
        Prompt::Fuzzy(_) => ("FUZZY".into(), Color::LightMagenta),
        Prompt::Rename(_) => ("RENAME".into(), Color::LightCyan),
        Prompt::CodeActionMenu { .. } => ("CODE ACTION".into(), Color::LightMagenta),
    }
}

fn mode_color(mode: Mode) -> Color {
    match mode {
        Mode::Normal => Color::Cyan,
        Mode::Insert => Color::Green,
        Mode::Visual => Color::Magenta,
        Mode::VisualLine => Color::LightMagenta,
        Mode::VisualBlock => Color::LightRed,
    }
}

/// Render the un-resolved token stream as a short vim-style hint
/// (e.g. `[Count(2), Count(0), Op(Delete)]` → `"20d"`).
fn format_pending(tokens: &[Token]) -> String {
    let mut s = String::new();
    for t in tokens {
        match t {
            Token::Count(d) => s.push_str(&d.to_string()),
            Token::Op(Operator::Delete) => s.push('d'),
            Token::Op(Operator::Yank) => s.push('y'),
            Token::Op(Operator::Change) => s.push('c'),
            Token::SelfDouble(Operator::Delete) => s.push('d'),
            Token::SelfDouble(Operator::Yank) => s.push('y'),
            Token::SelfDouble(Operator::Change) => s.push('c'),
            Token::Scope(crate::action::Scope::Inner) => s.push('i'),
            Token::Scope(crate::action::Scope::Around) => s.push('a'),
            Token::LeaderPrefix => s.push_str("<space>"),
            Token::GotoPrefix => s.push('g'),
            Token::FindCharPrefix { forward, till } => {
                s.push(match (forward, till) {
                    (true, false) => 'f',
                    (false, false) => 'F',
                    (true, true) => 't',
                    (false, true) => 'T',
                });
            }
            Token::ZPrefix => s.push('z'),
            Token::ReplaceCharPrefix => s.push('r'),
            // These shouldn't be in pending state (they would've fired
            // immediately or completed the parse).
            Token::Motion(_) | Token::Direct(_) | Token::Object(_) => s.push('?'),
        }
    }
    s
}
