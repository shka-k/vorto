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
    self, CodeAction, CompletionItem, Diagnostic, Hover, Location, LspEvent, TextEdit,
    WorkspaceEdit,
};

use super::completion::{CompletionState, prefix_slice};
use super::{App, LspEventOutcome, Status, root_cause};

impl App {
    /// Apply an event from an LSP reader thread. Diagnostics replace
    /// whatever we had stored for that URI; messages are surfaced as
    /// non-error status; reader errors do the same.
    pub fn handle_lsp_event(&mut self, ev: LspEvent) {
        match self.lsp.handle_event(ev) {
            LspEventOutcome::Nothing => {}
            LspEventOutcome::InfoMessage(s) => self.status = Status::info(s),
            LspEventOutcome::ErrorMessage(s) => self.status = Status::error(s),
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
            LspEventOutcome::CompletionResolved { uri, edits } => {
                self.apply_completion_resolved_outcome(uri, edits)
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
    }

    /// Apply the `additionalTextEdits` that came back on a
    /// `completionItem/resolve` after the user already accepted the
    /// completion. The primary insertion has already been applied to
    /// the buffer; these are the auto-import / `use …;` lines the
    /// server deferred until acceptance.
    ///
    /// Dropped silently when the user has switched buffers since the
    /// resolve request was issued — applying imports to the wrong file
    /// would be worse than skipping them.
    fn apply_completion_resolved_outcome(&mut self, uri: String, edits: Vec<TextEdit>) {
        if edits.is_empty() {
            return;
        }
        let Some(current) = self.lsp.current_uri() else {
            return;
        };
        if current != uri {
            return;
        }
        // Edits above the cursor row shift the cursor down (or up, on
        // deletion); edits at or below leave it where it is. Compute
        // the net shift before applying so we can adjust the cursor.
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
            self.status = Status::info(format!("no {}", label));
            return;
        };
        if let Err(e) = self.jump_to_location(&first) {
            self.status = Status::error(format!("jump: {}", root_cause(&e)));
        }
    }

    fn apply_references_outcome(&mut self, locations: Vec<Location>) {
        if locations.is_empty() {
            self.status = Status::info("no references");
            return;
        }
        if locations.len() == 1 {
            if let Err(e) = self.jump_to_location(&locations[0]) {
                self.status = Status::error(format!("jump: {}", root_cause(&e)));
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
            self.status = Status::info("no code actions");
            return;
        }
        self.prompt.open_code_actions(actions);
    }

    fn apply_code_action_resolved_outcome(&mut self, action: Option<CodeAction>) {
        let Some(action) = action else {
            self.status = Status::error("code action: server returned no action");
            return;
        };
        self.apply_code_action(action);
    }

    fn apply_rename_outcome(&mut self, new_name: String, edit: Option<WorkspaceEdit>) {
        let Some(edit) = edit else {
            self.status = Status::info("rename: nothing to change");
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
                self.status = Status::info(format!(
                    "renamed to {} ({} occurrences in {} files)",
                    new_name, result.total_edits, result.files_touched
                ));
            }
            Err(e) => {
                self.status = Status::error(format!("rename: {}", root_cause(&e)));
            }
        }
    }

    fn apply_hover_outcome(&mut self, hover: Option<Hover>) {
        let Some(h) = hover else {
            self.status = Status::info("no hover info");
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
            self.status = Status::info(format!("code action: {} (no edit)", title));
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
                self.status = Status::info(format!(
                    "{} ({} edits in {} files)",
                    title, result.total_edits, result.files_touched
                ));
            }
            Err(e) => {
                self.status = Status::error(format!("code action: {}", root_cause(&e)));
            }
        }
    }

    /// Diagnostics for the current buffer's URI, if any. Convenience for
    /// the UI layer. Merged across every attached LSP server, so the
    /// status bar / gutter show findings from all of them at once.
    pub fn current_diagnostics(&self) -> Option<Vec<Diagnostic>> {
        self.lsp.current_diagnostics()
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
