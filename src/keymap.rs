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

// Shorthands for table readability.
const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

pub const KEYBINDS: &[KeyBind] = &[
    // ── Global ──────────────────────────────────────────────────────────
    KeyBind {
        mode: None,
        key: KeyCode::Char('c'),
        modifiers: CTRL,
        pending: PendingMatch::None,
        action: Action::QuitForce,
        description: "force quit",
    },
    // ── Normal: movement ────────────────────────────────────────────────
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('h'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveLeft),
        description: "move left",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Left,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveLeft),
        description: "move left",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('l'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveRight),
        description: "move right",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Right,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveRight),
        description: "move right",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('j'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveDown),
        description: "move down",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Down,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveDown),
        description: "move down",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('k'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveUp),
        description: "move up",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Up,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveUp),
        description: "move up",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('0'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveLineStart),
        description: "line start",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Home,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveLineStart),
        description: "line start",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('$'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveLineEnd),
        description: "line end",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::End,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveLineEnd),
        description: "line end",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('G'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveFileEnd),
        description: "file end",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('g'),
        modifiers: NONE,
        pending: PendingMatch::Lead('g'),
        action: Action::Buffer(B::MoveFileStart),
        description: "gg: file start",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('w'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveWordForward),
        description: "word forward",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('b'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveWordBackward),
        description: "word back",
    },
    // ── Normal: edits ───────────────────────────────────────────────────
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('i'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::EnterMode(Mode::Insert),
        description: "insert",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('a'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveRight),
        description: "append (right + insert TBD)",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('o'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::OpenLineBelow,
        description: "open line below",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('O'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::OpenLineAbove,
        description: "open line above",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('x'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::DeleteCharUnderCursor),
        description: "delete char",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('d'),
        modifiers: NONE,
        pending: PendingMatch::Lead('d'),
        action: Action::Buffer(B::DeleteLine),
        description: "dd: delete line",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('y'),
        modifiers: NONE,
        pending: PendingMatch::Lead('y'),
        action: Action::Buffer(B::Yank),
        description: "yy: yank line",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('p'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::Paste),
        description: "paste",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('u'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::Undo),
        description: "undo",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('v'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::EnterMode(Mode::Visual),
        description: "visual",
    },
    // ── Normal: prompts ─────────────────────────────────────────────────
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char(':'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::OpenPrompt(PromptKind::Command),
        description: "command",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('/'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::OpenPrompt(PromptKind::Search { forward: true }),
        description: "search forward",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('?'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::OpenPrompt(PromptKind::Search { forward: false }),
        description: "search back",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('n'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::SearchNext),
        description: "next match",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('N'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::SearchPrev),
        description: "prev match",
    },
    // ── Normal: leader (space) ──────────────────────────────────────────
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('f'),
        modifiers: NONE,
        pending: PendingMatch::Lead(LEADER),
        action: Action::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Files)),
        description: "<space>f: fuzzy files",
    },
    KeyBind {
        mode: Some(Mode::Normal),
        key: KeyCode::Char('l'),
        modifiers: NONE,
        pending: PendingMatch::Lead(LEADER),
        action: Action::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Lines)),
        description: "<space>l: fuzzy lines",
    },
    // ── Insert ──────────────────────────────────────────────────────────
    // (Bare char input is routed straight to the buffer in App::handle_key
    // and bypasses the bind table entirely.)
    KeyBind {
        mode: Some(Mode::Insert),
        key: KeyCode::Esc,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::EnterMode(Mode::Normal),
        description: "leave insert",
    },
    KeyBind {
        mode: Some(Mode::Insert),
        key: KeyCode::Enter,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::InsertNewline),
        description: "newline",
    },
    KeyBind {
        mode: Some(Mode::Insert),
        key: KeyCode::Backspace,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::DeleteCharBefore),
        description: "backspace",
    },
    KeyBind {
        mode: Some(Mode::Insert),
        key: KeyCode::Left,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveLeft),
        description: "move left",
    },
    KeyBind {
        mode: Some(Mode::Insert),
        key: KeyCode::Right,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveRight),
        description: "move right",
    },
    KeyBind {
        mode: Some(Mode::Insert),
        key: KeyCode::Up,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveUp),
        description: "move up",
    },
    KeyBind {
        mode: Some(Mode::Insert),
        key: KeyCode::Down,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveDown),
        description: "move down",
    },
    // ── Visual ──────────────────────────────────────────────────────────
    KeyBind {
        mode: Some(Mode::Visual),
        key: KeyCode::Esc,
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::EnterMode(Mode::Normal),
        description: "leave visual",
    },
    KeyBind {
        mode: Some(Mode::Visual),
        key: KeyCode::Char('h'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveLeft),
        description: "move left",
    },
    KeyBind {
        mode: Some(Mode::Visual),
        key: KeyCode::Char('l'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveRight),
        description: "move right",
    },
    KeyBind {
        mode: Some(Mode::Visual),
        key: KeyCode::Char('j'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveDown),
        description: "move down",
    },
    KeyBind {
        mode: Some(Mode::Visual),
        key: KeyCode::Char('k'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::MoveUp),
        description: "move up",
    },
    KeyBind {
        mode: Some(Mode::Visual),
        key: KeyCode::Char('y'),
        modifiers: NONE,
        pending: PendingMatch::None,
        action: Action::Buffer(B::Yank),
        description: "yank",
    },
];

