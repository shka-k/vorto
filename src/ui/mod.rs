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
mod status;
mod toast;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Color;

use crate::app::{App, Prompt};

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

    buffer::draw_buffer(f, app, chunks[0]);
    status::draw_status(f, app, chunks[1]);
    status::draw_command_line(f, app, chunks[2]);
    toast::draw_toast(f, app, chunks[0]);

    buffer::place_cursor(f, app, chunks[0]);

    if let Prompt::Command(query) = &app.prompt.state {
        hints::draw_command_hints(f, query, chunks[2]);
    }
    if matches!(app.prompt.state, Prompt::Fuzzy(_)) {
        fuzzy::draw_fuzzy(f, app, f.area());
    }
    if matches!(app.prompt.state, Prompt::CodeActionMenu { .. }) {
        code_action::draw_code_action_menu(f, app, chunks[0]);
    }
    if matches!(app.prompt.state, Prompt::Hover { .. }) {
        hover::draw_hover(f, app, chunks[0]);
    }
    if app.completion.is_some() {
        completion::draw_completion(f, app, chunks[0]);
    }
    if !app.prompt.is_open() {
        hints::draw_pending_hints(f, app, chunks[1]);
    }
}
