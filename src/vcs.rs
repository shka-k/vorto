//! Minimal git VCS integration.
//!
//! The editor calls into here to learn two things:
//!   - For an opened file, the HEAD blob lines (used as a diff base by
//!     [`diff_line_status`] to drive gutter coloring).
//!   - For the buffer picker, the set of paths that currently differ
//!     from HEAD in the working tree (single `git status --porcelain`).
//!
//! Everything shells out to `git` synchronously. Operations are
//! infrequent (file-open / picker-open) so the process-spawn cost
//! isn't on the hot path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Per-line VCS status for the active buffer, lifted from the line
/// diff between HEAD and the current buffer content.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum LineStatus {
    /// New line not present in HEAD.
    Added,
    /// Line present in HEAD but with different content.
    Modified,
    /// Marker that one or more lines from HEAD were deleted just above
    /// (or, for trailing deletions, just below) this row.
    DeletedAbove,
}

/// Read `HEAD:<repo-relative-path>` for `file`. Returns:
///   - `None` when git isn't available or `file` isn't inside a repo.
///   - `Some(Vec::new())` when the file is in a repo but not yet
///     tracked at HEAD — callers treat every current line as Added.
///   - `Some(lines)` otherwise.
///
/// The lines vector mirrors [`crate::editor::Buffer::load`] shape: we
/// `split('\n')` so a file ending in `\n` produces a trailing empty
/// line, and an empty file produces a single empty line.
pub fn head_blob_lines(file: &Path) -> Option<Vec<String>> {
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    let root = repo_root(parent)?;
    // Use a path the rev-parse just gave us as the anchor; the file
    // path may not exist on disk (e.g. when a buffer is renamed) but
    // we still want a useful relative form.
    let rel = file.strip_prefix(&root).ok()?;
    let rel_str = rel.to_str()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["show", "--textconv"])
        .arg(format!("HEAD:{}", rel_str))
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        // File isn't in HEAD (untracked or newly added). The directory
        // is still a git repo though — return an empty base so the
        // line diff marks every current line as Added.
        return Some(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    Some(lines)
}

/// `git status --porcelain` parsed into a canonical-path set. Each
/// entry corresponds to a path that differs from HEAD in the work
/// tree (modified, staged, deleted, renamed-to). Untracked files (`??`)
/// are included so a freshly-created buffer also shows the picker
/// marker.
///
/// Returns an empty set when not in a git repo or git isn't available.
pub fn changed_files(cwd: &Path) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    let Some(root) = repo_root(cwd) else {
        return set;
    };
    let Some(out) = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["status", "--porcelain", "-z"])
        .stderr(Stdio::null())
        .output()
        .ok()
    else {
        return set;
    };
    if !out.status.success() {
        return set;
    }
    // `-z` separates entries with NUL and emits paths unquoted, which
    // sidesteps the quoting rules of plain porcelain.
    let bytes = out.stdout;
    let mut it = bytes.split(|&b| b == 0).peekable();
    while let Some(entry) = it.next() {
        if entry.len() < 4 {
            continue;
        }
        // First two bytes are the status XY, byte 2 is the separating
        // space, then the path. For renames the *source* path follows
        // as the next NUL-terminated entry — we skip it because the
        // target path (this entry) is what's in the work tree now.
        let xy = &entry[..2];
        let path_bytes = &entry[3..];
        let path_str = match std::str::from_utf8(path_bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let p = root.join(path_str);
        let canon = p.canonicalize().unwrap_or(p);
        set.insert(canon);
        if xy[0] == b'R' || xy[0] == b'C' {
            // Consume the rename/copy source path (we don't need it).
            let _ = it.next();
        }
    }
    set
}

/// Per-line statuses for `current` against `base`. Always returns a
/// vector of exactly `current.len()` entries.
pub fn diff_line_status(base: &[String], current: &[String]) -> Vec<Option<LineStatus>> {
    let m = current.len();
    let mut statuses: Vec<Option<LineStatus>> = vec![None; m];
    if base.is_empty() {
        for s in statuses.iter_mut() {
            *s = Some(LineStatus::Added);
        }
        return statuses;
    }
    if current.is_empty() {
        return statuses;
    }
    let edits = myers_edits(base, current);
    let mut pending_delete: usize = 0;
    for edit in edits {
        match edit {
            Edit::Keep(bi) => {
                if pending_delete > 0 && statuses[bi].is_none() {
                    statuses[bi] = Some(LineStatus::DeletedAbove);
                }
                pending_delete = 0;
            }
            Edit::Insert(bi) => {
                let s = if pending_delete > 0 {
                    pending_delete -= 1;
                    LineStatus::Modified
                } else {
                    LineStatus::Added
                };
                statuses[bi] = Some(s);
            }
            Edit::Delete => {
                pending_delete += 1;
            }
        }
    }
    // Trailing deletions past the end of `current` — attach to the
    // last row so the gutter still signals "lines went missing here".
    if pending_delete > 0 && m > 0 {
        let i = m - 1;
        if statuses[i].is_none() {
            statuses[i] = Some(LineStatus::DeletedAbove);
        }
    }
    statuses
}