/// Result of attempting to interpret an accumulated key stream against the
/// bind table. `Partial` means the stream is the prefix of one or more
/// bindings — keep waiting for more input. `NoMatch` means nothing in the
/// table matches the stream even as a prefix — clear and drop.
// `NoMatch` reads more clearly than alternatives like `None` (which would
// collide with Option semantically) or `Failed`.
#[allow(clippy::enum_variant_names)]
pub enum Match {
    Complete { action: Action, count: u32 },
    Partial,
    NoMatch,
}

enum SeqMatch {
    Full,
    Partial,
    None,
}

/// Try to interpret the current key stream in the given mode. Returns
/// Complete as soon as any binding fully matches, Partial if any binding
/// is still a possible continuation, and NoMatch otherwise.
///
/// A leading run of digits (with `1`–`9` as the starter) is stripped off
/// as a vim-style count prefix and surfaced via `Match::Complete.count`.
/// `0` is excluded from the count starter set because it is already a
/// binding (`MoveLineStart`).
pub fn interpret(keys: &[KeyEvent], mode: Mode) -> Match {
    if keys.is_empty() {
        return Match::NoMatch;
    }

    let (count, rest) = split_count(keys);

    // The stream is currently just count digits with no action key yet —
    // we know more input must follow.
    if rest.is_empty() {
        return Match::Partial;
    }

    let mut partial = false;
    for bind in KEYBINDS {
        if let Some(m) = bind.mode
            && m != mode
        {
            continue;
        }
        match match_sequence(rest, bind) {
            SeqMatch::Full => {
                return Match::Complete {
                    action: bind.action,
                    count,
                };
            }
            SeqMatch::Partial => partial = true,
            SeqMatch::None => {}
        }
    }

    if partial {
        Match::Partial
    } else {
        Match::NoMatch
    }
}

/// Peel a leading count off the key stream. The first digit must be 1–9
/// (so a bare `0` still triggers MoveLineStart); subsequent digits 0–9
/// extend the count. Returns `(count, remaining_keys)`.
fn split_count(keys: &[KeyEvent]) -> (u32, &[KeyEvent]) {
    let starter = match keys.first().map(|k| k.code) {
        Some(KeyCode::Char(c @ '1'..='9')) => c,
        _ => return (1, keys),
    };
    let mut count: u32 = starter.to_digit(10).unwrap();
    let mut end = 1;
    for k in &keys[1..] {
        let KeyCode::Char(c) = k.code else { break };
        if !c.is_ascii_digit() {
            break;
        }
        count = count
            .saturating_mul(10)
            .saturating_add(c.to_digit(10).unwrap());
        end += 1;
    }
    (count, &keys[end..])
}

/// Match the accumulated key stream against a binding's logical sequence:
///   - `pending == None`             → 1-key sequence: [bind.key]
///   - `pending == Lead(c)`          → 2-key sequence: [Char(c), bind.key]
fn match_sequence(keys: &[KeyEvent], bind: &KeyBind) -> SeqMatch {
    let expected_len = match bind.pending {
        PendingMatch::None => 1,
        PendingMatch::Lead(_) => 2,
    };
    if keys.len() > expected_len {
        return SeqMatch::None;
    }

    // For Lead bindings, the first key must be Char(lead) with no modifiers.
    if let PendingMatch::Lead(c) = bind.pending {
        let first = keys.first().expect("keys non-empty by interpret() guard");
        if !key_event_matches(*first, KeyCode::Char(c), KeyModifiers::NONE) {
            return SeqMatch::None;
        }
        if keys.len() == 1 {
            return SeqMatch::Partial;
        }
    }

    // The final key in the stream must match the binding's main key.
    let main = keys.last().expect("non-empty");
    if !key_event_matches(*main, bind.key, bind.modifiers) {
        return SeqMatch::None;
    }
    SeqMatch::Full
}

fn key_event_matches(input: KeyEvent, code: KeyCode, mods: KeyModifiers) -> bool {
    if input.code != code {
        return false;
    }
    let mut input_mods = input.modifiers;
    if matches!(input.code, KeyCode::Char(_)) {
        input_mods -= KeyModifiers::SHIFT;
    }
    input_mods == mods
}
