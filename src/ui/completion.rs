//! Cursor-anchored LSP completion popup.
//!
//! Modelled after `code_action.rs` but anchored at the *prefix start*
//! rather than the cursor column — so the labels line up with the
//! beginning of the identifier the user is typing, not the column
//! they're currently at.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Padding};

use crate::app::App;
use crate::text_width::{char_cell_width, prefix_byte_len_for_width, str_cell_width};

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 24;
/// Per-row cap on the detail column. Long function signatures like
/// `fn(a: T, b: U, c: V) -> Result<X, Y>` dominated the popup width
/// before this; capping at 32 chars keeps the popup compact and lets
/// the label column breathe. Detail beyond this is elided with `…`.
const MAX_DETAIL_WIDTH: u16 = 32;
/// Detail popup (shown only while the popup is in selecting mode)
/// width / height caps. The popup wraps the resolved item's `detail`
/// across multiple lines so long signatures stay readable instead of
/// getting `…`-truncated like in the main popup.
const DETAIL_POPUP_WIDTH: u16 = 48;
const DETAIL_POPUP_HEIGHT: u16 = 16;
/// Smallest popup width worth drawing — 2 borders + 2 padding + 8
/// text cells. Below this the wrap looks ridiculous (one or two
/// chars per line), so we suppress the popup entirely rather than
/// render a useless sliver.
const MIN_DETAIL_WIDTH: u16 = 12;

