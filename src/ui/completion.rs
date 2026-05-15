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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem};

use crate::app::App;

const MAX_WIDTH: u16 = 60;
const MAX_HEIGHT: u16 = 10;

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

    // Width: longest visible label (capped) + space + kind badge.
    let label_w = state
        .filtered
        .iter()
        .map(|i| state.items[*i].label.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .min(MAX_WIDTH.saturating_sub(6));
    // 3 chars for kind badge ("Fn ", "Var", …) + 1 space.
    let inner_w = (label_w + 4).min(MAX_WIDTH);
    let popup_w = (inner_w + 2).min(buf_area.width);
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
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let body_h = inner.height as usize;
    let scroll = state
        .selected
        .saturating_sub(body_h.saturating_sub(1));
    let inner_w = inner.width as usize;

    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .enumerate()
        .skip(scroll)
        .take(body_h)
        .map(|(i, item_idx)| {
            let item = &state.items[*item_idx];
            let is_sel = i == state.selected;
            let badge = kind_badge(item.kind);
            // Layout: "Fn  label    detail" — badge takes 3, then a
            // single space, then label, then (when there's room and a
            // detail exists) a space-padded detail right-aligned.
            let badge_w = 3 + 1;
            let label_chars = item.label.chars().count();
            let detail = item.detail.as_deref().unwrap_or("");
            let detail_chars = detail.chars().count();

            let usable = inner_w.saturating_sub(badge_w);
            let (label_room, detail_room) = if detail_chars == 0 || label_chars >= usable {
                (usable, 0)
            } else {
                // Reserve at least 1 column gap between label and detail.
                let max_detail = usable.saturating_sub(label_chars).saturating_sub(1);
                (label_chars.min(usable), detail_chars.min(max_detail))
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
            let badge_style = if is_sel {
                row_style
            } else {
                Style::default().fg(Color::Rgb(140, 160, 200))
            };
            let detail_style = if is_sel {
                row_style
            } else {
                Style::default().fg(Color::Rgb(150, 150, 150))
            };
            // Pad between label and detail so detail right-aligns.
            let gap = inner_w
                .saturating_sub(badge_w)
                .saturating_sub(label.chars().count())
                .saturating_sub(detail_text.chars().count());
            let mut spans = vec![
                Span::styled(format!("{:<3}", badge), badge_style),
                Span::styled(format!(" {}", label), row_style),
            ];
            if !detail_text.is_empty() {
                spans.push(Span::styled(" ".repeat(gap), row_style));
                spans.push(Span::styled(detail_text, detail_style));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    f.render_widget(List::new(items), inner);
}

/// 3-character abbreviation for the LSP `CompletionItemKind` enum. Keeps
/// the popup readable at a glance without pulling in icon fonts.
fn kind_badge(kind: u8) -> &'static str {
    match kind {
        1 => "Txt",
        2 => "Fn",
        3 => "Fn",
        4 => "Ctr", // constructor
        5 => "Fld", // field
        6 => "Var",
        7 => "Cls", // class
        8 => "Itf", // interface
        9 => "Mod",
        10 => "Prp", // property
        11 => "Uni", // unit
        12 => "Val", // value
        13 => "Enu", // enum
        14 => "Kw",
        15 => "Sni", // snippet
        16 => "Col", // color
        17 => "Fil", // file
        18 => "Ref",
        19 => "Dir", // folder
        20 => "EnM", // enum member
        21 => "Cst", // constant
        22 => "Str", // struct
        23 => "Evt", // event
        24 => "Op",
        25 => "Tpr", // type parameter
        _ => "·",
    }
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
