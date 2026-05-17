//! App-side glue for the Copilot LSP client.
//!
//! Owns the lazy spawn decision, document-sync gate, request-kind
//! pending map, and the reader-thread event handler. Kept narrow on
//! purpose — wire protocol lives in [`crate::copilot`]; this file
//! decides *when* requests fire and what the editor does with the
//! events that come back.

use std::collections::HashMap;

use crate::app::App;
use crate::app::toast::Toast;
use crate::copilot::{
    self, CheckStatus, CopilotClient, CopilotEvent, InlineCompletionRaw, SignInInitiate,
};
use crate::editor::{Cursor, RequestId, Suggestion, SuggestionState};
use crate::event::AppEvent;
use crate::lsp::path_to_uri;
use crate::vlog;

/// Trim the prefix of `raw.text` that the user has already typed at
/// the request anchor. Copilot includes those characters in the
/// `insertText` field (with `range` covering them) so the suggestion
/// can replace any client-side normalisation. Vorto inserts on accept
/// without replacing, so the prefix has to come off here.
///
/// Returns `None` when the server's range references a position vorto
/// can't represent in the current buffer — treat that as a stale
/// suggestion rather than guessing.
fn strip_already_typed(
    raw: &InlineCompletionRaw,
    anchor: Cursor,
    lines: &[String],
) -> Option<String> {
    let Some(range) = raw.range else {
        return Some(raw.text.clone());
    };
    // Single-line ranges that end at the anchor cover the common case
    // (Copilot anchors completions at the cursor and stretches `start`
    // back over the partial token already on the line). Multi-line or
    // backwards ranges fall back to using the text verbatim — better
    // to show something than to drop a valid suggestion.
    if range.start_line != range.end_line
        || range.end_line as usize != anchor.row
        || range.end_character as usize != anchor.col
        || (range.start_character as usize) > anchor.col
    {
        return Some(raw.text.clone());
    }
    let line = lines.get(anchor.row)?;
    let start = range.start_character as usize;
    let end = anchor.col;
    let prefix: String = line.chars().skip(start).take(end - start).collect();
    if raw.text.starts_with(&prefix) {
        Some(raw.text[prefix.len()..].to_string())
    } else {
        // Server-side `insertText` doesn't begin with what the buffer
        // shows in the replace range — likely the user typed more than
        // the model expected. Skip rather than paint a misaligned ghost.
        None
    }
}

/// What an outstanding Copilot request was for. Routed against the
/// reader thread's generic `Response{id, result, error}` event so
/// each kind can fan out to its own dispatcher.
#[derive(Debug, Clone, Copy)]
pub enum CopilotRequestKind {
    InlineCompletion,
    CheckStatus,
    SignInInitiate,
    SignInConfirm,
    SignOut,
}

/// Known auth state for the Copilot client. `Unknown` is the initial
/// value between spawn and the first `checkStatus` reply; until we
/// know the user is signed in we don't bother sending
/// `inlineCompletion` requests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum CopilotAuthState {
    #[default]
    Unknown,
    SignedIn { user: Option<String> },
    NotSignedIn,
    NotAuthorized { reason: Option<String> },
}

impl CopilotAuthState {
    fn signed_in(&self) -> bool {
        matches!(self, Self::SignedIn { .. })
    }
}

/// Pending-request map used to route generic
/// [`CopilotEvent::Response`] events back to the request that fired
/// them. Built as its own type so the App field stays tiny and the
/// kind enum can grow without touching every consumer.
#[derive(Default)]
pub struct CopilotPending {
    inner: HashMap<u64, CopilotRequestKind>,
}

impl CopilotPending {
    pub fn insert(&mut self, id: u64, kind: CopilotRequestKind) {
        self.inner.insert(id, kind);
    }

