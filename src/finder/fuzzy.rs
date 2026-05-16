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
        };
        f.refilter();
        f
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.refilter();
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

/// Contiguous substring match (case-insensitive). The needle must
/// appear as a single run inside the haystack — typing `f` no longer
/// matches every path with an `f` somewhere; you have to spell out
/// enough of the word to anchor it. Returns `(score, char positions)`
/// where positions cover the matched run for highlighting.
///
/// Scoring (higher is better):
///   * earlier match position is preferred.
///   * the rightmost-found occurrence inside the haystack is picked so
///     `src/foo` typing `foo` highlights the filename, not an earlier
///     stray substring (matters for paths like `src/foo/foo.rs`).
///   * word-boundary anchored matches (after `/`, `_`, `-`, `.`, ` `,
///     or at the very start) get a bonus.
///   * exact-case characters add a small bonus per char.
///   * shorter haystacks win on ties.
pub fn fuzzy_match(haystack: &str, needle: &str) -> Option<(i32, Vec<usize>)> {
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }
    let hay: Vec<char> = haystack.chars().collect();
    let ndl: Vec<char> = needle.chars().collect();
    if ndl.len() > hay.len() {
        return None;
    }

    // Scan all candidate start positions; keep the one that scores
    // highest. Word-boundary starts dominate, so this typically picks
    // the most semantically obvious occurrence.
    let mut best: Option<(i32, usize)> = None;
    for start in 0..=hay.len() - ndl.len() {
        let mut exact_case = 0i32;
        let mut ok = true;
        for (j, &nc) in ndl.iter().enumerate() {
            let hc = hay[start + j];
            if !hc.eq_ignore_ascii_case(&nc) {
                ok = false;
                break;
            }
            if hc == nc {
                exact_case += 1;
            }
        }
        if !ok {
            continue;
        }
        let at_start = start == 0;
        let at_word_boundary = at_start
            || matches!(hay[start - 1], '/' | '_' | '-' | '.' | ' ');
        let mut score: i32 = 100;
        score -= start as i32; // earlier wins
        if at_start {
            score += 30;
        } else if at_word_boundary {
            score += 20;
        }
        score += exact_case * 2;
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, start));
        }
    }

    let (mut score, start) = best?;
    score -= (hay.len() as i32) / 4;
    let positions = (start..start + ndl.len()).collect();
    Some((score, positions))
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

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<String>,
    depth: usize,
    ignore: IgnoreOpts,
) {
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
