//! Workspace-root discovery for the `initialize` handshake.
//!
//! Two entry points:
//! - [`find_root_upward`] — climb the parent chain looking for a marker.
//! - [`discover_root`] — pick a root for the current `cwd`, falling
//!   through to BFS-into-subdirs and the upward walk as needed.

use std::path::{Path, PathBuf};

/// Walk up from `start_dir` looking for the first directory that contains
/// any of `markers`. Falls back to `start_dir` itself when nothing matches.
///
/// We canonicalize first because `Path::parent()` only strips a trailing
/// component — for a relative path like `src/main.rs` it'd bottom out at
/// `""` after one step instead of climbing into the real filesystem,
/// which would cause us to report a workspace root that doesn't contain
/// the marker (and rust-analyzer to fail with "Failed to discover
/// workspace").
fn find_root_upward(start_dir: &Path, markers: &[String]) -> PathBuf {
    let abs = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    if markers.is_empty() {
        return abs;
    }
    let mut cur: &Path = &abs;
    loop {
        if markers.iter().any(|m| cur.join(m).exists()) {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return abs.clone(),
        }
    }
}

/// Resolve a workspace root for `file` against a stable anchor `cwd`.
///
/// Strategy:
/// 1. If `markers` is empty, return canonicalized `cwd`.
/// 2. If `cwd` itself contains a marker, return it.
/// 3. BFS from `cwd` into subdirectories (capped depth, common build /
///    VCS dirs skipped) for a marker. First match wins.
/// 4. If `file` is provided **and** lives outside `cwd`'s subtree, fall
///    back to [`find_root_upward`] from the file's parent — that covers
///    `vorto ../other_project/main.rs` from an unrelated cwd.
/// 5. Otherwise return canonicalized `cwd`.
///
/// We deliberately don't walk **up** from `cwd`. The user being in this
/// directory is a signal; escaping it could land on a monorepo parent
/// or other unrelated workspace.
pub fn discover_root(cwd: &Path, file: Option<&Path>, markers: &[String]) -> PathBuf {
    let cwd_abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if markers.is_empty() {
        return cwd_abs;
    }
    if markers.iter().any(|m| cwd_abs.join(m).exists()) {
        return cwd_abs;
    }
    if let Some(found) = bfs_for_marker(&cwd_abs, markers) {
        return found;
    }
    if let Some(file) = file {
        let file_abs = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        let outside_cwd = !file_abs.starts_with(&cwd_abs);
        if outside_cwd && let Some(parent) = file_abs.parent() {
            return find_root_upward(parent, markers);
        }
    }
    cwd_abs
}

/// Max directory depth scanned by [`discover_root`]'s descent. Chosen to
/// cover typical monorepo layouts (`apps/<name>/Cargo.toml`,
/// `packages/<name>/package.json`) without melting on huge trees.
const DESCEND_MAX_DEPTH: usize = 6;

/// Directories skipped during descent — anything noisy, generated, or
/// containing nested dependency manifests we don't want to mistake for
/// the user's own project root.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".direnv",
    ".cache",
    ".idea",
    ".vscode",
];

fn bfs_for_marker(root: &Path, markers: &[String]) -> Option<PathBuf> {
    use std::collections::VecDeque;
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    while let Some((dir, depth)) = queue.pop_front() {
        if depth > 0 && markers.iter().any(|m| dir.join(m).exists()) {
            return Some(dir);
        }
        if depth >= DESCEND_MAX_DEPTH {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            // Skip all dotdirs — keeps results predictable and avoids
            // wandering into `.git`/`.cache`/etc. that we'd otherwise
            // have to enumerate by name.
            if name_s.starts_with('.') {
                continue;
            }
            if SKIP_DIRS.iter().any(|d| *d == name_s) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                queue.push_back((path, depth + 1));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_root_upward_walks_to_marker() {
        let tmp = std::env::temp_dir().join(format!("vorto-lsp-{}", std::process::id()));
        let inner = tmp.join("a/b/c");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        let root = find_root_upward(&inner, &["Cargo.toml".to_string()]);
        // Compare canonicalised — temp dirs on macOS resolve via /private.
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_root_upward_handles_relative_path() {
        // The pre-fix bug: a relative path bottomed out at "" after one
        // parent() step and reported the start dir instead of climbing.
        let tmp = std::env::temp_dir().join(format!("vorto-lsp-rel-{}", std::process::id()));
        let inner = tmp.join("nested");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        let root = find_root_upward(&inner, &["Cargo.toml".to_string()]);
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_picks_cwd_when_marker_at_cwd() {
        let tmp = std::env::temp_dir().join(format!("vorto-disc1-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        let root = discover_root(&tmp, None, &["Cargo.toml".to_string()]);
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_descends_into_subdir() {
        // cwd has no Cargo.toml; one of its grandchildren does. BFS must
        // surface that nested project.
        let tmp = std::env::temp_dir().join(format!("vorto-disc2-{}", std::process::id()));
        let nested = tmp.join("apps/foo");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Cargo.toml"), "").unwrap();
        let root = discover_root(&tmp, None, &["Cargo.toml".to_string()]);
        assert_eq!(root, nested.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_falls_back_to_cwd_when_no_marker() {
        let tmp = std::env::temp_dir().join(format!("vorto-disc3-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let root = discover_root(&tmp, None, &["Cargo.toml".to_string()]);
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_walks_up_for_outside_file() {
        // cwd is empty; the file lives in a separate tree that does have
        // a marker further up. Fall through to upward walk from the
        // file's parent rather than reporting cwd.
        let tmp = std::env::temp_dir().join(format!("vorto-disc4-{}", std::process::id()));
        let other = std::env::temp_dir().join(format!("vorto-disc4other-{}", std::process::id()));
        let nested = other.join("src");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(other.join("Cargo.toml"), "").unwrap();
        let file = nested.join("main.rs");
        std::fs::write(&file, "").unwrap();
        let root = discover_root(&tmp, Some(&file), &["Cargo.toml".to_string()]);
        assert_eq!(root, other.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn discover_root_skips_target_dir() {
        // Make sure descent doesn't dive into `target/` etc. where vendored
        // crates can have their own Cargo.toml.
        let tmp = std::env::temp_dir().join(format!("vorto-disc5-{}", std::process::id()));
        let bogus = tmp.join("target/debug/some_crate");
        std::fs::create_dir_all(&bogus).unwrap();
        std::fs::write(bogus.join("Cargo.toml"), "").unwrap();
        let root = discover_root(&tmp, None, &["Cargo.toml".to_string()]);
        // Should fall back to cwd, NOT find the Cargo.toml under target/.
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
