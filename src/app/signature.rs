//! Active signature-help popup state.
//!
//! Lives on `App` for the same reason `CompletionState` does: the popup
//! is non-modal — the user keeps typing in insert mode while it's open
//! and the help just updates in place after each keystroke.
//!
//! Lifecycle:
//! 1. User types a server-declared trigger character (typically `(`) or
//!    `accept_completion` auto-appends `()` after a callable item.
//!    `App::lsp_signature_help` fires `textDocument/signatureHelp`,
//!    snapshotting the cursor row as the anchor.
//! 2. The response arrives and `apply_signature_help_outcome` either
//!    opens / refreshes the popup or — when the server returns `null` —
//!    closes it.
//! 3. While open, every keystroke in `handle_insert_key` re-fires the
//!    request as a retrigger (`isRetrigger: true`). The server is
//!    responsible for tracking which parameter the cursor is currently
//!    in and updating `activeParameter` accordingly.
//! 4. Esc / cursor row change / explicit close drops the state.

use crate::lsp::SignatureHelp;

/// Active signature-help popup. `None` on `App` when nothing is showing.
pub struct SignatureState {
    /// Whatever the server last returned. The popup renders
    /// `help.signatures[help.active_signature]` and uses `active_parameter`
    /// (with the per-signature override taking precedence) to know which
    /// argument span to highlight.
    pub help: SignatureHelp,
}

/// How a signature-help request was initiated. Maps onto LSP's
/// `SignatureHelpContext` so the server can branch its behavior — most
/// notably, `(` versus `,` typically map to `TriggerCharacter` /
/// `ContentChange` respectively, and accept-completion follow-ups map
/// to `Invoked`.
#[derive(Debug, Clone, Copy)]
pub enum SignatureTrigger {
    /// Programmatic — no user keystroke directly caused this. Used by
    /// the accept-completion path after the popup auto-appended `()`.
    Invoked,
    /// User typed `c` and it matched the server's
    /// `signatureHelpProvider.triggerCharacters`.
    TriggerCharacter(char),
    /// User typed `c` while the popup was already open and `c` matched
    /// `retriggerCharacters` — or the user typed *any* char while the
    /// popup was open (the popup needs to follow the cursor, so every
    /// content change retriggers).
    ContentChange(Option<char>),
}
