//! Apply a `Cmd` stream to `App`.
//!
//! The third stage of the input pipeline: where `handle_expr` produced a
//! list of non-buffer state changes, `run_cmds` actually performs them.
//! Most variants are thin shims over existing `App` helpers
//! (`enter_mode`, `open_prompt`, `buffer_cycle`, the `lsp_*` methods,
//! …); this module is the dispatcher, not the implementer.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use super::eval::word_under_cursor;
use super::{App, Toast, root_cause};
use crate::effect::{Cmd, ScrollAnchor};
use crate::lsp;

/// Upper bound on how long save waits for `textDocument/formatting`
/// before giving up and writing the un-formatted buffer. Generous
/// enough for rust-analyzer's first-format-after-startup; short enough
/// that a wedged server doesn't strand the user.
const LSP_FORMAT_TIMEOUT: Duration = Duration::from_secs(3);

impl App {
    pub(super) fn run_cmds(&mut self, cmds: Vec<Cmd>) -> Result<()> {
        for cmd in cmds {
            self.run_cmd(cmd)?;
        }
        Ok(())
    }

    fn run_cmd(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::EnterMode(m) => self.enter_mode(m),
            Cmd::ToastInfo(s) => self.push_toast(Toast::info(s)),
            Cmd::ToastError(s) => self.push_toast(Toast::error(s)),
            Cmd::OpenPrompt(k) => self.open_prompt(k),
            Cmd::OpenRenamePrompt => self.open_rename_prompt(),
            Cmd::SetSearch { pattern, forward } => self.search.set(pattern, forward),
            Cmd::JumpSearch { reverse } => {
                let forward = self.search.last_forward ^ reverse;
                self.run_jump_search(forward);
            }
            Cmd::SearchSelectMatch { reverse } => {
                let forward = self.search.last_forward ^ reverse;
                self.run_search_select(forward);
            }
            Cmd::SetLastFind(lf) => self.last_find = Some(lf),
            Cmd::Scroll(anchor) => self.run_scroll(anchor),
            Cmd::Save { path, then_quit } => self.run_save(path.as_deref(), then_quit)?,
            Cmd::OpenPath(path) => self.open_path(&path)?,
            Cmd::LspJump { method, label } => self.lsp_jump(method, label),
            Cmd::LspFindReferences => self.lsp_find_references(),
            Cmd::LspCodeAction => self.lsp_code_action(),
            Cmd::LspHover => self.lsp_hover(),
            Cmd::BufferCycle { forward } => self.buffer_cycle(forward)?,
            Cmd::BufferDelete { force } => self.buffer_delete(force)?,
            Cmd::Quit => self.should_quit = true,
            Cmd::StartJumpLabel => self.start_jump_label(),
            Cmd::SelectWholeBuffer => self.run_select_whole_buffer(),
            Cmd::SyncYank => self.sync_yank_to_clipboard(),
        }
        Ok(())
    }

    /// Push the current `Buffer.yank` onto the OS clipboard. Initializes
    /// the `arboard` handle on first use; both init failure and a failed
    /// `set_text` are swallowed silently so that headless / sandboxed
    /// environments don't surface a noisy error on every yank — the
    /// internal register keeps working and `p` paste-in-vorto is
    /// unaffected.
    pub(super) fn sync_yank_to_clipboard(&mut self) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(self.buffer.yank.clone());
        }
    }

    /// `gA` — select every line in the buffer. Sets the visual anchor
    /// at (0, 0) directly rather than going through `enter_mode`, since
    /// the latter only pins the anchor on a Normal→Visual transition
    /// and we want a fresh selection even if we're already in some
    /// visual mode.
    fn run_select_whole_buffer(&mut self) {
        let last = self.buffer.lines.len().saturating_sub(1);
        self.visual_anchor = Some(crate::editor::Cursor { row: 0, col: 0 });
        self.mode = crate::mode::Mode::VisualLine;
        self.buffer.cursor = crate::editor::Cursor { row: last, col: 0 };
    }

    fn run_jump_search(&mut self, forward: bool) {
        if let Some(c) = self.search.find_next(&self.buffer, forward) {
            self.buffer.cursor = c;
        } else {
            self.push_toast(Toast::error("pattern not found"));
        }
    }

    /// Body of `gn` / `gN`. Looks up the next match in the requested
    /// direction; in Normal mode, drop the cursor on the match start
    /// and enter Visual (which pins the anchor there); in Visual,
    /// keep the existing anchor and only extend the active end. Either
    /// way, the cursor lands on the match's last char so the selection
    /// covers the whole match. Shared with Visual-mode key handling.
    pub(super) fn run_search_select(&mut self, forward: bool) {
        let Some((start, end_incl)) =
            self.search.find_match_range(&self.buffer, forward)
        else {
            self.push_toast(Toast::error("pattern not found"));
            return;
        };
        if !self.mode.is_visual() {
            self.buffer.cursor = start;
            self.enter_mode(crate::mode::Mode::Visual);
        }
        self.buffer.cursor = end_incl;
    }

    /// Visual mode's `*` / `#` — extract the word under the cursor,
    /// seed the search state, then jump. The Normal-mode counterpart
    /// goes through `Cmd::SetSearch` + `Cmd::JumpSearch` from
    /// `handle_motion`; visual mode bypasses the Cmd pipeline so this
    /// shim collapses both into one call.
    pub(super) fn search_word_under_cursor(&mut self, forward: bool) {
        let Some(word) = word_under_cursor(&self.buffer) else {
            self.push_toast(Toast::error("no word under cursor"));
            return;
        };
        self.search.set(word, forward);
        self.run_jump_search(forward);
    }

    fn run_scroll(&mut self, anchor: ScrollAnchor) {
        let height = self.buffer.viewport_height.get();
        if height == 0 {
            return;
        }
        let cur = self.buffer.cursor.row;
        let last = self.buffer.lines.len().saturating_sub(1);
        let scroll = match anchor {
            ScrollAnchor::Top => cur,
            ScrollAnchor::Center => cur.saturating_sub(height / 2),
            ScrollAnchor::Bottom => cur + 1 - height.min(cur + 1),
        };
        let max_scroll = last.saturating_sub(height.saturating_sub(1));
        self.buffer.scroll.set(scroll.min(max_scroll));
    }

    /// Persist the active buffer to disk and, when `then_quit`, set
    /// `should_quit` only if the write succeeded. Mirrors the old
    /// `do_save` semantics: a failed save (e.g. `:wq` on a no-name
    /// buffer) surfaces the error and the editor stays open.
    fn run_save(&mut self, path: Option<&Path>, then_quit: bool) -> Result<()> {
        // Format-on-save runs only for in-place saves (not `:w <path>`):
        // for a save-as, the buffer's current language is ambiguous
        // with respect to the new path, and we'd rather avoid surprising
        // the user by rewriting their text right before changing where
        // it lives. In-place saves go through the formatter step,
        // which is no-op when no formatter is configured and no LSP
        // is attached.
        if path.is_none() && self.buffer.path.is_some() {
            self.run_format_on_save();
        }

        let wrote = if let Some(p) = path {
            self.buffer.save_as(p)?;
            self.push_toast(Toast::info(format!("written to {}", p.display())));
            true
        } else if self.buffer.path.is_some() {
            self.buffer.save()?;
            self.push_toast(Toast::info("written"));
            true
        } else {
            self.push_toast(Toast::error("no file name (use :w <path>)"));
            false
        };
        if wrote {
            // Many servers (rust-analyzer in particular) only re-run
            // their full checker on save, so this notify is what makes
            // fresh diagnostics arrive.
            self.run_notify_lsp_save();
            if then_quit {
                self.should_quit = true;
            }
        }
        Ok(())
    }

    /// External formatter > LSP `textDocument/formatting` > no-op.
    /// Errors surface as toasts but never abort the save: the user
    /// asked to save and we'd rather write the un-formatted bytes
    /// than refuse the action. Format failures during save (e.g.
    /// rustfmt rejecting a syntax error) are common enough that
    /// blocking the save would be hostile.
    fn run_format_on_save(&mut self) {
        let eff = self.effective_editor();
        if !eff.format_on_save {
            return;
        }
        let language = self
            .buffer
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(|ext| self.config.languages.by_extension(ext));

        // External formatter wins when configured: it's the user's
        // explicit choice, and the LSP would typically just shell out
        // to the same tool anyway (gopls → gofmt, rust-analyzer →
        // rustfmt).
        if let Some(lang) = language
            && let Some(formatter) = lang.formatter.clone()
        {
            let cwd = self
                .buffer
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.lsp.startup_cwd().to_path_buf());
            let text = self.buffer.lines.join("\n");
            match crate::format::run_external(&formatter, &text, &cwd) {
                Ok(formatted) => self.apply_formatted_text(formatted),
                Err(e) => {
                    self.push_toast(Toast::fatal(format!(
                        "format `{}`: {}",
                        formatter.command,
                        root_cause(&e)
                    )));
                }
            }
            return;
        }

        // Fall through to LSP. `format_first_client` returns Ok(None)
        // when no client is attached — quietly do nothing in that
        // case so saves on plain-text buffers don't surface noise.
        let options = self.formatting_options();
        match self.lsp.format_first_client(options, LSP_FORMAT_TIMEOUT) {
            Ok(Some(edits)) if !edits.is_empty() => self.apply_format_edits(edits),
            Ok(_) => {}
            Err(e) => {
                self.push_toast(Toast::fatal(format!("lsp format: {}", root_cause(&e))));
            }
        }
    }

    /// Replace the buffer's text wholesale with the external
    /// formatter's stdout. Snapshots first so undo lands on the
    /// pre-format state. Cursor is clamped — the formatter typically
    /// only adds/removes whitespace so the row is usually still valid,
    /// but a wholesale rewrite is allowed to break that.
    fn apply_formatted_text(&mut self, formatted: String) {
        let new_lines: Vec<String> = formatted.split('\n').map(|s| s.to_string()).collect();
        let new_lines = if new_lines.is_empty() {
            vec![String::new()]
        } else {
            // External formatters typically end output with a trailing
            // newline, which `split('\n')` turns into a stray empty
            // last element. Drop it so the buffer doesn't grow an
            // extra blank line on every save.
            let mut v = new_lines;
            if v.len() > 1 && v.last().map(|s| s.is_empty()).unwrap_or(false) {
                v.pop();
            }
            v
        };
        if new_lines == self.buffer.lines {
            return;
        }
        self.buffer.snapshot();
        self.buffer.lines = new_lines;
        self.buffer.bump_version();
        self.buffer.dirty = true;
        self.clamp_cursor_to_buffer();
    }

    /// Apply a list of LSP `TextEdit`s to the buffer. Snapshots first
    /// so undo lands on the pre-format state; bumps the version so
    /// the highlighter re-runs against the rewritten text.
    fn apply_format_edits(&mut self, edits: Vec<lsp::TextEdit>) {
        self.buffer.snapshot();
        let mut lines = std::mem::take(&mut self.buffer.lines);
        lsp::apply_text_edits(&mut lines, edits);
        if lines.is_empty() {
            lines.push(String::new());
        }
        self.buffer.lines = lines;
        self.buffer.bump_version();
        self.buffer.dirty = true;
        self.clamp_cursor_to_buffer();
    }

    /// Pin the cursor inside the (possibly shrunken) buffer after a
    /// format rewrite. Conservative: just clamps row/col without
    /// trying to track the cursor's logical position through the
    /// edit — formatters mostly preserve structure, and the user
    /// can scroll back if the cursor lands somewhere unexpected.
    fn clamp_cursor_to_buffer(&mut self) {
        let last_row = self.buffer.lines.len().saturating_sub(1);
        if self.buffer.cursor.row > last_row {
            self.buffer.cursor.row = last_row;
        }
        let row_len = self
            .buffer
            .lines
            .get(self.buffer.cursor.row)
            .map(|s| s.chars().count())
            .unwrap_or(0);
        if self.buffer.cursor.col > row_len {
            self.buffer.cursor.col = row_len;
        }
    }

    /// LSP `FormattingOptions` derived from the buffer's effective
    /// editor settings. Servers honour these to pick tab vs. space
    /// (gopls in particular needs `insertSpaces: false`).
    fn formatting_options(&self) -> serde_json::Value {
        let eff = self.effective_editor();
        serde_json::json!({
            "tabSize": eff.indent_width,
            "insertSpaces": !eff.use_tabs,
            "trimTrailingWhitespace": true,
            "insertFinalNewline": true,
            "trimFinalNewlines": true,
        })
    }

    fn run_notify_lsp_save(&mut self) {
        let text = self.buffer.lines.join("\n");
        if let Err(e) = self.lsp.did_save(&text) {
            self.push_toast(Toast::error(format!("lsp didSave: {}", root_cause(&e))));
        }
    }
}
