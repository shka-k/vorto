//! Status bar (mode badge, filename, cursor position) and the
//! `:` / `/` / rename line directly under it. The floating toast
//! that surfaces info / error messages lives in `ui::toast`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::action::{Operator, Token};
use crate::app::{App, Prompt};
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
    let (prefix, input) = match &app.prompt.state {
        Prompt::Command(cp) => (":", &cp.input),
        Prompt::Search {
            forward: true,
            query,
        } => ("/", query),
        Prompt::Search {
            forward: false,
            query,
        } => ("?", query),
        Prompt::Rename(buf) => ("rename ▸ ", buf),
        _ => return,
    };
    let text = format!("{}{}", prefix, input.as_str());
    f.render_widget(Paragraph::new(text), area);

    // Park the terminal cursor at the input's insertion point so the
    // user can see where typing/backspace will land. Cell width — not
    // char count — so fullwidth input (CJK, emoji) still parks the
    // caret on the right column.
    let col = (crate::text_width::str_cell_width(prefix) + input.cursor_cell_col()) as u16;
    let x = area.x + col.min(area.width.saturating_sub(1));
    f.set_cursor_position((x, area.y));
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
        Prompt::Hover { .. } => ("HOVER".into(), Color::LightBlue),
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
            Token::Op(Operator::Indent) => s.push('>'),
            Token::Op(Operator::Dedent) => s.push('<'),
            Token::SelfDouble(Operator::Delete) => s.push('d'),
            Token::SelfDouble(Operator::Yank) => s.push('y'),
            Token::SelfDouble(Operator::Change) => s.push('c'),
            Token::SelfDouble(Operator::Indent) => s.push('>'),
            Token::SelfDouble(Operator::Dedent) => s.push('<'),
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
            Token::WindowPrefix => s.push('w'),
            Token::CtrlWPrefix => s.push_str("<C-w>"),
            Token::BracketPrefix { forward } => s.push(if *forward { ']' } else { '[' }),
            // These shouldn't be in pending state (they would've fired
            // immediately or completed the parse).
            Token::Motion(_) | Token::Direct(_) | Token::Object(_) => s.push('?'),
        }
    }
    s
}