    pub fn take(&mut self, id: u64) -> Option<CopilotRequestKind> {
        self.inner.remove(&id)
    }
}

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
            Ok(Some(mut client)) => {
                // Fire checkStatus immediately so the App knows whether
                // inline completion requests should be skipped (no
                // point asking when the user is signed out). The reply
                // lands asynchronously via handle_copilot_event.
                match client.check_status(true) {
                    Ok(id) => {
                        self.copilot_pending
                            .insert(id, CopilotRequestKind::CheckStatus);
                    }
                    Err(e) => vlog!("copilot checkStatus send failed: {e:#}"),
                }
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

    /// True when the active buffer's content has drifted from what
    /// Copilot saw last, *or* the buffer was never sent. All per-URI
    /// state lives inside [`CopilotClient`] so buffer switches don't
    /// need to reach in and reset anything App-side.
    pub(super) fn copilot_needs_sync(&self) -> bool {
        let Some(copilot) = &self.copilot else {
            return false;
        };
        let Some(uri) = self.copilot_active_uri() else {
            return false;
        };
        copilot.needs_sync(&uri, self.buffer.version)
    }

    /// Push the active buffer to Copilot — `didOpen` on first sight,
    /// `didChange` thereafter. Caller materialises `text` once so a
    /// paired LSP sync can reuse the same string.
    pub(super) fn sync_buffer_to_copilot(&mut self, text: &str) {
        let Some(uri) = self.copilot_active_uri() else {
            return;
        };
        let language_id = self.copilot_active_language_id();
        let version = self.buffer.version;
        let Some(copilot) = self.copilot.as_mut() else {
            return;
        };
        let result = if copilot.is_open(&uri) {
            copilot.did_change(&uri, text, version)
        } else {
            copilot.did_open(&uri, &language_id, text, version)
        };
        if let Err(e) = result {
            vlog!("copilot sync failed uri={uri}: {e:#}");
        }
    }

    /// Fire `textDocument/inlineCompletion` for the cursor, install
    /// `Pending` state, and return `true` when a request actually
    /// went out. Caller should call `cancel_inline_suggestion` ahead
    /// of time if it wants the dismissal-on-no-request path.
    pub(super) fn request_copilot_inline_completion(&mut self) -> bool {
        if !self.copilot_auth.signed_in() {
            return false;
        }
        let Some(uri) = self.copilot_active_uri() else {
            return false;
        };
        let anchor = self.buffer.cursor;
        let Some(copilot) = self.copilot.as_mut() else {
            return false;
        };
        let id = match copilot.inline_completion(
            &uri,
            anchor.row as u32,
            anchor.col as u32,
        ) {
            Ok(id) => id,
            Err(e) => {
                vlog!("copilot inlineCompletion send failed: {e:#}");
                return false;
            }
        };
        self.copilot_pending
            .insert(id, CopilotRequestKind::InlineCompletion);
        self.inline_suggestion = SuggestionState::Pending {
            id: RequestId(id),
            anchor,
        };
        true
    }

    /// Schedule an inline-completion request when conditions look
    /// favourable (cursor at end of line, no LSP popup, Copilot
    /// available). Replaces the Phase-0 stub provider that synthesised
    /// suggestions locally.
    ///
    /// Forces a `didOpen`/`didChange` *before* the request fires —
    /// without this the request would race the main loop's
    /// `sync_buffer_if_dirty` and Copilot would answer against the
    /// previous buffer snapshot (or empty content, for the first
    /// keystroke after open). Lossy context shows up to the user as
    /// completions that pretend the file has only the current line.
    pub(super) fn update_inline_suggestion(&mut self) {
        if self.completion.is_some() {
            self.inline_suggestion.dismiss();
            return;
        }
        if self.copilot.is_none() {
            self.inline_suggestion.dismiss();
            return;
        }
        let cursor = self.buffer.cursor;
        let row_len = self
            .buffer
            .lines
            .get(cursor.row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        if cursor.col != row_len {
            self.inline_suggestion.dismiss();
            return;
        }
        // Drop any prior Showing/Pending first — superseded by the
        // request we're about to fire.
        self.inline_suggestion.dismiss();
        if self.copilot_needs_sync() {
            let text = self.buffer.lines.join("\n");
            self.sync_buffer_to_copilot(&text);
        }
        let _ = self.request_copilot_inline_completion();
    }

    /// Handle a reader-thread event from the Copilot client.
    pub fn handle_copilot_event(&mut self, ev: CopilotEvent) {
        match ev {
            CopilotEvent::Response { id, result, error } => {
                let Some(kind) = self.copilot_pending.take(id) else {
                    return;
                };
                match kind {
                    CopilotRequestKind::InlineCompletion => {
                        self.handle_copilot_inline_completion(id, result, error);
                    }
                    CopilotRequestKind::CheckStatus => {
                        self.handle_copilot_check_status(result, error);
                    }
                    CopilotRequestKind::SignInInitiate => {
                        self.handle_copilot_sign_in_initiate(result, error);
                    }
                    CopilotRequestKind::SignInConfirm => {
                        self.handle_copilot_sign_in_confirm(result, error);
                    }
                    CopilotRequestKind::SignOut => {
                        self.handle_copilot_sign_out(result, error);
                    }
                }
            }
            CopilotEvent::Error { message } => {
                vlog!("copilot client dropped: {message}");
                // Drop the dead client so a future request triggers a
                // fresh spawn attempt instead of writing into a closed
                // pipe. Pending entries are abandoned — their responses
                // can never arrive now.
                self.copilot = None;
                self.copilot_pending = CopilotPending::default();
                self.inline_suggestion.dismiss();
            }
        }
    }

    /// `:copilot <sub>` dispatcher. Returns `Ok(())` after pushing
    /// the appropriate toast — callers don't need to distinguish
    /// "succeeded" from "told the user something" because both end up
    /// as a status message anyway.
    pub(super) fn run_copilot_command(&mut self, sub: &str) {
        let sub = sub.trim();
        // Default (`:copilot`) shows status — matches the convention of
        // `:git`, `:fugitive`, etc.
        match sub {
            "" | "status" => self.copilot_status_toast(),
            "signin" | "login" => self.copilot_signin(),
            "signout" | "logout" => self.copilot_signout(),
            other => {
                self.push_toast(Toast::error(format!(
                    "unknown copilot subcommand: {other}"
                )));
            }
        }
    }

    fn copilot_status_toast(&mut self) {
        let msg = match &self.copilot_auth {
            CopilotAuthState::Unknown => match &self.copilot {
                None => "Copilot: not running (binary missing?)".to_string(),
                Some(_) => "Copilot: checking status...".to_string(),
            },
            CopilotAuthState::SignedIn { user } => format!(
                "Copilot: signed in as {}",
                user.as_deref().unwrap_or("(unknown user)")
            ),
            CopilotAuthState::NotSignedIn => {
                "Copilot: not signed in — run :copilot signin".to_string()
            }
            CopilotAuthState::NotAuthorized { reason } => format!(
                "Copilot: not authorized ({})",
                reason.as_deref().unwrap_or("no entitlement")
            ),
        };
        self.push_toast(Toast::info(msg));
    }

    fn copilot_signin(&mut self) {
        if matches!(self.copilot_auth, CopilotAuthState::SignedIn { .. }) {
            self.push_toast(Toast::info("Copilot: already signed in".to_string()));
            return;
        }
        let Some(copilot) = self.copilot.as_mut() else {
            self.push_toast(Toast::error(
                "Copilot: server not running (install copilot-language-server)"
                    .to_string(),
            ));
            return;
        };
        match copilot.sign_in_initiate() {
            Ok(id) => {
                self.copilot_pending
                    .insert(id, CopilotRequestKind::SignInInitiate);
            }
            Err(e) => {
                vlog!("copilot signInInitiate send failed: {e:#}");
                self.push_toast(Toast::error(format!(
                    "Copilot: signin failed to start ({e})"
                )));
            }
        }
    }

    fn copilot_signout(&mut self) {
        let Some(copilot) = self.copilot.as_mut() else {
            self.push_toast(Toast::error("Copilot: server not running".to_string()));
            return;
        };
        match copilot.sign_out() {
            Ok(id) => {
                self.copilot_pending
                    .insert(id, CopilotRequestKind::SignOut);
            }
            Err(e) => {
                vlog!("copilot signOut send failed: {e:#}");
                self.push_toast(Toast::error(format!("Copilot: signout failed ({e})")));
            }
        }
    }

    fn handle_copilot_sign_in_initiate(
        &mut self,
        result: Option<serde_json::Value>,
        error: Option<String>,
    ) {
        if let Some(msg) = error {
            vlog!("copilot signInInitiate error: {msg}");
            self.push_toast(Toast::error(format!("Copilot signin: {msg}")));
            return;
        }
        let parsed = result.as_ref().and_then(copilot::parse_sign_in_initiate);
        match parsed {
            Some(SignInInitiate::AlreadySignedIn { user }) => {
                self.copilot_auth = CopilotAuthState::SignedIn { user };
                self.push_toast(Toast::info("Copilot: already signed in".to_string()));
            }
            Some(SignInInitiate::PromptDeviceFlow {
                user_code,
                verification_uri,
            }) => {
                // Copy the code to the OS clipboard so a paste in the
                // browser is one keystroke away.
                self.sync_text_to_clipboard(&user_code);
                self.push_toast(Toast::fatal(format!(
                    "Copilot: visit {verification_uri} and enter code {user_code} \
                     (copied) — press Esc to dismiss"
                )));
                // Auto-fire signInConfirm. The server holds the response
                // until the user authorizes or it times out — our reader
                // thread surfaces the reply asynchronously, the editor
                // stays interactive.
                if let Some(copilot) = self.copilot.as_mut() {
                    match copilot.sign_in_confirm(&user_code) {
                        Ok(id) => {
                            self.copilot_pending
                                .insert(id, CopilotRequestKind::SignInConfirm);
                        }
                        Err(e) => vlog!("copilot signInConfirm send failed: {e:#}"),
                    }
                }
            }
            None => {
                vlog!("copilot signInInitiate: unparseable result {result:?}");
                self.push_toast(Toast::error(
                    "Copilot signin: unexpected response from server".to_string(),
                ));
            }
        }
    }

    fn handle_copilot_sign_in_confirm(
        &mut self,
        result: Option<serde_json::Value>,
        error: Option<String>,
    ) {
        if let Some(msg) = error {
            vlog!("copilot signInConfirm error: {msg}");
            self.push_toast(Toast::error(format!("Copilot signin: {msg}")));
            return;
        }
        // Reuse the checkStatus parser — the confirm reply shares the
        // status/user shape.
        let status = result.as_ref().and_then(copilot::parse_check_status);
        match status {
            Some(CheckStatus::SignedIn { user }) => {
                self.copilot_auth = CopilotAuthState::SignedIn { user: user.clone() };
                self.push_toast(Toast::info(format!(
                    "Copilot: signed in as {}",
                    user.as_deref().unwrap_or("(unknown user)")
                )));
            }
            Some(CheckStatus::NotSignedIn) | None => {
                self.push_toast(Toast::error(
                    "Copilot signin: not completed (timed out or rejected)".to_string(),
                ));
            }
            Some(CheckStatus::NotAuthorized { reason }) => {
                self.copilot_auth = CopilotAuthState::NotAuthorized { reason: reason.clone() };
                self.push_toast(Toast::error(format!(
                    "Copilot signin: not authorized ({})",
                    reason.as_deref().unwrap_or("no entitlement")
                )));
            }
            Some(CheckStatus::Other(s)) => {
                vlog!("copilot signInConfirm unexpected status: {s}");
                self.push_toast(Toast::error(format!("Copilot signin: {s}")));
            }
        }
    }

    fn handle_copilot_sign_out(
        &mut self,
        _result: Option<serde_json::Value>,
        error: Option<String>,
    ) {
        if let Some(msg) = error {
            vlog!("copilot signOut error: {msg}");
            self.push_toast(Toast::error(format!("Copilot signout: {msg}")));
            return;
        }
        self.copilot_auth = CopilotAuthState::NotSignedIn;
        self.inline_suggestion.dismiss();
        self.push_toast(Toast::info("Copilot: signed out".to_string()));
    }

    /// Push `text` onto the OS clipboard. Mirrors
    /// [`Self::sync_yank_to_clipboard`] but takes an explicit value so
    /// callers (sign-in code paste) don't have to thread through the
    /// buffer's yank register.
    fn sync_text_to_clipboard(&mut self, text: &str) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text.to_string());
        }
    }

    fn handle_copilot_check_status(
        &mut self,
        result: Option<serde_json::Value>,
        error: Option<String>,
    ) {
        if let Some(msg) = error {
            vlog!("copilot checkStatus error: {msg}");
            self.copilot_auth = CopilotAuthState::Unknown;
            return;
        }
        let status = result.as_ref().and_then(copilot::parse_check_status);
        let new_state = match status {
            Some(CheckStatus::SignedIn { user }) => CopilotAuthState::SignedIn { user },
            Some(CheckStatus::NotSignedIn) => CopilotAuthState::NotSignedIn,
            Some(CheckStatus::NotAuthorized { reason }) => {
                CopilotAuthState::NotAuthorized { reason }
            }
            Some(CheckStatus::Other(s)) => {
                vlog!("copilot checkStatus unrecognised status: {s}");
                CopilotAuthState::Unknown
            }
            None => {
                vlog!("copilot checkStatus: missing/unparseable result");
                CopilotAuthState::Unknown
            }
        };
        // Only surface a toast on the *transition* into an unsigned
        // state, not on every checkStatus reply. Avoids spam when the
        // user re-checks status manually.
        let just_logged_out = !matches!(self.copilot_auth, CopilotAuthState::NotSignedIn)
            && matches!(new_state, CopilotAuthState::NotSignedIn);
        let just_unauthorized = !matches!(self.copilot_auth, CopilotAuthState::NotAuthorized { .. })
            && matches!(new_state, CopilotAuthState::NotAuthorized { .. });
        match &new_state {
            CopilotAuthState::SignedIn { user } => {
                vlog!(
                    "copilot signed in user={}",
                    user.as_deref().unwrap_or("?")
                );
            }
            CopilotAuthState::NotSignedIn if just_logged_out => {
                self.push_toast(Toast::warn(
                    "Copilot: not signed in — run :copilot signin".to_string(),
                ));
            }
            CopilotAuthState::NotAuthorized { reason } if just_unauthorized => {
                let msg = reason
                    .clone()
                    .unwrap_or_else(|| "no Copilot entitlement on this account".to_string());
                self.push_toast(Toast::warn(format!("Copilot: not authorized ({msg})")));
            }
            _ => {}
        }
        self.copilot_auth = new_state;
    }

    fn handle_copilot_inline_completion(
        &mut self,
        id: u64,
        result: Option<serde_json::Value>,
        error: Option<String>,
    ) {
        if let Some(msg) = error {
            vlog!("copilot inlineCompletion error id={id} {msg}");
            self.maybe_dismiss_pending(id);
            return;
        }
        let raw = match result.as_ref().and_then(copilot::parse_inline_completion) {
            Some(r) => r,
            None => {
                self.maybe_dismiss_pending(id);
                return;
            }
        };
        // Guard: state must still be Pending for this exact request id,
        // and the cursor must not have moved since the request fired —
        // otherwise the suggestion is stale.
        let (matches, anchor) = match &self.inline_suggestion {
            SuggestionState::Pending { id: pid, anchor } => (pid.0 == id, *anchor),
            _ => (false, self.buffer.cursor),
        };
        if !matches || self.buffer.cursor != anchor {
            return;
        }
        // Copilot returns the full completion including the chars the
        // user has already typed (the `range` covers them). Strip that
        // prefix so the ghost text shows only what *would* be added,
        // and a future accept just appends — no buffer-side replace
        // needed for the single-line case.
        let suffix = strip_already_typed(&raw, anchor, &self.buffer.lines);
        let Some(suffix) = suffix else {
            self.inline_suggestion.dismiss();
            return;
        };
        // Multi-line ghost-text rendering isn't wired yet; trim to the
        // first line so we never paint or accept content that the
        // renderer can't represent. Continuation rows come later.
        let first_line = match suffix.split_once('\n') {
            Some((head, _)) => head.to_string(),
            None => suffix,
        };
        if first_line.is_empty() {
            self.inline_suggestion.dismiss();
            return;
        }
        self.inline_suggestion = SuggestionState::Showing {
            id: RequestId(id),
            suggestion: Suggestion {
                text: first_line,
                anchor,
            },
        };
    }

    /// Clear `inline_suggestion` only when it's still the `Pending`
    /// entry for this request id — protects against erasing a newer
    /// Showing/Pending that already superseded the failing one.
    fn maybe_dismiss_pending(&mut self, id: u64) {
        if let SuggestionState::Pending { id: pid, .. } = &self.inline_suggestion
            && pid.0 == id
        {
            self.inline_suggestion.dismiss();
        }
    }

    fn copilot_active_uri(&self) -> Option<String> {
        self.buffer.path.as_ref().map(|p| path_to_uri(p))
    }

    /// Language id Copilot expects in `didOpen`. Falls back to
    /// `"plaintext"` when the file's extension doesn't resolve to a
    /// configured language — Copilot still produces sensible
    /// completions there.
    fn copilot_active_language_id(&self) -> String {
        let ext = self
            .buffer
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str());
        let Some(ext) = ext else {
            return "plaintext".to_string();
        };
        self.config
            .languages
            .by_extension(ext)
            .map(|spec| spec.name.clone())
            .unwrap_or_else(|| "plaintext".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copilot::ReplaceRange;

    fn cur(row: usize, col: usize) -> Cursor {
        Cursor { row, col }
    }

    fn raw(text: &str, range: Option<ReplaceRange>) -> InlineCompletionRaw {
        InlineCompletionRaw {
            text: text.to_string(),
            range,
        }
    }

    #[test]
    fn strip_returns_text_verbatim_when_no_range() {
        let r = raw("hello", None);
        let lines = vec!["abc".to_string()];
        assert_eq!(
            strip_already_typed(&r, cur(0, 3), &lines).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn strip_removes_already_typed_prefix() {
        let r = raw(
            "fn hello() {}",
            Some(ReplaceRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 8,
            }),
        );
        let lines = vec!["fn hello".to_string()];
        assert_eq!(
            strip_already_typed(&r, cur(0, 8), &lines).as_deref(),
            Some("() {}")
        );
    }

    #[test]
    fn strip_returns_none_when_buffer_diverges_from_insert_text() {
        let r = raw(
            "let x = 1;",
            Some(ReplaceRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 5,
            }),
        );
        // Buffer says "const" but suggestion starts with "let x" — the
        // model expected a different prefix. Don't paint a misaligned
        // ghost — caller will dismiss.
        let lines = vec!["const".to_string()];
        assert!(strip_already_typed(&r, cur(0, 5), &lines).is_none());
    }

    #[test]
    fn strip_falls_back_to_verbatim_for_multi_line_ranges() {
        let r = raw(
            "foo",
            Some(ReplaceRange {
                start_line: 0,
                start_character: 0,
                end_line: 1,
                end_character: 0,
            }),
        );
        let lines = vec!["x".to_string(), "y".to_string()];
        assert_eq!(
            strip_already_typed(&r, cur(1, 0), &lines).as_deref(),
            Some("foo")
        );
    }

    #[test]
    fn strip_falls_back_when_range_end_isnt_at_anchor() {
        let r = raw(
            "abcdef",
            Some(ReplaceRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 3,
            }),
        );
        let lines = vec!["xyz".to_string()];
        // anchor (col 5) ≠ range.end (col 3) → use verbatim.
        assert_eq!(
            strip_already_typed(&r, cur(0, 5), &lines).as_deref(),
            Some("abcdef")
        );
    }

    #[test]
    fn strip_handles_empty_prefix() {
        let r = raw(
            "hello",
            Some(ReplaceRange {
                start_line: 0,
                start_character: 5,
                end_line: 0,
                end_character: 5,
            }),
        );
        let lines = vec!["abcde".to_string()];
        assert_eq!(
            strip_already_typed(&r, cur(0, 5), &lines).as_deref(),
            Some("hello")
        );
    }
}
