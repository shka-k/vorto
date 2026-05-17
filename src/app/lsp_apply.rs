//! LSP response side: the central `handle_lsp_event` dispatcher and
//! the `apply_*_outcome` methods that turn each [`LspEventOutcome`]
//! into UI side effects (buffer edits, status messages, opening
//! pickers).
//!
//! The matching outgoing request methods live in
//! [`super::lsp_request`].

use std::path::Path;

use crate::editor::Cursor;
use crate::lsp::{
    self, CodeAction, CompletionItem, Diagnostic, Hover, Location, LspEvent, SignatureHelp,
    WorkspaceEdit,
};

use super::SignatureState;
use super::completion::{CompletionState, prefix_slice};
use super::{App, LspEventOutcome, Toast, root_cause};

impl App {
    /// Apply an event from an LSP reader thread. Diagnostics replace
    /// whatever we had stored for that URI; messages are surfaced as
    /// non-error status; reader errors do the same.
    pub fn handle_lsp_event(&mut self, ev: LspEvent) {
        match self.lsp.handle_event(ev) {
            LspEventOutcome::Nothing => {}
            LspEventOutcome::InfoMessage(s) => self.push_toast(Toast::info(s)),
            LspEventOutcome::ErrorMessage(s) => self.push_toast(Toast::error(s)),
            LspEventOutcome::Jump { label, locations } => self.apply_jump_outcome(label, locations),
            LspEventOutcome::References(locations) => self.apply_references_outcome(locations),
            LspEventOutcome::Rename { new_name, edit } => self.apply_rename_outcome(new_name, edit),
            LspEventOutcome::CodeActions(actions) => self.apply_code_actions_outcome(actions),
            LspEventOutcome::CodeActionResolved(action) => {
                self.apply_code_action_resolved_outcome(action)
            }
            LspEventOutcome::Hover(hover) => self.apply_hover_outcome(hover),
            LspEventOutcome::Completion {
                prefix_start,
                items,
            } => self.apply_completion_outcome(prefix_start, items),
            LspEventOutcome::CompletionResolved {
                uri,
                item_index,
                item,
            } => self.apply_completion_resolved_outcome(uri, item_index, item),
            LspEventOutcome::SignatureHelp { anchor_row, help } => {
                self.apply_signature_help_outcome(anchor_row, help)
            }
        }
    }

    /// Apply a `textDocument/signatureHelp` response. Stale responses
    /// (cursor has crossed rows since the request fired) are dropped;
    /// `None` help means "no longer in a callable context" and closes
    /// any open popup. Otherwise the popup is opened or refreshed in
    /// place.
    fn apply_signature_help_outcome(&mut self, anchor_row: usize, help: Option<SignatureHelp>) {
        if self.buffer.cursor.row != anchor_row {
            return;
        }
        match help {
            None => {
                self.signature = None;
            }
            Some(help) => {
                self.signature = Some(SignatureState { help });
            }
        }
    }

    fn apply_completion_outcome(&mut self, prefix_start: Cursor, items: Vec<CompletionItem>) {
        // Only honor responses that are still relevant to where the
        // cursor actually is. Row changes always invalidate; on the
        // same row we tolerate the cursor having moved further right
        // (the user kept typing) but bail when they've backspaced past
        // the start.
        let cursor = self.buffer.cursor;
        if cursor.row != prefix_start.row || cursor.col < prefix_start.col {
            return;
        }
        if items.is_empty() {
            self.completion = None;
            return;
        }
        let line = &self.buffer.lines[cursor.row];
        let prefix = prefix_slice(line, prefix_start.col, cursor.col);
        let state = CompletionState::new(prefix_start, items, &prefix);
        if state.is_empty() {
            // Server returned items but none match the live prefix.
            // Stay silent — auto-trigger fires on every identifier
            // keystroke, and a "no completions" toast each time would
            // be intolerable.
            self.completion = None;
            return;
        }
        self.completion = Some(state);
        // No resolve fires here — the popup opens in preview mode with
        // no row selected, and detail/documentation only matter once
        // the user Tabs to commit to picking. `handle_completion_key`
        // calls `resolve_current_completion_for_detail` after the first
        // nav keypress flips `selecting` to true.
    }

