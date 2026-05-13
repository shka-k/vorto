//! `Cmd` — discrete app-level state changes produced by command evaluation.
//!
//! The input pipeline is:
//!
//! 1. `tokenize` / `classify` — `KeyEvent` → `Expr` (pure parsing).
//! 2. `handle_expr` — `Expr` → buffer mutations + `Vec<Cmd>` for everything
//!    beyond the buffer (mode, status, LSP, IO, prompt, ...).
//! 3. `App::run_cmds` — apply the cmds to the rest of the app state.
//!
//! Pure buffer ops are *not* listed here — they're applied directly inside
//! `handle_expr` against `&mut Buffer`, which already exposes a clean
//! mutation surface. Wrapping every cursor move and yank in a Cmd variant
//! would balloon the enum without buying anything; the split is drawn at
//! the buffer boundary instead.

// Suppressed while the refactor is in flight — every variant lands a
// consumer in the next two steps (handle.rs and runtime.rs).
#![allow(dead_code)]

use std::path::PathBuf;

use crate::action::PromptKind;
use crate::app::LastFind;
use crate::lsp::Location;
use crate::mode::Mode;

/// Viewport anchor for `zz` / `zt` / `zb`. Mirrors the enum that used to
/// live in `eval.rs`; promoted here so `handle_expr` can hand it off.
#[derive(Debug, Clone, Copy)]
pub enum ScrollAnchor {
    Top,
    Center,
    Bottom,
}

/// A single non-buffer state change. One `Expr` may produce several
/// `Cmd`s; the runtime applies them in order.
#[derive(Debug)]
pub enum Cmd {
    // ── App state ────────────────────────────────────────────
    EnterMode(Mode),
    StatusInfo(String),
    StatusError(String),

    // ── Prompt / picker ──────────────────────────────────────
    OpenPrompt(PromptKind),
    OpenRenamePrompt,
    OpenLocationsPicker {
        items: Vec<String>,
        locations: Vec<Location>,
    },

    // ── Search state ─────────────────────────────────────────
    SetSearch {
        pattern: String,
        forward: bool,
    },
    JumpSearch {
        forward: bool,
    },
    SetLastFind(LastFind),

    // ── Viewport ─────────────────────────────────────────────
    Scroll(ScrollAnchor),

    // ── File / LSP ───────────────────────────────────────────
    /// `:w` / `:w <path>` — persist the buffer to disk.
    Save {
        path: Option<PathBuf>,
    },
    /// Tell the LSP server the buffer is now on disk (`didSave`).
    NotifyLspSave,
    /// `:e <path>` — switch the active buffer to a file.
    OpenPath(PathBuf),
    /// `gd` / `gD` / `gi` — send a definition-shaped request.
    LspJump {
        method: &'static str,
        label: &'static str,
    },
    /// `gr` — `textDocument/references`.
    LspFindReferences,
    /// `<space>r` follow-up after the user typed the new name.
    LspRename(String),

    // ── Multi-buffer / lifecycle ─────────────────────────────
    BufferCycle {
        forward: bool,
    },
    BufferDelete {
        force: bool,
    },
    /// `:q` (force=false) or `:q!` (force=true).
    Quit {
        force: bool,
    },
}