#[derive(Debug)]
enum Edit {
    /// Matched line — carries the index in `current` so the
    /// status mapper can clear pending deletions onto the right row.
    Keep(usize),
    /// New line in `current` (relative to `base`).
    Insert(usize),
    /// Deleted line from `base`. We don't need its index — the
    /// status mapper only counts deletions until the next non-delete
    /// edit closes them out.
    Delete,
}

/// Standard Myers O((N+M)D) line diff. Returns the edit script in
/// document order.
fn myers_edits(a: &[String], b: &[String]) -> Vec<Edit> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = (n + m) as usize;
    if max == 0 {
        return Vec::new();
    }
    let offset = max as isize;
    let mut v = vec![0_isize; 2 * max + 1];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found_d: Option<isize> = None;
    'outer: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let i = (k + offset) as usize;
            let mut x: isize = if k == -d || (k != d && v[i - 1] < v[i + 1]) {
                v[i + 1]
            } else {
                v[i - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            if x >= n && y >= m {
                found_d = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    if found_d.is_none() {
        return Vec::new();
    }
    // Backtrack through `trace` reconstructing the edits.
    let mut x = n;
    let mut y = m;
    let mut edits: Vec<Edit> = Vec::new();
    for d in (0..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let i = (k + offset) as usize;
        let prev_k = if k == -d || (k != d && v[i - 1] < v[i + 1]) {
            k + 1
        } else {
            k - 1
        };
        let prev_i = (prev_k + offset) as usize;
        let prev_x = v[prev_i];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            edits.push(Edit::Keep((y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                edits.push(Edit::Insert((y - 1) as usize));
            } else {
                edits.push(Edit::Delete);
            }
        }
        x = prev_x;
        y = prev_y;
    }
    edits.reverse();
    edits
}

fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(ss: &[&str]) -> Vec<String> {
        ss.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pure_addition_marks_each_added_line() {
        let base = lines(&["a", "b", "c"]);
        let cur = lines(&["a", "X", "b", "c"]);
        let st = diff_line_status(&base, &cur);
        assert_eq!(st[0], None);
        assert_eq!(st[1], Some(LineStatus::Added));
        assert_eq!(st[2], None);
        assert_eq!(st[3], None);
    }

    #[test]
    fn pure_modification_marks_modified() {
        let base = lines(&["a", "b", "c"]);
        let cur = lines(&["a", "B!", "c"]);
        let st = diff_line_status(&base, &cur);
        assert_eq!(st[0], None);
        assert_eq!(st[1], Some(LineStatus::Modified));
        assert_eq!(st[2], None);
    }

    #[test]
    fn deletion_marks_line_below() {
        let base = lines(&["a", "b", "c"]);
        let cur = lines(&["a", "c"]);
        let st = diff_line_status(&base, &cur);
        assert_eq!(st[0], None);
        assert_eq!(st[1], Some(LineStatus::DeletedAbove));
    }

    #[test]
    fn trailing_deletion_attaches_to_last_row() {
        let base = lines(&["a", "b", "c"]);
        let cur = lines(&["a", "b"]);
        let st = diff_line_status(&base, &cur);
        assert_eq!(st[0], None);
        assert_eq!(st[1], Some(LineStatus::DeletedAbove));
    }

    #[test]
    fn empty_base_marks_everything_added() {
        let base: Vec<String> = Vec::new();
        let cur = lines(&["a", "b"]);
        let st = diff_line_status(&base, &cur);
        assert_eq!(st, vec![Some(LineStatus::Added), Some(LineStatus::Added)]);
    }

    #[test]
    fn identical_produces_no_markers() {
        let v = lines(&["a", "b", "c"]);
        let st = diff_line_status(&v, &v);
        assert!(st.iter().all(|s| s.is_none()));
    }
}
