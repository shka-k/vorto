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
    },
    CommandBind {
        name: "q!",
        aliases: &["quit!"],
        description: "force quit",
        kind: DirectKind::QuitForce,
    },
    CommandBind {
        name: "w",
        aliases: &["write"],
        description: "save (or :w <path>)",
        kind: DirectKind::Save,
    },
    CommandBind {
        name: "wq",
        aliases: &["x"],
        description: "save & quit",
        kind: DirectKind::SaveAndQuit,
    },
    CommandBind {
        name: "e",
        aliases: &["edit"],
        description: "open <path>",
        kind: DirectKind::Open,
    },
    CommandBind {
        name: "bn",
        aliases: &["bnext"],
        description: "next buffer",
        kind: DirectKind::BufferNext,
    },
    CommandBind {
        name: "bp",
        aliases: &["bprev"],
        description: "previous buffer",
        kind: DirectKind::BufferPrev,
    },
    CommandBind {
        name: "bd",
        aliases: &["bdelete", "bc"],
        description: "delete buffer",
        kind: DirectKind::BufferDelete,
    },
    CommandBind {
        name: "bd!",
        aliases: &["bdelete!", "bc!"],
        description: "force delete buffer",
        kind: DirectKind::BufferDeleteForce,
    },
    CommandBind {
        name: "bls",
        aliases: &["buffers"],
        description: "buffer picker",
        kind: DirectKind::BufferList,
    },
    CommandBind {
        name: "goto",
        aliases: &[],
        description: "go to line <n>",
        kind: DirectKind::GotoLine,
    },
    CommandBind {
        name: "noh",
        aliases: &["nohl", "nohlsearch"],
        description: "clear search highlight",
        kind: DirectKind::ClearSearch,
    },
];
