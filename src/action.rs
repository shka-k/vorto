use std::path::PathBuf;

use crate::fuzzy::FuzzyKind;
use crate::mode::Mode;

#[derive(Debug, Clone)]
pub enum Action {
    Buffer(BufferAction),
    Workspace(WorkspaceAction),
    EnterMode(Mode),
    OpenPrompt(PromptKind),
    Quit,
}

#[derive(Debug, Clone)]
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
    InsertLineBelow,
    InsertLineAbove,
    DeleteCharUnderCursor,
    DeleteCharBefore,
    DeleteLine,
    Yank,
    Paste,
    Undo,
    SearchNext,
    SearchPrev,
}

#[derive(Debug, Clone)]
pub enum WorkspaceAction {
    Save,
    SaveAs(PathBuf),
    Open(PathBuf),
}

#[derive(Debug, Clone)]
pub enum PromptKind {
    Command,
    Search { forward: bool },
    Fuzzy(FuzzyKind),
}
