//! Buffer viewport: gutter (diagnostic signs + line numbers),
//! per-character syntax highlighting layered with the visual selection,
//! and the terminal cursor placement that goes with it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, JumpState, Selection};
use crate::config::EditorConfig;
use crate::editor::Buffer;
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

/// Foreground used to mark the bracket pair when the cursor sits on
/// one half of `()`, `[]`, or `{}`. Combined with `BOLD` rather than a
/// background fill so the highlight remains legible on top of any
/// other layer (search hit, selection, syntax bg) without competing
/// for the same channel.
const MATCH_BRACKET_FG: Color = Color::Yellow;

/// Foreground used for `gw` jump labels. Bright magenta on a near-black
/// background so the label always pops over surrounding syntax.
const JUMP_LABEL_FG: Color = Color::Rgb(255, 100, 200);
const JUMP_LABEL_BG: Color = Color::Rgb(40, 0, 40);

/// Foreground used for the whitespace marker glyphs (middle-dot and
/// tab arrow) when `show_whitespace` is enabled. Dim enough to fade
/// into the background but still legible.
const WHITESPACE_FG: Color = Color::DarkGray;

/// Width of the gutter prefix (severity sign + space). Kept in sync with
/// [`place_cursor`] so the cursor lands on the right column.
const GUTTER_SIGN_WIDTH: u16 = 1;

/// Width of the VCS-bar column rendered between the line number and the
/// buffer text. One cell wide regardless of status — the bar character
/// itself is single-width.
const GUTTER_VCS_WIDTH: u16 = 1;

/// Minimum rows kept above and below the cursor inside the viewport
/// (vim's `scrolloff`). Near the end of the file this lets scroll
/// advance past the last source row, leaving blank rows below — so the
/// cursor isn't pinned to the bottom edge when sitting on the last few
/// lines. Disabled automatically when the viewport is too small to
/// give the cursor room (height ≤ 2 * SCROLL_OFF + 1).
const SCROLL_OFF: usize = 5;

pub(super) fn draw_buffer(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height as usize;
    let row_diag = build_row_diag_summary(app, app.buffer.cursor.row);
    let scroll = compute_scroll(app, height, &row_diag);

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
    let cursor_col = app.buffer.cursor.col;
    let extras = &app.buffer.extra_cursors;
    let search_query = &app.search.query;
    let jump_overlay = build_jump_overlay(app.jump_state.as_ref());
    // Tree-sitter–driven matching-bracket highlight. Yields the two
    // cells to paint (cursor's bracket + its mate) only when the
    // cursor sits on a syntactic bracket token; brackets inside
    // strings/comments resolve to the containing literal node and
    // naturally don't match here.
    let bracket_pair: Vec<(usize, usize)> = app
        .buffer
        .highlighter
        .as_ref()
        .and_then(|h| h.matching_bracket(cursor_row, cursor_col))
        .map(|mate| vec![(cursor_row, cursor_col), mate])
        .unwrap_or_default();
    let eff = app.effective_editor();
    let tab_width = eff.tab_width.max(1);
    let show_whitespace = eff.show_whitespace;

    // Interleave one virtual diagnostic line below each source row that
    // has any diagnostics. Stop accumulating once we've consumed
    // `height` visual rows.
    let mut visible: Vec<Line> = Vec::with_capacity(height);
    let mut visual_y: u16 = 0;
    let mut cursor_visual_y: u16 = 0;
    let inner_text_width =
        area.width
            .saturating_sub(GUTTER_SIGN_WIDTH + 5 + GUTTER_VCS_WIDTH) as usize;
    let col_scroll = compute_col_scroll(app, inner_text_width, tab_width);
    for (i, line) in app.buffer.lines.iter().enumerate().skip(scroll) {
        if visual_y as usize >= height {
            break;
        }
        if i == cursor_row {
            cursor_visual_y = visual_y;
        }
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
        let row_bracket_cols: Vec<usize> = bracket_pair
            .iter()
            .filter_map(|(r, c)| if *r == i { Some(*c) } else { None })
            .collect();
        spans.extend(render_line(
            i,
            line,
            sel.as_ref(),
            &captures,
            &extra_cols,
            &hits,
            &row_jumps,
            &row_bracket_cols,
            tab_width,
            col_scroll,
            inner_text_width,
            show_whitespace,
        ));
        // Inline suggestion (ghost text) — Phase 0 only paints
        // single-line suggestions at end of the cursor row. The stub
        // provider only fires when the cursor is at EOL, so appending
        // after the rendered line lands the ghost spans flush against
        // the cursor cell. Multi-line continuation and mid-line
        // overlay come later.
        if i == cursor_row
            && app.completion.is_none()
            && let Some(s) = app.inline_suggestion.showing()
            && s.is_anchored_at(app.buffer.cursor)
        {
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(ratatui::style::Modifier::ITALIC);
            if let Some(first) = s.text.lines().next() {
                spans.push(Span::styled(first.to_string(), style));
            }
        }
        visible.push(Line::from(spans));
        visual_y += 1;
        if visual_y as usize >= height {
            break;
        }
        if let Some(summary) = row_diag.get(&i) {
            visible.push(diagnostic_line(summary, inner_text_width));
            visual_y += 1;
        }
    }

    app.buffer.cursor_visual_y.set(cursor_visual_y);
    f.render_widget(Paragraph::new(visible), area);
}

