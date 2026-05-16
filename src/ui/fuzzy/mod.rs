//! Fuzzy picker popup: the match list on the left, source preview on
//! the right. The preview reads through a per-`App` highlighter cache
//! so scrolling between matches in the same file is cheap.

mod list;
mod preview;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use crate::app::{App, Prompt};
use crate::finder::FuzzyKind;

pub(super) fn draw_fuzzy(f: &mut Frame, app: &App, area: Rect) {
    let Prompt::Fuzzy(finder) = &app.prompt.state else {
        return;
    };
    let popup = centered_rect(90, 80, area);
    f.render_widget(Clear, popup);

    let title = match finder.kind {
        FuzzyKind::Files { ignore } if !ignore.hidden => " fuzzy: files (+hidden) ",
        FuzzyKind::Files { .. } => " fuzzy: files ",
        FuzzyKind::Lines => " fuzzy: lines ",
        FuzzyKind::Locations => " references ",
        FuzzyKind::WorkspaceSearch => " fuzzy: workspace ",
        FuzzyKind::Buffers => " fuzzy: buffers ",
    };
    let total = finder.matches.len();
    let footer = format!(" {}/{} ", finder.selected + 1, total.max(1));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(Line::from(footer).right_aligned())
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Left: query + matches list. Right: source preview for the current
    // selection. A vertical separator visually divides the two panes.
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    list::draw_fuzzy_list(f, finder, panes[0]);

    let sep_v: Vec<Line> = (0..panes[1].height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(Color::DarkGray))))
        .collect();
    f.render_widget(Paragraph::new(sep_v), panes[1]);

    preview::draw_fuzzy_preview(f, app, finder, panes[2]);
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
