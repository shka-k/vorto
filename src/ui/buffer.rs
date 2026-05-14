//! Buffer viewport: gutter (diagnostic signs + line numbers),
//! per-character syntax highlighting layered with the visual selection,
//! and the terminal cursor placement that goes with it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, JumpState, Selection};
use crate::lsp::Severity;
use crate::syntax::{self, Capture};
use crate::vcs::LineStatus;

use std::collections::HashMap;

/// Color used to paint visually-selected text. Picked to read clearly on
/// both dark and light terminals.
const SEL_BG: Color = Color::Rgb(58, 78, 122);

/// Background used to highlight every visible match of the active
/// search pattern (vim's `hlsearch`). ANSI bright-black (the terminal's
/// dim gray) so it sits underneath text without competing with a
/// visual selection.
const SEARCH_HIT_BG: Color = Color::DarkGray;

/// Background used to render each extra-cursor cell. Distinct from
/// `SEL_BG` and `SEARCH_HIT_BG` so a stacked cursor remains visible
/// even when it sits inside a selection or a search match.
const EXTRA_CURSOR_BG: Color = Color::Rgb(160, 110, 60);

/// Foreground used for `gw` jump labels. Bright magenta on a near-black
/// background so the label always pops over surrounding syntax.
const JUMP_LABEL_FG: Color = Color::Rgb(255, 100, 200);
const JUMP_LABEL_BG: Color = Color::Rgb(40, 0, 40);

/// Width of the gutter prefix (severity sign + space). Kept in sync with
/// [`place_cursor`] so the cursor lands on the right column.
const GUTTER_SIGN_WIDTH: u16 = 1;

/// Width of the VCS-bar column rendered between the line number and the
/// buffer text. One cell wide regardless of status — the bar character
/// itself is single-width.
const GUTTER_VCS_WIDTH: u16 = 1;

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
    let vcs_statuses = app.buffer.vcs_statuses();
    let cursor_row = app.buffer.cursor.row;
    let extras = &app.buffer.extra_cursors;
    let search_query = &app.search.query;
    let jump_overlay = build_jump_overlay(app.jump_state.as_ref());

    let visible: Vec<Line> = app
        .buffer
        .lines
        .iter()
        .enumerate()
        .skip(scroll)
        .take(height)
        .map(|(i, line)| {
            let mut spans = vec![sign_span(row_severity.get(&i).copied())];
            // Gutter layout: <sign><4-digit num><space><vcs-bar><buffer>.
            // The breathing-room space sits between the number and the
            // bar; cursor column math in `place_cursor` matches.
            let num = format!("{:>4} ", i + 1);
            // The cursor's row gets the terminal's default foreground
            // (`Color::Reset`) so the number stays in sync with whatever
            // color the terminal paints the cursor itself.
            let num_style = if i == cursor_row {
                Style::default().fg(Color::Reset)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(num, num_style));
            let vcs_status = vcs_statuses.get(i).copied().flatten();
            spans.push(vcs_bar_span(vcs_status));
            let extra_cols: Vec<usize> = extras
                .iter()
                .filter_map(|c| if c.row == i { Some(c.col) } else { None })
                .collect();
            let hits = find_matches_in_line(line, search_query);
            let row_jumps: Vec<(usize, char)> = jump_overlay
                .iter()
                .filter_map(|(pos, ch)| if pos.0 == i { Some((pos.1, *ch)) } else { None })
                .collect();
            spans.extend(render_line(
                i,
                line,
                sel.as_ref(),
                &captures,
                &extra_cols,
                &hits,
                &row_jumps,
            ));
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
    let x = buf_area.x
        + GUTTER_SIGN_WIDTH
        + line_no_width
        + GUTTER_VCS_WIDTH
        + app.buffer.cursor.col as u16;
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

/// Gutter cell rendered between the line number and the buffer text.
/// A thin vertical bar colored per VCS status, or a plain space when
/// the row has no status (and the trailing-space slot is preserved).
fn vcs_bar_span(status: Option<LineStatus>) -> Span<'static> {
    match status {
        Some(LineStatus::Added) => Span::styled("▎", Style::default().fg(Color::Green)),
        Some(LineStatus::Modified) => Span::styled("▎", Style::default().fg(Color::Yellow)),
        Some(LineStatus::DeletedAbove) => Span::styled("▁", Style::default().fg(Color::Red)),
        None => Span::raw(" "),
    }
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
    extra_cols: &[usize],
    search_hits: &[(usize, usize)],
    jump_labels: &[(usize, char)],
) -> Vec<Span<'static>> {
    let is_extra_cursor = |col: usize| -> bool { extra_cols.contains(&col) };
    let is_search_hit =
        |col: usize| -> bool { search_hits.iter().any(|(lo, hi)| col >= *lo && col < *hi) };
    let jump_label_at = |col: usize| -> Option<char> {
        jump_labels.iter().find_map(|(c, ch)| if *c == col { Some(*ch) } else { None })
    };
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
        let mut style = Style::default();
        if is_selected(0) {
            style = style.bg(SEL_BG);
        }
        if is_extra_cursor(0) {
            style = extra_cursor_style(style);
        }
        if style == Style::default() {
            return Vec::new();
        }
        return vec![Span::styled(" ".to_string(), style)];
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

    // Backgrounds layered from least to most specific: search hit →
    // visual selection → extra cursor (which uses an outline modifier
    // rather than a fill, so it sits on top of any underlying bg).
    let style_at = |col: usize| -> Style {
        let mut s = base[col];
        if is_search_hit(col) {
            s = s.bg(SEARCH_HIT_BG);
        }
        if is_selected(col) {
            s = s.bg(SEL_BG);
        }
        if is_extra_cursor(col) {
            s = extra_cursor_style(s);
        }
        s
    };

    // Per-col character + style. A `gw` jump label overlays its char on
    // top of the underlying buffer char with `JUMP_LABEL_*` styling.
    let cell_at = |col: usize| -> (char, Style) {
        if let Some(label) = jump_label_at(col) {
            (
                label,
                Style::default()
                    .fg(JUMP_LABEL_FG)
                    .bg(JUMP_LABEL_BG)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )
        } else {
            (chars[col], style_at(col))
        }
    };

    let mut spans = Vec::new();
    let mut buf = String::new();
    let (c0, s0) = cell_at(0);
    let mut buf_style = s0;
    buf.push(c0);
    for col in 1..chars.len() {
        let (c, s) = cell_at(col);
        if s != buf_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
            buf_style = s;
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, buf_style));
    }
    // Past-end extra cursor — paint one extra cell so a cursor sitting
    // one column past the last char (the natural Insert-mode position
    // after typing) stays visible.
    if is_extra_cursor(chars.len()) {
        spans.push(Span::styled(
            " ".to_string(),
            extra_cursor_style(Style::default()),
        ));
    }
    spans
}

