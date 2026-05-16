//! `:s/pat/repl/[g]` — buffer substitution.
//!
//! Plain-string replacement (no regex). Lives next to [`super::search`]
//! since both layers use the same byte-level `str::find` convention and
//! share the empty-pattern-falls-back-to-last-search policy enforced by
//! the caller.

use super::{Buffer, Cursor};

/// What rows a substitute applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubsRange {
    /// `:s/...` — current row only.
    Current,
    /// `:%s/...` — every row in the buffer.
    All,
}

/// Parsed `:s` arguments. Lifetime-bound to the raw command string so we
/// don't copy until we have to.
#[derive(Debug)]
pub struct SubsArgs<'a> {
    pub range: SubsRange,
    pub pattern: &'a str,
    pub replacement: &'a str,
    pub global: bool,
}

/// Parse one of:
///
/// - `s/pat/repl/[flags]`
/// - `%s/pat/repl/[flags]`
///
/// The trailing `/` may be omitted (`:s/pat/repl` with no flags).
/// Returns `None` when the head isn't a substitute form; returns
/// `Err(msg)` when it is a substitute form but malformed (e.g.
/// `s/foo` with only one delimiter).
pub fn parse_substitute(line: &str) -> Option<Result<SubsArgs<'_>, &'static str>> {
    let (range, rest) = if let Some(r) = line.strip_prefix("%s/") {
        (SubsRange::All, r)
    } else if let Some(r) = line.strip_prefix("s/") {
        (SubsRange::Current, r)
    } else {
        return None;
    };

    // Split into [pattern, replacement, flags?]. Vim allows unescaped
    // `/` only as the delimiter, so a plain split works for v1.
    let mut parts = rest.splitn(3, '/');
    let pattern = match parts.next() {
        Some(p) => p,
        None => return Some(Err("usage: :s/pat/repl/[g]")),
    };
    let replacement = match parts.next() {
        Some(r) => r,
        None => return Some(Err("usage: :s/pat/repl/[g]")),
    };
    let flags = parts.next().unwrap_or("");

    let mut global = false;
    for c in flags.chars() {
        match c {
            'g' => global = true,
            _ => return Some(Err("unknown flag (only `g` is supported)")),
        }
    }
    Some(Ok(SubsArgs {
        range,
        pattern,
        replacement,
        global,
    }))
}

/// Result of [`Buffer::substitute`]. The buffer's own `cursor` is
/// moved to the last replacement before the call returns; this struct
/// only carries the counts the status bar needs.
#[derive(Debug)]
pub struct SubsOutcome {
    pub matches: usize,
    pub lines_changed: usize,
}

impl Buffer {
    /// Apply a substitute. Returns the count of replacements made plus
    /// the cursor target. Caller is responsible for snapshotting (the
    /// standard `expr_modifies_buffer` path handles that).
    pub fn substitute(&mut self, args: &SubsArgs<'_>) -> SubsOutcome {
        if args.pattern.is_empty() {
            return SubsOutcome {
                matches: 0,
                lines_changed: 0,
            };
        }
        let (row_lo, row_hi) = match args.range {
            SubsRange::Current => (self.cursor.row, self.cursor.row),
            SubsRange::All => (0, self.lines.len().saturating_sub(1)),
        };

        let mut matches = 0usize;
        let mut lines_changed = 0usize;
        let mut last_hit: Option<Cursor> = None;

        for row in row_lo..=row_hi {
            let line = &self.lines[row];
            let (new_line, count, last_match_byte) =
                replace_line(line, args.pattern, args.replacement, args.global);
            if count == 0 {
                continue;
            }
            matches += count;
            lines_changed += 1;
            if let Some(byte_idx) = last_match_byte {
                let col = super::byte_to_char(&new_line, byte_idx);
                last_hit = Some(Cursor { row, col });
            }
            self.lines[row] = new_line;
        }

        if matches > 0 {
            if let Some(c) = last_hit {
                self.cursor = c;
            }
            self.clamp_col(false);
            self.touch();
        }
        SubsOutcome {
            matches,
            lines_changed,
        }
    }
}

