use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Action, BufferAction, PromptKind};
use crate::fuzzy::FuzzyKind;
use crate::mode::Mode;

/// Leader key for Workspace-level shortcuts (e.g. `<leader>w` to save).
pub const LEADER: char = ' ';

/// Translate a key event into a sequence of high-level Actions based on the
/// current mode. Returns an empty Vec when the key doesn't map to anything.
/// Most bindings produce a single action; `o`/`O` produce two (insert a line
/// + enter Insert mode).
///
/// This is only called when no prompt (command line / search / fuzzy) is
/// open — `App::handle_key` short-circuits to the prompt handler first.
pub fn translate(mode: Mode, key: KeyEvent, pending: Option<char>) -> Vec<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return vec![Action::Quit];
    }

    match mode {
        Mode::Normal => normal(key, pending),
        Mode::Insert => insert(key),
        Mode::Visual => visual(key),
    }
}

fn normal(key: KeyEvent, pending: Option<char>) -> Vec<Action> {
    use BufferAction as B;

    if let Some(prev) = pending {
        if prev == LEADER {
            return leader(key);
        }
        return match (prev, key.code) {
            ('g', KeyCode::Char('g')) => vec![Action::Buffer(B::MoveFileStart)],
            ('d', KeyCode::Char('d')) => vec![Action::Buffer(B::DeleteLine)],
            ('y', KeyCode::Char('y')) => vec![Action::Buffer(B::Yank)],
            _ => vec![],
        };
    }

    match key.code {
        KeyCode::Char('h') | KeyCode::Left => vec![Action::Buffer(B::MoveLeft)],
        KeyCode::Char('l') | KeyCode::Right => vec![Action::Buffer(B::MoveRight)],
        KeyCode::Char('j') | KeyCode::Down => vec![Action::Buffer(B::MoveDown)],
        KeyCode::Char('k') | KeyCode::Up => vec![Action::Buffer(B::MoveUp)],
        KeyCode::Char('0') | KeyCode::Home => vec![Action::Buffer(B::MoveLineStart)],
        KeyCode::Char('$') | KeyCode::End => vec![Action::Buffer(B::MoveLineEnd)],
        KeyCode::Char('G') => vec![Action::Buffer(B::MoveFileEnd)],
        KeyCode::Char('w') => vec![Action::Buffer(B::MoveWordForward)],
        KeyCode::Char('b') => vec![Action::Buffer(B::MoveWordBackward)],
        KeyCode::Char('i') => vec![Action::EnterMode(Mode::Insert)],
        KeyCode::Char('a') => vec![Action::Buffer(B::MoveRight)],
        KeyCode::Char('o') => vec![Action::OpenLineBelow],
        KeyCode::Char('O') => vec![Action::OpenLineAbove],
        KeyCode::Char('x') => vec![Action::Buffer(B::DeleteCharUnderCursor)],
        KeyCode::Char('p') => vec![Action::Buffer(B::Paste)],
        KeyCode::Char('u') => vec![Action::Buffer(B::Undo)],
        KeyCode::Char('v') => vec![Action::EnterMode(Mode::Visual)],
        KeyCode::Char(':') => vec![Action::OpenPrompt(PromptKind::Command)],
        KeyCode::Char('/') => vec![Action::OpenPrompt(PromptKind::Search { forward: true })],
        KeyCode::Char('?') => vec![Action::OpenPrompt(PromptKind::Search { forward: false })],
        KeyCode::Char('n') => vec![Action::Buffer(B::SearchNext)],
        KeyCode::Char('N') => vec![Action::Buffer(B::SearchPrev)],
        _ => vec![],
    }
}

fn leader(key: KeyEvent) -> Vec<Action> {
    match key.code {
        KeyCode::Char('f') => vec![Action::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Files))],
        KeyCode::Char('l') => vec![Action::OpenPrompt(PromptKind::Fuzzy(FuzzyKind::Lines))],
        _ => vec![],
    }
}

fn insert(key: KeyEvent) -> Vec<Action> {
    use BufferAction as B;
    match key.code {
        KeyCode::Esc => vec![Action::EnterMode(Mode::Normal)],
        KeyCode::Enter => vec![Action::Buffer(B::InsertNewline)],
        KeyCode::Backspace => vec![Action::Buffer(B::DeleteCharBefore)],
        KeyCode::Left => vec![Action::Buffer(B::MoveLeft)],
        KeyCode::Right => vec![Action::Buffer(B::MoveRight)],
        KeyCode::Up => vec![Action::Buffer(B::MoveUp)],
        KeyCode::Down => vec![Action::Buffer(B::MoveDown)],
        // Char input is routed straight to the buffer in App::handle_key.
        _ => vec![],
    }
}

fn visual(key: KeyEvent) -> Vec<Action> {
    use BufferAction as B;
    match key.code {
        KeyCode::Esc => vec![Action::EnterMode(Mode::Normal)],
        KeyCode::Char('h') => vec![Action::Buffer(B::MoveLeft)],
        KeyCode::Char('l') => vec![Action::Buffer(B::MoveRight)],
        KeyCode::Char('j') => vec![Action::Buffer(B::MoveDown)],
        KeyCode::Char('k') => vec![Action::Buffer(B::MoveUp)],
        KeyCode::Char('y') => vec![Action::Buffer(B::Yank)],
        _ => vec![],
    }
}

/// Detect if this key starts a 2-key normal-mode sequence: `g`/`d`/`y` for
/// `gg`/`dd`/`yy`, or the leader key for `<leader>…` workspace shortcuts.
pub fn is_pending_lead(mode: Mode, key: KeyEvent, pending: Option<char>) -> Option<char> {
    if mode != Mode::Normal || pending.is_some() {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Char(c) if c == LEADER || matches!(c, 'g' | 'd' | 'y') => Some(c),
        _ => None,
    }
}
