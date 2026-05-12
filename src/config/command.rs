//! `:` command table.
//!
//! Maps the head of a `:`-prompt (`q`, `w`, `wq`, `e`, `goto`, ...) to
//! the [`DirectKind`] the evaluator dispatches on. Pure data — kept
//! out of `app` so the UI can list bindings without depending on the
//! editor state machine.

use crate::action::DirectKind;

pub struct CommandBind {
    pub name: &'static str,
    pub description: &'static str,
    pub kind: DirectKind,
}

impl CommandBind {
    pub fn find(name: &str) -> Option<&'static CommandBind> {
        COMMAND_BINDS.iter().find(|b| b.name == name)
    }
}

pub const COMMAND_BINDS: &[CommandBind] = &[
    CommandBind {
        name: "q",
        description: "quit",
        kind: DirectKind::Quit,
    },
    CommandBind {
        name: "q!",
        description: "force quit",
        kind: DirectKind::QuitForce,
    },
    CommandBind {
        name: "w",
        description: "save (or :w <path>)",
        kind: DirectKind::Save,
    },
    CommandBind {
        name: "wq",
        description: "save & quit",
        kind: DirectKind::SaveAndQuit,
    },
    CommandBind {
        name: "x",
        description: "save & quit",
        kind: DirectKind::SaveAndQuit,
    },
    CommandBind {
        name: "e",
        description: "open <path>",
        kind: DirectKind::Open,
    },
    CommandBind {
        name: "goto",
        description: "go to line <n>",
        kind: DirectKind::GotoLine,
    },
];