/// Style overlay applied to every extra-cursor cell. Solid background
/// so the cell stays visible against any underlying syntax / search /
/// selection layer.
fn extra_cursor_style(base: Style) -> Style {
    base.bg(EXTRA_CURSOR_BG)
}

/// Lower the active `gw` jump state into a `(row, col) → char` overlay
/// map suitable for the per-line renderer.
///
/// - Before any keystroke: each label contributes its first char at
///   the target col, and (when present) its second char at col+1.
/// - After the first keystroke: only labels whose `first` matches the
///   typed char survive; they show as just their second char at the
///   target col. Single-char labels never reach this state because
///   `handle_jump_key` short-circuits to the jump.
fn build_jump_overlay(state: Option<&JumpState>) -> HashMap<(usize, usize), char> {
    let mut out = HashMap::new();
    let Some(s) = state else { return out };
    match s.typed_first {
        None => {
            for label in &s.labels {
                out.insert((label.pos.row, label.pos.col), label.first);
                if let Some(c2) = label.second {
                    out.insert((label.pos.row, label.pos.col + 1), c2);
                }
            }
        }
        Some(first) => {
            for label in &s.labels {
                if label.first != first {
                    continue;
                }
                if let Some(c2) = label.second {
                    out.insert((label.pos.row, label.pos.col), c2);
                }
            }
        }
    }
    out
}

/// All matches of `query` in `line`, returned as half-open char
/// ranges. Empty `query` returns no hits, so callers don't accidentally
/// paint the entire buffer when no search is active.
fn find_matches_in_line(line: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let q_chars = query.chars().count();
    let mut hits = Vec::new();
    let mut search_from = 0;
    while let Some(byte_idx) = line[search_from..].find(query) {
        let abs_byte = search_from + byte_idx;
        let start_col = line[..abs_byte].chars().count();
        hits.push((start_col, start_col + q_chars));
        // Advance past this match so we don't re-find overlapping
        // occurrences. `query.len()` is byte length, which is safe to
        // add at a UTF-8 boundary.
        search_from = abs_byte + query.len();
        if search_from >= line.len() {
            break;
        }
    }
    hits
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
