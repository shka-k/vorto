//! Buffer viewport: gutter (diagnostic signs + line numbers),
//! per-character syntax highlighting layered with the visual selection,
//! and the terminal cursor placement that goes with it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Selection};
use crate::lsp::Severity;
use crate::syntax::{self, Capture};

/// Color used to paint visually-selected text. Picked to read clearly on
/// both dark and light terminals.
const SEL_BG: Color = Color::Rgb(58, 78, 122);

/// Width of the gutter prefix (severity sign + space). Kept in sync with
/// [`place_cursor`] so the cursor lands on the right column.
const GUTTER_SIGN_WIDTH: u16 = 1;

pub(super) fn draw_buffer(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height as usize;
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

    f.render_widget(Paragraph::new(visible), area);
}

pub(super) fn place_cursor(f: &mut Frame, app: &App, buf_area: Rect) {
    if app.prompt.is_open() {
        return;
    }
    let height = buf_area.height as usize;
    let scroll = compute_scroll(app, height);
    let line_no_width: u16 = 5;
    let x = buf_area.x + GUTTER_SIGN_WIDTH + line_no_width + app.buffer.cursor.col as u16;
    let y = buf_area.y + (app.buffer.cursor.row - scroll) as u16;
    f.set_cursor_position((x, y));
}

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
        let style = syntax::style_for(&cap.name);
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
    // Publish the height so `H`/`M`/`L` and the `<C-d>`/`<C-u>` family
    // (handled in the input thread) can read what's currently visible.
    app.buffer.viewport_height.set(height);
    scroll
}
