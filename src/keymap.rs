use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Action, BufferAction as B, PromptKind};
use crate::fuzzy::FuzzyKind;
use crate::mode::Mode;

/// Leader key for prompt-opening shortcuts (e.g. `<leader>f` for files).
pub const LEADER: char = ' ';

/// Constraint on the `pending` lead char that must precede this binding to
/// trigger it. `None` means the binding fires with no lead; `Lead(c)` means
/// `pending == Some(c)` is required (covers `gg`/`dd`/`yy` and `<leader>…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingMatch {
    None,
    Lead(char),
}

/// A single key binding. Pure data — no behavior. The `action` field
/// identifies what to dispatch when this binding matches; runtime context
/// (counts, etc.) flows through `Ctx` at dispatch time.
///
/// `mode == None` means the binding applies in every mode (used for the
/// global `Ctrl-C → QuitForce` binding).
pub struct KeyBind {
    pub mode: Option<Mode>,
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub pending: PendingMatch,
    pub action: Action,
    /// Human-readable label, e.g. for a future keymap help overlay.
    /// Symmetric with `CommandBind::description`.
    #[allow(dead_code)]
    pub description: &'static str,
}

impl KeyBind {
    fn matches(&self, mode: Mode, key: KeyEvent, pending: Option<char>) -> bool {
        if let Some(m) = self.mode {
            if m != mode {
                return false;
            }
        }
        if self.key != key.code {
            return false;
        }
        // For Char keys the shift state is already baked into the character
        // ('G' vs 'g'). Strip SHIFT from the input modifiers so that
        // terminals which report it explicitly behave the same as ones
        // that don't.
        let mut input_mods = key.modifiers;
        if matches!(key.code, KeyCode::Char(_)) {
            input_mods -= KeyModifiers::SHIFT;
        }
        if self.modifiers != input_mods {
            return false;
        }
        match self.pending {
            PendingMatch::None => pending.is_none(),
            PendingMatch::Lead(c) => pending == Some(c),
        }
    }
}

// Shorthands for table readability.
const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

