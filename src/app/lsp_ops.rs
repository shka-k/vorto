//! LSP-facing actions: jump-style requests, rename, references, and
//! the bridge between [`LspEventOutcome`] from the coordinator and the
//! UI side effects each outcome implies (buffer edits, status messages,
//! opening pickers).

use std::path::Path;

use anyhow::{Result, anyhow};

use crate::lsp::{self, CodeAction, Diagnostic, Location, LspEvent, LspEventOutcome, WorkspaceEdit};

use super::{App, Status, root_cause};

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
        }
    }

    /// Diagnostics for the current buffer's URI, if any. Convenience for
    /// the UI layer.
    pub fn current_diagnostics(&self) -> Option<&[Diagnostic]> {
        self.lsp.current_diagnostics()
    }

    /// First diagnostic that covers the cursor row, prioritising errors.
    pub fn diagnostic_on_cursor(&self) -> Option<&Diagnostic> {
        self.lsp.diagnostic_on_cursor(self.buffer.cursor.row as u32)
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
