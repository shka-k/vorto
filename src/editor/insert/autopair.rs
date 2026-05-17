//! Auto-pair classification: which characters open / close a pair, and
//! whether typing an opener at the cursor should also drop in its closer.
//!
//! Pure helpers — no `Buffer` access. The buffer-side wrappers in
//! `super` (`insert_char_smart`, `insert_newline`, `delete_char_before_smart`)
//! drive these by passing the surrounding chars they read off the cursor row.

/// Maps an auto-pair opener to its closer. Quotes are self-paired
/// (closer == opener). Returns `None` for any non-opener.
pub(super) fn auto_pair_closer(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        _ => None,
    }
}

/// True when `c` is a closer that participates in skip-over (typing it
/// where the same char already sits just advances the cursor).
pub(super) fn is_auto_pair_closer(c: char) -> bool {
    matches!(c, ')' | ']' | '}' | '"' | '\'' | '`')
}

/// Decide whether typing `opener` should also insert its closer, given
/// the chars to either side of the cursor. Brackets only check the
/// right side (don't capture an existing identifier); quotes also gate
/// on the left side to dodge apostrophes inside words and the inner
/// edge of an existing quoted region.
pub(super) fn should_auto_pair(opener: char, prev: Option<char>, next: Option<char>) -> bool {
    if let Some(n) = next
        && (n.is_alphanumeric() || n == '_')
    {
        return false;
    }
    if matches!(opener, '"' | '\'' | '`')
        && let Some(p) = prev
        && (p.is_alphanumeric() || p == '_' || p == opener)
    {
        return false;
    }
    true
}
