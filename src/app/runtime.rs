//! Apply a `Cmd` stream to `App`.
//!
//! The third stage of the input pipeline: where `handle_expr` produced a
//! list of non-buffer state changes, `run_cmds` actually performs them.
//! Most variants are thin shims over existing `App` helpers
//! (`enter_mode`, `open_prompt`, `buffer_cycle`, the `lsp_*` methods,
//! …); this module is the dispatcher, not the implementer.

use std::path::Path;

use anyhow::Result;

use super::eval::word_under_cursor;
use super::{App, Toast, root_cause};
use crate::effect::{Cmd, ScrollAnchor};

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
            Cmd::ToastInfo(s) => self.toast = Toast::info(s),
            Cmd::ToastError(s) => self.toast = Toast::error(s),
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
            self.toast = Toast::error("pattern not found");
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
            self.toast = Toast::error("pattern not found");
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
            self.toast = Toast::error("no word under cursor");
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
        let wrote = if let Some(p) = path {
            self.buffer.save_as(p)?;
            self.toast = Toast::info(format!("written to {}", p.display()));
            true
        } else if self.buffer.path.is_some() {
            self.buffer.save()?;
            self.toast = Toast::info("written");
            true
        } else {
            self.toast = Toast::error("no file name (use :w <path>)");
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

    fn run_notify_lsp_save(&mut self) {
        let text = self.buffer.lines.join("\n");
        if let Err(e) = self.lsp.did_save(&text) {
            self.toast = Toast::error(format!("lsp didSave: {}", root_cause(&e)));
        }
    }
}
