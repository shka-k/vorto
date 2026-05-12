use crate::fuzzy::FuzzyKind;
use crate::mode::Mode;

/// A semantic command that the editor can execute. Bind tables (keymap and
/// command-line) map their inputs to one of these. Runtime arguments
/// (`:w <path>`, leading counts like `10j`) are NOT carried in the action —
/// they flow through `Ctx` at dispatch time, so the action enum stays
/// `Copy` and can live in `const` bind tables.
#[derive(Debug, Clone, Copy)]
pub enum Action {
    Buffer(BufferAction),
    Workspace(WorkspaceAction),
    EnterMode(Mode),
    OpenPrompt(PromptKind),

    /// Clean quit (refuses when the buffer is dirty).
    Quit,
    /// Force quit, ignoring unsaved changes.
    QuitForce,
    /// Save the current buffer and quit.
    SaveAndQuit,

    /// `o` — insert blank line below + enter Insert mode.
    OpenLineBelow,
    /// `O` — insert blank line above + enter Insert mode.
    OpenLineAbove,
}

#[derive(Debug, Clone, Copy)]
pub enum BufferAction {
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveLineStart,
    MoveLineEnd,
    MoveFileStart,
    MoveFileEnd,
    MoveWordForward,
    MoveWordBackward,
    InsertNewline,
    DeleteCharUnderCursor,
    DeleteCharBefore,
    DeleteLine,
    Yank,
    Paste,
    Undo,
    SearchNext,
    SearchPrev,
}

/// Workspace-level I/O. Path arguments are not part of the variant — the
/// handler reads `ctx.rest` to decide between "save (current path)" and
/// "save-as (rest)" or to require a path for `Open`.
#[derive(Debug, Clone, Copy)]
pub enum WorkspaceAction {
    Save,
    Open,
}

#[derive(Debug, Clone, Copy)]
pub enum PromptKind {
    Command,
    Search { forward: bool },
    Fuzzy(FuzzyKind),
}

/// Context passed to every action at dispatch time. Bridges runtime input
/// (command-line rest, future count prefix) into action handlers without
/// the bind tables having to know about either.
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    /// Trailing argument from the command line (`:w foo.txt` → "foo.txt").
    /// Empty for key-triggered dispatches.
    pub rest: &'a str,
    /// Repeat count prefix (e.g. `10j` → 10). Defaults to 1 when absent.
    /// Currently always 1 — present for future count support.
    #[allow(dead_code)]
    pub count: u32,
}

impl<'a> Ctx<'a> {
    pub fn with_rest(rest: &'a str) -> Self {
        Self { rest, count: 1 }
    }
}

impl Default for Ctx<'_> {
    fn default() -> Self {
        Self { rest: "", count: 1 }
    }
}
