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
    ensure_tool(
        "tree-sitter",
        "tree-sitter CLI is required to build grammars (try `cargo install tree-sitter-cli` or `npm i -g tree-sitter-cli`)",
    )?;

    std::fs::create_dir_all(grammar_dir)
        .with_context(|| format!("creating grammar dir {}", grammar_dir.display()))?;

    let clone_root = tmp_clone_dir(recipe.name);
    // Wipe any stale leftover from a prior failed run — both the
    // tarball extractor and `git clone` refuse to write into a
    // non-empty directory.
    let _ = std::fs::remove_dir_all(&clone_root);

    // Prefer a release tarball when the upstream publishes one (and
    // it carries `src/parser.c`) — that saves `tree-sitter generate`,
    // which is the slow step for grammars whose `grammar.js` does
    // expensive expansion (SQL takes ~15s). Fall back to `git clone`
    // when no tarball is available, the URL isn't GitHub-hosted, or
    // anything goes wrong fetching it.
    let used_tarball = match try_release_tarball(recipe, &clone_root) {
        Ok(true) => true,
        Ok(false) => false,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&clone_root);
            false
        }
    };
    if !used_tarball {
        ensure_tool("git", "git is required to fetch grammar sources")?;
        clone(recipe, &clone_root)?;
    }

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
        if file.path().extension().and_then(|s| s.to_str()) != Some("scm") {
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
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
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

/// Try to fetch a release tarball that ships pre-generated parser
/// sources (and unpack it into `dest`). Returns:
///
/// - `Ok(true)` — tarball fetched and extracted, `dest` populated.
/// - `Ok(false)` — no release / not a GitHub URL / no usable asset.
///   Caller falls back to `git clone`.
/// - `Err(_)` — the network call or extraction failed mid-flight.
///   Caller wipes `dest` and falls back to `git clone`.
///
/// "Usable asset" = a `.tar.gz` whose contents contain `src/parser.c`.
/// We don't check that ahead of time — we just try the most likely
/// asset and rely on the subsequent build to fail (and fall through
/// the `Err` path) when the contents aren't what we expected.
fn try_release_tarball(recipe: &GrammarRecipe, dest: &Path) -> Result<bool> {
    let Some((owner, repo)) = parse_github_url(recipe.repo) else {
        return Ok(false);
    };
    if which("curl").is_none() || which("tar").is_none() {
        return Ok(false);
    }
    let api_url = match recipe.rev {
        Some(rev) => format!(
            "https://api.github.com/repos/{}/{}/releases/tags/{}",
            owner, repo, rev
        ),
        None => format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            owner, repo
        ),
    };
    let json = match curl_text(&api_url) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    let v: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    let Some(assets) = v.get("assets").and_then(|a| a.as_array()) else {
        return Ok(false);
    };
    let Some(asset_url) = pick_source_tarball(assets, &repo) else {
        return Ok(false);
    };

    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating tarball dest {}", dest.display()))?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "curl -fsSL '{}' | tar -xz --strip-components=0 -C '{}'",
            asset_url.replace('\'', "'\\''"),
            dest.display(),
        ))
        .status()
        .context("spawning curl|tar pipeline")?;
    if !status.success() {
        bail!("downloading or extracting tarball {} failed", asset_url);
    }

    // Tarballs sometimes ship a single top-level directory
    // (`tree-sitter-sql-v0.3.11/...`). Flatten when we detect that
    // shape so callers can treat `dest` as the build root.
    flatten_single_subdir(dest)?;
    Ok(true)
}

/// `(owner, repo)` for a `github.com/...` URL, else `None`. `.git`
/// suffix and trailing slashes are stripped.
fn parse_github_url(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let trimmed = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = trimmed.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// GET `url` as text. Errors when curl exits non-zero (404, network
/// down, etc.) — caller treats that as "no release to use."
fn curl_text(url: &str) -> Result<String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-fsSL",
        "-H",
        "User-Agent: vorto-grammar-installer",
        "-H",
        "Accept: application/vnd.github+json",
    ]);
    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN"))
        && !token.is_empty()
    {
        cmd.args(["-H", &format!("Authorization: Bearer {}", token)]);
    }
    cmd.arg(url);
    let output = cmd.output().context("spawning curl")?;
    if !output.status.success() {
        bail!("curl {} exited with {}", url, output.status);
    }
    String::from_utf8(output.stdout).context("curl output not utf-8")
}

