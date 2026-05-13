//! Fuzzy picker popup: the match list on the left, source preview on
//! the right. The preview reads through a per-`App` highlighter cache
//! so scrolling between matches in the same file is cheap.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

use crate::app::{App, Prompt};
use crate::fuzzy::{Finder, FuzzyKind};
use crate::highlight::Capture;
use crate::theme;

/// Color of the highlight band drawn behind the target preview line.
const PREVIEW_HIT_BG: Color = Color::Rgb(45, 45, 60);

pub(super) fn draw_fuzzy(f: &mut Frame, app: &App, area: Rect) {
    let Prompt::Fuzzy(finder) = &app.prompt.state else {
        return;
    };
    let popup = centered_rect(90, 80, area);
    f.render_widget(Clear, popup);

    let title = match finder.kind {
        FuzzyKind::Files => " fuzzy: files ",
        FuzzyKind::Lines => " fuzzy: lines ",
        FuzzyKind::Locations => " references ",
        FuzzyKind::Buffers => " fuzzy: buffers ",
    };
    let total = finder.matches.len();
    let footer = format!(" {}/{} ", finder.selected + 1, total.max(1));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(Line::from(footer).right_aligned());
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

    draw_fuzzy_list(f, finder, panes[0]);

    let sep_v: Vec<Line> = (0..panes[1].height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(Color::DarkGray))))
        .collect();
    f.render_widget(Paragraph::new(sep_v), panes[1]);

    draw_fuzzy_preview(f, app, finder, panes[2]);
}

fn draw_fuzzy_list(f: &mut Frame, finder: &Finder, area: Rect) {
    // Inside the pane: query line on top, separator, then matches.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(area);

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

/// Render the right-hand preview pane: the source file (or buffer) for
/// the currently-selected fuzzy match, scrolled so the target line sits
/// near the middle of the viewport and rendered with a highlight band
/// behind the target line plus tree-sitter syntax coloring on the rest.
fn draw_fuzzy_preview(f: &mut Frame, app: &App, finder: &Finder, area: Rect) {
    let Some(sel) = finder.selection() else {
        return;
    };
    match finder.kind {
        FuzzyKind::Files => {
            let rel = &finder.items[sel.idx];
            let path = app.startup_cwd.join(rel);
            preview_from_file(f, app, area, &path, 0);
        }
        FuzzyKind::Lines => {
            preview_from_buffer(f, app, area, sel.idx);
        }
        FuzzyKind::Locations => {
            let Some(loc) = app.prompt.locations().get(sel.idx) else {
                return;
            };
            let Some(path) = crate::lsp::uri_to_path(&loc.uri) else {
                return;
            };
            preview_from_file(f, app, area, &path, loc.range.start.line as usize);
        }
        FuzzyKind::Buffers => {
            // The picker keeps a parallel `BufferRef` slice on the
            // prompt. Files reuse the file-preview path; Scratch has
            // no on-disk content, so we just leave the preview pane
            // blank for that entry.
            let Some(r) = app.prompt.buffer_paths().get(sel.idx) else {
                return;
            };
            match r {
                crate::app::BufferRef::Scratch => {}
                crate::app::BufferRef::File(path) => {
                    preview_from_file(f, app, area, path, 0);
                }
            }
        }
    }
}

/// Render a Lines-kind preview using the current buffer and its existing
/// highlighter (no file I/O needed).
fn preview_from_buffer(f: &mut Frame, app: &App, area: Rect, target_row: usize) {
    let lines = &app.buffer.lines;
    let height = area.height as usize;
    let (scroll, end) = preview_scroll(lines.len(), target_row, height);
    let captures = app
        .buffer
        .highlighter
        .as_ref()
        .map(|h| h.captures_in_rows(scroll, end.saturating_sub(1)))
        .unwrap_or_default();
    render_preview_lines(f, area, lines, &captures, target_row, scroll, end);
}

/// Render a Files-/Locations-kind preview. Looks up `path` in the
/// per-`App` LRU populated by the preview worker; on miss, enqueues a
/// worker request and renders plain text for this frame. On hit,
/// renders with the cached tree-sitter highlights.
fn preview_from_file(
    f: &mut Frame,
    app: &App,
    area: Rect,
    path: &std::path::Path,
    target_row: usize,
) {
    let mut lru = app.preview_lru.borrow_mut();
    let Some(entry) = lru.get(path) else {
        drop(lru);
        enqueue_preview(app, path);
        preview_plain_fallback(f, area, path, target_row);
        return;
    };
    let height = area.height as usize;
    let (scroll, end) = preview_scroll(entry.lines.len(), target_row, height);
    let captures = entry
        .highlighter
        .captures_in_rows(scroll, end.saturating_sub(1));
    render_preview_lines(f, area, &entry.lines, &captures, target_row, scroll, end);
}

/// Ask the preview worker to build a highlighted snapshot of `path`.
/// Coalesced via `last_preview_request` so we don't spam the channel
/// with duplicates while the worker is busy with the same path.
fn enqueue_preview(app: &App, path: &std::path::Path) {
    let mut last = app.last_preview_request.borrow_mut();
    if last.as_deref() == Some(path) {
        return;
    }
    *last = Some(path.to_path_buf());
    let _ = app.preview_tx.send(path.to_path_buf());
}

fn preview_plain_fallback(
    f: &mut Frame,
    area: Rect,
    path: &std::path::Path,
    target_row: usize,
) {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            let height = area.height as usize;
            let (scroll, end) = preview_scroll(lines.len(), target_row, height);
            render_preview_lines(f, area, &lines, &[], target_row, scroll, end);
        }
        Err(_) => {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "(cannot read file)",
                    Style::default().fg(Color::DarkGray),
                )),
                area,
            );
        }
    }
}