pub(super) fn draw_completion(f: &mut Frame, app: &App, buf_area: Rect) {
    let Some(state) = app.completion.as_ref() else {
        return;
    };
    if state.is_empty() {
        return;
    }

    let row = state.prefix_start.row;
    let Some(rel_y) = app.visual_row_offset(row) else {
        return;
    };
    // Same gutter math as the other cursor-anchored popups: 1-char
    // severity sign + 5-char line number column. Use the *visual* col
    // of the prefix-start position so fullwidth chars on the same line
    // don't pull the popup left.
    let gutter_width: u16 = 1 + 5;
    let prefix_visual_col = app.char_col_visual(state.prefix_start.row, state.prefix_start.col);
    let anchor_x = buf_area.x + gutter_width + prefix_visual_col as u16;
    let anchor_y = buf_area.y + rel_y;

    // Width: enough to fit the longest visible label and (if any) its
    // detail side-by-side with a small gap between them, capped at
    // MAX_WIDTH. The badge column was dropped in favor of the detail
    // text which already carries the kind information.
    let label_w = state
        .filtered
        .iter()
        .map(|i| str_cell_width(&state.items[*i].label) as u16)
        .max()
        .unwrap_or(0);
    let detail_w = state
        .filtered
        .iter()
        .map(|i| {
            state.items[*i]
                .detail
                .as_deref()
                .map(|s| str_cell_width(s) as u16)
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
        .min(MAX_DETAIL_WIDTH);
    let inner_w = if detail_w == 0 {
        label_w
    } else {
        // 2-col gap between label and detail so the right edge breathes.
        label_w.saturating_add(2).saturating_add(detail_w)
    }
    .min(MAX_WIDTH);
    // popup width = inner text + 2 border cols + 2 horizontal padding cols.
    let popup_w = (inner_w + 4).min(buf_area.width);
    let visible = state.filtered.len() as u16;
    let popup_h = (visible + 2).min(MAX_HEIGHT + 2);

    // Prefer below the anchor row; flip above when it would clip.
    let below_y = anchor_y.saturating_add(1);
    let space_below = buf_area.bottom().saturating_sub(below_y);
    let y = if space_below >= popup_h {
        below_y
    } else if anchor_y >= buf_area.y + popup_h {
        anchor_y - popup_h
    } else {
        below_y.min(buf_area.bottom().saturating_sub(1))
    };

    let max_x = buf_area.right().saturating_sub(popup_w);
    let x = anchor_x.min(max_x).max(buf_area.x);

    let area = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h.min(buf_area.bottom().saturating_sub(y)),
    };
    if area.width <= 2 || area.height <= 2 {
        return;
    }

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let body_h = inner.height as usize;
    let scroll = state.selected.saturating_sub(body_h.saturating_sub(1));
    let inner_w = inner.width as usize;

    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .enumerate()
        .skip(scroll)
        .take(body_h)
        .map(|(i, item_idx)| {
            let item = &state.items[*item_idx];
            // In preview mode no row is "the selected one" yet — the
            // first Tab/Up/Down flips `selecting` to true, and only
            // then does the highlight appear.
            let is_sel = state.selecting && i == state.selected;
            // Layout: "label    detail" — label on the left, detail
            // right-aligned with at least a 2-col gap when both fit;
            // detail elides with `…` only when the popup actually
            // can't fit it (popup_w is sized to make that rare). All
            // widths are in *cells*, not chars, so CJK labels keep
            // the layout aligned.
            let label_cells = str_cell_width(&item.label);
            let detail = item.detail.as_deref().unwrap_or("");
            let detail_cells = str_cell_width(detail);

            let (label_room, detail_room) = if detail_cells == 0 || label_cells >= inner_w {
                (inner_w, 0)
            } else {
                let max_detail = inner_w
                    .saturating_sub(label_cells)
                    .saturating_sub(2)
                    .min(MAX_DETAIL_WIDTH as usize);
                (label_cells.min(inner_w), detail_cells.min(max_detail))
            };
            let label = truncate(&item.label, label_room);
            let detail_text = truncate(detail, detail_room);
            let row_style = if is_sel {
                Style::default()
                    .bg(Color::Rgb(58, 78, 122))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let detail_style = if is_sel {
                row_style
            } else {
                Style::default().fg(Color::Rgb(150, 150, 150))
            };
            // Pad between label and detail so detail right-aligns.
            let gap = inner_w
                .saturating_sub(str_cell_width(&label))
                .saturating_sub(str_cell_width(&detail_text));
            let mut spans = vec![Span::styled(label, row_style)];
            if !detail_text.is_empty() {
                spans.push(Span::styled(" ".repeat(gap), row_style));
                spans.push(Span::styled(detail_text, detail_style));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    f.render_widget(List::new(items), inner);

    // Side detail popup. Only shown while the user is actively
    // scrolling the completion list (selecting mode) — keeps the
    // preview-mode popup minimal so typing doesn't get visually
    // crowded by a documentation box for an item the user hasn't
    // committed to.
    if state.selecting {
        draw_detail_popup(f, state, buf_area, area);
    }
}

/// Companion popup above or below `main` showing the currently-selected
/// completion item's `detail` text, wrapped across multiple lines so
/// long signatures stay readable.
///
/// Positioning strategy: pick whichever side (below / above the main
/// popup) has more vertical room. The popup is x-aligned with the main
/// popup and shifts left when its width would extend past the right
/// edge. When neither side has enough room the popup is suppressed.
fn draw_detail_popup(
    f: &mut Frame,
    state: &crate::app::CompletionState,
    buf_area: Rect,
    main: Rect,
) {
    let Some(idx) = state.current_index() else {
        return;
    };
    let Some(item) = state.items.get(idx) else {
        return;
    };
    // Prefer the richer resolved_detail when it's landed, falling back
    // to the compact `detail` from the initial completion response
    // while resolve is still in flight.
    let Some(detail) = item
        .resolved_detail
        .as_deref()
        .or(item.detail.as_deref())
        .filter(|s| !s.is_empty())
    else {
        return;
    };

    let avail_w = buf_area.width.min(DETAIL_POPUP_WIDTH);
    if avail_w < MIN_DETAIL_WIDTH {
        return;
    }
    let popup_w = avail_w;

    let space_below = buf_area.bottom().saturating_sub(main.bottom());
    let space_above = main.y.saturating_sub(buf_area.y);

    enum Placement {
        Below,
        Above,
    }
    // Need at least 3 rows for borders + one line of text to be useful.
    let (placement, max_h) = if space_below >= 3 && space_below >= space_above {
        (Placement::Below, space_below)
    } else if space_above >= 3 {
        (Placement::Above, space_above)
    } else {
        return;
    };

    let text_w = popup_w.saturating_sub(4) as usize;
    if text_w == 0 {
        return;
    }
    let wrapped = wrap_text(detail, text_w);
    let lines_n = wrapped.len() as u16;
    let popup_h = (lines_n + 2).min(DETAIL_POPUP_HEIGHT + 2).min(max_h);

    let max_x = buf_area.right().saturating_sub(popup_w);
    let x = main.x.min(max_x).max(buf_area.x);
    let y = match placement {
        Placement::Below => main.bottom(),
        Placement::Above => main.y - popup_h,
    };

    let height = popup_h.min(buf_area.bottom().saturating_sub(y));
    let area = Rect {
        x,
        y,
        width: popup_w,
        height,
    };
    if area.width <= 2 || area.height <= 2 {
        return;
    }

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let body_h = inner.height as usize;
    let lines: Vec<Line> = wrapped
        .into_iter()
        .take(body_h)
        .map(|s| {
            Line::from(Span::styled(
                s,
                Style::default().fg(Color::Rgb(200, 200, 200)),
            ))
        })
        .collect();
    let para = ratatui::widgets::Paragraph::new(lines);
    f.render_widget(para, inner);
}

/// Greedy word-wrap. Splits on whitespace; long tokens that exceed
/// `width` get hard-broken at the char boundary instead of overflowing
/// (rare but happens with stitched type signatures like
/// `Result<HashMap<String,Vec<u32>>,Error>`).
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    // Hard-break a token wider than `width` into cell-width chunks. A
    // fullwidth char straddling the boundary stays on the next line
    // rather than getting bisected.
    let hard_break = |word: &str, out: &mut Vec<String>, current: &mut String| {
        let mut chunk = String::new();
        let mut chunk_w = 0usize;
        for c in word.chars() {
            let cw = char_cell_width(c);
            if chunk_w + cw > width {
                out.push(std::mem::take(&mut chunk));
                chunk_w = 0;
            }
            chunk.push(c);
            chunk_w += cw;
        }
        if !chunk.is_empty() {
            *current = chunk;
        }
    };
    for raw_line in s.lines() {
        let mut current = String::new();
        for word in raw_line.split_whitespace() {
            let word_w = str_cell_width(word);
            if current.is_empty() {
                if word_w <= width {
                    current.push_str(word);
                } else {
                    hard_break(word, &mut out, &mut current);
                }
            } else {
                let needed = str_cell_width(&current) + 1 + word_w;
                if needed <= width {
                    current.push(' ');
                    current.push_str(word);
                } else {
                    out.push(std::mem::take(&mut current));
                    if word_w <= width {
                        current.push_str(word);
                    } else {
                        hard_break(word, &mut out, &mut current);
                    }
                }
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        if raw_line.is_empty() {
            out.push(String::new());
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if str_cell_width(s) <= max {
        return s.to_string();
    }
    let cut = prefix_byte_len_for_width(s, max.saturating_sub(1));
    let mut out = String::with_capacity(cut + 3);
    out.push_str(&s[..cut]);
    out.push('…');
    out
}