pub(super) fn place_cursor(f: &mut Frame, app: &App, buf_area: Rect) {
    if app.prompt.is_open() {
        return;
    }
    let line_no_width: u16 = 5;
    let tab_width = app.effective_editor().tab_width.max(1);
    let line = &app.buffer.lines[app.buffer.cursor.row];
    let visual_col = char_col_to_visual(line, app.buffer.cursor.col, tab_width);
    let col_scroll = app.buffer.col_scroll.get();
    let on_screen_col = visual_col.saturating_sub(col_scroll);
    let x =
        buf_area.x + GUTTER_SIGN_WIDTH + line_no_width + GUTTER_VCS_WIDTH + on_screen_col as u16;
    // `draw_buffer` ran first this frame and published the cursor's
    // visual y, accounting for any virtual diagnostic lines pushing it
    // down. Use it directly so the terminal cursor stays glued to the
    // rendered cursor row.
    let y = buf_area.y + app.buffer.cursor_visual_y.get();
    f.set_cursor_position((x, y));
}

/// Convert a character index on `line` into the visual column the
/// character lands in once tabs have been expanded to `tab_width`-aligned
/// stops. Walks the prefix exactly the way [`render_line`] does, so the
/// cursor stays glued to the rendered char.
fn char_col_to_visual(line: &str, char_col: usize, tab_width: usize) -> usize {
    let mut v = 0usize;
    for ch in line.chars().take(char_col) {
        if ch == '\t' {
            v += tab_width - (v % tab_width);
        } else {
            v += 1;
        }
    }
    v
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
#[allow(clippy::too_many_arguments)]
fn render_line(
    row: usize,
    line: &str,
    sel: Option<&Selection>,
    captures: &[Capture],
    extra_cols: &[usize],
    search_hits: &[(usize, usize)],
    jump_labels: &[(usize, char)],
    bracket_cols: &[usize],
    tab_width: usize,
    col_scroll: usize,
    viewport_width: usize,
    show_whitespace: bool,
) -> Vec<Span<'static>> {
    let is_extra_cursor = |col: usize| -> bool { extra_cols.contains(&col) };
    let is_search_hit =
        |col: usize| -> bool { search_hits.iter().any(|(lo, hi)| col >= *lo && col < *hi) };
    let is_match_bracket = |col: usize| -> bool { bracket_cols.contains(&col) };
    let jump_label_at = |col: usize| -> Option<char> {
        jump_labels
            .iter()
            .find_map(|(c, ch)| if *c == col { Some(*ch) } else { None })
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
    let viewport_right = col_scroll.saturating_add(viewport_width);
    if chars.is_empty() {
        // The empty-line cursor/selection cell lives at visual col 0;
        // if we've scrolled past it, there's nothing to paint.
        if col_scroll > 0 {
            return Vec::new();
        }
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
    // Matching-bracket is a fg/bold overlay applied last so the pair
    // remains identifiable even when sitting inside a selection or
    // search match.
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
        if is_match_bracket(col) {
            s = s
                .fg(MATCH_BRACKET_FG)
                .add_modifier(ratatui::style::Modifier::BOLD);
        }
        s
    };

    // Per-col character + style. A `gw` jump label overlays its char on
    // top of the underlying buffer char with `JUMP_LABEL_*` styling.
    // When `show_whitespace` is on, plain spaces become `·` and the
    // leading cell of a tab becomes `→`, both painted in `WHITESPACE_FG`
    // so they sit visibly above (but quietly with) the surrounding text.
    let cell_at = |col: usize| -> (char, Style) {
        if let Some(label) = jump_label_at(col) {
            return (
                label,
                Style::default()
                    .fg(JUMP_LABEL_FG)
                    .bg(JUMP_LABEL_BG)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            );
        }
        let original = chars[col];
        let style = style_at(col);
        if show_whitespace {
            match original {
                ' ' => return ('·', style.fg(WHITESPACE_FG)),
                '\t' => return ('→', style.fg(WHITESPACE_FG)),
                _ => {}
            }
        }
        (original, style)
    };

    // Each char takes one visible cell except `\t`, which jumps to the
    // next `tab_width`-aligned stop. The expanded tab is filled with
    // spaces so its background style (selection / search hit / extra
    // cursor) covers the entire run, and `visual_col` tracks the running
    // cell position so each tab measures from where it actually sits.
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut buf_style = Style::default();
    let mut visual_col = 0usize;
    let mut started = false;
    for (col, &original) in chars.iter().enumerate() {
        let (ch, style) = cell_at(col);
        let width = if original == '\t' {
            tab_width - (visual_col % tab_width)
        } else {
            1
        };
        let cell_start = visual_col;
        let cell_end = visual_col + width;
        visual_col = cell_end;

        // Stop once we've passed the right edge: ratatui's Paragraph
        // would truncate anyway, but bailing early keeps very long
        // lines from materializing megabytes of spans per draw.
        if viewport_width > 0 && cell_start >= viewport_right {
            break;
        }
        // Skip cells entirely to the left of the horizontal scroll.
        if cell_end <= col_scroll {
            continue;
        }
        let left_skip = col_scroll.saturating_sub(cell_start);
        let emit_width = width - left_skip;

        if !started {
            buf_style = style;
            started = true;
        } else if style != buf_style {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
            }
            buf_style = style;
        }
        if original == '\t' {
            // A jump label can overlay the tab at its leading cell. If
            // the left clip ate that cell, only spaces survive.
            if ch != '\t' && left_skip == 0 {
                buf.push(ch);
                for _ in 1..emit_width {
                    buf.push(' ');
                }
            } else {
                for _ in 0..emit_width {
                    buf.push(' ');
                }
            }
        } else {
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, buf_style));
    }
    // Past-end extra cursor — paint one extra cell so a cursor sitting
    // one column past the last char (the natural Insert-mode position
    // after typing) stays visible. Only when it falls inside the
    // horizontal viewport.
    if is_extra_cursor(chars.len())
        && visual_col >= col_scroll
        && (viewport_width == 0 || visual_col < viewport_right)
    {
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
///
/// `row_diag` is the per-row diagnostic summary; rows with diagnostics
/// each consume one extra visual row, so the "does the cursor fit"
/// check uses visual heights rather than raw source-row counts.
fn compute_scroll(app: &App, height: usize, row_diag: &HashMap<usize, RowDiag>) -> usize {
    let cur = app.buffer.cursor.row;
    let mut scroll = app.buffer.scroll.get();
    // Deferred centering from a picker-driven jump that fired before
    // the viewport size was known. Take-and-clear so it's a one-shot
    // override, then fall through to publishing the new scroll/height.
    if app.buffer.pending_center.replace(false) && height > 0 {
        let last = app.buffer.lines.len().saturating_sub(1);
        let max_scroll = last.saturating_sub(height.saturating_sub(1));
        scroll = cur.saturating_sub(height / 2).min(max_scroll);
        app.buffer.scroll.set(scroll);
        app.buffer.viewport_height.set(height);
        return scroll;
    }
    // Shrink the scroll-off to 0 on viewports too small to give the
    // cursor room on both sides; otherwise the padding would fight
    // itself and lock the cursor in place.
    let off = if height > 2 * SCROLL_OFF + 1 {
        SCROLL_OFF
    } else {
        0
    };

    if cur < scroll + off {
        scroll = cur.saturating_sub(off);
    } else if height > 0 {
        // Walk rows [scroll..cur], accumulating each row's visual
        // height (1 + 1 if it has diagnostics). Advance scroll forward
        // until the cursor's source row fits with `off` rows of room
        // below it — i.e. `consumed_above_cursor < height - off`. Past
        // EOF this lets scroll exceed `last - height + 1`; the render
        // loop just stops emitting rows when source lines run out.
        let effective_height = height.saturating_sub(off);
        loop {
            if scroll >= cur {
                break;
            }
            let mut consumed: usize = 0;
            for row in scroll..cur {
                consumed += 1 + row_diag.get(&row).map_or(0, |_| 1);
                if consumed >= effective_height {
                    break;
                }
            }
            if consumed < effective_height {
                break;
            }
            scroll += 1;
        }
    }
    // Keep at least the last source line visible — don't let past-EOF
    // padding push every real row off the top.
    let last_row = app.buffer.lines.len().saturating_sub(1);
    scroll = scroll.min(last_row);
    app.buffer.scroll.set(scroll);
    // Publish the height so `H`/`M`/`L` and the `<C-d>`/`<C-u>` family
    // (handled in the input thread) can read what's currently visible.
    app.buffer.viewport_height.set(height);
    scroll
}

/// Update and return the horizontal scroll offset. Sticky like
/// [`compute_scroll`]: shifts the visible window only when the cursor's
/// visual column would otherwise fall outside `[col_scroll, col_scroll
/// + width)`. `width == 0` collapses to no scroll (degenerate frame).
fn compute_col_scroll(app: &App, width: usize, tab_width: usize) -> usize {
    if width == 0 {
        app.buffer.col_scroll.set(0);
        return 0;
    }
    let line = &app.buffer.lines[app.buffer.cursor.row];
    let visual_col = char_col_to_visual(line, app.buffer.cursor.col, tab_width);
    let mut col_scroll = app.buffer.col_scroll.get();
    if visual_col < col_scroll {
        col_scroll = visual_col;
    } else if visual_col >= col_scroll + width {
        col_scroll = visual_col + 1 - width;
    }
    app.buffer.col_scroll.set(col_scroll);
    col_scroll
}

/// Per-source-row diagnostic summary used for inline rendering. We
/// fold every diagnostic that *starts* on a row into a single virtual
/// line: the worst-severity message, with `(+N)` appended when more
/// than one diagnostic shares the row. Capping at one virtual row per
/// source row keeps the visual layout — and the cursor-y math — simple.
pub(super) struct RowDiag {
    pub severity: Severity,
    pub message: String,
    pub extra: usize,
}

/// Build the row → summary lookup, applying the cursor-vs-other-row
/// filter: the cursor's row shows any severity, every other row only
/// surfaces `Error` diagnostics inline. Keeps the buffer quiet when
/// the cursor is elsewhere — warnings/info/hints stay accessible via
/// the gutter sign and the status-bar toast.
fn build_row_diag_summary(app: &App, cursor_row: usize) -> HashMap<usize, RowDiag> {
    let mut out: HashMap<usize, RowDiag> = HashMap::new();
    let Some(diags) = app.current_diagnostics() else {
        return out;
    };
    for d in diags {
        let row = d.range.start.line as usize;
        if row != cursor_row && d.severity != Severity::Error {
            continue;
        }
        // First line only — multi-line messages would blow past our
        // single-virtual-row budget.
        let msg = d.message.lines().next().unwrap_or("").to_string();
        match out.get_mut(&row) {
            None => {
                out.insert(
                    row,
                    RowDiag {
                        severity: d.severity,
                        message: msg,
                        extra: 0,
                    },
                );
            }
            Some(existing) => {
                if (d.severity as u8) < (existing.severity as u8) {
                    existing.severity = d.severity;
                    existing.message = msg;
                }
                existing.extra += 1;
            }
        }
    }
    out
}

/// Render one virtual diagnostic row. Layout mirrors a real source
/// row's gutter (sign + line-number column + vcs bar) but with blanks
/// so the message column-aligns with the source text above it.
fn diagnostic_line(diag: &RowDiag, inner_text_width: usize) -> Line<'static> {
    let color = severity_color(diag.severity);
    // Blank gutter: 1 (sign) + 5 (line number column) + 1 (vcs bar).
    let gutter = " ".repeat((GUTTER_SIGN_WIDTH + 5 + GUTTER_VCS_WIDTH) as usize);
    let mut text = String::from("↳ ");
    text.push_str(&diag.message);
    if diag.extra > 0 {
        text.push_str(&format!(" (+{})", diag.extra));
    }
    if inner_text_width > 0 && text.chars().count() > inner_text_width {
        let mut t: String = text
            .chars()
            .take(inner_text_width.saturating_sub(1))
            .collect();
        t.push('…');
        text = t;
    }
    Line::from(vec![
        Span::raw(gutter),
        Span::styled(
            text,
            Style::default()
                .fg(color)
                .add_modifier(ratatui::style::Modifier::ITALIC),
        ),
    ])
}