pub const KEYBINDS: &[KeyBind] = &[
    // ── Global ──────────────────────────────────────────────────────────
    KeyBind { mode: None, key: KeyCode::Char('c'), modifiers: CTRL, pending: PendingMatch::None,
              action: Action::QuitForce, description: "force quit" },

    // ── Normal: movement ────────────────────────────────────────────────
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('h'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveLeft), description: "move left" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Left, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveLeft), description: "move left" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('l'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveRight), description: "move right" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Right, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveRight), description: "move right" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('j'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveDown), description: "move down" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Down, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveDown), description: "move down" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('k'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveUp), description: "move up" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Up, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveUp), description: "move up" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('0'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveLineStart), description: "line start" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Home, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveLineStart), description: "line start" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('$'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveLineEnd), description: "line end" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::End, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveLineEnd), description: "line end" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('G'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveFileEnd), description: "file end" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('g'), modifiers: NONE, pending: PendingMatch::Lead('g'),
              action: Action::Buffer(B::MoveFileStart), description: "gg: file start" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('w'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveWordForward), description: "word forward" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('b'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveWordBackward), description: "word back" },

    // ── Normal: edits ───────────────────────────────────────────────────
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('i'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::EnterMode(Mode::Insert), description: "insert" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('a'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveRight), description: "append (right + insert TBD)" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('o'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::OpenLineBelow, description: "open line below" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('O'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::OpenLineAbove, description: "open line above" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('x'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::DeleteCharUnderCursor), description: "delete char" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('d'), modifiers: NONE, pending: PendingMatch::Lead('d'),
              action: Action::Buffer(B::DeleteLine), description: "dd: delete line" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('y'), modifiers: NONE, pending: PendingMatch::Lead('y'),
              action: Action::Buffer(B::Yank), description: "yy: yank line" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('p'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::Paste), description: "paste" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('u'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::Undo), description: "undo" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('v'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::EnterMode(Mode::Visual), description: "visual" },

    // ── Normal: prompts ─────────────────────────────────────────────────
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char(':'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::OpenPrompt(PromptKind::Command), description: "command" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('/'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::OpenPrompt(PromptKind::Search { forward: true }), description: "search forward" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('?'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::OpenPrompt(PromptKind::Search { forward: false }), description: "search back" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('n'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::SearchNext), description: "next match" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('N'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::SearchPrev), description: "prev match" },

    // ── Normal: leader (space) ──────────────────────────────────────────
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('f'), modifiers: NONE, pending: PendingMatch::Lead(LEADER),
              action: Action::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Files)), description: "<space>f: fuzzy files" },
    KeyBind { mode: Some(Mode::Normal), key: KeyCode::Char('l'), modifiers: NONE, pending: PendingMatch::Lead(LEADER),
              action: Action::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Lines)), description: "<space>l: fuzzy lines" },

    // ── Insert ──────────────────────────────────────────────────────────
    // (Bare char input is routed straight to the buffer in App::handle_key
    // and bypasses the bind table entirely.)
    KeyBind { mode: Some(Mode::Insert), key: KeyCode::Esc, modifiers: NONE, pending: PendingMatch::None,
              action: Action::EnterMode(Mode::Normal), description: "leave insert" },
    KeyBind { mode: Some(Mode::Insert), key: KeyCode::Enter, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::InsertNewline), description: "newline" },
    KeyBind { mode: Some(Mode::Insert), key: KeyCode::Backspace, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::DeleteCharBefore), description: "backspace" },
    KeyBind { mode: Some(Mode::Insert), key: KeyCode::Left, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveLeft), description: "move left" },
    KeyBind { mode: Some(Mode::Insert), key: KeyCode::Right, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveRight), description: "move right" },
    KeyBind { mode: Some(Mode::Insert), key: KeyCode::Up, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveUp), description: "move up" },
    KeyBind { mode: Some(Mode::Insert), key: KeyCode::Down, modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveDown), description: "move down" },

    // ── Visual ──────────────────────────────────────────────────────────
    KeyBind { mode: Some(Mode::Visual), key: KeyCode::Esc, modifiers: NONE, pending: PendingMatch::None,
              action: Action::EnterMode(Mode::Normal), description: "leave visual" },
    KeyBind { mode: Some(Mode::Visual), key: KeyCode::Char('h'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveLeft), description: "move left" },
    KeyBind { mode: Some(Mode::Visual), key: KeyCode::Char('l'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveRight), description: "move right" },
    KeyBind { mode: Some(Mode::Visual), key: KeyCode::Char('j'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveDown), description: "move down" },
    KeyBind { mode: Some(Mode::Visual), key: KeyCode::Char('k'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::MoveUp), description: "move up" },
    KeyBind { mode: Some(Mode::Visual), key: KeyCode::Char('y'), modifiers: NONE, pending: PendingMatch::None,
              action: Action::Buffer(B::Yank), description: "yank" },
];

/// Look up the first KeyBind that matches the current input and return its
/// action (still returns a `Vec<Action>` for API stability with the old
/// translate signature; will become a single Action in Stage 3 once the
/// interpreter takes over).
pub fn translate(mode: Mode, key: KeyEvent, pending: Option<char>) -> Vec<Action> {
    KEYBINDS
        .iter()
        .find(|b| b.matches(mode, key, pending))
        .map(|b| vec![b.action])
        .unwrap_or_default()
}

/// Detect whether this key starts a multi-key sequence in `mode` — derived
/// from the bind table by looking for any binding whose `pending` field
/// requires this key as the lead.
pub fn is_pending_lead(mode: Mode, key: KeyEvent, pending: Option<char>) -> Option<char> {
    if pending.is_some() {
        return None;
    }
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    let needed = PendingMatch::Lead(c);
    let any = KEYBINDS.iter().any(|b| {
        b.mode.map_or(true, |m| m == mode) && b.pending == needed
    });
    any.then_some(c)
}
