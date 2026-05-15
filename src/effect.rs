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

use std::path::PathBuf;

use crate::action::{LastFind, PromptKind};
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

    // ── Search state ─────────────────────────────────────────
    SetSearch {
        pattern: String,
        forward: bool,
    },
    /// Jump to the next match of the current search. `reverse` flips
    /// the stored direction (so `N` becomes `JumpSearch { reverse:
    /// true }` against a forward search). Direction is resolved by the
    /// runtime against `App::search.last_forward` — keeping that read
    /// off the handler keeps `handle_expr` from touching non-buffer
    /// `App` state.
    JumpSearch {
        reverse: bool,
    },
    /// `gn` / `gN` — find the next/previous match of the current
    /// search pattern, jump the cursor to its start, enter Visual,
    /// then extend the cursor to the end of the match. `reverse`
    /// flips against the stored search direction, mirroring
    /// `JumpSearch`'s semantics.
    SearchSelectMatch {
        reverse: bool,
    },
    SetLastFind(LastFind),

    // ── Viewport ─────────────────────────────────────────────
    Scroll(ScrollAnchor),

    // ── File / LSP ───────────────────────────────────────────
    /// `:w` / `:w <path>` — persist the buffer to disk. When
    /// `then_quit` is set, the runtime quits after a successful
    /// write (`:wq` / `:x`); a failed save (e.g. no file name)
    /// surfaces the error and the editor stays open.
    Save {
        path: Option<PathBuf>,
        then_quit: bool,
    },
    /// `:e <path>` — switch the active buffer to a file.
    OpenPath(PathBuf),
    /// `gd` / `gD` / `gi` — send a definition-shaped request.
    LspJump {
        method: &'static str,
        label: &'static str,
    },
    /// `gr` — `textDocument/references`.
    LspFindReferences,
    /// `<space>a` — `textDocument/codeAction` at the cursor.
    LspCodeAction,
    /// `K` — `textDocument/hover` for the symbol under the cursor.
    LspHover,

    // ── Multi-buffer / lifecycle ─────────────────────────────
    BufferCycle {
        forward: bool,
    },
    BufferDelete {
        force: bool,
    },
    /// Tear-down — emitted by `:q` (after the dirty-buffer check has
    /// already cleared) and `:q!`. The runtime sets `should_quit`
    /// either way; the variant exists in its own right so the input
    /// pipeline can log "what was requested" rather than "what got
    /// set".
    Quit,

    // ── Jump-label overlay ───────────────────────────────────
    /// `gw` — enter 2-char label jump mode over the current viewport.
    /// The runtime computes targets (depends on the viewport metrics
    /// that `App::buffer` carries) and seeds `App::jump_state`.
    StartJumpLabel,

    // ── Selection ────────────────────────────────────────────
    /// `gA` — select the whole buffer. Pins the visual anchor at
    /// (0, 0), enters Visual-line, lands the cursor on the last row.
    SelectWholeBuffer,

    // ── Clipboard ────────────────────────────────────────────
    /// Push the current `Buffer.yank` to the OS clipboard. Emitted
    /// alongside each yank so `p` works inside vorto *and* other apps
    /// (browser, another terminal, …) can paste what was just yanked.
    SyncYank,
}
