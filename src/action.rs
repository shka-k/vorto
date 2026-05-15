use crate::finder::FuzzyKind;
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
    /// `z` prefix — for `zz`/`zt`/`zb` viewport-scroll actions.
    ZPrefix,
    /// `f`/`F`/`t`/`T` waiting for the literal target character. The
    /// next key press in `FindCharPending` context resolves to a
    /// `Motion(FindChar { … })` carrying this prefix's direction/till
    /// flags plus the typed character.
    FindCharPrefix { forward: bool, till: bool },
    /// `r` — waiting for the replacement char.
    ReplaceCharPrefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Yank,
    Change,
    /// `>` — shift target lines right by one indent level.
    Indent,
    /// `<` — shift target lines left by one indent level.
    Dedent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    /// `^` — first non-whitespace char on the line.
    LineFirstNonBlank,
    /// `g_` — last non-whitespace char on the line.
    LineLastNonBlank,
    WordForward,
    WordBack,
    /// `e` — end of the current/next word (char-class).
    WordEnd,
    /// `W` — WORD forward (whitespace-delimited, no punctuation split).
    BigWordForward,
    /// `B` — WORD back.
    BigWordBack,
    /// `E` — WORD end forward.
    BigWordEnd,
    /// `ge` — backward word end.
    WordEndBack,
    /// `gE` — backward WORD end.
    BigWordEndBack,
    /// `f{c}` / `F{c}` / `t{c}` / `T{c}` — find/till a literal char.
    /// `forward=false` is the uppercase backward variant; `till=true`
    /// places the cursor one char short of the target.
    FindChar {
        ch: char,
        forward: bool,
        till: bool,
    },
    /// `;` / `,` — repeat the last find-char motion, optionally with
    /// direction reversed (`,`).
    RepeatFind { reverse: bool },
    FileStart,
    FileEnd,
    /// `%` — jump to the matching bracket of the pair under (or just
    /// after) the cursor. Treats `()`, `[]`, `{}` as pairs.
    BracketMatch,
    /// `*` — search forward for the word under the cursor.
    SearchWordForward,
    /// `#` — search backward for the word under the cursor.
    SearchWordBack,
    /// `H` — top of the visible viewport (count = offset from top).
    ViewportTop,
    /// `M` — middle of the visible viewport.
    ViewportMiddle,
    /// `L` — bottom of the visible viewport (count = offset from bottom).
    ViewportBottom,
    /// `<C-d>` — half-page down. Count multiplies the half-height
    /// step (so `2<C-d>` covers a full page when supported).
    HalfPageDown,
    /// `<C-u>` — half-page up.
    HalfPageUp,
    /// `<C-f>` — full page down.
    PageDown,
    /// `<C-b>` — full page up.
    PageUp,
    SearchNext,
    SearchPrev,
    /// `{` — move to the previous blank line (or file start). Treats
    /// the buffer as paragraphs separated by all-whitespace lines.
    ParagraphBack,
    /// `}` — move to the next blank line (or file end).
    ParagraphForward,
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
    // Syntactic objects resolved through tree-sitter `textobjects.scm`.
    // `Inner` / `Around` map to the query capture suffixes `.inner` /
    // `.outer` respectively.
    Function,
    Class,
    Parameter,
    /// `p` — vim's paragraph: a contiguous run of non-blank lines
    /// bordered by blank lines (or file start/end). Char-class
    /// equivalent at the line level: `is_blank` vs not.
    Paragraph,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DirectKind {
    EnterMode(Mode),
    OpenPrompt(PromptKind),
    OpenLineBelow,
    OpenLineAbove,
    /// `a` — move past the cursor and enter Insert.
    AppendAfterCursor,
    /// `A` — jump to end-of-line and enter Insert.
    AppendAtLineEnd,
    /// `I` — jump to first non-blank of the line and enter Insert.
    InsertAtLineStart,
    /// `C` — change from cursor to end of line.
    ChangeToEol,
    /// `D` — delete from cursor to end of line.
    DeleteToEol,
    /// `Y` — yank the current line (vim's classic `yy` semantics).
    YankLine,
    /// `J` — join the next line into this one, replacing the line
    /// break with a single space (or nothing when joining onto an
    /// empty line).
    JoinLines,
    /// `~` — toggle case of the character under the cursor, then
    /// advance one column.
    ToggleCase,
    /// `s` — delete the char under the cursor and enter Insert.
    SubstituteChar,
    /// `S` — clear the current line and enter Insert at col 0.
    SubstituteLine,
    /// `zz` — center the cursor's line in the viewport.
    ViewportCenter,
    /// `zt` — scroll so the cursor's line is the top of the viewport.
    ViewportTopAtCursor,
    /// `zb` — scroll so the cursor's line is the bottom of the viewport.
    ViewportBottomAtCursor,
    /// `r<c>` — replace the char under the cursor with `c`.
    ReplaceChar { ch: char },
    Paste,
    Undo,
    Redo,
    DeleteCharUnderCursor,
    Quit,
    QuitForce,
    /// `:bn` / `:bnext` — switch to the next buffer in MRU order.
    BufferNext,
    /// `:bp` / `:bprev` — switch to the previous buffer in MRU order.
    BufferPrev,
    /// `:bd` / `:bdelete` — drop the current buffer (refuse if dirty).
    BufferDelete,
    /// `:bd!` — force-drop the current buffer (discards unsaved edits).
    BufferDeleteForce,
    /// `:ls` / `:buffers` — open the buffer picker.
    BufferList,
    SaveAndQuit,
    Save,
    Open,
    GotoLine,
    /// `gd` — `textDocument/definition` for the symbol under the cursor.
    GotoDefinition,
    /// `gD` — `textDocument/declaration` (distinct from definition in
    /// languages like C/C++; most others alias the two).
    GotoDeclaration,
    /// `gi` — `textDocument/implementation` (jump from trait method /
    /// interface decl to a concrete impl).
    GotoImplementation,
    /// `gr` — `textDocument/references` for the symbol under the cursor.
    FindReferences,
    /// `<space>r` — open a prompt to enter the new name, then send
    /// `textDocument/rename` and apply the returned `WorkspaceEdit`.
    Rename,
    /// `<space>a` — request `textDocument/codeAction` at the cursor
    /// and surface the results in a picker.
    CodeAction,
    /// `K` — request `textDocument/hover` for the symbol under the
    /// cursor and display the result in a scrollable popup.
    Hover,
    /// `<space>c` — toggle a single-line comment on the current line
    /// using the active language's `comment_token`.
    ToggleComment,
    /// `.` — replay the last buffer-modifying change. Intercepted in
    /// `App::evaluate` before reaching the normal dispatch path, so this
    /// variant never appears in `handle_direct`'s match arms.
    RepeatLast,
    /// `gn` / `gN` — find the next/previous match of the current search
    /// pattern, enter Visual mode, and select the match. `reverse`
    /// flips against the stored search direction (so `gN` after a `/`
    /// becomes `reverse: true`).
    SearchSelectNext { reverse: bool },
    /// `g*` / `g#` — seed the search pattern from the word under the
    /// cursor (same extraction as `*` / `#`) without jumping. Useful
    /// when you want to highlight or set up for `n` / `gn` without
    /// losing your position.
    SearchWordKeep { forward: bool },
    /// `:noh` — clear the active search pattern so `hlsearch` stops
    /// painting matches. The pattern goes back to empty; `n` / `N`
    /// after this do nothing until a new search is performed.
    ClearSearch,
    /// `+` — multi-cursor: find the next occurrence of the word under
    /// the cursor, push the current primary into `extra_cursors`, and
    /// jump primary to the match. Also seeds the search pattern so
    /// `n` / `N` walk the same matches.
    MultiCursorAddNext,
    /// `-` — pop the most recently added extra cursor and move primary
    /// back to its position. No-op when there are no extras.
    MultiCursorPop,
    /// `<space>,` — drop every extra cursor and keep only primary.
    MultiCursorClear,
    /// `gw` — easymotion / hop-style two-character label jump. Computes
    /// jump targets at every visible word start, overlays a 2-char label
    /// on each, and waits for the user to type the label. After the
    /// first character only matching labels remain (showing their
    /// second char); a unique first-char prefix jumps immediately. Esc
    /// or any non-label key cancels.
    JumpLabel,
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
    Direct { kind: DirectKind, count: u32 },
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

/// Remembered parameters of a `MotionKind::FindChar` — what `;` and `,`
/// replay. Structurally a flattened `FindChar`, but kept as its own type
/// so the "last find" slot on `App` is clearly typed. Lives in the
/// grammar layer so `effect::Cmd::SetLastFind` and any downstream
/// consumers can reference it without inverting module dependencies.
#[derive(Debug, Clone, Copy)]
pub struct LastFind {
    pub ch: char,
    pub forward: bool,
    pub till: bool,
}

/// What `.` replays. Either a one-shot Expr (e.g. `dw`, `x`, `p`, `r<c>`)
/// or an Insert-mode session — the trigger that entered Insert plus the
/// keystrokes typed before Esc.
#[derive(Debug, Clone)]
pub enum LastChange {
    Expr(Expr),
    Insert { trigger: Expr, keys: Vec<InsertKey> },
}

/// Replay-able Insert-mode keystrokes. Cursor motions (arrow keys) end
/// the recording in vim; we follow that by simply not recording them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertKey {
    Char(char),
    Newline,
    Backspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// d3w → motion `w` with count 3.
    Motion(MotionExpr),
    /// dib → text object (inner block).
    TextObject { scope: Scope, object: Object },
    /// dd, yy — operator applied to the current line line-wise.
    LineWise,
    /// `dgn` / `cgn` / `ygn` — operator applied to the next/previous
    /// match of the current search pattern. Differs from a normal
    /// motion target: the range starts at the match's first char (not
    /// the cursor), so it can't be expressed as `Target::Motion`.
    /// `reverse` flips against the stored search direction, same
    /// convention as `DirectKind::SearchSelectNext`.
    SearchMatch { reverse: bool },
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
