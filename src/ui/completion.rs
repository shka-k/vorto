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

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 24;
/// Per-row cap on the detail column. Long function signatures like
/// `fn(a: T, b: U, c: V) -> Result<X, Y>` dominated the popup width
/// before this; capping at 32 chars keeps the popup compact and lets
/// the label column breathe. Detail beyond this is elided with `…`.
const MAX_DETAIL_WIDTH: u16 = 32;
/// Side detail popup (shown only while the popup is in selecting mode)
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
    // severity sign + 5-char line number column.
    let gutter_width: u16 = 1 + 5;
    let anchor_x = buf_area.x + gutter_width + state.prefix_start.col as u16;
    let anchor_y = buf_area.y + rel_y;

    // Width: enough to fit the longest visible label and (if any) its
    // detail side-by-side with a small gap between them, capped at
    // MAX_WIDTH. The badge column was dropped in favor of the detail
    // text which already carries the kind information.
    let label_w = state
        .filtered
        .iter()
        .map(|i| state.items[*i].label.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let detail_w = state
        .filtered
        .iter()
        .map(|i| {
            state.items[*i]
                .detail
                .as_deref()
                .map(|s| s.chars().count() as u16)
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
            // can't fit it (popup_w is sized to make that rare).
            let label_chars = item.label.chars().count();
            let detail = item.detail.as_deref().unwrap_or("");
            let detail_chars = detail.chars().count();

            let (label_room, detail_room) = if detail_chars == 0 || label_chars >= inner_w {
                (inner_w, 0)
            } else {
                let max_detail = inner_w
                    .saturating_sub(label_chars)
                    .saturating_sub(2)
                    .min(MAX_DETAIL_WIDTH as usize);
                (label_chars.min(inner_w), detail_chars.min(max_detail))
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
                .saturating_sub(label.chars().count())
                .saturating_sub(detail_text.chars().count());
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
        draw_detail_side(f, state, buf_area, area);
    }
}

/// Companion popup beside `area` showing the currently-selected
/// completion item's `detail` text, wrapped across multiple lines so
/// long signatures stay readable.
///
/// Positioning strategy:
/// 1. Try the right of the main popup. Cap the popup width at the
///    actual right-side gap so it never extends past `buf_area`.
/// 2. If the right gap is too narrow to be useful (< [`MIN_DETAIL_WIDTH`]),
///    try the left side with the same cap.
/// 3. If neither side has room, try below the main popup, capped to
///    the area's width and remaining vertical room.
///
/// The popup width adapts every frame, so the wrap width follows.
/// When the available width is below the minimum the popup is
/// suppressed silently. Detail with no content also short-circuits.
fn draw_detail_side(
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
    let Some(detail) = item.detail.as_deref().filter(|s| !s.is_empty()) else {
        return;
    };

    // Decide which side (right/left/below) wins and how wide the
    // popup is allowed to be. Borders + 1-cell horizontal padding on
    // each side eats 4 cells, so refuse anything narrower than that
    // plus a useful text width.
    let space_right = buf_area.right().saturating_sub(main.right());
    let space_left = main.x.saturating_sub(buf_area.x);
    let space_below_h = buf_area.bottom().saturating_sub(main.bottom());

    enum Placement {
        Right,
        Left,
        Below,
    }
    let preferred_w = DETAIL_POPUP_WIDTH;
    let (placement, popup_w) = if space_right >= MIN_DETAIL_WIDTH {
        (Placement::Right, preferred_w.min(space_right))
    } else if space_left >= MIN_DETAIL_WIDTH {
        (Placement::Left, preferred_w.min(space_left))
    } else if space_below_h >= 3 {
        let avail_w = buf_area.width.min(preferred_w);
        if avail_w < MIN_DETAIL_WIDTH {
            return;
        }
        (Placement::Below, avail_w)
    } else {
        return;
    };

    let text_w = popup_w.saturating_sub(4) as usize;
    if text_w == 0 {
        return;
    }
    let wrapped = wrap_text(detail, text_w);
    let lines_n = wrapped.len() as u16;
    let popup_h = (lines_n + 2).min(DETAIL_POPUP_HEIGHT + 2);

    let (x, y, max_h) = match placement {
        Placement::Right => (
            main.right(),
            main.y,
            popup_h.min(main.height),
        ),
        Placement::Left => (
            main.x - popup_w,
            main.y,
            popup_h.min(main.height),
        ),
        Placement::Below => {
            // Anchor at main.x but shift left when popup_w extends
            // past the right edge — same trick the main popup uses.
            let max_x = buf_area.right().saturating_sub(popup_w);
            let x = main.x.min(max_x).max(buf_area.x);
            (x, main.bottom(), popup_h.min(space_below_h))
        }
    };

    let height = max_h.min(buf_area.bottom().saturating_sub(y));
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
    for raw_line in s.lines() {
        let mut current = String::new();
        for word in raw_line.split_whitespace() {
            let word_len = word.chars().count();
            if current.is_empty() {
                if word_len <= width {
                    current.push_str(word);
                } else {
                    // Hard-break the oversized token.
                    let mut chunk = String::new();
                    for c in word.chars() {
                        chunk.push(c);
                        if chunk.chars().count() == width {
                            out.push(std::mem::take(&mut chunk));
                        }
                    }
                    if !chunk.is_empty() {
                        current = chunk;
                    }
                }
            } else {
                let needed = current.chars().count() + 1 + word_len;
                if needed <= width {
                    current.push(' ');
                    current.push_str(word);
                } else {
                    out.push(std::mem::take(&mut current));
                    if word_len <= width {
                        current.push_str(word);
                    } else {
                        let mut chunk = String::new();
                        for c in word.chars() {
                            chunk.push(c);
                            if chunk.chars().count() == width {
                                out.push(std::mem::take(&mut chunk));
                            }
                        }
                        if !chunk.is_empty() {
                            current = chunk;
                        }
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
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
