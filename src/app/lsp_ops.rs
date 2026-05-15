//! LSP-facing actions: jump-style requests, rename, references, and
//! the bridge between [`LspEventOutcome`] from the coordinator and the
//! UI side effects each outcome implies (buffer edits, status messages,
//! opening pickers).

use std::path::Path;

use anyhow::{Result, anyhow};

use crate::lsp::{
    self, CodeAction, CompletionItem, Diagnostic, Hover, Location, LspEvent, LspEventOutcome,
    WorkspaceEdit,
};

use super::completion::{CompletionState, identifier_prefix_start, prefix_slice};
use super::{App, Status, root_cause};
use crate::editor::Cursor;

impl App {
    /// Send a request whose result is a list of `Location`s and whose
    /// expected handling is "jump to the first one". Covers
    /// `definition`, `declaration`, and `implementation` — all three
    /// answer with the same shape.
    pub(super) fn lsp_jump(&mut self, method: &str, label: &'static str) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        if let Err(e) = self.lsp.request_jump(method, label, self.buffer.cursor) {
            self.status = Status::error(format!("lsp {}: {}", method, root_cause(&e)));
        }
    }

    pub(super) fn lsp_find_references(&mut self) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        if let Err(e) = self.lsp.request_references(self.buffer.cursor) {
            self.status = Status::error(format!("lsp references: {}", root_cause(&e)));
        }
    }

    pub(super) fn open_rename_prompt(&mut self) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        self.prompt.open_rename();
    }

    /// Trigger a `textDocument/completion` request at the current
    /// cursor. The "prefix start" — where the identifier under the
    /// cursor begins — is snapshotted now so the response can be
    /// matched against the live cursor when it arrives.
    ///
    /// Completion fires from inside `handle_insert_key`, **before** the
    /// main loop's post-keypress `sync_buffer_if_dirty`. Without an
    /// up-front sync the server would resolve the cursor position
    /// against a stale buffer and either return nothing or the wrong
    /// items, so we flush pending edits here first.
    pub(super) fn lsp_completion(&mut self) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        self.sync_buffer_if_dirty();
        let cursor = self.buffer.cursor;
        let line = &self.buffer.lines[cursor.row];
        let start_col = identifier_prefix_start(line, cursor.col);
        let prefix_start = Cursor {
            row: cursor.row,
            col: start_col,
        };
        if let Err(e) = self.lsp.request_completion(cursor, prefix_start) {
            self.status = Status::error(format!("lsp completion: {}", root_cause(&e)));
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

    /// Re-filter the open completion popup against the live prefix.
    /// Called from `handle_insert_key` after every insert / backspace.
    /// Closes the popup when the cursor has left the row or backspaced
    /// past `prefix_start`.
    pub(super) fn update_completion_filter(&mut self) {
        let Some(state) = self.completion.as_mut() else {
            return;
        };
        let cursor = self.buffer.cursor;
        if cursor.row != state.prefix_start.row || cursor.col < state.prefix_start.col {
            self.completion = None;
            return;
        }
        let line = &self.buffer.lines[cursor.row];
        let prefix = prefix_slice(line, state.prefix_start.col, cursor.col);
        state.refilter(&prefix);
        if state.is_empty() {
            self.completion = None;
        }
    }

    /// Apply the currently-selected completion. The replacement target
    /// is always `[prefix_start..cursor]` (in column terms on the
    /// prefix-start row), regardless of what range the server attached
    /// to its `textEdit` — the server's range was computed against the
    /// buffer state at request time, and the user may have kept typing
    /// since (auto-trigger fires the request as you type), so trusting
    /// the server's range would leave the post-request keystrokes
    /// stranded after the inserted completion. The text to insert is
    /// picked in spec order: `textEdit.newText` → `insertText` → `label`.
    pub(super) fn accept_completion(&mut self) {
        let Some(state) = self.completion.take() else {
            return;
        };
        let Some(item) = state.current().cloned() else {
            return;
        };
        let replacement = item
            .text_edit
            .as_ref()
            .map(|te| te.new_text.clone())
            .or_else(|| item.insert_text.clone())
            .unwrap_or_else(|| item.label.clone());

        self.buffer.snapshot();
        let row = state.prefix_start.row;
        let line = &mut self.buffer.lines[row];
        let chars: Vec<char> = line.chars().collect();
        let cursor_col = self.buffer.cursor.col.min(chars.len());
        let start_col = state.prefix_start.col.min(cursor_col);
        let head: String = chars.iter().take(start_col).collect();
        let tail: String = chars.iter().skip(cursor_col).collect();
        // Single-line replacement is the common case; only the textEdit
        // path can carry newlines (and we've disabled snippet support,
        // so multi-line replacements are rare). Handle both shapes here
        // rather than in two diverging branches.
        if !replacement.contains('\n') {
            *line = format!("{}{}{}", head, replacement, tail);
            self.buffer.cursor.col = start_col + replacement.chars().count();
        } else {
            let parts: Vec<&str> = replacement.split('\n').collect();
            let first = parts[0];
            let last = parts[parts.len() - 1];
            let mut new_lines: Vec<String> = Vec::with_capacity(parts.len());
            new_lines.push(format!("{}{}", head, first));
            for mid in &parts[1..parts.len() - 1] {
                new_lines.push((*mid).to_string());
            }
            new_lines.push(format!("{}{}", last, tail));
            self.buffer.lines.splice(row..=row, new_lines);
            self.buffer.cursor.row = row + parts.len() - 1;
            self.buffer.cursor.col = last.chars().count();
        }
        self.buffer.bump_version();
        self.buffer.dirty = true;
    }

    pub(super) fn cancel_completion(&mut self) {
        self.completion = None;
    }

    pub(super) fn lsp_hover(&mut self) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        if let Err(e) = self.lsp.request_hover(self.buffer.cursor) {
            self.status = Status::error(format!("lsp hover: {}", root_cause(&e)));
        }
    }

    pub(super) fn lsp_code_action(&mut self) {
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        let cursor = self.buffer.cursor;
        // Diagnostics borrow ends before the mutable `request_code_action`
        // call, but the borrow checker can't prove that across `self`, so
        // collect into an owned Vec first.
        let diagnostics: Vec<Diagnostic> = self
            .lsp
            .current_diagnostics()
            .map(|d| d.to_vec())
            .unwrap_or_default();
        if let Err(e) = self.lsp.request_code_action(cursor, &diagnostics) {
            self.status = Status::error(format!("lsp codeAction: {}", root_cause(&e)));
        }
    }

    pub(super) fn submit_code_action(&mut self, action: CodeAction) {
        // Already-resolved actions go straight through. Otherwise round
        // trip via `codeAction/resolve` so servers (rust-analyzer in
        // particular) can fill in the heavy `edit` lazily.
        if action.edit.is_some() {
            self.apply_code_action(action);
            return;
        }
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        if let Err(e) = self.lsp.request_code_action_resolve(action.raw) {
            self.status = Status::error(format!("lsp codeAction/resolve: {}", root_cause(&e)));
        }
    }

    fn apply_code_action(&mut self, action: CodeAction) {
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

    pub(super) fn submit_rename(&mut self, new_name: String) {
        if new_name.is_empty() {
            self.status = Status::error("rename: empty name");
            return;
        }
        if !self.lsp.has_lsp() {
            self.status = Status::error("no LSP for this buffer");
            return;
        }
        if let Err(e) = self.lsp.request_rename(new_name, self.buffer.cursor) {
            self.status = Status::error(format!("lsp rename: {}", root_cause(&e)));
        }
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

    pub(super) fn jump_to_location(&mut self, loc: &Location) -> Result<()> {
        let path = lsp::uri_to_path(&loc.uri)
            .ok_or_else(|| anyhow!("unsupported uri scheme: {}", loc.uri))?;
        let need_open = match &self.buffer.path {
            Some(p) => p.canonicalize().ok() != path.canonicalize().ok(),
            None => true,
        };
        if need_open {
            self.open_path(&path)?;
        }
        let row = loc.range.start.line as usize;
        let col = loc.range.start.character as usize;
        let last = self.buffer.lines.len().saturating_sub(1);
        self.buffer.cursor.row = row.min(last);
        self.buffer.cursor.col = col;
        self.buffer.clamp_col(false);
        Ok(())
    }

    /// Send `didChange` if the buffer has been mutated since the last
    /// sync. Called from the main loop after every key handled.
    pub fn sync_buffer_if_dirty(&mut self) {
        if self.buffer.version == self.lsp.last_synced_version() {
            return;
        }
        self.lsp.set_last_synced_version(self.buffer.version);
        let text = self.buffer.lines.join("\n");
        if let Err(e) = self.lsp.did_change(&text) {
            self.status = Status::error(format!("lsp didChange: {}", root_cause(&e)));
        }
    }

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
        }
    }

    fn apply_hover_outcome(&mut self, hover: Option<Hover>) {
        let Some(h) = hover else {
            self.status = Status::info("no hover info");
            return;
        };
        self.prompt.open_hover(h.contents);
    }

    /// Diagnostics for the current buffer's URI, if any. Convenience for
    /// the UI layer.
    pub fn current_diagnostics(&self) -> Option<&[Diagnostic]> {
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
