//! Inline completion suggestion (ghost text) state.
//!
//! Phase-0 plumbing: holds the lifecycle of a single suggestion so the
//! UI can paint ghost text and the input layer can route the accept /
//! dismiss key. The actual provider (Copilot, Claude, …) lives behind
//! a later abstraction — this module only models what the editor needs
//! to render and confirm.
//!
//! Anchor semantics: a suggestion is only valid while the cursor is
//! exactly at the position the request was issued from. Any cursor
//! movement or buffer edit must drop back to [`SuggestionState::Idle`]
//! so we never paint stale text against a shifted cursor.

use super::Cursor;

/// Monotonic id for an in-flight suggestion request. Lets a late
/// provider response be discarded when a newer request has already
/// superseded it (debounce / cancellation race).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// A completion the provider would insert at [`Suggestion::anchor`].
/// `text` is the full insertion verbatim — may contain `\n` for
/// multi-line completions. The renderer splits on `\n` to paint
/// continuation rows below the cursor row.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub text: String,
    pub anchor: Cursor,
}

impl Suggestion {
    /// True when this suggestion is currently anchored at `cursor`.
    /// Callers use this before painting / accepting to confirm the
    /// cursor hasn't drifted since the request fired.
    pub fn is_anchored_at(&self, cursor: Cursor) -> bool {
        self.anchor == cursor
    }
}

#[derive(Debug, Default)]
pub enum SuggestionState {
    #[default]
    Idle,
    Pending {
        id: RequestId,
        anchor: Cursor,
    },
    Showing {
        /// Request id of the response that produced this suggestion.
        /// Currently unread but kept so future telemetry
        /// (`didShowCompletion` / `didPartiallyAccept`) can identify
        /// the originating request without an extra lookup.
        #[allow(dead_code)]
        id: RequestId,
        suggestion: Suggestion,
    },
}

impl SuggestionState {
    pub fn showing(&self) -> Option<&Suggestion> {
        match self {
            SuggestionState::Showing { suggestion, .. } => Some(suggestion),
            _ => None,
        }
    }

    pub fn dismiss(&mut self) {
        *self = SuggestionState::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur(row: usize, col: usize) -> Cursor {
        Cursor { row, col }
    }

    #[test]
    fn default_is_idle() {
        let s = SuggestionState::default();
        assert!(matches!(s, SuggestionState::Idle));
        assert!(s.showing().is_none());
    }

    #[test]
    fn showing_returns_suggestion_only_in_showing_state() {
        let mut s = SuggestionState::Pending {
            id: RequestId(1),
            anchor: cur(0, 0),
        };
        assert!(s.showing().is_none());

        s = SuggestionState::Showing {
            id: RequestId(1),
            suggestion: Suggestion {
                text: "abc".into(),
                anchor: cur(2, 4),
            },
        };
        let got = s.showing().expect("should be showing");
        assert_eq!(got.text, "abc");
        assert_eq!(got.anchor, cur(2, 4));
    }

    #[test]
    fn dismiss_resets_to_idle() {
        let mut s = SuggestionState::Showing {
            id: RequestId(7),
            suggestion: Suggestion {
                text: "x".into(),
                anchor: cur(0, 0),
            },
        };
        s.dismiss();
        assert!(matches!(s, SuggestionState::Idle));
    }

    #[test]
    fn anchor_check_distinguishes_positions() {
        let s = Suggestion {
            text: "y".into(),
            anchor: cur(3, 5),
        };
        assert!(s.is_anchored_at(cur(3, 5)));
        assert!(!s.is_anchored_at(cur(3, 6)));
        assert!(!s.is_anchored_at(cur(4, 5)));
    }
}
