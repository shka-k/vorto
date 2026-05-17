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
use super::signature::SignatureTrigger;
use super::{App, Toast, root_cause};
use crate::vlog;

impl App {
    /// Send a request whose result is a list of `Location`s and whose
    /// expected handling is "jump to the first one". Covers
    /// `definition`, `declaration`, and `implementation` — all three
    /// answer with the same shape.
    pub(super) fn lsp_jump(&mut self, method: &str, label: &'static str) {
        if !self.lsp.has_lsp() {
            self.push_toast(Toast::error("no LSP for this buffer"));
            return;
        }
        if let Err(e) = self.lsp.request_jump(method, label, self.buffer.cursor) {
            self.push_toast(Toast::error(format!("lsp {}: {}", method, root_cause(&e))));
        }
    }

    pub(super) fn lsp_find_references(&mut self) {
        if !self.lsp.has_lsp() {
            self.push_toast(Toast::error("no LSP for this buffer"));
            return;
        }
        if let Err(e) = self.lsp.request_references(self.buffer.cursor) {
            self.push_toast(Toast::error(format!("lsp references: {}", root_cause(&e))));
        }
    }

    pub(super) fn open_rename_prompt(&mut self) {
        if !self.lsp.has_lsp() {
            self.push_toast(Toast::error("no LSP for this buffer"));
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
        self.lsp_completion_inner(None);
    }

    /// Like `lsp_completion`, but tags the request with the trigger
    /// character that fired it. rust-analyzer (and others) special-case
    /// path completions when they see `triggerKind: TriggerCharacter` +
    /// `triggerCharacter: ":"`, so we need to forward the char that
    /// actually caused the auto-trigger.
    pub(super) fn lsp_completion_triggered(&mut self, trigger: char) {
        self.lsp_completion_inner(Some(trigger));
    }

    fn lsp_completion_inner(&mut self, trigger: Option<char>) {
        if !self.lsp.has_lsp() {
            self.push_toast(Toast::error("no LSP for this buffer"));
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
        if let Err(e) = self.lsp.request_completion(cursor, prefix_start, trigger) {
            self.push_toast(Toast::error(format!("lsp completion: {}", root_cause(&e))));
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
        let source = item.source.clone();
        let base = item
            .text_edit
            .as_ref()
            .map(|te| te.new_text.clone())
            .or_else(|| item.insert_text.clone())
            .unwrap_or_else(|| item.label.clone());

        // Auto-append `()` for callable kinds (Method=2, Function=3,
        // Constructor=4) when the server's replacement is a bare name —
        // single-line and without an existing paren. Snippet support is
        // disabled at handshake time, so callables come back as the raw
        // identifier; tacking on `()` saves the user a keystroke and
        // matches what other editors do. The cursor lands between the
        // parens so the user can start typing args immediately.
        let kind_is_callable = matches!(item.kind, 2..=4);
        let appended_call =
            kind_is_callable && !base.contains('(') && !base.contains('\n') && !base.is_empty();
        let replacement = if appended_call {
            format!("{}()", base)
        } else {
            base
        };

        self.buffer.snapshot();

        let prefix_start = state.prefix_start;
        let cursor = self.buffer.cursor;
        // Honor the server's `textEdit` start column when it sits
        // before our notion of the prefix start — TypeScript and other
        // servers triggered on `.` return items whose range covers the
        // trigger char itself, with `newText` already including the
        // `.`. Replacing only `[prefix_start..cursor]` (which starts
        // *after* the dot) would leave the dot in place and prepend
        // another from `newText`, producing `..foo`. The end is always
        // the live cursor — the original concern about trusting the
        // server's range was about losing post-request keystrokes
        // typed after `range.end`, which only affects the END side.
        let replace_start_col = item
            .text_edit
            .as_ref()
            .filter(|te| te.range.start.line as usize == prefix_start.row)
            .map(|te| (te.range.start.character as usize).min(prefix_start.col))
            .unwrap_or(prefix_start.col);
        let primary = TextEdit {
            range: Range {
                start: Position {
                    line: prefix_start.row as u32,
                    character: replace_start_col as u32,
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
            let end = replace_start_col + replacement.chars().count();
            // When we auto-appended `()`, drop the cursor between the
            // parens so the user can start typing args.
            if appended_call { end - 1 } else { end }
        } else {
            // Multi-line replacement: cursor lands at the end of the
            // last inserted line.
            replacement
                .rsplit('\n')
                .next()
                .unwrap_or("")
                .chars()
                .count()
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
            // `None` index: this resolve is for fetching auto-import
            // edits after the user already accepted the item; the popup
            // is already closed and there's no item slot to refresh.
            let _ = self.lsp.request_completion_resolve(raw, &source, None);
        }

        // When we auto-appended `()` for a callable, the cursor now
        // sits between the parens — the natural place to start typing
        // arguments. The `(` was inserted by us, not the user, so the
        // trigger-character path in insert mode won't fire; we have to
        // request signature help explicitly here.
        if appended_call {
            self.lsp_signature_help(SignatureTrigger::Invoked);
        }
    }

    pub(super) fn cancel_completion(&mut self) {
        self.completion = None;
    }

    /// Fire `textDocument/signatureHelp` at the current cursor.
    /// `trigger` distinguishes a fresh open (`TriggerCharacter` / first
    /// `Invoked`) from a per-keystroke refresh (`ContentChange`) so the
    /// server can branch its bookkeeping.
    ///
    /// We flush pending edits first — the cursor position the server
    /// resolves against has to match the live buffer, same reason
    /// `lsp_completion` does the up-front sync.
    pub(super) fn lsp_signature_help(&mut self, trigger: SignatureTrigger) {
        if !self.lsp.has_lsp() {
            return;
        }
        self.sync_buffer_if_dirty();
        let cursor = self.buffer.cursor;
        let active = self.signature.as_ref().map(|s| &s.help);
        if let Err(e) = self.lsp.request_signature_help(cursor, trigger, active) {
            self.push_toast(Toast::error(format!(
                "lsp signatureHelp: {}",
                root_cause(&e)
            )));
        }
    }

    pub(super) fn cancel_signature_help(&mut self) {
        self.signature = None;
    }

    /// Issue `completionItem/resolve` for the currently-selected popup
    /// row when we haven't already resolved it. Lets us pull deferred
    /// `detail` / `documentation` into the popup while the user is
    /// still scrolling — without this, servers that defer those fields
    /// (typescript-language-server, pyright, rust-analyzer with
    /// `resolveSupport`) leave the right column blank until acceptance.
    /// No-op when the popup is closed, the item is already resolved,
    /// or no LSP client is attached.
    pub(super) fn resolve_current_completion_for_detail(&mut self) {
        let Some(state) = self.completion.as_ref() else {
            return;
        };
        // Preview-mode popups don't fire resolve — there's no row the
        // user has committed to, so spending a round-trip on `selected`
        // (which is the placeholder default, not a chosen item) would
        // be wasted both at the server and in the side detail popup
        // (which is hidden until selecting mode).
        if !state.selecting {
            return;
        }
        let Some(idx) = state.current_index() else {
            return;
        };
        if state.resolved.get(idx).copied().unwrap_or(true) {
            return;
        }
        let Some(item) = state.items.get(idx) else {
            return;
        };
        if !self.lsp.has_lsp() {
            return;
        }
        let raw = item.raw.clone();
        let source = item.source.clone();
        let _ = self.lsp.request_completion_resolve(raw, &source, Some(idx));
    }

    pub(super) fn lsp_hover(&mut self) {
        if !self.lsp.has_lsp() {
            self.push_toast(Toast::error("no LSP for this buffer"));
            return;
        }
        if let Err(e) = self.lsp.request_hover(self.buffer.cursor) {
            self.push_toast(Toast::error(format!("lsp hover: {}", root_cause(&e))));
        }
    }

    pub(super) fn lsp_code_action(&mut self) {
        if !self.lsp.has_lsp() {
            self.push_toast(Toast::error("no LSP for this buffer"));
            return;
        }
        let cursor = self.buffer.cursor;
        // Diagnostics borrow ends before the mutable `request_code_action`
        // call, but the borrow checker can't prove that across `self`, so
        // collect into an owned Vec first.
        let diagnostics: Vec<Diagnostic> = self.lsp.current_diagnostics().unwrap_or_default();
        if let Err(e) = self.lsp.request_code_action(cursor, &diagnostics) {
            self.push_toast(Toast::error(format!("lsp codeAction: {}", root_cause(&e))));
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
            self.push_toast(Toast::error("no LSP for this buffer"));
            return;
        }
        let source = action.source.clone();
        if let Err(e) = self.lsp.request_code_action_resolve(action.raw, &source) {
            self.push_toast(Toast::error(format!(
                "lsp codeAction/resolve: {}",
                root_cause(&e)
            )));
        }
    }

    pub(super) fn submit_rename(&mut self, new_name: String) {
        if new_name.is_empty() {
            self.push_toast(Toast::error("rename: empty name"));
            return;
        }
        if !self.lsp.has_lsp() {
            self.push_toast(Toast::error("no LSP for this buffer"));
            return;
        }
        if let Err(e) = self.lsp.request_rename(new_name, self.buffer.cursor) {
            self.push_toast(Toast::error(format!("lsp rename: {}", root_cause(&e))));
        }
    }

    /// Sync the active buffer to every language server / Copilot that
    /// hasn't seen the latest content. Called from the main loop after
    /// every key handled; pays for the `lines.join` snapshot only when
    /// at least one consumer needs it.
    pub fn sync_buffer_if_dirty(&mut self) {
        let needs_lsp = self.buffer.version != self.lsp.last_synced_version();
        let needs_copilot = self.copilot_needs_sync();
        if !needs_lsp && !needs_copilot {
            return;
        }
        let text = self.buffer.lines.join("\n");
        if needs_lsp {
            self.lsp.set_last_synced_version(self.buffer.version);
            if let Err(e) = self.lsp.did_change(&text) {
                // Background sync after every keystroke — toasting on
                // each failure would flood the screen. Log only; if the
                // client is wedged, subsequent user-initiated requests
                // will surface their own errors.
                vlog!("lsp didChange failed: {:#}", e);
            }
        }
        if needs_copilot {
            self.sync_buffer_to_copilot(&text);
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
