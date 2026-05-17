//! App-side glue for the Copilot LSP client.
//!
//! Owns the lazy spawn decision and the reader-thread event handler.
//! Kept narrow on purpose — the client itself lives in
//! [`crate::copilot`]; this file just decides *when* it gets started
//! and what the editor does with the events it produces.

use crate::app::App;
use crate::copilot::{CopilotClient, CopilotEvent};
use crate::event::AppEvent;
use crate::lsp::path_to_uri;
use crate::vlog;

impl App {
    /// Best-effort spawn of the Copilot client. Idempotent: returns
    /// immediately once a live client is already attached. The spawn is
    /// synchronous (the `initialize` handshake is fast for Copilot
    /// relative to language servers), runs at startup time, and silently
    /// no-ops when `copilot-language-server` isn't on `PATH` — vorto
    /// stays usable without it.
    pub fn spawn_copilot_if_needed(&mut self) {
        if self.copilot.is_some() {
            return;
        }
        let root_uri = path_to_uri(&self.startup_cwd);
        let tx = self.event_tx.clone();
        let emit: Box<dyn Fn(CopilotEvent) + Send + 'static> =
            Box::new(move |ev| {
                let _ = tx.send(AppEvent::Copilot(ev));
            });
        match CopilotClient::spawn(&root_uri, emit) {
            Ok(Some(client)) => {
                self.copilot = Some(client);
            }
            Ok(None) => {
                // Binary not on PATH. Already logged inside the client;
                // nothing surfaces to the UI by design.
            }
            Err(e) => {
                vlog!("copilot spawn failed: {e:#}");
            }
        }
    }

    /// Handle a reader-thread event from the Copilot client. Phase 1
    /// only sees message + error variants; future phases will fan out
    /// inline-completion responses and sign-in status updates here.
    pub fn handle_copilot_event(&mut self, ev: CopilotEvent) {
        match ev {
            CopilotEvent::Message { level, text } => {
                vlog!("copilot message level={level} {text}");
            }
            CopilotEvent::Error { message } => {
                vlog!("copilot client dropped: {message}");
                // Drop the dead client so a future request triggers a
                // fresh spawn attempt instead of writing into a closed
                // pipe.
                self.copilot = None;
            }
        }
    }
}
