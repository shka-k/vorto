//! Fetch a grammar repo, produce a `.so`/`.dylib`/`.dll` in
//! `grammar_dir`, and write the vendored query files into
//! `query_dir/<name>/` so the editor's highlighter has something to
//! consume.
//!
//! Strategy: shell out to `git` for the fetch and to `tree-sitter` for
//! the build. Both are required to be on `PATH` — we want the same
//! toolchain the grammar's own README uses, not a half-baked
//! reimplementation. We probe for them upfront so the user gets a clear
//! "install X" error instead of a cryptic "process exited with status 1".
//!
//! The clone lives in `$TMPDIR/vorto-grammar-<name>-<pid>` and is wiped
//! after the build (best-effort — leaving turds behind is annoying but
//! not catastrophic, hence no `?` on the cleanup).
//!
//! Query files come from [`crate::grammar::assets`], which is a
//! `include_dir!` of the `assets/queries/<lang>/*.scm` tree vendored in
//! this repo. We intentionally don't read the clone's own `queries/`
//! at install time: vendoring keeps queries pinned to whatever revision
//! we audited, and decouples query availability from upstream layout
//! changes (modern tree-sitter-typescript and similar have shuffled
//! their queries around more than once).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::recipe::GrammarRecipe;

/// Platform-appropriate shared-library extension. The loader accepts
/// `.so`, `.dylib`, and `.dll` regardless of platform, but we emit the
/// native one so the artifact looks normal to the rest of the system.
pub fn dylib_ext() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    }
}

/// Summary of files written by a successful [`install`] call. Lets the
/// CLI layer report exactly what landed on disk without re-walking the
/// filesystem.
pub struct InstallReport {
    pub library: PathBuf,
    pub queries: Vec<PathBuf>,
}

/// Build and install a single grammar.
///
/// On success, `<grammar_dir>/<recipe.name>.<dylib_ext()>` exists and is
/// loadable by [`crate::syntax::Loader`], and any `*.scm` files shipped
/// in the repo's `queries/` directory (subpath-relative for monorepos)
/// are copied into `<query_dir>/<recipe.name>/`. Missing `queries/` is
/// not an error — the grammar simply has no upstream queries, and the
/// caller can drop their own under the same path later.
///
/// On failure, returns an `anyhow::Error` with enough context to point
/// the user at the failing step (clone, checkout, or build).
pub fn install(
    recipe: &GrammarRecipe,
    grammar_dir: &Path,
    query_dir: &Path,
) -> Result<InstallReport> {
    ensure_tool("git", "git is required to fetch grammar sources")?;
    ensure_tool(
        "tree-sitter",
        "tree-sitter CLI is required to build grammars (try `cargo install tree-sitter-cli` or `npm i -g tree-sitter-cli`)",
    )?;

    std::fs::create_dir_all(grammar_dir)
        .with_context(|| format!("creating grammar dir {}", grammar_dir.display()))?;

    let clone_root = tmp_clone_dir(recipe.name);
    // Wipe any stale leftover from a prior failed run — `git clone` will
    // refuse to write into a non-empty directory.
    let _ = std::fs::remove_dir_all(&clone_root);

    clone(recipe, &clone_root)?;

    let build_dir = match recipe.subpath {
        Some(sub) => clone_root.join(sub),
        None => clone_root.clone(),
    };
    if !build_dir.exists() {
        // Subpath was specified but the repo layout doesn't match — most
        // likely the upstream restructured. Surface it clearly.
        let _ = std::fs::remove_dir_all(&clone_root);
        bail!(
            "subpath `{}` not found in clone of {}",
            recipe.subpath.unwrap_or(""),
            recipe.repo
        );
    }

    let library = grammar_dir.join(format!("{}.{}", recipe.name, dylib_ext()));
    build(&build_dir, &library)?;
    let queries = write_vendored_queries(query_dir, recipe.name)?;

    // Best-effort cleanup — losing tmp space is the only consequence.
    let _ = std::fs::remove_dir_all(&clone_root);

    Ok(InstallReport { library, queries })
}

/// Materialize the compile-time-embedded `.scm` files for `name` into
/// `<query_dir>/<name>/`. Returns the list of destination paths
/// actually written. A language with no vendored queries returns an
/// empty vec — the caller surfaces that to the user.
///
/// Public so the CLI layer can offer a queries-only refresh
/// (`grammar install-queries`) without having to rebuild the `.so`.
pub fn write_vendored_queries(query_dir: &Path, name: &str) -> Result<Vec<PathBuf>> {
    let files = super::assets::files_for(name);
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let dest_dir = query_dir.join(name);
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating query dir {}", dest_dir.display()))?;

    let mut written = Vec::new();
    for file in files {
        // `file.path()` is relative to `assets/queries/`, e.g.
        // `rust/highlights.scm` — strip the leading lang dir to get the
        // bare filename.
        let Some(filename) = file.path().file_name() else {
            continue;
        };
        // Skip anything that isn't `.scm` — `include_dir!` can pick up
        // README/LICENSE files if someone drops one alongside the
        // queries.
        if file
            .path()
            .extension()
            .and_then(|s| s.to_str())
            != Some("scm")
        {
            continue;
        }
        let dst = dest_dir.join(filename);
        std::fs::write(&dst, file.contents())
            .with_context(|| format!("writing {}", dst.display()))?;
        written.push(dst);
    }
    Ok(written)
}

