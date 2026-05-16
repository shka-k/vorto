//! Cursor-anchored LSP signature-help popup.
//!
//! Anchored at the cursor row (one row above by default — the signature
//! reads naturally as "this is what's above the call you're typing").
//! The active parameter is rendered with a bold/colored span inside the
//! signature label so the user can see which argument they're filling
//! in.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use crate::app::App;
use crate::lsp::ParameterLabel;

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 8;

pub(super) fn draw_signature(f: &mut Frame, app: &App, buf_area: Rect) {
    let Some(state) = app.signature.as_ref() else {
        return;
    };
    let Some(sig) = state.help.signatures.get(state.help.active_signature) else {
        return;
    };

    // Active-parameter index: per-signature override wins, then the
    // help-level value. `None` means "no highlight" — the server hasn't
    // told us which arg the cursor is in (e.g. between `(` and the
    // first char of the first arg, some servers return null).
    let active_param = sig.active_parameter.or(state.help.active_parameter);

    let row = app.buffer.cursor.row;
    let Some(rel_y) = app.visual_row_offset(row) else {
        return;
    };
    // Same gutter math as the completion popup: 1-char severity sign +
    // 5-char line-number column.
    let gutter_width: u16 = 1 + 5;
    let anchor_x = buf_area.x + gutter_width + app.buffer.cursor.col as u16;
    let anchor_y = buf_area.y + rel_y;

    let line = build_signature_line(sig, active_param);
    // Width: the rendered signature length + 2 border + 2 padding cols,
    // capped at MAX_WIDTH and the buffer width. Long signatures wrap
    // naturally inside the paragraph below; we still cap so a single
    // monstrous signature can't dominate the viewport.
    let text_len: u16 = line
        .spans
        .iter()
        .map(|s| s.content.chars().count() as u16)
        .sum();
    let popup_w = (text_len + 4).min(MAX_WIDTH).min(buf_area.width);

    // Wrap the signature manually so we know how many rows it'll take —
    // ratatui's `Paragraph::wrap` works but doesn't tell us the row
    // count up front, which we need to decide above-vs-below placement.
    let text_w = popup_w.saturating_sub(4) as usize;
    if text_w == 0 {
        return;
    }
    let row_count = wrapped_row_count(&line, text_w).max(1) as u16;
    let popup_h = (row_count + 2).min(MAX_HEIGHT + 2);

    // Prefer above the cursor row — that's where the function call is
    // being written, so reading "fn foo(x, y)" naturally lives above
    // the cursor. Flip below when there isn't room.
    let space_above = anchor_y.saturating_sub(buf_area.y);
    let space_below = buf_area.bottom().saturating_sub(anchor_y.saturating_add(1));
    let y = if space_above >= popup_h {
        anchor_y - popup_h
    } else if space_below >= popup_h {
        anchor_y.saturating_add(1)
    } else if space_above >= 3 {
        anchor_y.saturating_sub(popup_h.min(space_above))
    } else {
        anchor_y.saturating_add(1).min(buf_area.bottom().saturating_sub(1))
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

    let para = Paragraph::new(line).wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(para, inner);
}

/// Build a styled `Line` for the signature, splitting the label into
/// three spans when the active-parameter range is known: before /
/// active / after. The active span gets a brighter color + bold; the
/// rest stays at the default popup foreground.
fn build_signature_line(
    sig: &crate::lsp::SignatureInformation,
    active: Option<usize>,
) -> Line<'static> {
    let label = &sig.label;
    let base = Style::default().fg(Color::Rgb(220, 220, 220));
    let highlight = Style::default()
        .fg(Color::Rgb(255, 200, 120))
        .add_modifier(Modifier::BOLD);

    let range = active
        .and_then(|i| sig.parameters.get(i))
        .and_then(|p| resolve_param_range(&p.label, label));

    match range {
        Some((start, end)) if start < end && end <= label.chars().count() => {
            // Slice by char boundaries — `label` may contain multibyte
            // chars, and `start`/`end` are char offsets.
            let chars: Vec<char> = label.chars().collect();
            let before: String = chars[..start].iter().collect();
            let active_text: String = chars[start..end].iter().collect();
            let after: String = chars[end..].iter().collect();
            Line::from(vec![
                Span::styled(before, base),
                Span::styled(active_text, highlight),
                Span::styled(after, base),
            ])
        }
        _ => Line::from(Span::styled(label.clone(), base)),
    }
}

/// Resolve a [`ParameterLabel`] to a `[start, end)` character-offset
/// range inside `signature_label`. `Offsets` are already in the right
/// shape; `Text` triggers a substring search (first match wins).
/// Returns `None` when a text label doesn't appear in the signature —
/// the popup falls back to no-highlight rather than painting a guess.
fn resolve_param_range(label: &ParameterLabel, signature_label: &str) -> Option<(usize, usize)> {
    match label {
        ParameterLabel::Offsets(s, e) => Some((*s as usize, *e as usize)),
        ParameterLabel::Text(t) => {
            // We can't use `str::find` directly — it returns byte
            // offsets, but the popup downstream slices by chars. Walk
            // the signature char-by-char looking for the first
            // char-aligned match of `t`.
            let sig_chars: Vec<char> = signature_label.chars().collect();
            let needle_chars: Vec<char> = t.chars().collect();
            if needle_chars.is_empty() || needle_chars.len() > sig_chars.len() {
                return None;
            }
            let last_start = sig_chars.len() - needle_chars.len();
            for i in 0..=last_start {
                if sig_chars[i..i + needle_chars.len()] == needle_chars[..] {
                    return Some((i, i + needle_chars.len()));
                }
            }
            None
        }
    }
}

/// Cheap upper bound on how many rows the styled `line` occupies when
/// wrapped at `width` columns. Counts total chars (collapsing styled
/// spans) and divides by width; overcounting by one is fine — it just
/// reserves an extra empty row inside the popup.
fn wrapped_row_count(line: &Line, width: usize) -> usize {
    let total: usize = line
        .spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum();
    if total == 0 {
        return 1;
    }
    total.div_ceil(width)
}
