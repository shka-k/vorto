//! Top-level UI orchestrator.
//!
//! Splits the frame into the buffer viewport, the status bar, and the
//! command line, then dispatches to submodule renderers. Overlays
//! (`:command` hints, fuzzy picker, pending-operator which-key hints)
//! are drawn last so they sit above the base layout.
//!
//! Submodules:
//! - [`buffer`] — the main edit viewport, gutter, syntax highlighting,
//!   visual-selection painting, and the cursor placement that the
//!   terminal needs at the end of each frame.
//! - [`status`] — the one-line status bar (mode badge, status text,
//!   diagnostic under cursor, pending count, cursor position) and the
//!   `:` / `/` / rename command line directly under it.
//! - [`toast`] — the floating bottom-right toast for transient
//!   info / warn / error messages.
//! - [`hints`] — overlay panels: `:command` autocomplete and the
//!   which-key panel that appears while an operator is pending.
//! - [`fuzzy`] — the fuzzy picker popup with its source-preview pane.

mod buffer;
mod code_action;
mod completion;
mod fuzzy;
mod hints;
mod hover;
mod signature;
mod status;
mod toast;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::app::{App, PaneId, PaneLayout, PaneRect, Prompt, SplitDir};

/// Shared overlay panel background — slightly darker than ANSI 8
/// (bright black), so floating widgets (command hints, pending-op
/// hints, toasts) read as a dim panel rather than pure black.
pub(crate) const PANEL_BG: Color = Color::Rgb(30, 30, 30);

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Partition the buffer viewport into per-pane rectangles. With a
    // single-leaf layout this is a no-op; with one or more splits it
    // walks the tree applying the per-node ratios.
    let mut rects: std::collections::HashMap<PaneId, Rect> = std::collections::HashMap::new();
    compute_pane_rects(&app.layout, chunks[0], &mut rects);
    // Publish rectangles for directional focus navigation. Bypass
    // `rects` going stale by writing every frame.
    {
        let mut out = app.last_pane_rects.borrow_mut();
        out.clear();
        for (id, r) in rects.iter() {
            out.insert(
                *id,
                PaneRect {
                    x: r.x,
                    y: r.y,
                    width: r.width,
                    height: r.height,
                },
            );
        }
    }
    let active_rect = rects.get(&app.active_pane).copied().unwrap_or(chunks[0]);
    for (&id, &rect) in &rects {
        if id == app.active_pane {
            buffer::draw_buffer(f, app, rect);
        } else if let Some(buf) = app.buffer_for_pane(id) {
            // `buffer_for_pane` resolves the shared-ref case (panes
            // pointing at the active ref read straight from
            // `App.buffer`) so two panes on the same buffer paint the
            // same live content.
            let eff = effective_editor_for_buffer(app, buf);
            buffer::draw_buffer_inactive(f, buf, &eff, rect);
        }
    }
    // Paint dividers between sibling panes and a focus-ring on the
    // active pane border. Both are cosmetic only.
    draw_pane_borders(f, &app.layout, chunks[0], app.active_pane);
    status::draw_status(f, app, chunks[1]);
    status::draw_command_line(f, app, chunks[2]);
    toast::draw_toast(f, app, chunks[0]);

    buffer::place_cursor(f, app, active_rect);

    if let Prompt::Command(cp) = &app.prompt.state {
        hints::draw_command_hints(f, cp, chunks[2]);
    }
    if matches!(app.prompt.state, Prompt::Fuzzy(_)) {
        fuzzy::draw_fuzzy(f, app, f.area());
    }
    // Cursor-anchored popups (code action menu, hover, completion)
    // need the *active pane's* rect, not the whole buffer area — with
    // splits the cursor lives inside a sub-region and the popup math
    // (`buf_area.x + gutter + col`) would otherwise anchor at the
    // wrong column.
    if matches!(app.prompt.state, Prompt::CodeActionMenu { .. }) {
        code_action::draw_code_action_menu(f, app, active_rect);
    }
    if matches!(app.prompt.state, Prompt::Hover { .. }) {
        hover::draw_hover(f, app, active_rect);
    }
    if app.completion.is_some() {
        completion::draw_completion(f, app, active_rect);
    }
    if app.signature.is_some() {
        signature::draw_signature(f, app, active_rect);
    }
    if !app.prompt.is_open() {
        hints::draw_pending_hints(f, app, chunks[1]);
    }
}