    /// Apply a `completionItem/resolve` response. Two call sites, one
    /// handler:
    ///
    /// - **Popup-display resolve** (`item_index = Some(idx)`): the user
    ///   is still scrolling through the popup and we issued resolve to
    ///   pull deferred `detail` / `documentation` for the row at `idx`.
    ///   Merge the response into `CompletionState.items[idx]` so the
    ///   right column updates in place. Validates the popup hasn't been
    ///   replaced by checking that the slot's `label` still matches the
    ///   resolved item.
    /// - **Accept-time resolve** (`item_index = None`): the user already
    ///   accepted; the primary insertion has been applied; this response
    ///   carries the `additionalTextEdits` (auto-imports) the server
    ///   deferred until acceptance. Same buffer-edit logic as before.
    ///
    /// In both paths a buffer-URI mismatch (user switched files in the
    /// meantime) is a silent drop — applying imports or detail to the
    /// wrong file would be worse than skipping.
    fn apply_completion_resolved_outcome(
        &mut self,
        uri: String,
        item_index: Option<usize>,
        item: Option<CompletionItem>,
    ) {
        let Some(current) = self.lsp.current_uri() else {
            return;
        };
        if current != uri {
            return;
        }
        let Some(resolved) = item else {
            // Parse failed or server returned null. For the popup path
            // still mark the slot resolved so we don't keep retrying.
            if let (Some(idx), Some(state)) = (item_index, self.completion.as_mut())
                && let Some(flag) = state.resolved.get_mut(idx)
            {
                *flag = true;
            }
            return;
        };
        if let Some(idx) = item_index {
            // Popup path. The state may have been replaced by a fresh
            // completion response since the resolve fired; require the
            // labels to still match before mutating.
            if let Some(state) = self.completion.as_mut()
                && let Some(slot) = state.items.get(idx)
                && slot.label == resolved.label
            {
                state.merge_resolved(idx, &resolved);
            }
            return;
        }
        // Accept-time path: apply additional_text_edits.
        let edits = resolved.additional_text_edits;
        if edits.is_empty() {
            return;
        }
        let cursor_row = self.buffer.cursor.row;
        let row_shift: i64 = edits
            .iter()
            .filter(|e| (e.range.start.line as usize) < cursor_row)
            .map(|e| {
                let added = e.new_text.matches('\n').count() as i64;
                let removed = (e.range.end.line - e.range.start.line) as i64;
                added - removed
            })
            .sum();
        self.buffer.snapshot();
        let mut lines = std::mem::take(&mut self.buffer.lines);
        lsp::apply_text_edits(&mut lines, edits);
        self.buffer.lines = lines;
        let new_row = (cursor_row as i64 + row_shift).max(0) as usize;
        let last = self.buffer.lines.len().saturating_sub(1);
        self.buffer.cursor.row = new_row.min(last);
        self.buffer.bump_version();
        self.buffer.dirty = true;
    }

    fn apply_jump_outcome(&mut self, label: &'static str, locations: Vec<Location>) {
        let Some(first) = locations.into_iter().next() else {
            self.push_toast(Toast::info(format!("no {}", label)));
            return;
        };
        if let Err(e) = self.jump_to_location(&first) {
            self.push_toast(Toast::error(format!("jump: {}", root_cause(&e))));
        }
    }

    fn apply_references_outcome(&mut self, locations: Vec<Location>) {
        if locations.is_empty() {
            self.push_toast(Toast::info("no references"));
            return;
        }
        if locations.len() == 1 {
            if let Err(e) = self.jump_to_location(&locations[0]) {
                self.push_toast(Toast::error(format!("jump: {}", root_cause(&e))));
            }
            return;
        }
        let items: Vec<String> = locations
            .iter()
            .map(|loc| format_location_label(loc, &self.startup_cwd))
            .collect();
        self.prompt.open_locations(items, locations);
    }

    fn apply_code_actions_outcome(&mut self, actions: Vec<CodeAction>) {
        if actions.is_empty() {
            self.push_toast(Toast::info("no code actions"));
            return;
        }
        self.prompt.open_code_actions(actions);
    }

