use crate::fuzzy::FuzzyKind;
use crate::mode::Mode;

// ════════════════════════════════════════════════════════════════════════
// Tokens (syntactic level)
// ════════════════════════════════════════════════════════════════════════
//
// A `Token` is what a single key press resolves to in the current parsing
// context. The token list accumulates until `classify` decides it forms a
// complete vim-style command, at which point `build_expr` turns it into
// the semantic AST (`Expr`).

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Token {
    /// A digit being typed as a count prefix (e.g. `2`, `0` for "20").
    /// Multiple consecutive `Count` tokens combine into one number.
    Count(u32),
    /// Operator pending: `d`, `y`, `c`.
    Op(Operator),
    /// The same operator key pressed again immediately — vim's
    /// `dd` / `yy` "operator on current line" shortcut.
    SelfDouble(Operator),
    /// A motion / cursor-move (h, j, w, $, G, etc.).
    Motion(MotionKind),
    /// A standalone action that fires immediately (i, o, :, /, p, u, …).
    Direct(DirectKind),
    /// Text-object scope marker, only valid after an operator (i / a).
    Scope(Scope),
    /// Text-object body marker, valid after a Scope (w, ", (, …).
    Object(Object),
    /// `<space>` leader — by itself transitions tokenization context but
    /// is otherwise dropped during `build_expr`.
    LeaderPrefix,
    /// `g` prefix — for two-key sequences like `gg` (goto file start).
    GotoPrefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Yank,
    Change,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    WordForward,
    WordBack,
    FileStart,
    FileEnd,
    SearchNext,
    SearchPrev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Inner,
    Around,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    Word,
    DoubleQuote,
    SingleQuote,
    Paren,
    Brace,
    Bracket,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DirectKind {
    EnterMode(Mode),
    OpenPrompt(PromptKind),
    OpenLineBelow,
    OpenLineAbove,
    Paste,
    Undo,
    DeleteCharUnderCursor,
    Quit,
    QuitForce,
    SaveAndQuit,
    Save,
    Open,
    GotoLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Command,
    Search { forward: bool },
    Fuzzy(FuzzyKind),
}

// ════════════════════════════════════════════════════════════════════════
// AST (semantic level)
// ════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Standalone action, optionally repeated (e.g. `5p` could be "5 pastes").
    Direct {
        kind: DirectKind,
        count: u32,
    },
    /// Cursor movement only — no operator wrapping it.
    Motion(MotionExpr),
    /// Operator applied to a target.
    Op {
        op: Operator,
        target: Target,
        /// Outer count: `3d2w` → 3. Multiplied with any inner motion count.
        outer_count: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionExpr {
    pub motion: MotionKind,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// d3w → motion `w` with count 3.
    Motion(MotionExpr),
    /// dib → text object (inner block).
    TextObject { scope: Scope, object: Object },
    /// dd, yy — operator applied to the current line line-wise.
    LineWise,
}

// ════════════════════════════════════════════════════════════════════════
// Dispatch context
// ════════════════════════════════════════════════════════════════════════

/// Context passed to evaluators when an Expr fires. Carries the runtime
/// argument from `:` command lines (`rest`) and the count from the parse.
/// The count is *not* the same as `Expr`'s count fields — those are part
/// of the parsed AST. `Ctx::count` is here for command-line commands that
/// take a count separately (currently only `:goto`).
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    pub rest: &'a str,
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
