//! Match-list pane: the query line on top and the scrollable list of
//! fuzzy matches below, with hit-character highlighting and head-
//! truncation for long path entries.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use crate::finder::{Finder, FuzzyKind};

/// Color of the directory prefix in path-like picker rows.
const DIR_FG: Color = Color::Blue;
/// Color of fuzzy-match hit characters.
const HIT_FG: Color = Color::Magenta;

/// Map a one-letter severity badge to its fg color. Tracks
/// [`crate::lsp::Severity::from_code`] / [`super::super::App`]'s
/// `severity_tag` — the `E`/`W`/`I`/`H` letters the picker builder
/// emits. Returns `None` for anything else so the caller can fall
/// back to the base style rather than recoloring random rows.
fn severity_color_from_badge(c: char) -> Option<Color> {
    match c {
        'E' => Some(Color::Red),
        'W' => Some(Color::Yellow),
        'I' => Some(Color::Cyan),
        'H' => Some(Color::DarkGray),
        _ => None,
    }
}

pub(super) fn draw_fuzzy_list(f: &mut Frame, finder: &Finder, area: Rect) {
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

    // Park the terminal cursor at the finder's insertion point so the
    // user can see where typing/backspace will land. `› ` is two
    // single-cell glyphs.
    let col = (2 + finder.cursor) as u16;
    let x = chunks[0].x + col.min(chunks[0].width.saturating_sub(1));
    f.set_cursor_position((x, chunks[0].y));

    let sep = "─".repeat(chunks[1].width as usize);
    f.render_widget(
        Paragraph::new(Span::styled(sep, Style::default().fg(Color::DarkGray))),
        chunks[1],
    );

    let list_h = chunks[2].height as usize;
    let list_w = chunks[2].width as usize;
    let scroll = finder.selected.saturating_sub(list_h.saturating_sub(1));
    let items: Vec<ListItem> = finder
        .matches
        .iter()
        .enumerate()
        .skip(scroll)
        .take(list_h)
        .map(|(i, m)| {
            let raw = &finder.items[m.idx];
            let line = if matches!(finder.kind, FuzzyKind::WorkspaceSearch) {
                let row = m.line_hits.first().copied().unwrap_or(0);
                render_workspace_match(raw, row, i == finder.selected, list_w)
            } else {
                render_match(raw, &m.positions, i == finder.selected, finder.kind, list_w)
            };
            ListItem::new(line)
        })
        .collect();
    f.render_widget(List::new(items), chunks[2]);
}

/// Render a `<space>/` row: `path:line`. Path's directory portion is
/// dimmed; the line number is cyan. Matching is against line content
/// but we don't reproduce the content in the row — the preview pane on
/// the right already shows it under a target band.
fn render_workspace_match<'a>(
    path: &'a str,
    row: usize,
    selected: bool,
    width: usize,
) -> Line<'a> {
    let base = if selected {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let dir = base.fg(DIR_FG).add_modifier(Modifier::BOLD);
    let lineno = base.fg(Color::Cyan);
    let sep = base.fg(Color::DarkGray);
    let dim = base.fg(Color::DarkGray);

    let lineno_str = format!(":{}", row + 1);
    let lineno_w = lineno_str.chars().count();
    let path_chars: Vec<char> = path.chars().collect();
    let dir_end = path_chars
        .iter()
        .rposition(|c| *c == '/')
        .map(|i| i + 1)
        .unwrap_or(0);

    // Head-truncate the path so the basename stays visible, leaving
    // room for `:NN` at the tail.
    let path_budget = width.saturating_sub(lineno_w);
    let mut spans: Vec<Span<'a>> = Vec::new();
    let path_start = if path_budget >= 2 && path_chars.len() > path_budget {
        spans.push(Span::styled("…", dim));
        path_chars.len() - (path_budget - 1)
    } else {
        0
    };
    let dir_end_visible = dir_end.saturating_sub(path_start);

    let mut buf = String::new();
    let mut buf_style = base;
    for (offset, &c) in path_chars[path_start..].iter().enumerate() {
        let in_dir = offset < dir_end_visible;
        let style = if in_dir { dir } else { base };
        if style != buf_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
        }
        buf_style = style;
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, buf_style));
    }

    // ":NN" — dim separator, colored line number. No trailing content.
    spans.push(Span::styled(":", sep));
    spans.push(Span::styled(format!("{}", row + 1), lineno));
    Line::from(spans)
}

