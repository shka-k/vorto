use std::fs;
use std::path::Path;

/// Filter toggles for the fuzzy file picker. Both axes are kept
/// available even though the current keymap only flips `hidden`
/// (`<space>f` vs `<space>F`) — the `vcs` flag stays in the data
/// model so a config or future binding can opt out of `.gitignore`
/// without changing types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IgnoreOpts {
    /// Honor VCS ignore rules. When true and we're inside a git repo
    /// the source is `git ls-files --cached --others
    /// --exclude-standard`; outside a repo the walker still applies
    /// its small build-dir blacklist as a stand-in.
    pub vcs: bool,
    /// Drop any path with a dotfile segment (`.github/...`, `.env`,
    /// etc.).
    pub hidden: bool,
}

impl IgnoreOpts {
    /// Standard `<space>f` behavior: filter both gitignored and hidden.
    pub const DEFAULT: Self = Self {
        vcs: true,
        hidden: true,
    };
    /// `<space>F` behavior: still respect `.gitignore`, but surface
    /// dotfiles.
    pub const SHOW_HIDDEN: Self = Self {
        vcs: true,
        hidden: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuzzyKind {
    /// Fuzzy file picker. See [`IgnoreOpts`] for the filter axes.
    Files {
        ignore: IgnoreOpts,
    },
    Lines,
    /// Cross-file location results (LSP references). The Finder carries
    /// a parallel `locations` Vec so the picker can jump on selection.
    Locations,
    /// Recently-opened files (MRU). Display strings are paths
    /// (typically relative to startup_cwd); the
    /// [`PromptController`](crate::prompt::PromptController) keeps a
    /// parallel `buffer_paths` Vec for the absolute path to actually
    /// open on selection.
    Buffers,
}

#[derive(Debug, Clone)]
pub struct MatchItem {
    pub idx: usize,
    pub score: i32,
    pub positions: Vec<usize>,
}

#[derive(Debug)]
pub struct Finder {
    pub kind: FuzzyKind,
    pub query: String,
    /// Char index of the insertion point into `query`, in `[0, char_count]`.
    pub cursor: usize,
    pub items: Vec<String>,
    pub matches: Vec<MatchItem>,
    pub selected: usize,
}

impl Finder {
    pub fn files(root: &Path, ignore: IgnoreOpts) -> Self {
        // Prefer git when VCS filtering is on AND we're in a repo — it's
        // both faster and exact (matches `.gitignore`, global excludes,
        // etc.). The hidden filter is applied as a post-pass since git
        // doesn't know about our dotfile convention.
        let mut items = if ignore.vcs
            && let Some(paths) = crate::vcs::tracked_files(root)
        {
            paths
                .into_iter()
                .filter(|p| !ignore.hidden || !is_hidden_path(p))
                .filter(|p| !is_symlink(&root.join(p)))
                .take(5000)
                .collect()
        } else {
            let mut v = Vec::new();
            collect_files(root, root, &mut v, 0, ignore);
            v
        };
        items.sort();
        let mut f = Self {
            kind: FuzzyKind::Files { ignore },
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            cursor: 0,
        };
        f.refilter();
        f
    }

    pub fn lines(buffer_lines: &[String]) -> Self {
        let items: Vec<String> = buffer_lines.to_vec();
        let mut f = Self {
            kind: FuzzyKind::Lines,
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            cursor: 0,
        };
        f.refilter();
        f
    }

    /// Build a [`FuzzyKind::Buffers`] picker. `items` are the display
    /// strings (newest first); the caller stashes the absolute path
    /// for each one separately and uses `selection().idx` to look it
    /// up on submit.
    pub fn buffers(items: Vec<String>) -> Self {
        let mut f = Self {
            kind: FuzzyKind::Buffers,
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            cursor: 0,
        };
        f.refilter();
        f
    }

    /// Build a [`FuzzyKind::Locations`] picker. Display strings are
    /// arbitrary; the caller keeps a parallel `Vec` (typically of
    /// `lsp::Location`) and looks up the selected index to decide what
    /// to do on submit.
    pub fn locations(items: Vec<String>) -> Self {
        let mut f = Self {
            kind: FuzzyKind::Locations,
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
            cursor: 0,
        };
        f.refilter();
        f
    }

    fn char_len(&self) -> usize {
        self.query.chars().count()
    }

    fn byte_idx(&self, char_idx: usize) -> usize {
        self.query
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.query.len())
    }

    fn insert(&mut self, c: char) {
        let byte = self.byte_idx(self.cursor);
        self.query.insert(byte, c);
        self.cursor += 1;
        self.refilter();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_idx(self.cursor);
        let start = self.byte_idx(self.cursor - 1);
        self.query.replace_range(start..end, "");
        self.cursor -= 1;
        self.refilter();
    }

    fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let start = self.byte_idx(self.cursor);
        let end = self.byte_idx(self.cursor + 1);
        self.query.replace_range(start..end, "");
        self.refilter();
    }

