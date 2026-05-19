//! Workspace-root discovery for the `initialize` handshake.
//!
//! Two entry points:
//! - [`find_root_upward`] — climb the parent chain looking for a marker.
//! - [`discover_root`] — pick a root for the current `cwd`, walking up
//!   from the opened file's parent toward `cwd` (and beyond, when the
//!   file lives outside `cwd`'s subtree).

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

/// Walk up from `start_dir` toward (and including) `cap`, looking for a
/// marker. Returns `None` if no marker is found before passing `cap`.
///
/// Used when the opened file lives inside the workspace root — we don't
/// want to escape above the workspace and accidentally land on a
/// monorepo parent or unrelated project.
fn find_root_upward_capped(start_dir: &Path, cap: &Path, markers: &[String]) -> Option<PathBuf> {
    let abs = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    let mut cur: &Path = &abs;
    loop {
        if markers.iter().any(|m| cur.join(m).exists()) {
            return Some(cur.to_path_buf());
        }
        if cur == cap {
            return None;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

/// Resolve a workspace root for `file` against a stable anchor `cwd`.
///
/// Strategy:
/// 1. If `markers` is empty, return canonicalized `cwd`.
/// 2. If `cwd` itself contains a marker, return it — no walking needed.
/// 3. If `file` is provided and lives inside `cwd`'s subtree, walk up
///    from the file's parent toward `cwd` looking for a marker. The
///    walk is capped at `cwd` so we don't escape into a monorepo
///    parent or unrelated workspace.
/// 4. If `file` is provided and lives outside `cwd`'s subtree, walk
///    up unbounded from the file's parent — covers
///    `vorto ../other_project/main.rs` from an unrelated cwd.
/// 5. Otherwise return canonicalized `cwd`.
pub fn discover_root(cwd: &Path, file: Option<&Path>, markers: &[String]) -> PathBuf {
    let cwd_abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if markers.is_empty() {
        return cwd_abs;
    }
    if markers.iter().any(|m| cwd_abs.join(m).exists()) {
        return cwd_abs;
    }
    if let Some(file) = file {
        let file_abs = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        if let Some(parent) = file_abs.parent() {
            if file_abs.starts_with(&cwd_abs) {
                if let Some(found) = find_root_upward_capped(parent, &cwd_abs, markers) {
                    return found;
                }
            } else {
                return find_root_upward(parent, markers);
            }
        }
    }
    cwd_abs
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
    fn discover_root_walks_up_from_file_to_subproject() {
        // cwd is a monorepo root with no marker; the opened file lives in
        // a subproject that has one. The upward walk from the file's
        // parent should surface that subproject's root.
        let tmp = std::env::temp_dir().join(format!("vorto-disc2-{}", std::process::id()));
        let subproj = tmp.join("apps/foo");
        let src = subproj.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(subproj.join("Cargo.toml"), "").unwrap();
        let file = src.join("main.rs");
        std::fs::write(&file, "").unwrap();
        let root = discover_root(&tmp, Some(&file), &["Cargo.toml".to_string()]);
        assert_eq!(root, subproj.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_falls_back_to_cwd_when_no_marker() {
        // No marker anywhere between file and cwd — return cwd as anchor.
        let tmp = std::env::temp_dir().join(format!("vorto-disc3-{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("main.rs");
        std::fs::write(&file, "").unwrap();
        let root = discover_root(&tmp, Some(&file), &["Cargo.toml".to_string()]);
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
    fn discover_root_upward_walk_does_not_escape_cwd() {
        // Marker sits ABOVE cwd. The upward walk must stop at cwd rather
        // than escaping into the parent (which could be an unrelated
        // monorepo root or someone else's workspace).
        let outer = std::env::temp_dir().join(format!("vorto-disc5-{}", std::process::id()));
        let cwd = outer.join("inner");
        let src = cwd.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(outer.join("Cargo.toml"), "").unwrap();
        let file = src.join("main.rs");
        std::fs::write(&file, "").unwrap();
        let root = discover_root(&cwd, Some(&file), &["Cargo.toml".to_string()]);
        assert_eq!(root, cwd.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&outer);
    }
}