/// Render an inactive pane's buffer. Deliberately a thin renderer:
/// gutter (line numbers + VCS bars) and lines with syntax highlighting,
/// but no diagnostics, no selection, no extra cursors, no jump-label
/// overlay, no search-hit painting — those overlays all belong to the
/// active pane. Scroll is anchored on the inactive pane's own
/// `Buffer.cursor.row` / `Buffer.scroll`, so each pane remembers where
/// the user was last looking.
pub(super) fn draw_buffer_inactive(f: &mut Frame, buf: &Buffer, eff: &EditorConfig, area: Rect) {
    let height = area.height as usize;
    let cur = buf.cursor.row;
    let mut scroll = buf.scroll.get();
    let off = if height > 2 * SCROLL_OFF + 1 {
        SCROLL_OFF
    } else {
        0
    };
    if cur < scroll + off {
        scroll = cur.saturating_sub(off);
    } else if height > 0 && cur + off >= scroll + height {
        scroll = (cur + off + 1).saturating_sub(height);
    }
    let last_row = buf.lines.len().saturating_sub(1);
    scroll = scroll.min(last_row);
    buf.scroll.set(scroll);
    buf.viewport_height.set(height);
    let last_visible = scroll + height;
    let captures = buf
        .highlighter
        .as_ref()
        .map(|h| h.captures_in_rows(scroll, last_visible))
        .unwrap_or_default();
    let vcs_statuses = buf.vcs_statuses();
    let tab_width = eff.tab_width.max(1);
    let show_whitespace = eff.show_whitespace;
    let inner_text_width =
        area.width
            .saturating_sub(GUTTER_SIGN_WIDTH + 5 + GUTTER_VCS_WIDTH) as usize;
    // Track col_scroll on the inactive pane's own cell so horizontal
    // jumps still work when the user re-focuses it.
    let line = buf.lines.get(cur).map(String::as_str).unwrap_or("");
    let visual_col = char_col_to_visual(line, buf.cursor.col, tab_width);
    let mut col_scroll = buf.col_scroll.get();
    if inner_text_width > 0 {
        if visual_col < col_scroll {
            col_scroll = visual_col;
        } else if visual_col >= col_scroll + inner_text_width {
            col_scroll = visual_col + 1 - inner_text_width;
        }
    } else {
        col_scroll = 0;
    }
    buf.col_scroll.set(col_scroll);

    let mut visible: Vec<Line> = Vec::with_capacity(height);
    for (visual_y, (i, line)) in (0_usize..).zip(buf.lines.iter().enumerate().skip(scroll)) {
        if visual_y >= height {
            break;
        }
        let mut spans = vec![sign_span(None)];
        let num = format!("{:>4} ", i + 1);
        spans.push(Span::styled(num, Style::default().fg(Color::DarkGray)));
        let vcs_status = vcs_statuses.get(i).copied().flatten();
        spans.push(vcs_bar_span(vcs_status));
        spans.extend(render_line(
            i,
            line,
            None,
            &captures,
            &[],
            &[],
            &[],
            &[],
            tab_width,
            col_scroll,
            inner_text_width,
            show_whitespace,
        ));
        visible.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(visible), area);
}

fn severity_color(sev: Severity) -> Color {
    match sev {
        Severity::Error => Color::Red,
        Severity::Warning => Color::Yellow,
        Severity::Info => Color::LightBlue,
        Severity::Hint => Color::DarkGray,
    }
}