    pub fn apply_line_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right if self.cursor < self.char_len() => self.cursor += 1,
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.char_len(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl && self.cursor < self.char_len() => self.cursor += 1,
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.char_len(),
            KeyCode::Char(c) if !ctrl => self.insert(c),
            _ => {}
        }
    }

    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
        }
    }

    pub fn prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selection(&self) -> Option<&MatchItem> {
        self.matches.get(self.selected)
    }

    fn refilter(&mut self) {
        self.matches.clear();
        if self.query.is_empty() {
            for (i, _) in self.items.iter().enumerate().take(500) {
                self.matches.push(MatchItem {
                    idx: i,
                    score: 0,
                    positions: Vec::new(),
                });
            }
        } else {
            for (i, item) in self.items.iter().enumerate() {
                if let Some((score, positions)) = fuzzy_match(item, &self.query) {
                    self.matches.push(MatchItem {
                        idx: i,
                        score,
                        positions,
                    });
                }
            }
            self.matches.sort_by_key(|m| -m.score);
            self.matches.truncate(500);
        }
        self.selected = 0;
    }
}

// Smith-Waterman-style scoring constants, matching nucleo/fzf v2 so
// the picker ranks results the way Helix users expect. Word-boundary
// hits dominate; long gaps are punished but not lethal.
const SCORE_MATCH: i32 = 16;
const SCORE_GAP_START: i32 = -3;
const SCORE_GAP_EXTEND: i32 = -1;
const BONUS_BOUNDARY: i32 = SCORE_MATCH / 2; // 8
const BONUS_CAMEL: i32 = BONUS_BOUNDARY - 1; // 7
const BONUS_CONSECUTIVE: i32 = -(SCORE_GAP_START + SCORE_GAP_EXTEND); // 4
const BONUS_FIRST_CHAR_MULT: i32 = 2;
const SCORE_NEG_INF: i32 = i32::MIN / 4;

#[derive(Copy, Clone, PartialEq)]
enum CharKind {
    NonWord,
    Lower,
    Upper,
    Number,
}

fn char_kind(c: char) -> CharKind {
    if c.is_ascii_lowercase() {
        CharKind::Lower
    } else if c.is_ascii_uppercase() {
        CharKind::Upper
    } else if c.is_ascii_digit() {
        CharKind::Number
    } else if c.is_alphanumeric() {
        // Treat non-ASCII letters (e.g. CJK) as word characters so a
        // transition from a separator into them still earns the
        // boundary bonus.
        CharKind::Lower
    } else {
        CharKind::NonWord
    }
}

fn boundary_bonus(prev: CharKind, curr: CharKind) -> i32 {
    use CharKind::*;
    match (prev, curr) {
        (NonWord, c) if c != NonWord => BONUS_BOUNDARY,
        (Lower, Upper) => BONUS_CAMEL,
        (Lower | Upper, Number) => BONUS_CAMEL,
        _ => 0,
    }
}

