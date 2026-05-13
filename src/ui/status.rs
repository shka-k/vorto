//! Status bar (mode badge, message, diagnostic, pending tokens, cursor
//! position) and the `:` / `/` / rename command line directly under it.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::action::{Operator, Token};
use crate::app::{App, Prompt};
use crate::lsp::{Diagnostic, Severity};
use crate::mode::Mode;

const STATUS_RIGHT_WIDTH: u16 = 24;

pub(super) fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    // Split the status bar so the right end can carry pending-key feedback
    // and the cursor position right-aligned, while the left grows the
    // mode badge + status message.
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(STATUS_RIGHT_WIDTH)])
        .split(area);

    let (label, color) = status_label(app);
    let mode_span = Span::styled(
        format!(" {} ", label),
        Style::default()
            .bg(color)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    let status_style = if app.status.is_error() {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    // Show the diagnostic under the cursor only when the regular status
    // text is the default info — explicit errors and recent commands
    // should still win the line.
    let mut spans = vec![
        mode_span,
        Span::raw(" "),
        Span::styled(app.status.text().to_string(), status_style),
    ];
    let show_diag = !app.status.is_error();
    if show_diag && let Some(d) = app.diagnostic_on_cursor() {
        spans.push(Span::raw("  "));
        spans.push(diagnostic_span(d));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), halves[0]);

    let pos = format!(
        "{}:{} ",
        app.buffer.cursor.row + 1,
        app.buffer.cursor.col + 1
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
        halves[1],
    );
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

fn diagnostic_span(d: &Diagnostic) -> Span<'static> {
    let color = match d.severity {
        Severity::Error => Color::Red,
        Severity::Warning => Color::Yellow,
        Severity::Info => Color::LightBlue,
        Severity::Hint => Color::DarkGray,
    };
    // First line only — diagnostics with embedded newlines (rust-analyzer
    // does this for explanations) would otherwise wreck the status bar.
    let text = d.message.lines().next().unwrap_or("").to_string();
    let prefix = match &d.source {
        Some(s) => format!("[{}] ", s),
        None => String::new(),
    };
    Span::styled(format!("{}{}", prefix, text), Style::default().fg(color))
}

fn status_label(app: &App) -> (String, Color) {
    match &app.prompt.state {
        Prompt::None => (app.mode.to_string(), mode_color(app.mode)),
        Prompt::Command(_) => ("COMMAND".into(), Color::Yellow),
        Prompt::Search { forward: true, .. } => ("SEARCH/".into(), Color::LightBlue),
        Prompt::Search { forward: false, .. } => ("SEARCH?".into(), Color::LightBlue),
        Prompt::Fuzzy(_) => ("FUZZY".into(), Color::LightMagenta),
        Prompt::Rename(_) => ("RENAME".into(), Color::LightCyan),
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
