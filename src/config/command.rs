//! `:` command table.
//!
//! Maps the head of a `:`-prompt (`q`, `w`, `wq`, `e`, `goto`, ...) to
//! the [`DirectKind`] the evaluator dispatches on. Pure data — kept
//! out of `app` so the UI can list bindings without depending on the
//! editor state machine.

use crate::action::DirectKind;

pub struct CommandBind {
    /// Canonical (primary) name. Shown in the which-key hint panel
    /// and used as the documentation handle.
    pub name: &'static str,
    /// Extra names that resolve to the same `kind`. Same pattern as
    /// [`crate::config::Binding::aliases`] — used by [`Self::find`]
    /// and matched-against (but not displayed) by the hint panel
    /// when the user types a prefix.
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub kind: DirectKind,
    /// True when the command accepts a filesystem path as its
    /// argument. Read by the `:` prompt so Tab completes paths after
    /// `:e ` / `:w `, mirroring shell-style argument completion.
    pub takes_path: bool,
}

impl CommandBind {
    pub fn find(name: &str) -> Option<&'static CommandBind> {
        COMMAND_BINDS
            .iter()
            .find(|b| b.name == name || b.aliases.contains(&name))
    }

    /// Iterator over the primary name followed by each alias. Lets
    /// the hint panel enumerate every typeable form as its own row.
    pub fn all_names(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.name).chain(self.aliases.iter().copied())
    }
}

pub const COMMAND_BINDS: &[CommandBind] = &[
    CommandBind {
        name: "q",
        aliases: &["quit"],
        description: "quit",
        kind: DirectKind::Quit,
        takes_path: false,
    },
    CommandBind {
        name: "q!",
        aliases: &["quit!"],
        description: "force quit",
        kind: DirectKind::QuitForce,
        takes_path: false,
    },
    CommandBind {
        name: "w",
        aliases: &["write"],
        description: "save (or :w <path>)",
        kind: DirectKind::Save,
        takes_path: true,
    },
    CommandBind {
        name: "w!",
        aliases: &["write!"],
        description: "save, creating dirs",
        kind: DirectKind::SaveForce,
        takes_path: true,
    },
    CommandBind {
        name: "wq",
        aliases: &["x"],
        description: "save & quit",
        kind: DirectKind::SaveAndQuit,
        takes_path: true,
    },
    CommandBind {
        name: "e",
        aliases: &["edit"],
        description: "open <path>",
        kind: DirectKind::Open,
        takes_path: true,
    },
    CommandBind {
        name: "bn",
        aliases: &["bnext"],
        description: "next buffer",
        kind: DirectKind::BufferNext,
        takes_path: false,
    },
    CommandBind {
        name: "bp",
        aliases: &["bprev"],
        description: "previous buffer",
        kind: DirectKind::BufferPrev,
        takes_path: false,
    },
    CommandBind {
        name: "bd",
        aliases: &["bdelete"],
        description: "delete buffer",
        kind: DirectKind::BufferDelete,
        takes_path: false,
    },
    CommandBind {
        name: "bd!",
        aliases: &["bdelete!", "bc", "bc!"],
        description: "force delete buffer",
        kind: DirectKind::BufferDeleteForce,
        takes_path: false,
    },
    CommandBind {
        name: "bca",
        aliases: &["bca!"],
        description: "force delete all buffers",
        kind: DirectKind::BufferDeleteAll,
        takes_path: false,
    },
    CommandBind {
        name: "bls",
        aliases: &["buffers"],
        description: "buffer picker",
        kind: DirectKind::BufferList,
        takes_path: false,
    },
    CommandBind {
        name: "new",
        aliases: &["enew"],
        description: "new scratch buffer",
        kind: DirectKind::NewScratchBuffer,
        takes_path: false,
    },
    CommandBind {
        name: "goto",
        aliases: &[],
        description: "go to line <n>",
        kind: DirectKind::GotoLine,
        takes_path: false,
    },
    CommandBind {
        name: "log",
        aliases: &[],
        description: "open debug log file",
        kind: DirectKind::OpenLog,
        takes_path: false,
    },
    CommandBind {
        name: "lsp",
        aliases: &[],
        description: "show LSP for current buffer (:lsp all for every language)",
        kind: DirectKind::LspStatus,
        takes_path: false,
    },
    CommandBind {
        name: "reload",
        aliases: &["e!"],
        description: "reload buffer from disk (undo restores)",
        kind: DirectKind::Reload,
        takes_path: false,
    },
    CommandBind {
        name: "reload-all",
        aliases: &[],
        description: "reload every file-backed buffer",
        kind: DirectKind::ReloadAll,
        takes_path: false,
    },
    CommandBind {
        name: "noh",
        aliases: &["nohl", "nohlsearch"],
        description: "clear search highlight",
        kind: DirectKind::ClearSearch,
        takes_path: false,
    },
    CommandBind {
        name: "split",
        aliases: &["sp", "nh"],
        description: "split pane below",
        kind: DirectKind::SplitWindowHorizontal,
        takes_path: false,
    },
    CommandBind {
        name: "vsplit",
        aliases: &["vsp", "vs", "nv"],
        description: "split pane right",
        kind: DirectKind::SplitWindowVertical,
        takes_path: false,
    },
    CommandBind {
        name: "close",
        aliases: &["clo"],
        description: "close active pane",
        kind: DirectKind::CloseWindow,
        takes_path: false,
    },
    CommandBind {
        name: "only",
        aliases: &["on"],
        description: "(future) close all but active pane",
        kind: DirectKind::CloseWindow,
        takes_path: false,
    },
];
