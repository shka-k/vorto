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
use super::{App, Status, root_cause};
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
            Cmd::StatusInfo(s) => self.status = Status::info(s),
            Cmd::StatusError(s) => self.status = Status::error(s),
            Cmd::OpenPrompt(k) => self.open_prompt(k),
            Cmd::OpenRenamePrompt => self.open_rename_prompt(),
            Cmd::SetSearch { pattern, forward } => self.search.set(pattern, forward),
            Cmd::JumpSearch { reverse } => {
                let forward = self.search.last_forward ^ reverse;
                self.run_jump_search(forward);
            }
            Cmd::SetLastFind(lf) => self.last_find = Some(lf),
            Cmd::Scroll(anchor) => self.run_scroll(anchor),
            Cmd::Save { path, then_quit } => self.run_save(path.as_deref(), then_quit)?,
            Cmd::OpenPath(path) => self.open_path(&path)?,
            Cmd::LspJump { method, label } => self.lsp_jump(method, label),
            Cmd::LspFindReferences => self.lsp_find_references(),
            Cmd::BufferCycle { forward } => self.buffer_cycle(forward)?,
            Cmd::BufferDelete { force } => self.buffer_delete(force)?,
            Cmd::Quit => self.should_quit = true,
        }
        Ok(())
    }

    fn run_jump_search(&mut self, forward: bool) {
        if let Some(c) = self.search.find_next(&self.buffer, forward) {
            self.buffer.cursor = c;
        } else {
            self.status = Status::error("pattern not found");
        }
    }

    /// Visual mode's `*` / `#` — extract the word under the cursor,
    /// seed the search state, then jump. The Normal-mode counterpart
    /// goes through `Cmd::SetSearch` + `Cmd::JumpSearch` from
    /// `handle_motion`; visual mode bypasses the Cmd pipeline so this
    /// shim collapses both into one call.
    pub(super) fn search_word_under_cursor(&mut self, forward: bool) {
        let Some(word) = word_under_cursor(&self.buffer) else {
            self.status = Status::error("no word under cursor");
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
            self.status = Status::info(format!("written to {}", p.display()));
            true
        } else if self.buffer.path.is_some() {
            self.buffer.save()?;
            self.status = Status::info("written");
            true
        } else {
            self.status = Status::error("no file name (use :w <path>)");
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
            self.status = Status::error(format!("lsp didSave: {}", root_cause(&e)));
        }
    }
}