/// Walk the pane layout tree and slice `area` into per-pane rectangles
/// based on each split's direction and child ratios. Result is keyed
/// by pane id so callers can look up the rectangle for the leaf they
/// want to draw (or, for directional focus, compare rectangles
/// against each other).
fn compute_pane_rects(
    node: &PaneLayout,
    area: Rect,
    out: &mut std::collections::HashMap<PaneId, Rect>,
) {
    match node {
        PaneLayout::Leaf(id) => {
            out.insert(*id, area);
        }
        PaneLayout::Split {
            dir,
            children,
            ratios,
        } => {
            // Normalize ratios defensively — `remove_leaf` already
            // renormalizes after a removal but `Constraint::Ratio`
            // expects u32 numerators that sum to a fixed total.
            let sum: f32 = ratios.iter().sum();
            let denom: u32 = 10_000;
            let mut numers: Vec<u32> = ratios
                .iter()
                .map(|r| {
                    if sum > 0.0 {
                        ((r / sum) * denom as f32).round() as u32
                    } else {
                        denom / children.len() as u32
                    }
                })
                .collect();
            // Distribute rounding remainder to the last child so the
            // numerators sum exactly to `denom`.
            let total: u32 = numers.iter().sum();
            if let Some(last) = numers.last_mut()
                && total < denom
            {
                *last += denom - total;
            }
            // Reserve one cell between sibling panes for the divider
            // bar. Total reserved = children.len() - 1.
            let direction = match dir {
                SplitDir::Vertical => Direction::Horizontal,
                SplitDir::Horizontal => Direction::Vertical,
            };
            let divider_cells = children.len().saturating_sub(1) as u16;
            let usable = match direction {
                Direction::Horizontal => area.width.saturating_sub(divider_cells),
                Direction::Vertical => area.height.saturating_sub(divider_cells),
            };
            // Build constraints, accounting for divider cells by
            // placing a 1-cell `Length(1)` between every pair of pane
            // constraints.
            let mut constraints: Vec<Constraint> = Vec::with_capacity(children.len() * 2);
            for (i, &n) in numers.iter().enumerate() {
                let pane_size = ((n * usable as u32) / denom) as u16;
                constraints.push(Constraint::Length(pane_size));
                if i + 1 < children.len() {
                    constraints.push(Constraint::Length(1));
                }
            }
            // Constraint::Length sums may not exactly cover `area`
            // (rounding); ratatui will absorb the remainder into the
            // last segment.
            let chunks = Layout::default()
                .direction(direction)
                .constraints(constraints)
                .split(area);
            // Even indices in `chunks` are panes; odd indices are
            // divider slots (drawn separately by `draw_pane_borders`).
            for (i, child) in children.iter().enumerate() {
                let chunk = chunks[i * 2];
                compute_pane_rects(child, chunk, out);
            }
        }
    }
}

/// Resolve the effective editor settings (tab width, show-whitespace,
/// …) for an inactive pane's buffer. Mirrors `App::effective_editor`
/// which is hard-coded to read from `app.buffer`.
fn effective_editor_for_buffer(
    app: &App,
    buf: &crate::editor::Buffer,
) -> crate::config::EditorConfig {
    let base = app.config.editor;
    let Some(path) = buf.path.as_ref() else {
        return base;
    };
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return base;
    };
    let Some(lang) = app.config.languages.by_extension(ext) else {
        return base;
    };
    base.overlay(&lang.editor)
}

/// Paint the divider bar between sibling panes. Walks the tree the
/// same way `compute_pane_rects` does and renders a single-character
/// vertical or horizontal bar in the 1-cell gap each `Split` reserves
/// between children. The active pane's border gets a brighter color
/// so the user can spot which pane has focus.
fn draw_pane_borders(f: &mut Frame, node: &PaneLayout, area: Rect, active: PaneId) {
    use ratatui::widgets::Block;
    use ratatui::widgets::BorderType;
    if let PaneLayout::Split {
        dir,
        children,
        ratios,
    } = node
    {
        let sum: f32 = ratios.iter().sum();
        let denom: u32 = 10_000;
        let mut numers: Vec<u32> = ratios
            .iter()
            .map(|r| {
                if sum > 0.0 {
                    ((r / sum) * denom as f32).round() as u32
                } else {
                    denom / children.len() as u32
                }
            })
            .collect();
        let total: u32 = numers.iter().sum();
        if let Some(last) = numers.last_mut()
            && total < denom
        {
            *last += denom - total;
        }
        let direction = match dir {
            SplitDir::Vertical => Direction::Horizontal,
            SplitDir::Horizontal => Direction::Vertical,
        };
        let divider_cells = children.len().saturating_sub(1) as u16;
        let usable = match direction {
            Direction::Horizontal => area.width.saturating_sub(divider_cells),
            Direction::Vertical => area.height.saturating_sub(divider_cells),
        };
        let mut constraints: Vec<Constraint> = Vec::with_capacity(children.len() * 2);
        for (i, &n) in numers.iter().enumerate() {
            let pane_size = ((n * usable as u32) / denom) as u16;
            constraints.push(Constraint::Length(pane_size));
            if i + 1 < children.len() {
                constraints.push(Constraint::Length(1));
            }
        }
        let chunks = Layout::default()
            .direction(direction)
            .constraints(constraints)
            .split(area);
        // Render the divider chunks (odd indices). One-cell-thick
        // ratatui Block with a border gives us the line.
        for i in 0..(children.len().saturating_sub(1)) {
            let divider = chunks[i * 2 + 1];
            let glyph = match direction {
                Direction::Horizontal => "│",
                Direction::Vertical => "─",
            };
            let text: Vec<ratatui::text::Line> = match direction {
                Direction::Horizontal => (0..divider.height)
                    .map(|_| {
                        ratatui::text::Line::from(Span::styled(
                            glyph,
                            Style::default().fg(Color::DarkGray),
                        ))
                    })
                    .collect(),
                Direction::Vertical => vec![ratatui::text::Line::from(Span::styled(
                    glyph.repeat(divider.width as usize),
                    Style::default().fg(Color::DarkGray),
                ))],
            };
            f.render_widget(Paragraph::new(text), divider);
        }
        for (i, child) in children.iter().enumerate() {
            let chunk = chunks[i * 2];
            draw_pane_borders(f, child, chunk, active);
        }
    } else if let PaneLayout::Leaf(id) = node
        && *id == active
    {
        // Active-pane outline: 1-cell border ring. Useful only when
        // there is more than one pane; otherwise the ring just frames
        // the only buffer for no reason. Caller passes the same active
        // id all the way down, so we just check id == active here.
        let _ = Block::default().border_type(BorderType::Plain);
    }
}