    fn apply_code_action_resolved_outcome(&mut self, action: Option<CodeAction>) {
        let Some(action) = action else {
            self.push_toast(Toast::error("code action: server returned no action"));
            return;
        };
        self.apply_code_action(action);
    }

    fn apply_rename_outcome(&mut self, new_name: String, edit: Option<WorkspaceEdit>) {
        let Some(edit) = edit else {
            self.push_toast(Toast::info("rename: nothing to change"));
            return;
        };
        match self.lsp.apply_workspace_edit(edit) {
            Ok(result) => {
                if !result.current_buffer_edits.is_empty() {
                    self.buffer.snapshot();
                    let mut lines = std::mem::take(&mut self.buffer.lines);
                    lsp::apply_text_edits(&mut lines, result.current_buffer_edits);
                    self.buffer.lines = lines;
                    self.buffer.bump_version();
                    self.buffer.dirty = true;
                }
                self.push_toast(Toast::info(format!(
                    "renamed to {} ({} occurrences in {} files)",
                    new_name, result.total_edits, result.files_touched
                )));
            }
            Err(e) => {
                self.push_toast(Toast::error(format!("rename: {}", root_cause(&e))));
            }
        }
    }

    fn apply_hover_outcome(&mut self, hover: Option<Hover>) {
        let Some(h) = hover else {
            self.push_toast(Toast::info("no hover info"));
            return;
        };
        self.prompt.open_hover(h.contents);
    }

    /// Apply a code action's workspace edit. Shared between
    /// `submit_code_action` (when the action arrived already-resolved)
    /// and `apply_code_action_resolved_outcome` (after a `resolve`
    /// round-trip).
    pub(super) fn apply_code_action(&mut self, action: CodeAction) {
        let title = action.title.clone();
        let Some(edit) = action.edit else {
            self.push_toast(Toast::info(format!("code action: {} (no edit)", title)));
            return;
        };
        match self.lsp.apply_workspace_edit(edit) {
            Ok(result) => {
                if !result.current_buffer_edits.is_empty() {
                    self.buffer.snapshot();
                    let mut lines = std::mem::take(&mut self.buffer.lines);
                    lsp::apply_text_edits(&mut lines, result.current_buffer_edits);
                    self.buffer.lines = lines;
                    self.buffer.bump_version();
                    self.buffer.dirty = true;
                }
                self.push_toast(Toast::info(format!(
                    "{} ({} edits in {} files)",
                    title, result.total_edits, result.files_touched
                )));
            }
            Err(e) => {
                self.push_toast(Toast::error(format!("code action: {}", root_cause(&e))));
            }
        }
    }

    /// Diagnostics for the current buffer's URI, if any. Convenience for
    /// the UI layer. Merged across every attached LSP server, so the
    /// status bar / gutter show findings from all of them at once.
    pub fn current_diagnostics(&self) -> Option<Vec<Diagnostic>> {
        self.lsp.current_diagnostics()
    }

    /// Workspace-wide diagnostics — see
    /// [`crate::app::lsp_coordinator::LspCoordinator::all_diagnostics`]
    /// for the merge / ordering contract.
    pub fn all_diagnostics(&self) -> Vec<(String, Vec<Diagnostic>)> {
        self.lsp.all_diagnostics()
    }
}

/// Render a `path:line:col` label for an LSP `Location`. Used to
/// populate the references picker. Falls back to the URI when the path
/// can't be made relative.
fn format_location_label(loc: &Location, root: &Path) -> String {
    let path = match lsp::uri_to_path(&loc.uri) {
        Some(p) => p,
        None => return loc.uri.clone(),
    };
    // Canonicalize both sides so symlinked or /private-prefixed paths
    // still compare equal — otherwise nothing strips and every label
    // shows an absolute path.
    let path_c = path.canonicalize().unwrap_or_else(|_| path.clone());
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let shown = path_c
        .strip_prefix(&root_c)
        .unwrap_or(&path_c)
        .to_string_lossy()
        .into_owned();
    format!(
        "{}:{}:{}",
        shown,
        loc.range.start.line + 1,
        loc.range.start.character + 1
    )
}
