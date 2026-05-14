//! Two-character label jump (`gw`) — the "easymotion / hop / leap"
//! style overlay.
//!
//! When the user presses `gw`, every word start in the visible viewport
//! gets a 2-character label drawn over its first few cells. The user
//! then types the label to jump:
//!
//! - First keypress filters to labels starting with that char. If only
//!   one matches, the jump fires immediately.
//! - Second keypress disambiguates within that filtered set and jumps.
//! - Esc (or any key that matches no remaining label) cancels.
//!
//! Targets are word starts (vim's `\w` char-class: alphanumeric + `_`).
//! Labels are drawn from an ergonomics-first alphabet (home row first)
//! and assigned by `i % N` for the first char, `i / N` for the second
//! so consecutive targets get distinct first chars — meaning a small
//! number of targets all jump on a single keypress.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::editor::{Buffer, Cursor};

use super::{App, Status};

/// Alphabet used to construct labels. Home row first, then top row,
/// then bottom row — same ergonomics ordering hop/leap converged on.
/// 26 chars, so `26 * 26 = 676` distinct labels — more than fits in any
/// reasonable viewport.
const ALPHABET: &[char] = &[
    'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l',
    'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p',
    'z', 'x', 'c', 'v', 'b', 'n', 'm',
];

#[derive(Debug, Clone)]
pub struct JumpLabel {
    pub pos: Cursor,
    pub first: char,
    /// `None` when fewer targets than the alphabet size — a single
    /// keypress is enough to pick the target.
    pub second: Option<char>,
}

#[derive(Debug)]
pub struct JumpState {
    pub labels: Vec<JumpLabel>,
    /// `Some` after the user has typed the first character. The render
    /// path then hides labels whose `first` doesn't match and shows the
    /// remaining ones as just their `second` char.
    pub typed_first: Option<char>,
}

impl App {
    /// Enter jump-label mode. Scans every visible line for word starts
    /// and assigns labels. Cancels (with a status message) when there
    /// is nothing in the viewport to label.
    pub(super) fn start_jump_label(&mut self) {
        let targets = collect_jump_targets(&self.buffer);
        if targets.is_empty() {
            self.status = Status::info("no jump targets");
            return;
        }
        let labels = assign_labels(targets);
        self.jump_state = Some(JumpState {
            labels,
            typed_first: None,
        });
        self.status = Status::info("jump: type label (Esc to cancel)");
    }

    /// Handle a key while jump-label mode is active. Always consumes
    /// the key (the caller routes here unconditionally when
    /// `self.jump_state` is `Some`). Returns silently — state changes
    /// are mutations to `self.jump_state` / `self.buffer.cursor`.
    pub(super) fn handle_jump_key(&mut self, key: KeyEvent) {
        // Esc / Ctrl-C / Ctrl-G — cancel.
        if key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('g')))
        {
            self.cancel_jump();
            return;
        }
        let KeyCode::Char(ch) = key.code else {
            self.cancel_jump();
            return;
        };

        let Some(state) = self.jump_state.as_mut() else {
            return;
        };

        match state.typed_first {
            None => {
                // First keystroke. Filter labels by `first == ch`.
                let mut matched: Vec<&JumpLabel> =
                    state.labels.iter().filter(|l| l.first == ch).collect();
                if matched.is_empty() {
                    self.cancel_jump();
                    return;
                }
                // If only one (or all share a `None` second), jump now.
                if matched.len() == 1 {
                    let pos = matched.remove(0).pos;
                    self.finish_jump(pos);
                    return;
                }
                state.typed_first = Some(ch);
            }
            Some(first) => {
                let target = state
                    .labels
                    .iter()
                    .find(|l| l.first == first && l.second == Some(ch))
                    .map(|l| l.pos);
                match target {
                    Some(pos) => self.finish_jump(pos),
                    None => self.cancel_jump(),
                }
            }
        }
    }

    fn finish_jump(&mut self, pos: Cursor) {
        self.buffer.cursor = pos;
        self.jump_state = None;
        self.status = Status::info("");
    }

    fn cancel_jump(&mut self) {
        self.jump_state = None;
        self.status = Status::info("jump cancelled");
    }
}

/// Walk every visible row and emit a `Cursor` at every word start
/// (`\w` char-class: alphanumeric or `_`, preceded by a non-word char
/// or line start). Order is top-to-bottom, left-to-right.
fn collect_jump_targets(buffer: &Buffer) -> Vec<Cursor> {
    let scroll = buffer.scroll.get();
    let height = buffer.viewport_height.get();
    if height == 0 {
        return Vec::new();
    }
    let last = (scroll + height).min(buffer.lines.len());
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    for row in scroll..last {
        let mut prev_word = false;
        for (col, c) in buffer.lines[row].chars().enumerate() {
            let cur_word = is_word(c);
            if cur_word && !prev_word {
                out.push(Cursor { row, col });
            }
            prev_word = cur_word;
        }
    }
    out
}

/// Assign a label to each target.
///
/// - When there are no more targets than alphabet letters, every label
///   is single-char (`second = None`) and one keystroke jumps.
/// - Beyond that, labels become two-char. First char varies fastest
///   (`i % a`) so consecutive targets get distinct first chars — when
///   the user's intended target is the only one with its first char,
///   the unique-match branch in `handle_jump_key` jumps after a single
///   keystroke even though a two-char label is drawn.
///
/// Targets past `a * a` aren't labelled — the viewport would need to
/// be > 676 word starts before that mattered.
fn assign_labels(targets: Vec<Cursor>) -> Vec<JumpLabel> {
    let a = ALPHABET.len();
    let n = targets.len();
    let max = a * a;
    targets
        .into_iter()
        .take(max)
        .enumerate()
        .map(|(i, pos)| {
            let (first, second) = if n <= a {
                (ALPHABET[i], None)
            } else {
                (ALPHABET[i % a], Some(ALPHABET[i / a]))
            };
            JumpLabel { pos, first, second }
        })
        .collect()
}