/// Window the preview shows: center `target` in `height` rows, then pin
/// to the file bounds so we never scroll past the last line.
fn preview_scroll(lines_len: usize, target: usize, height: usize) -> (usize, usize) {
    if height == 0 || lines_len == 0 {
        return (0, 0);
    }
    let target = target.min(lines_len - 1);
    let half = height / 2;
    let max_scroll = lines_len.saturating_sub(height);
    let scroll = target.saturating_sub(half).min(max_scroll);
    let end = (scroll + height).min(lines_len);
    (scroll, end)
}

/// Render `[scroll..end)` lines into `area` with line numbers, a band
/// behind the target row, and per-character styling from `captures`
/// (tree-sitter highlight names resolved through `theme::style_for`).
/// `captures` should already be scoped to the visible window so the
/// inner-loop filter is cheap.
fn render_preview_lines(
    f: &mut Frame,
    area: Rect,
    lines: &[String],
    captures: &[Capture],
    target_row: usize,
    scroll: usize,
    end: usize,
) {
    let height = area.height as usize;
    let width = area.width as usize;
    if height == 0 || width == 0 || end <= scroll {
        return;
    }
    let target = target_row.min(lines.len().saturating_sub(1));
    let lineno_w = end.to_string().len().max(3);
    let text_width = width.saturating_sub(lineno_w + 1);

    let mut out: Vec<Line> = Vec::with_capacity(end - scroll);
    for (i, line) in lines.iter().enumerate().take(end).skip(scroll) {
        out.push(render_preview_row(
            i,
            line,
            captures,
            i == target,
            lineno_w,
            text_width,
        ));
    }
    f.render_widget(Paragraph::new(out), area);
}

fn render_preview_row(
    row: usize,
    line: &str,
    captures: &[Capture],
    is_target: bool,
    lineno_w: usize,
    text_width: usize,
) -> Line<'static> {
    // Truncate to the visible column window before doing any work — the
    // capture filter below is still keyed off the *full* char index so it
    // stays correct for lines that wrap past the right edge.
    let chars: Vec<char> = line.chars().take(text_width).collect();

    // Per-char base style from captures intersecting this row. Later
    // captures win, matching how the buffer renders highlights.
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

    let num_style = if is_target {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
            .bg(PREVIEW_HIT_BG)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let num = format!("{:>width$} ", row + 1, width = lineno_w);
    let mut spans: Vec<Span<'static>> = vec![Span::styled(num, num_style)];

    let resolve = |col: usize| -> Style {
        let mut s = base[col];
        if is_target {
            s = s.bg(PREVIEW_HIT_BG);
        }
        s
    };

    if chars.is_empty() {
        if is_target {
            spans.push(Span::styled(
                " ".repeat(text_width),
                Style::default().bg(PREVIEW_HIT_BG),
            ));
        }
        return Line::from(spans);
    }

    let mut buf = String::new();
    let mut buf_style = resolve(0);
    for (col, &c) in chars.iter().enumerate() {
        let s = resolve(col);
        if s != buf_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
            buf_style = s;
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, buf_style));
    }
    // Pad target row to the right edge so the highlight band reaches the
    // separator even when the line is shorter than the pane.
    if is_target && chars.len() < text_width {
        spans.push(Span::styled(
            " ".repeat(text_width - chars.len()),
            Style::default().bg(PREVIEW_HIT_BG),
        ));
    }
    Line::from(spans)
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