/// Pick the asset most likely to be a source tarball with the
/// generated parser sources. Heuristic: prefer `.tar.gz` whose name
/// contains the repo name; fall back to the first `.tar.gz` in the
/// list. Returns `None` when no asset looks like a tarball.
///
/// We deliberately avoid platform-specific binaries (`.dylib`, `.so`)
/// — almost no grammar repo ships those, and trying to dlopen
/// `.node` files (the common Node-Native-Module shape) drags in libnode
/// dependencies that aren't safe to assume here.
fn pick_source_tarball(assets: &[serde_json::Value], repo: &str) -> Option<String> {
    let mut tarballs: Vec<(&str, &str)> = assets
        .iter()
        .filter_map(|a| {
            let name = a.get("name")?.as_str()?;
            let url = a.get("browser_download_url")?.as_str()?;
            if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
                Some((name, url))
            } else {
                None
            }
        })
        .collect();
    // Repo-name prefix wins (`tree-sitter-sql-v0.3.11.tar.gz`),
    // anything else acts as a secondary candidate.
    tarballs.sort_by_key(|(name, _)| if name.starts_with(repo) { 0 } else { 1 });
    tarballs.first().map(|(_, url)| url.to_string())
}

/// If `dir` contains exactly one entry and that entry is a directory,
/// move its contents up one level so `dir` itself becomes the build
/// root. No-op otherwise. Some release tarballs nest everything under
/// `<repo>-<tag>/`, others extract flat — this normalizes the shape.
fn flatten_single_subdir(dir: &Path) -> Result<()> {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", dir.display()))?;
    if entries.len() != 1 {
        return Ok(());
    }
    let only = &entries[0];
    if !only.file_type().map(|t| t.is_dir()).unwrap_or(false) {
        return Ok(());
    }
    let inner = only.path();
    for child in std::fs::read_dir(&inner)? {
        let child = child?;
        let from = child.path();
        let to = dir.join(child.file_name());
        std::fs::rename(&from, &to)
            .with_context(|| format!("moving {} -> {}", from.display(), to.display()))?;
    }
    std::fs::remove_dir(&inner).ok();
    Ok(())
}

/// Locate `name` on `PATH`. Returns the resolved path on hit. Used
/// for soft tool probes where missing isn't fatal — for the hard
/// "tool required" check, see [`ensure_tool`].
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for suf in suffixes {
            let candidate = dir.join(format!("{}{}", name, suf));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn clone(recipe: &GrammarRecipe, dest: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    // `--quiet` suppresses the `Cloning into '…'` progress line; with
    // parallel `--all` running 8 clones at once those lines just
    // interleave with our own report buffer.
    cmd.args(["clone", "--quiet"]);
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
    // Generate the C parser source only when it's missing. Most
    // upstreams commit `src/parser.c` + `src/grammar.json` and we
    // trust those — regenerating from `grammar.js` is the source of
    // truth in principle but costs seconds-to-tens-of-seconds for
    // large grammars (e.g. SQL). The handful of repos that skip the
    // commit (DerekStride/tree-sitter-sql) get caught by this fallback.
    if !build_dir.join("src/parser.c").exists() && build_dir.join("grammar.js").exists() {
        let status = Command::new("tree-sitter")
            .arg("generate")
            .current_dir(build_dir)
            .status()
            .context("spawning `tree-sitter generate`")?;
        if !status.success() {
            bail!("tree-sitter generate failed in {}", build_dir.display());
        }
    }

    // `tree-sitter build -o <path>` compiles the parser + scanner into
    // a shared library at the given path. It looks at
    // `tree-sitter.json` / `grammar.js` in the cwd, so we run it from
    // `build_dir`.
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
