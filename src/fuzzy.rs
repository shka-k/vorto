use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuzzyKind {
    Files,
    Lines,
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
    pub fn files(root: &Path) -> Self {
        let mut items = Vec::new();
        collect_files(root, root, &mut items, 0);
        items.sort();
        let mut f = Self {
            kind: FuzzyKind::Files,
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

/// Subsequence-based fuzzy match. Returns (score, char positions) on match.
/// Higher score is better. Bonuses for: consecutive matches, word boundary
/// starts (after `/`, `_`, `-`, `.`, ` `), and matching at the start.
pub fn fuzzy_match(haystack: &str, needle: &str) -> Option<(i32, Vec<usize>)> {
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }
    let hay: Vec<char> = haystack.chars().collect();
    let ndl: Vec<char> = needle.chars().collect();

    let mut positions = Vec::with_capacity(ndl.len());
    let mut score: i32 = 0;
    let mut last_match: Option<usize> = None;
    let mut ni = 0;

    for (hi, &hc) in hay.iter().enumerate() {
        if ni >= ndl.len() {
            break;
        }
        if hc.eq_ignore_ascii_case(&ndl[ni]) {
            score += 10;
            if hc == ndl[ni] {
                score += 2; // exact case bonus
            }
            if hi == 0 {
                score += 8;
            } else {
                let prev = hay[hi - 1];
                if matches!(prev, '/' | '_' | '-' | '.' | ' ') {
                    score += 6;
                }
            }
            if let Some(lm) = last_match {
                if hi == lm + 1 {
                    score += 12;
                } else {
                    score -= (hi - lm - 1) as i32;
                }
            }
            positions.push(hi);
            last_match = Some(hi);
            ni += 1;
        }
    }

    if ni == ndl.len() {
        // shorter haystacks are preferred when otherwise tied
        score -= (hay.len() as i32) / 4;
        Some((score, positions))
    } else {
        None
    }
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
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
        if name.starts_with('.') {
            continue;
        }
        if matches!(name.as_str(), "target" | "node_modules" | "dist" | "build") {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out, depth + 1);
        } else if path.is_file()
            && let Ok(rel) = path.strip_prefix(root)
            && let Some(s) = rel.to_str()
        {
            out.push(s.to_string());
        }
    }
}
