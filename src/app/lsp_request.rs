//! LSP request side: methods that initiate an LSP round trip on
//! behalf of the user (jump, references, hover, code action, rename,
//! completion) plus the active completion popup's user-input flow
//! (filter / accept / cancel) and the periodic `didChange` sync.
//!
//! The matching response handlers — `apply_*_outcome` and
//! `handle_lsp_event` — live in [`super::lsp_apply`].

use anyhow::Result;

use crate::editor::Cursor;
use crate::lsp::{self, CodeAction, Diagnostic, Position, Range, TextEdit};

use super::completion::{identifier_prefix_start, prefix_slice};
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

    /// Apply the currently-selected completion. The primary replacement
    /// target is always `[prefix_start..cursor]` (in column terms on the
    /// prefix-start row), regardless of what range the server attached
    /// to its `textEdit` — the server's range was computed against the
    /// buffer state at request time, and the user may have kept typing
    /// since (auto-trigger fires the request as you type), so trusting
    /// the server's range would leave the post-request keystrokes
    /// stranded after the inserted completion. The text to insert is
    /// picked in spec order: `textEdit.newText` → `insertText` → `label`.
    ///
    /// `additionalTextEdits` (auto-import / `use` insertions) are
    /// applied in the same batch via `apply_text_edits`. The post-edit
    /// cursor position is adjusted for any line-count shift caused by
    /// additional edits that sit above the cursor row.
    ///
    /// When the item arrived without `additionalTextEdits` we follow up
    /// with `completionItem/resolve`. Servers that opt into the
    /// `resolveSupport` contract (rust-analyzer, JDT.LS, …) defer the
    /// import-line computation to that round trip so they don't have
    /// to do it for every candidate in the popup; the result is
    /// applied asynchronously by `apply_completion_resolved_outcome`.
    pub(super) fn accept_completion(&mut self) {
        let Some(state) = self.completion.take() else {
            return;
        };
        let Some(item) = state.current().cloned() else {
            return;
        };
        let needs_resolve = item.additional_text_edits.is_empty();
        let raw = item.raw.clone();
        let replacement = item
            .text_edit
            .as_ref()
            .map(|te| te.new_text.clone())
            .or_else(|| item.insert_text.clone())
            .unwrap_or_else(|| item.label.clone());

        self.buffer.snapshot();

        let prefix_start = state.prefix_start;
        let cursor = self.buffer.cursor;
        let primary = TextEdit {
            range: Range {
                start: Position {
                    line: prefix_start.row as u32,
                    character: prefix_start.col as u32,
                },
                end: Position {
                    line: cursor.row as u32,
                    character: cursor.col as u32,
                },
            },
            new_text: replacement.clone(),
        };

        // Row shift contributed by auto-import edits that sit above the
        // cursor row — those move the primary edit's landing row down
        // (or up, on deletion). Same-row additional edits are vanishingly
        // rare for imports and would also require column tracking, so
        // we ignore them for the cursor-placement math.
        let row_shift: i64 = item
            .additional_text_edits
            .iter()
            .filter(|e| (e.range.start.line as usize) < prefix_start.row)
            .map(|e| {
                let added = e.new_text.matches('\n').count() as i64;
                let removed = (e.range.end.line - e.range.start.line) as i64;
                added - removed
            })
            .sum();

        let mut all_edits = item.additional_text_edits.clone();
        all_edits.push(primary);
        let mut lines = std::mem::take(&mut self.buffer.lines);
        lsp::apply_text_edits(&mut lines, all_edits);
        self.buffer.lines = lines;

        let replacement_newlines = replacement.matches('\n').count();
        let final_row =
            (prefix_start.row as i64 + row_shift + replacement_newlines as i64).max(0) as usize;
        let final_col = if replacement_newlines == 0 {
            prefix_start.col + replacement.chars().count()
        } else {
            // Multi-line replacement: cursor lands at the end of the
            // last inserted line.
            replacement.rsplit('\n').next().unwrap_or("").chars().count()
        };
        let last = self.buffer.lines.len().saturating_sub(1);
        self.buffer.cursor.row = final_row.min(last);
        self.buffer.cursor.col = final_col;
        self.buffer.bump_version();
        self.buffer.dirty = true;

        // Best-effort follow-up. Servers that don't support resolve
        // either echo the item back unchanged or surface an error — the
        // coordinator drops both into an empty-edit outcome, so the user
        // sees the primary insertion regardless.
        if needs_resolve && self.lsp.has_lsp() {
            let _ = self.lsp.request_completion_resolve(raw);
        }
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
}

impl App {
    /// Open `loc.uri` (switching buffers if needed) and place the cursor
    /// at `loc.range.start`. Used both by jump-style outcomes (incoming)
    /// and by user-driven location-picker selections — kept here as a
    /// `pub(super)` helper so both sides can reach it without
    /// duplicating the open-then-position dance.
    pub(super) fn jump_to_location(&mut self, loc: &crate::lsp::Location) -> Result<()> {
        let path = lsp::uri_to_path(&loc.uri)
            .ok_or_else(|| anyhow::anyhow!("unsupported uri scheme: {}", loc.uri))?;
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
}
