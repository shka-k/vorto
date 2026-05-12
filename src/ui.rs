use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Padding, Paragraph};

use crate::action::{Operator, Token};
use crate::app::{App, COMMAND_BINDS, Prompt, Selection};
use crate::fuzzy::{Finder, FuzzyKind};
use crate::highlight::Capture;
use crate::keymap::{OBJECT_BINDINGS, OP_PENDING_BINDINGS};
use crate::lsp::{Diagnostic, Severity};
use crate::mode::Mode;
use crate::theme;

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

    if let Prompt::Command(query) = &app.prompt.state {
        draw_command_hints(f, query, chunks[2]);
    }
    if let Prompt::Fuzzy(finder) = &app.prompt.state {
        draw_fuzzy(f, finder, f.area());
    }
    if !app.prompt.is_open() {
        draw_pending_hints(f, app, chunks[1]);
    }
}

const HINT_COLS: usize = 2;
const HINT_ROWS_MAX: usize = 10;
const HINT_MAX: usize = HINT_COLS * HINT_ROWS_MAX;
/// Slightly darker than ANSI 8 (bright black) — sits clearly behind the
/// buffer text without being pure black. Approximate `#1e1e1e`.
const HINT_BG: Color = Color::Rgb(30, 30, 30);
const HINT_PAD_X: u16 = 1;
const HINT_PAD_Y: u16 = 1;

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
    let height = rows as u16 + 2 * HINT_PAD_Y + 2;

    let screen = f.area();
    let area = Rect {
        x: 0,
        y: cmd_area.y.saturating_sub(height),
        width: screen.width,
        height: height.min(cmd_area.y),
    };
    if area.height == 0 {
        return;
    }

    let bg = Style::default().bg(HINT_BG);
    let title = " commands ";
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(bg.fg(Color::DarkGray))
        .title(Span::styled(
            title,
            bg.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
        .style(bg)
        .padding(Padding::new(HINT_PAD_X, HINT_PAD_X, HINT_PAD_Y, HINT_PAD_Y));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    // Split the inner area into two equal columns. Hints flow column-major
    // (column 0 takes hints[0..rows], column 1 takes hints[rows..2*rows]).
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    let render_column = |start: usize| -> Vec<Line<'static>> {
        hints
            .iter()
            .skip(start)
            .take(rows)
            .map(|c| {
                Line::from(vec![
                    Span::styled(
                        format!("{:5}", c.name),
                        bg.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {}", c.description), bg.fg(Color::Gray)),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(render_column(0)).style(bg), columns[0]);
    f.render_widget(Paragraph::new(render_column(rows)).style(bg), columns[1]);
}

fn draw_buffer(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    let scroll = compute_scroll(app, height);

    let sel = app.selection();
    let last_visible = scroll + height;
    let captures = app
        .buffer
        .highlighter
        .as_ref()
        .map(|h| h.captures_in_rows(scroll, last_visible))
        .unwrap_or_default();
    let row_severity = build_row_severity(app, scroll, last_visible);

    let visible: Vec<Line> = app
        .buffer
        .lines
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(i, line)| {
            let mut spans = vec![sign_span(row_severity.get(&i).copied())];
            let num = format!("{:>4} ", i + 1);
            spans.push(Span::styled(num, Style::default().fg(Color::DarkGray)));
            spans.extend(render_line(i, line, sel.as_ref(), &captures));
            Line::from(spans)
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

/// Color used to paint visually-selected text. Picked to read clearly on
/// both dark and light terminals.
const SEL_BG: Color = Color::Rgb(58, 78, 122);

/// Width of the gutter prefix (severity sign + space). Kept in sync with
/// `place_cursor` so the cursor lands on the right column.
const GUTTER_SIGN_WIDTH: u16 = 1;

/// Build a `row → highest severity` lookup for the visible window. Rows
/// outside `[scroll, last)` are skipped, multi-line diagnostics fill all
/// rows they span, and the most severe diagnostic wins per row.
fn build_row_severity(
    app: &App,
    scroll: usize,
    last: usize,
) -> std::collections::HashMap<usize, Severity> {
    let mut map: std::collections::HashMap<usize, Severity> = std::collections::HashMap::new();
    let diags = match app.current_diagnostics() {
        Some(d) => d,
        None => return map,
    };
    for d in diags {
        let lo = d.range.start.line as usize;
        let hi = d.range.end.line as usize;
        for row in lo.max(scroll)..=hi.min(last.saturating_sub(1)) {
            map.entry(row)
                .and_modify(|s| {
                    if (d.severity as u8) < (*s as u8) {
                        *s = d.severity;
                    }
                })
                .or_insert(d.severity);
        }
    }
    map
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

fn sign_span(sev: Option<Severity>) -> Span<'static> {
    match sev {
        Some(Severity::Error) => Span::styled("E", Style::default().fg(Color::Red)),
        Some(Severity::Warning) => Span::styled("W", Style::default().fg(Color::Yellow)),
        Some(Severity::Info) => Span::styled("I", Style::default().fg(Color::LightBlue)),
        Some(Severity::Hint) => Span::styled("H", Style::default().fg(Color::DarkGray)),
        None => Span::raw(" "),
    }
}

/// Render one buffer line, layering syntax-highlight captures
/// (foreground) underneath the visual-selection background. Spans
/// group consecutive characters that share the same resolved style so
/// the terminal sees as few escape changes as possible.
///
/// `captures` is the row-range slice produced by the highlighter for
/// the visible window; we filter per row internally rather than
/// re-extracting per call.
fn render_line(
    row: usize,
    line: &str,
    sel: Option<&Selection>,
    captures: &[Capture],
) -> Vec<Span<'static>> {
    let is_selected = |col: usize| -> bool {
        let Some(sel) = sel else { return false };
        match *sel {
            Selection::Char { from, to } => {
                if row < from.row || row > to.row {
                    return false;
                }
                let lo = if row == from.row { from.col } else { 0 };
                if row < to.row {
                    col >= lo
                } else {
                    col >= lo && col <= to.col
                }
            }
            Selection::Line { from_row, to_row } => row >= from_row && row <= to_row,
            Selection::Block { r0, c0, r1, c1 } => row >= r0 && row <= r1 && col >= c0 && col <= c1,
        }
    };

    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        if is_selected(0) {
            return vec![Span::styled(" ".to_string(), Style::default().bg(SEL_BG))];
        }
        return Vec::new();
    }

    // Build the per-character base (highlight) style. Captures are
    // sorted in document order; later-arriving captures overwrite
    // earlier ones for the same character, matching the convention
    // that more-specific rules appear later in `highlights.scm`.
    let mut base: Vec<Style> = vec![Style::default(); chars.len()];
    for cap in captures {
        if cap.end_row < row || cap.start_row > row {
            continue;
        }
        let lo = if cap.start_row == row {
            cap.start_col
        } else {
            0
        };
        let hi = if cap.end_row == row {
            cap.end_col.min(chars.len())
        } else {
            chars.len()
        };
        if lo >= hi {
            continue;
        }
        let style = theme::style_for(&cap.name);
        for slot in base.iter_mut().take(hi).skip(lo) {
            *slot = style;
        }
    }

    // Overlay the visual-selection background per char.
    let style_at = |col: usize| -> Style {
        let mut s = base[col];
        if is_selected(col) {
            s = s.bg(SEL_BG);
        }
        s
    };

    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut buf_style = style_at(0);
    for (col, &c) in chars.iter().enumerate() {
        let s = style_at(col);
        if s != buf_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
            buf_style = s;
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, buf_style));
    }
    spans
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
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
    if !app.status.is_error()
        && let Some(d) = app.diagnostic_on_cursor()
    {
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

const STATUS_RIGHT_WIDTH: u16 = 24;

const PENDING_HINT_WIDTH: u16 = 32;
const PENDING_HINT_ROWS_MAX: u16 = 12;

/// Which-key-style panel that lists valid continuations when the token
/// stream is mid-sequence. Derives hints by inspecting the trailing
/// token to figure out which parse context we're in.
fn draw_pending_hints(f: &mut Frame, app: &App, status_area: Rect) {
    let (name, entries) = match pending_hints(&app.tokens) {
        Some(p) => p,
        None => return,
    };
    if entries.is_empty() {
        return;
    }

    let rows = (entries.len() as u16).min(PENDING_HINT_ROWS_MAX);
    let width = PENDING_HINT_WIDTH + 2 * HINT_PAD_X + 2;
    let height = rows + 2 * HINT_PAD_Y + 2;

    let screen = f.area();
    let x = screen.width.saturating_sub(width);
    let y = status_area.y.saturating_sub(height);
    let area = Rect {
        x,
        y,
        width: width.min(screen.width.saturating_sub(x)),
        height: height.min(status_area.y),
    };
    if area.height == 0 {
        return;
    }

    let bg = Style::default().bg(HINT_BG);
    let title = format!(" {} ", name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(bg.fg(Color::DarkGray))
        .title(Span::styled(
            title,
            bg.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
        .style(bg)
        .padding(Padding::new(HINT_PAD_X, HINT_PAD_X, HINT_PAD_Y, HINT_PAD_Y));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let body_rows = inner.height as usize;
    let lines: Vec<Line> = entries
        .iter()
        .take(body_rows)
        .map(|(k, desc)| {
            Line::from(vec![
                Span::styled(
                    format!("{:>4} ", k),
                    bg.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc.to_string(), bg.fg(Color::Gray)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).style(bg), inner);
}

/// Hint entries for the current token state. Returns `None` when nothing
/// useful can be hinted (initial state, or in the middle of a count
/// without further context).
fn pending_hints(tokens: &[Token]) -> Option<(&'static str, Vec<(String, &'static str)>)> {
    // Find the trailing non-Count token — counts don't change what the
    // hint context is.
    let last = tokens
        .iter()
        .rev()
        .find(|t| !matches!(t, Token::Count(_)))?;
    let (name, entries) = match last {
        Token::LeaderPrefix => (
            "leader",
            vec![
                ("f".to_string(), "fuzzy files"),
                ("l".to_string(), "fuzzy lines"),
            ],
        ),
        Token::GotoPrefix => (
            "goto",
            vec![
                ("g".to_string(), "goto file start"),
                ("d".to_string(), "definition (lsp)"),
                ("D".to_string(), "declaration (lsp)"),
                ("i".to_string(), "implementation (lsp)"),
                ("r".to_string(), "references (lsp)"),
            ],
        ),
        Token::Op(op) => {
            // Each operator's repeat-self shortcut (dd/yy/cc) is the only
            // hint entry that depends on `op`; the rest of the menu is
            // the static OpPending binding table.
            let (name, self_key, self_label) = match op {
                Operator::Delete => ("delete", "d", "delete line (dd)"),
                Operator::Yank => ("yank", "y", "yank line (yy)"),
                Operator::Change => ("change", "c", "change line (cc)"),
            };
            let mut entries = vec![(self_key.to_string(), self_label)];
            entries.extend(
                OP_PENDING_BINDINGS
                    .iter()
                    .map(|b| (b.display_key(), b.label)),
            );
            (name, entries)
        }
        Token::Scope(scope) => {
            let name = match scope {
                crate::action::Scope::Inner => "inner",
                crate::action::Scope::Around => "around",
            };
            let entries = OBJECT_BINDINGS
                .iter()
                .map(|b| (b.display_key(), b.label))
                .collect();
            (name, entries)
        }
        _ => return None,
    };
    Some((name, entries))
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
            // These shouldn't be in pending state (they would've fired
            // immediately or completed the parse).
            Token::Motion(_) | Token::Direct(_) | Token::Object(_) => s.push('?'),
        }
    }
    s
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

fn draw_command_line(f: &mut Frame, app: &App, area: Rect) {
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

fn place_cursor(f: &mut Frame, app: &App, buf_area: Rect) {
    if app.prompt.is_open() {
        return;
    }
    let height = buf_area.height.saturating_sub(2) as usize;
    let scroll = compute_scroll(app, height);
    let line_no_width: u16 = 5;
    let x = buf_area.x + 1 + GUTTER_SIGN_WIDTH + line_no_width + app.buffer.cursor.col as u16;
    let y = buf_area.y + 1 + (app.buffer.cursor.row - scroll) as u16;
    f.set_cursor_position((x, y));
}

/// Update and return the viewport scroll position. Sticky: the scroll
/// only moves when the cursor would otherwise fall outside the
/// visible `height`-row window. Cursor-above-viewport scrolls up so
/// the cursor sits on the top line; cursor-below-viewport scrolls
/// down so the cursor sits on the bottom line. Otherwise the existing
/// scroll is preserved — which is what fixes "cursor stuck at the
/// bottom" on upward movement.
fn compute_scroll(app: &App, height: usize) -> usize {
    let cur = app.buffer.cursor.row;
    let mut scroll = app.buffer.scroll.get();
    if cur < scroll {
        scroll = cur;
    } else if height > 0 && cur >= scroll + height {
        scroll = cur + 1 - height;
    }
    app.buffer.scroll.set(scroll);
    scroll
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

fn draw_fuzzy(f: &mut Frame, finder: &Finder, area: Rect) {
    let popup = centered_rect(70, 60, area);
    f.render_widget(Clear, popup);

    let title = match finder.kind {
        FuzzyKind::Files => " fuzzy: files ",
        FuzzyKind::Lines => " fuzzy: lines ",
        FuzzyKind::Locations => " references ",
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