/// Sub-sequence fuzzy match, modelled on Helix's nucleo / fzf v2.
/// Each needle character must appear in the haystack in order; gaps
/// between matches are permitted but penalised. Returns `(score, char
/// positions)` where positions are the indices of the matched needle
/// characters in the haystack (used for highlighting).
///
/// Scoring rewards: word-boundary anchored matches (after `/`, `_`,
/// `-`, `.`, ` ` or at start), camelCase boundaries
/// (`fooBar`-style), consecutive matches, and exact-case characters.
/// Scoring penalises every skipped haystack character (affine gap:
/// the first skip costs `SCORE_GAP_START`, each subsequent skip costs
/// `SCORE_GAP_EXTEND`).
pub fn fuzzy_match(haystack: &str, needle: &str) -> Option<(i32, Vec<usize>)> {
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }
    let hay: Vec<char> = haystack.chars().collect();
    let ndl: Vec<char> = needle.chars().collect();
    let n = ndl.len();
    let m = hay.len();
    if n > m {
        return None;
    }

    // Per-position transition bonus: `bonus[j]` is what you earn for
    // matching at `hay[j]` given the kind of `hay[j-1]`. Position 0
    // is treated as if preceded by a NonWord character, so a match
    // there is always a boundary hit.
    let mut bonus = vec![0i32; m];
    let mut prev_kind = CharKind::NonWord;
    for (j, &c) in hay.iter().enumerate() {
        let k = char_kind(c);
        bonus[j] = boundary_bonus(prev_kind, k);
        prev_kind = k;
    }

    // Two DP tables. `mscore[i][j]` is the best score for matching
    // needle[..=i] with `hay[j]` taken as the final match. `gscore[i][j]`
    // is the best score for matching needle[..=i] when `hay[j]` is *not*
    // a match — i.e. we're currently extending a gap that follows the
    // last needle char. Parent tables record where the predecessor
    // needle character landed, so we can rebuild the position list.
    let cell = |i: usize, j: usize| i * m + j;
    let mut mscore = vec![SCORE_NEG_INF; n * m];
    let mut gscore = vec![SCORE_NEG_INF; n * m];
    let mut mparent = vec![usize::MAX; n * m];
    let mut gmatch = vec![usize::MAX; n * m]; // gscore traceback: where needle[i] matched

    for i in 0..n {
        for j in i..m {
            let nc = ndl[i];
            let hc = hay[j];
            let is_match = nc.eq_ignore_ascii_case(&hc);

            if is_match {
                let case_bonus = if nc == hc { 1 } else { 0 };
                let ms = if i == 0 {
                    SCORE_MATCH + bonus[j] * BONUS_FIRST_CHAR_MULT + case_bonus
                } else if j == 0 {
                    SCORE_NEG_INF
                } else {
                    let from_m = mscore[cell(i - 1, j - 1)];
                    let from_g = gscore[cell(i - 1, j - 1)];
                    let consec_bonus = BONUS_CONSECUTIVE.max(bonus[j]);
                    let via_m = from_m.saturating_add(SCORE_MATCH + consec_bonus + case_bonus);
                    let via_g = from_g.saturating_add(SCORE_MATCH + bonus[j] + case_bonus);
                    if from_m == SCORE_NEG_INF && from_g == SCORE_NEG_INF {
                        SCORE_NEG_INF
                    } else if via_m >= via_g {
                        mparent[cell(i, j)] = j - 1;
                        via_m
                    } else {
                        // Predecessor's match position lives in the gap-trace table.
                        mparent[cell(i, j)] = gmatch[cell(i - 1, j - 1)];
                        via_g
                    }
                };
                mscore[cell(i, j)] = ms;
            }

            if j > 0 {
                let from_m = mscore[cell(i, j - 1)];
                let from_g = gscore[cell(i, j - 1)];
                let start = from_m.saturating_add(SCORE_GAP_START);
                let extend = from_g.saturating_add(SCORE_GAP_EXTEND);
                if from_m == SCORE_NEG_INF && from_g == SCORE_NEG_INF {
                    // no predecessor yet
                } else if extend >= start {
                    gscore[cell(i, j)] = extend;
                    gmatch[cell(i, j)] = gmatch[cell(i, j - 1)];
                } else {
                    gscore[cell(i, j)] = start;
                    gmatch[cell(i, j)] = j - 1;
                }
            }
        }
    }

    // The match must end on an actual needle character — trailing
    // characters are unmatched and don't contribute. Pick the column
    // where the last needle row scores highest.
    let mut best_score = SCORE_NEG_INF;
    let mut best_j = usize::MAX;
    for j in (n - 1)..m {
        let s = mscore[cell(n - 1, j)];
        if s > best_score {
            best_score = s;
            best_j = j;
        }
    }
    if best_j == usize::MAX || best_score == SCORE_NEG_INF {
        return None;
    }

    // Walk parent pointers back from (n-1, best_j) collecting match
    // positions for highlighting.
    let mut positions = Vec::with_capacity(n);
    let mut i = n - 1;
    let mut j = best_j;
    positions.push(j);
    while i > 0 {
        let pj = mparent[cell(i, j)];
        if pj == usize::MAX {
            return None;
        }
        j = pj;
        i -= 1;
        positions.push(j);
    }
    positions.reverse();
    Some((best_score, positions))
}

