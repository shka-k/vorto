//! Unified event type for the main loop.
//!
//! Terminal input and LSP reader threads both feed into a single
//! `mpsc::Sender<AppEvent>` so the main loop can block on one channel
//! and drain bursts of either kind.

use crossterm::event::Event;

use crate::lsp::LspEvent;

pub enum AppEvent {
    Term(Event),
    Lsp(LspEvent),
}
