//! Preview pane: render the source file (or buffer) for the currently-
//! selected match. Files come through the per-`App` `preview_lru`
//! populated by the preview worker — on miss we enqueue a build and
//! draw a plain-text fallback for this frame.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::finder::{Finder, FuzzyKind};
use crate::syntax::{self, Capture};

/// Color of the highlight band drawn behind the target preview line.
const PREVIEW_HIT_BG: Color = Color::Rgb(45, 45, 60);

pub(super) fn draw_fuzzy_preview(f: &mut Frame, app: &App, finder: &Finder, area: Rect) {
    let Some(sel) = finder.selection() else {
        return;
    };
    match finder.kind {
        FuzzyKind::Files { .. } => {
            let rel = &finder.items[sel.idx];
            let path = app.startup_cwd.join(rel);
            preview_from_file(f, app, area, &path, 0);
        }
        FuzzyKind::Lines => {
            preview_from_buffer(f, app, area, sel.idx);
        }
        FuzzyKind::Locations | FuzzyKind::Diagnostics { .. } => {
            let Some(loc) = app.prompt.locations().get(sel.idx) else {
                return;
            };
            let Some(path) = crate::lsp::uri_to_path(&loc.uri) else {
                return;
            };
            let row = loc.range.start.line as usize;
            // Active / parked / sleeping all reach the buffer copy
            // before falling through to disk — that way `:e new.rs`
            // followed by a switch keeps previewing the typed-but-
            // never-saved content, instead of "(cannot read file)".
            if app.buffer.path.as_deref() == Some(path.as_path()) {
                preview_from_buffer(f, app, area, row);
                return;
            }
            for (key, buf) in &app.parked_buffers {
                if let crate::buffer_ref::BufferRef::File(p) = key
                    && p == &path
                {
                    preview_from_parked_buffer(f, area, buf, row);
                    return;
                }
            }
            for (key, snap) in &app.sleeping {
                if let crate::buffer_ref::BufferRef::File(p) = key
                    && p == &path
                {
                    preview_from_sleeping(f, area, snap, row);
                    return;
                }
            }
            preview_from_file(f, app, area, &path, row);
        }
        FuzzyKind::WorkspaceSearch => {
            // The base location stores line 0; the actual preview target
            // is the best-scoring matched row carried on the match item.
            let Some(loc) = app.prompt.locations().get(sel.idx) else {
                return;
            };
            let Some(path) = crate::lsp::uri_to_path(&loc.uri) else {
                return;
            };
            let target = sel.line_hits.first().copied().unwrap_or(0);
            preview_from_file(f, app, area, &path, target);
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
                crate::buffer_ref::BufferRef::Scratch(_) => {}
                crate::buffer_ref::BufferRef::File(path) => {
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

/// Render a preview from a parked buffer (one shown by a different
/// pane). Same shape as [`preview_from_buffer`] but operates against
/// the passed-in [`crate::editor::Buffer`] rather than the active one.
fn preview_from_parked_buffer(
    f: &mut Frame,
    area: Rect,
    buf: &crate::editor::Buffer,
    target_row: usize,
) {
    let lines = &buf.lines;
    let height = area.height as usize;
    let (scroll, end) = preview_scroll(lines.len(), target_row, height);
    let captures = buf
        .highlighter
        .as_ref()
        .map(|h| h.captures_in_rows(scroll, end.saturating_sub(1)))
        .unwrap_or_default();
    render_preview_lines(f, area, lines, &captures, target_row, scroll, end);
}

/// Render a preview from a sleeping buffer — same window/centering
/// rules as the other preview paths, but without syntax highlighting
/// since the highlighter is dropped on freeze. The line payload is
/// decompressed on demand (see
/// [`crate::app::SleepingBuffer::peek_lines`]); the preview re-renders
/// rarely enough that one decompress per keypress is cheaper than
/// keeping a thawed copy around just for the preview.
fn preview_from_sleeping(
    f: &mut Frame,
    area: Rect,
    snap: &crate::app::SleepingBuffer,
    target_row: usize,
) {
    let lines = snap.peek_lines();
    let height = area.height as usize;
    let (scroll, end) = preview_scroll(lines.len(), target_row, height);
    render_preview_lines(f, area, &lines, &[], target_row, scroll, end);
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

fn preview_plain_fallback(f: &mut Frame, area: Rect, path: &std::path::Path, target_row: usize) {
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

/// Cells emitted for one literal `\t` in the source. Tabs in preview
/// content would otherwise be sent to the terminal as a literal `\t` byte
/// — `unicode-width 0.1.14`'s `UnicodeWidthStr::width` reports tab as
/// width 1 (see `width_in_str`'s catch-all `_ => (1, …)` branch for
/// `c <= '\u{A0}'`), so ratatui happily stores the tab in a cell and
/// queues a `Print("\t")` to the backend. The terminal then interprets
/// the tab as "move to next tab stop", which slides every subsequent
/// glyph on that row past the cell it was meant to occupy. The cells in
/// between are never written, so any leftover content there shines
/// through. Expanding tabs into spaces up front keeps every grapheme at
/// width 1 and the per-cell diff in sync with what the terminal will
/// actually display.
const PREVIEW_TAB_WIDTH: usize = 4;

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
        let style = syntax::style_for(&cap.name);
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
    let mut written_cells = 0usize;
    for (col, &c) in chars.iter().enumerate() {
        let s = resolve(col);
        if s != buf_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
            buf_style = s;
        }
        // Tabs become PREVIEW_TAB_WIDTH spaces — see the const docs for
        // why a literal `\t` in the span content is a footgun here.
        if c == '\t' {
            for _ in 0..PREVIEW_TAB_WIDTH {
                buf.push(' ');
            }
            written_cells += PREVIEW_TAB_WIDTH;
        } else {
            buf.push(c);
            if !c.is_control() {
                written_cells += 1;
            }
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, buf_style));
    }
    // Pad to the right edge. For the target row we use the highlight
    // background so the band reaches the separator; everything else
    // pads with plain spaces. The padding is what defends against ghost
    // cells leaking through between successive preview renders — without
    // it, cells past the line content stay at the buffer's default
    // state, and ratatui's per-cell diff has been observed to leave
    // stale syntax-highlighted fragments visible there when the picker
    // navigates between files with different line lengths.
    if written_cells < text_width {
        let pad = text_width - written_cells;
        let style = if is_target {
            Style::default().bg(PREVIEW_HIT_BG)
        } else {
            Style::default()
        };
        spans.push(Span::styled(" ".repeat(pad), style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    /// Render a single preview row for `\t"context"` into a TestBackend
    /// and dump every cell's symbol — used to verify there's no
    /// duplication of line content in the rendered output.
    #[test]
    fn preview_row_no_duplication() {
        // Tab + "context" (9 chars). Line is rendered into a 30-col area.
        let line = "\t\"context\"".to_string();
        // Pretend tree-sitter gave us one string capture covering chars
        // 1..10 (the `"context"` portion, excluding the leading tab).
        let captures = vec![Capture {
            start_row: 0,
            start_col: 1,
            end_row: 0,
            end_col: 10,
            name: "string".to_string(),
        }];
        let rendered = render_preview_row(0, &line, &captures, false, 3, 26);

        let backend = TestBackend::new(30, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(
                    ratatui::widgets::Paragraph::new(rendered),
                    Rect::new(0, 0, 30, 1),
                );
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        for x in 0..30 {
            let cell = &buf[(x, 0)];
            eprintln!(
                "cell[{:2}]: symbol={:?} fg={:?} bg={:?}",
                x,
                cell.symbol(),
                cell.fg,
                cell.bg
            );
        }
        let row: String = (0..30).map(|x| buf[(x, 0)].symbol()).collect();
        eprintln!("rendered row: {:?}", row);
        // Should contain "context" exactly once.
        let count = row.matches("context").count();
        assert_eq!(count, 1, "rendered row: {:?}", row);
    }
}