/// Replace occurrences of `pat` with `repl` in `line`. Returns the
/// rebuilt line, the number of substitutions, and the byte index of
/// the *last* substitution's start in the rebuilt line (used to park
/// the cursor on the final replacement).
fn replace_line(line: &str, pat: &str, repl: &str, global: bool) -> (String, usize, Option<usize>) {
    let mut out = String::with_capacity(line.len());
    let mut count = 0usize;
    let mut last_start: Option<usize> = None;
    let mut cursor = 0usize;

    loop {
        let slice = &line[cursor..];
        let Some(rel) = slice.find(pat) else {
            out.push_str(slice);
            break;
        };
        let abs = cursor + rel;
        out.push_str(&line[cursor..abs]);
        last_start = Some(out.len());
        out.push_str(repl);
        count += 1;
        let next = abs + pat.len();
        if !global {
            out.push_str(&line[next..]);
            break;
        }
        if next == abs {
            // Zero-width match (pat is empty) — guarded earlier, but
            // be defensive so we can't spin.
            break;
        }
        cursor = next;
    }
    (out, count, last_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_of(lines: &[&str]) -> Buffer {
        let mut b = Buffer::new();
        b.lines = lines.iter().map(|s| s.to_string()).collect();
        b.cursor = Cursor { row: 0, col: 0 };
        b
    }

    #[test]
    fn parse_basic_forms() {
        let a = parse_substitute("s/foo/bar/g").unwrap().unwrap();
        assert_eq!(a.range, SubsRange::Current);
        assert_eq!(a.pattern, "foo");
        assert_eq!(a.replacement, "bar");
        assert!(a.global);

        let a = parse_substitute("%s/foo/bar/").unwrap().unwrap();
        assert_eq!(a.range, SubsRange::All);
        assert!(!a.global);

        // Trailing slash optional.
        let a = parse_substitute("s/foo/bar").unwrap().unwrap();
        assert_eq!(a.replacement, "bar");
        assert!(!a.global);

        // Empty replacement is allowed (deletes matches).
        let a = parse_substitute("s/foo//").unwrap().unwrap();
        assert_eq!(a.replacement, "");
    }

    #[test]
    fn parse_non_substitute_returns_none() {
        assert!(parse_substitute("q").is_none());
        assert!(parse_substitute("save").is_none());
        // Bare `s` without delimiter isn't a substitute form.
        assert!(parse_substitute("s").is_none());
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        assert!(parse_substitute("s/a/b/xyz").unwrap().is_err());
    }

    #[test]
    fn substitute_current_line_first_only() {
        let mut b = buf_of(&["foo foo foo", "foo"]);
        let args = parse_substitute("s/foo/bar").unwrap().unwrap();
        let r = b.substitute(&args);
        assert_eq!(r.matches, 1);
        assert_eq!(b.lines[0], "bar foo foo");
        assert_eq!(b.lines[1], "foo");
    }

    #[test]
    fn substitute_current_line_global() {
        let mut b = buf_of(&["foo foo foo", "foo"]);
        let args = parse_substitute("s/foo/bar/g").unwrap().unwrap();
        let r = b.substitute(&args);
        assert_eq!(r.matches, 3);
        assert_eq!(b.lines[0], "bar bar bar");
        assert_eq!(b.lines[1], "foo");
        // Cursor parks on the last replacement (column 8 — start of
        // the third "bar").
        assert_eq!(b.cursor, Cursor { row: 0, col: 8 });
    }

    #[test]
    fn substitute_whole_buffer_global() {
        let mut b = buf_of(&["foo foo", "foo", "no match"]);
        let args = parse_substitute("%s/foo/x/g").unwrap().unwrap();
        let r = b.substitute(&args);
        assert_eq!(r.matches, 3);
        assert_eq!(r.lines_changed, 2);
        assert_eq!(b.lines, vec!["x x", "x", "no match"]);
    }

    #[test]
    fn substitute_no_match_leaves_buffer_alone() {
        let mut b = buf_of(&["hello"]);
        let before_dirty = b.dirty;
        let args = parse_substitute("%s/zzz/x/g").unwrap().unwrap();
        let r = b.substitute(&args);
        assert_eq!(r.matches, 0);
        assert_eq!(b.lines, vec!["hello".to_string()]);
        assert_eq!(b.dirty, before_dirty);
    }

    #[test]
    fn substitute_empty_pattern_noop() {
        let mut b = buf_of(&["hello"]);
        let args = SubsArgs {
            range: SubsRange::All,
            pattern: "",
            replacement: "x",
            global: true,
        };
        let r = b.substitute(&args);
        assert_eq!(r.matches, 0);
    }

    #[test]
    fn substitute_replacement_longer_than_pattern() {
        let mut b = buf_of(&["aaa"]);
        let args = parse_substitute("s/a/XX/g").unwrap().unwrap();
        let r = b.substitute(&args);
        assert_eq!(r.matches, 3);
        assert_eq!(b.lines[0], "XXXXXX");
    }

    #[test]
    fn substitute_with_unicode() {
        let mut b = buf_of(&["café café"]);
        let args = parse_substitute("s/café/tea/g").unwrap().unwrap();
        let r = b.substitute(&args);
        assert_eq!(r.matches, 2);
        assert_eq!(b.lines[0], "tea tea");
    }
}