fn render_match<'a>(
    item: &'a str,
    positions: &[usize],
    selected: bool,
    kind: FuzzyKind,
    width: usize,
) -> Line<'a> {
    let base = if selected {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let dir = base.fg(DIR_FG).add_modifier(Modifier::BOLD);
    let hit = base.fg(HIT_FG).add_modifier(Modifier::BOLD);
    let ellipsis = base.fg(Color::DarkGray);
    // Severity badge color — only when the row is a diagnostic and the
    // first four chars look like `[X] `. Mirrors the four
    // [`crate::lsp::Severity`] variants so the badge at-a-glance tells
    // the user whether they're looking at a real error or just a hint.
    let sev_style: Option<Style> =
        if matches!(kind, FuzzyKind::Diagnostics { .. }) && item.len() >= 4 {
            severity_color_from_badge(item.as_bytes()[1] as char)
                .map(|c| base.fg(c).add_modifier(Modifier::BOLD))
        } else {
            None
        };

    // Compute dir_end as a char index. For path-like kinds it's the
    // index just past the last `/`; for Locations we stop searching at
    // the `:line:col` suffix so `:` doesn't get colored as directory.
    let chars: Vec<char> = item.chars().collect();
    let dir_end_char: Option<usize> = match kind {
        FuzzyKind::Files { .. } | FuzzyKind::Buffers => {
            chars.iter().rposition(|c| *c == '/').map(|i| i + 1)
        }
        FuzzyKind::Locations | FuzzyKind::WorkspaceSearch => {
            let path_end = chars.iter().position(|c| *c == ':').unwrap_or(chars.len());
            chars[..path_end]
                .iter()
                .rposition(|c| *c == '/')
                .map(|i| i + 1)
        }
        // Diagnostic entries are `[X] path:line  content` — skip the
        // `[X] ` severity prefix before searching for the path's
        // trailing `/`. Bare `[X] line` (current-buffer kind, no path)
        // returns None so nothing gets miscolored as a directory.
        FuzzyKind::Diagnostics { .. } => {
            let prefix_end = chars
                .iter()
                .position(|c| *c == ' ')
                .map(|i| i + 1)
                .unwrap_or(chars.len());
            let path_end = chars[prefix_end..]
                .iter()
                .position(|c| *c == ':')
                .map(|i| i + prefix_end)
                .unwrap_or(chars.len());
            chars[prefix_end..path_end]
                .iter()
                .rposition(|c| *c == '/')
                .map(|i| i + prefix_end + 1)
        }
        FuzzyKind::Lines => None,
    };

    // Head-truncate when the item is longer than the available width so
    // the filename (right side of the path) stays visible. One column is
    // reserved for the leading ellipsis.
    //
    // For Diagnostics rows, the leading `[X] ` severity badge is also
    // pinned: when the row needs to be cut down we render the badge
    // verbatim, then an ellipsis, then the tail. Without this the
    // head-truncation would eat the badge first — the very thing the
    // picker is meant to show at a glance.
    let mut spans = Vec::new();
    let pinned_prefix = match kind {
        FuzzyKind::Diagnostics { .. }
            if chars.len() >= 4
                && chars[0] == '['
                && chars[2] == ']'
                && chars[3] == ' ' =>
        {
            4
        }
        _ => 0,
    };
    let start = if width >= 2 && chars.len() > width {
        if pinned_prefix > 0 && width > pinned_prefix + 1 {
            // Render the badge first; honor hit highlighting so a
            // query that landed inside the badge still glows, and
            // tint with the severity color when one is available.
            for (idx, c) in chars[..pinned_prefix].iter().enumerate() {
                let is_hit = positions.binary_search(&idx).is_ok();
                let style = if is_hit {
                    hit
                } else {
                    sev_style.unwrap_or(base)
                };
                spans.push(Span::styled(c.to_string(), style));
            }
            spans.push(Span::styled("…", ellipsis));
            chars.len() - (width - pinned_prefix - 1)
        } else {
            spans.push(Span::styled("…", ellipsis));
            chars.len() - (width - 1)
        }
    } else {
        0
    };
    // When truncation lands inside the directory portion, the remaining
    // dir prefix still gets the dir color up to dir_end. When the cut
    // falls past dir_end, dir_end - start <= 0 and the whole tail is
    // filename-colored — exactly what we want.
    let dir_end_visible = dir_end_char.map(|e| e.saturating_sub(start));

    let mut buf = String::new();
    let mut buf_style = base;
    for (offset, &c) in chars[start..].iter().enumerate() {
        let orig_i = start + offset;
        let is_hit = positions.binary_search(&orig_i).is_ok();
        let in_dir = dir_end_visible.map(|e| offset < e).unwrap_or(false);
        // Severity badge characters live in the first `pinned_prefix`
        // slots; pick the severity color when we're inside that range
        // and we didn't already truncate them out via the pinned-prefix
        // path above (which would have advanced `start` past them).
        let in_badge = sev_style.is_some() && orig_i < pinned_prefix;
        let style = if is_hit {
            hit
        } else if in_badge {
            sev_style.unwrap()
        } else if in_dir {
            dir
        } else {
            base
        };
        if style != buf_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
        }
        buf_style = style;
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, buf_style));
    }
    Line::from(spans)
}