/// Remove the installed library for `name`, if it exists. Returns
/// `Ok(true)` when a file was removed, `Ok(false)` when nothing was
/// there to remove.
pub fn remove(name: &str, grammar_dir: &Path) -> Result<bool> {
    let mut removed = false;
    // Try every extension we might have written under, in case the user
    // switched platforms (or installed from a tarball using a different
    // suffix).
    for ext in ["so", "dylib", "dll"] {
        let p = grammar_dir.join(format!("{}.{}", name, ext));
        if p.exists() {
            std::fs::remove_file(&p)
                .with_context(|| format!("removing {}", p.display()))?;
            removed = true;
        }
    }
    Ok(removed)
}

/// Where on disk a grammar would land if installed. Used by the `list`
/// command to report installed/missing without trying to load.
pub fn installed_path(name: &str, grammar_dir: &Path) -> Option<PathBuf> {
    for ext in ["so", "dylib", "dll"] {
        let p = grammar_dir.join(format!("{}.{}", name, ext));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// True when both the shared library and every bundled `.scm` query for
/// `name` already exist on disk. Used by `install` to skip work that
/// would be a no-op. A grammar with no bundled queries counts as
/// "installed" once the library is present.
pub fn is_fully_installed(name: &str, grammar_dir: &Path, query_dir: &Path) -> bool {
    if installed_path(name, grammar_dir).is_none() {
        return false;
    }
    let bundled = super::assets::bundled_query_names(name);
    if bundled.is_empty() {
        return true;
    }
    let installed: std::collections::HashSet<String> =
        installed_queries(name, query_dir).into_iter().collect();
    bundled.iter().all(|n| installed.contains(n))
}

/// Names of the `.scm` files installed under `query_dir/<name>/`
/// (without extension). Empty vec when the directory doesn't exist or
/// has no `.scm` files. Used by `list` to summarize query status.
pub fn installed_queries(name: &str, query_dir: &Path) -> Vec<String> {
    let dir = query_dir.join(name);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("scm") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            out.push(stem.to_string());
        }
    }
    out.sort();
    out
}

fn tmp_clone_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("vorto-grammar-{}-{}", name, std::process::id()))
}

fn clone(recipe: &GrammarRecipe, dest: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone");
    // Shallow clone for the default-branch case; if a rev is pinned we
    // need full history to check it out by SHA.
    if recipe.rev.is_none() {
        cmd.args(["--depth", "1"]);
    }
    cmd.arg(recipe.repo).arg(dest);

    let status = cmd
        .status()
        .with_context(|| format!("spawning `git clone {}`", recipe.repo))?;
    if !status.success() {
        bail!("git clone failed for {}", recipe.repo);
    }

    if let Some(rev) = recipe.rev {
        let status = Command::new("git")
            .args(["checkout", rev])
            .current_dir(dest)
            .status()
            .context("spawning `git checkout`")?;
        if !status.success() {
            bail!("git checkout {} failed in {}", rev, dest.display());
        }
    }
    Ok(())
}

fn build(build_dir: &Path, out_path: &Path) -> Result<()> {
    // `tree-sitter build -o <path>` runs `generate` if needed and then
    // compiles the parser + scanner into a shared library at the given
    // path. It looks at `tree-sitter.json` / `grammar.js` in the cwd, so
    // we run it from `build_dir`.
    let status = Command::new("tree-sitter")
        .arg("build")
        .arg("-o")
        .arg(out_path)
        .current_dir(build_dir)
        .status()
        .context("spawning `tree-sitter build`")?;
    if !status.success() {
        bail!(
            "tree-sitter build failed in {} (output: {})",
            build_dir.display(),
            out_path.display()
        );
    }
    Ok(())
}

/// Verify that `name` resolves on `PATH`; otherwise produce a
/// user-facing error.
fn ensure_tool(name: &str, hint: &str) -> Result<()> {
    // `command -v` is the portable POSIX test; on Windows we'd need
    // `where`, but tree-sitter dev there typically goes through
    // WSL/git-bash anyway. Use `which`-style via PATH walk to stay
    // shell-independent.
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow!("PATH is unset"))?;
    let exe_suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for suf in exe_suffixes {
            let candidate = dir.join(format!("{}{}", name, suf));
            if candidate.is_file() {
                return Ok(());
            }
        }
    }
    Err(anyhow!("{} not found in PATH — {}", name, hint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dylib_ext_is_platform_native() {
        let ext = dylib_ext();
        assert!(matches!(ext, "so" | "dylib" | "dll"));
    }

    #[test]
    fn installed_path_returns_none_for_missing() {
        let dir = std::env::temp_dir();
        // Vanishingly unlikely to collide.
        assert!(installed_path("vorto-test-no-such-grammar-xyz", &dir).is_none());
    }
}
