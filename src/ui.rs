use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

use crate::app::{App, COMMAND_BINDS, Prompt};
use crate::fuzzy::{Finder, FuzzyKind};
use crate::mode::Mode;

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_buffer(f, app, chunks[0]);
    draw_status(f, app, chunks[1]);
    draw_command_line(f, app, chunks[2]);

    place_cursor(f, app, chunks[0]);

    if let Prompt::Command(query) = &app.prompt {
        draw_command_hints(f, query, chunks[2]);
    }
    if let Prompt::Fuzzy(finder) = &app.prompt {
        draw_fuzzy(f, finder, f.area());
    }
}

const HINT_COLS: usize = 2;
const HINT_ROWS_MAX: usize = 10;
const HINT_MAX: usize = HINT_COLS * HINT_ROWS_MAX;
const HINT_CELL_WIDTH: u16 = 28;

fn draw_command_hints(f: &mut Frame, query: &str, cmd_area: Rect) {
    // Once the user types a space they're entering an argument — hints
    // about the command name no longer help.
    if query.contains(' ') {
        return;
    }

    let hints: Vec<&crate::app::CommandBind> = COMMAND_BINDS
        .iter()
        .filter(|b| b.name.starts_with(query))
        .take(HINT_MAX)
        .collect();
    if hints.is_empty() {
        return;
    }

    let rows = hints.len().div_ceil(HINT_COLS).min(HINT_ROWS_MAX);
    let width = HINT_CELL_WIDTH * HINT_COLS as u16 + 2;
    let height = rows as u16 + 2;

    let max_w = f.area().width.saturating_sub(cmd_area.x);
    let area = Rect {
        x: cmd_area.x,
        y: cmd_area.y.saturating_sub(height),
        width: width.min(max_w),
        height: height.min(cmd_area.y),
    };
    if area.height < 3 {
        return;
    }

    f.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(" commands ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines: Vec<Line> = (0..rows)
        .map(|i| {
            let mut spans = Vec::new();
            for col in 0..HINT_COLS {
                let Some(c) = hints.get(col * rows + i) else {
                    continue;
                };
                spans.push(Span::styled(
                    format!("{:5}", c.name),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" {:21}", c.description),
                    Style::default().fg(Color::Gray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_buffer(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    let cursor_row = app.buffer.cursor.row;
    let scroll = cursor_row.saturating_sub(height.saturating_sub(1));

    let visible: Vec<Line> = app
        .buffer
        .lines
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(i, line)| {
            let num = format!("{:>4} ", i + 1);
            Line::from(vec![
                Span::styled(num, Style::default().fg(Color::DarkGray)),
                Span::raw(line.clone()),
            ])
        })
        .collect();

    let title = match &app.buffer.path {
        Some(p) => format!(
            " {} {}",
            p.display(),
            if app.buffer.dirty { "[+]" } else { "" }
        ),
        None => " [scratch] ".to_string(),
    };

    let block = Block::default().borders(Borders::ALL).title(title);
    let para = Paragraph::new(visible).block(block);
    f.render_widget(para, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let (label, color) = status_label(app);
    let mode_span = Span::styled(
        format!(" {} ", label),
        Style::default()
            .bg(color)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    let pos = format!(
        " {}:{} ",
        app.buffer.cursor.row + 1,
        app.buffer.cursor.col + 1
    );
    let status_style = if app.status.is_error() {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let line = Line::from(vec![
        mode_span,
        Span::raw(" "),
        Span::styled(app.status.text().to_string(), status_style),
        Span::raw(" "),
        Span::styled(pos, Style::default().fg(Color::Gray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn status_label(app: &App) -> (String, Color) {
    match &app.prompt {
        Prompt::None => (app.mode.to_string(), mode_color(app.mode)),
        Prompt::Command(_) => ("COMMAND".into(), Color::Yellow),
        Prompt::Search { forward: true, .. } => ("SEARCH/".into(), Color::LightBlue),
        Prompt::Search { forward: false, .. } => ("SEARCH?".into(), Color::LightBlue),
        Prompt::Fuzzy(_) => ("FUZZY".into(), Color::LightMagenta),
    }
}

fn draw_command_line(f: &mut Frame, app: &App, area: Rect) {
    let (prefix, content) = match &app.prompt {
        Prompt::Command(buf) => (":", buf.as_str()),
        Prompt::Search {
            forward: true,
            query,
        } => ("/", query.as_str()),
        Prompt::Search {
            forward: false,
            query,
        } => ("?", query.as_str()),
        _ => return,
    };
    let text = format!("{}{}", prefix, content);
    f.render_widget(Paragraph::new(text), area);
}

fn place_cursor(f: &mut Frame, app: &App, buf_area: Rect) {
    if app.prompt.is_open() {
        return;
    }
    let height = buf_area.height.saturating_sub(2) as usize;
    let scroll = app
        .buffer
        .cursor
        .row
        .saturating_sub(height.saturating_sub(1));
    let line_no_width: u16 = 5;
    let x = buf_area.x + 1 + line_no_width + app.buffer.cursor.col as u16;
    let y = buf_area.y + 1 + (app.buffer.cursor.row - scroll) as u16;
    f.set_cursor_position((x, y));
}

fn mode_color(mode: Mode) -> Color {
    match mode {
        Mode::Normal => Color::Cyan,
        Mode::Insert => Color::Green,
        Mode::Visual => Color::Magenta,
    }
}

fn draw_fuzzy(f: &mut Frame, finder: &Finder, area: Rect) {
    let popup = centered_rect(70, 60, area);
    f.render_widget(Clear, popup);

    let title = match finder.kind {
        FuzzyKind::Files => " fuzzy: files ",
        FuzzyKind::Lines => " fuzzy: lines ",
    };
    let total = finder.matches.len();
    let footer = format!(" {}/{} ", finder.selected + 1, total.max(1));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(Line::from(footer).right_aligned());
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Inside the single frame: query line on top, separator, then matches.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let query_line = Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::Yellow)),
        Span::raw(finder.query.clone()),
    ]);
    f.render_widget(Paragraph::new(query_line), chunks[0]);

    let sep = "─".repeat(chunks[1].width as usize);
    f.render_widget(
        Paragraph::new(Span::styled(sep, Style::default().fg(Color::DarkGray))),
        chunks[1],
    );

    let list_h = chunks[2].height as usize;
    let scroll = finder.selected.saturating_sub(list_h.saturating_sub(1));
    let items: Vec<ListItem> = finder
        .matches
        .iter()
        .enumerate()
        .skip(scroll)
        .take(list_h)
        .map(|(i, m)| {
            let raw = &finder.items[m.idx];
            let line = render_match(raw, &m.positions, i == finder.selected);
            ListItem::new(line)
        })
        .collect();
    f.render_widget(List::new(items), chunks[2]);
}

fn render_match<'a>(item: &'a str, positions: &[usize], selected: bool) -> Line<'a> {
    let base = if selected {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let hit = base.fg(Color::Yellow).add_modifier(Modifier::BOLD);

    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut buf_is_hit = false;
    for (i, c) in item.chars().enumerate() {
        let is_hit = positions.binary_search(&i).is_ok();
        if is_hit != buf_is_hit && !buf.is_empty() {
            let style = if buf_is_hit { hit } else { base };
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        buf.push(c);
        buf_is_hit = is_hit;
    }
    if !buf.is_empty() {
        let style = if buf_is_hit { hit } else { base };
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}