/// True if any path segment starts with `.`. Mirrors the dotfile skip
/// that the manual walker applies, so the git-backed listing doesn't
/// surface `.github/…`, `.cargo/…` etc. in the picker.
fn is_hidden_path(rel: &str) -> bool {
    rel.split('/').any(|seg| seg.starts_with('.'))
}

/// True if `path` is a symlink (without following it). Symlinks are
/// filtered out of the picker because opening one whose target is a
/// directory or broken propagates an `io::Error` from `Buffer::load`
/// up to the main loop and terminates the editor.
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize, ignore: IgnoreOpts) {
    if depth > 12 || out.len() >= 5000 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if ignore.hidden && name.starts_with('.') {
            continue;
        }
        // The hardcoded blacklist stands in for `.gitignore` when we're
        // outside a repo (or git is unavailable). Gated on `ignore.vcs`
        // so a future binding that opts out of VCS filtering also sees
        // into build dirs.
        if ignore.vcs && matches!(name.as_str(), "target" | "node_modules" | "dist" | "build") {
            continue;
        }
        // Use `file_type` (not `is_dir`/`is_file`) so symlinks are
        // detected without being followed: traversing through a
        // directory symlink risks cycles, and listing a file symlink
        // can crash the editor on open (broken target / target is a
        // directory bubbles an io::Error out of the prompt path).
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_files(root, &path, out, depth + 1, ignore);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let rel = path.strip_prefix(root).ok().and_then(|p| p.to_str());
        if let Some(s) = rel {
            out.push(s.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched(haystack: &str, needle: &str) -> Option<String> {
        let (_score, positions) = fuzzy_match(haystack, needle)?;
        let hay: Vec<char> = haystack.chars().collect();
        Some(positions.iter().map(|&i| hay[i]).collect())
    }

    #[test]
    fn skips_dot_separator() {
        let (_score, positions) = fuzzy_match("xx.go", "xxgo").expect("should match");
        assert_eq!(positions, vec![0, 1, 3, 4]);
    }

    #[test]
    fn skips_underscore_and_slash() {
        assert_eq!(matched("foo_bar", "foobar").as_deref(), Some("foobar"));
        assert_eq!(matched("src/foo.rs", "foors").as_deref(), Some("foors"));
        assert_eq!(matched("foo bar", "foobar").as_deref(), Some("foobar"));
    }

    #[test]
    fn case_insensitive() {
        let (_s, pos) = fuzzy_match("README.md", "readme").unwrap();
        assert_eq!(pos, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn camel_case_boundary_preferred() {
        // Both haystacks contain "foobar" as a subsequence, but
        // `fooBar` has a camelCase boundary at 'B' which should rank
        // higher than the flat `foobar`.
        let (camel, _) = fuzzy_match("fooBar", "foob").unwrap();
        let (flat, _) = fuzzy_match("flooba", "foob").unwrap();
        assert!(camel > flat, "camelCase {camel} should outrank flat {flat}");
    }

    #[test]
    fn word_boundary_outranks_mid_word() {
        // `src/foo` should rank higher than `srcafoo` for needle "foo"
        // because the match starts on a path-segment boundary.
        let (boundary, _) = fuzzy_match("src/foo", "foo").unwrap();
        let (mid, _) = fuzzy_match("srcafoo", "foo").unwrap();
        assert!(
            boundary > mid,
            "boundary {boundary} should outrank mid-word {mid}"
        );
    }

    #[test]
    fn subsequence_with_long_gap() {
        // Needle chars need only appear in order anywhere in the
        // haystack — pure fzf-style subsequence matching.
        let (_score, positions) = fuzzy_match("alphabet_soup", "asp").unwrap();
        assert_eq!(positions.len(), 3);
        // Match must be in order and use distinct positions.
        assert!(positions.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn rejects_when_letters_missing() {
        assert!(fuzzy_match("xx.go", "xxrs").is_none());
        assert!(fuzzy_match("abc", "abcd").is_none());
        // Needle character not present anywhere.
        assert!(fuzzy_match("src/foo", "srcz").is_none());
    }
}
