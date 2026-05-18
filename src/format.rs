//! Save-time formatter dispatch.
//!
//! Two strategies live here:
//!
//! * [`run_external`] — pipe the buffer through an external command
//!   (`rustfmt`, `gofmt`, `zig fmt --stdin`, prettier, …) and treat
//!   stdout as the formatted text. Synchronous; the worst-case wait is
//!   capped by [`EXTERNAL_TIMEOUT`].
//! * The LSP path lives on `LspCoordinator` so it can pick a server
//!   from the current document's attached clients; this module owns
//!   only the external-command leg.
//!
//! Both legs return `Result<String>`; the save flow swaps the buffer
//! text on success and surfaces the error as a toast on failure
//! without aborting the save itself.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::FormatterConfig;

/// Walk up from `cwd` looking for a `Cargo.toml` and return the
/// declared Rust edition. rustfmt reading from stdin can't infer the
/// edition from the surrounding crate the way `cargo fmt` can, so it
/// defaults to 2015 and rejects modern syntax (let chains, `let-else`
/// in older positions, …). We look this up per-call so a single editor
/// process formatting files across crates of different editions does
/// the right thing in each.
///
/// Handles inherited editions: a package that says
/// `edition.workspace = true` causes us to continue walking up until
/// we hit the workspace's `[workspace.package].edition`.
fn rust_edition_from_cargo(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let cargo = d.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&cargo)
            && let Ok(value) = text.parse::<toml::Value>()
        {
            if let Some(ed) = value
                .get("package")
                .and_then(|p| p.get("edition"))
                .and_then(|e| e.as_str())
            {
                return Some(ed.to_string());
            }
            if let Some(ed) = value
                .get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|p| p.get("edition"))
                .and_then(|e| e.as_str())
            {
                return Some(ed.to_string());
            }
        }
        dir = d.parent();
    }
    None
}

/// True if any ancestor of `cwd` contains a `rustfmt.toml` or
/// `.rustfmt.toml`. When present we leave args alone — the user has
/// declared the edition explicitly and a CLI flag would override it.
fn has_rustfmt_config(cwd: &Path) -> bool {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join("rustfmt.toml").is_file() || d.join(".rustfmt.toml").is_file() {
            return true;
        }
        dir = d.parent();
    }
    false
}

/// rustfmt-specific arg massaging: inject `--edition=<edition>` from
/// the nearest Cargo.toml when the user hasn't already specified one
/// (either via formatter args or a project `rustfmt.toml`). A no-op
/// for any other formatter.
fn effective_args(formatter: &FormatterConfig, cwd: &Path) -> Vec<String> {
    let mut args = formatter.args.clone();
    let is_rustfmt = Path::new(&formatter.command)
        .file_stem()
        .and_then(|s| s.to_str())
        == Some("rustfmt");
    if is_rustfmt
        && !args
            .iter()
            .any(|a| a == "--edition" || a.starts_with("--edition="))
        && !has_rustfmt_config(cwd)
        && let Some(edition) = rust_edition_from_cargo(cwd)
    {
        args.push(format!("--edition={edition}"));
    }
    args
}

/// Upper bound on how long we'll wait for an external formatter before
/// reporting failure and saving the un-formatted buffer instead. Most
/// formatters return in well under a second; a hung process shouldn't
/// be allowed to block the editor indefinitely.
const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to poll between checks of `child.try_wait()`. Short enough
/// to feel instant on success, long enough that we don't burn cycles
/// on the rare slow run.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run `formatter` against `text`, piping `text` on stdin and reading
/// stdout as the result. `cwd` (typically the buffer file's parent
/// directory) is set so formatters that look up config in the working
/// tree (`rustfmt.toml`, `.prettierrc`, …) resolve from the right
/// place. Non-zero exit surfaces stderr as the error message — what
/// the user wants to see when their code has a syntax error and
/// rustfmt refuses to touch it.
pub fn run_external(formatter: &FormatterConfig, text: &str, cwd: &Path) -> Result<String> {
    let args = effective_args(formatter, cwd);
    let mut child = Command::new(&formatter.command)
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning formatter `{}`", formatter.command))?;

    // Write stdin in-place, then close it so the child sees EOF and
    // emits its output. Drop the handle by taking it out of the option.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .with_context(|| format!("writing stdin to `{}`", formatter.command))?;
        // Explicit drop closes the pipe — the child won't finish
        // otherwise. We pull the handle out via `take` rather than
        // letting the `Child` drop it implicitly so the close lands
        // before we start polling for exit below.
    }

    // Poll for completion with a hard cap. `wait_with_output` would be
    // simpler but blocks indefinitely; we want a runaway formatter to
    // surface as an error and let the save proceed un-formatted.
    let deadline = Instant::now() + EXTERNAL_TIMEOUT;
    loop {
        match child
            .try_wait()
            .with_context(|| format!("polling formatter `{}`", formatter.command))?
        {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    bail!(
                        "formatter `{}` timed out after {:?}",
                        formatter.command,
                        EXTERNAL_TIMEOUT
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    // `wait_with_output` re-waits but that's a cheap no-op now — and
    // it collects both pipes for us. The stdout/stderr handles were
    // never read into so they may contain bytes; do that here.
    let output = child
        .wait_with_output()
        .with_context(|| format!("collecting output from `{}`", formatter.command))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        if msg.is_empty() {
            bail!(
                "formatter `{}` exited with {}",
                formatter.command,
                output.status
            );
        }
        bail!("formatter `{}`: {}", formatter.command, msg);
    }
    let out = String::from_utf8(output.stdout).with_context(|| {
        format!(
            "formatter `{}` produced non-UTF-8 output",
            formatter.command
        )
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_external_pipes_stdin_through_cat() {
        let f = FormatterConfig {
            command: "cat".into(),
            args: vec![],
        };
        let out = run_external(&f, "hello\nworld\n", Path::new(".")).unwrap();
        assert_eq!(out, "hello\nworld\n");
    }

    #[test]
    fn run_external_surfaces_nonzero_exit_with_stderr() {
        // `sh -c 'cat >&2; exit 1'` echoes input to stderr then fails;
        // tests the error path that should bubble up to the user toast.
        let f = FormatterConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "cat >&2; exit 1".into()],
        };
        let err = run_external(&f, "boom", Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "stderr should bubble up: {}", err);
    }

    #[test]
    fn run_external_reports_spawn_failure() {
        let f = FormatterConfig {
            command: "this-binary-does-not-exist-zzz".into(),
            args: vec![],
        };
        assert!(run_external(&f, "x", Path::new(".")).is_err());
    }

    fn fresh_tmp(label: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "vorto-format-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn rust_edition_picked_up_from_package() {
        let root = fresh_tmp("pkg");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n",
        )
        .unwrap();
        assert_eq!(rust_edition_from_cargo(&root), Some("2024".into()));
    }

    #[test]
    fn rust_edition_inherited_from_workspace() {
        // Package says `edition.workspace = true`, so we walk up to
        // the workspace root to resolve it.
        let root = fresh_tmp("ws");
        let crate_dir = root.join("crates/inner");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/inner\"]\n\n[workspace.package]\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"inner\"\nedition.workspace = true\n",
        )
        .unwrap();
        assert_eq!(rust_edition_from_cargo(&crate_dir), Some("2021".into()));
    }

    #[test]
    fn effective_args_injects_edition_for_rustfmt() {
        let root = fresh_tmp("inj");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let f = FormatterConfig {
            command: "rustfmt".into(),
            args: vec![],
        };
        assert_eq!(
            effective_args(&f, &root),
            vec!["--edition=2024".to_string()]
        );
    }

    #[test]
    fn effective_args_respects_user_edition() {
        let root = fresh_tmp("usr");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let f = FormatterConfig {
            command: "rustfmt".into(),
            args: vec!["--edition=2021".into()],
        };
        // User-supplied --edition wins; we don't append our own.
        assert_eq!(
            effective_args(&f, &root),
            vec!["--edition=2021".to_string()]
        );
    }

    #[test]
    fn effective_args_backs_off_when_rustfmt_toml_present() {
        let root = fresh_tmp("cfg");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(root.join("rustfmt.toml"), "edition = \"2021\"\n").unwrap();
        let f = FormatterConfig {
            command: "rustfmt".into(),
            args: vec![],
        };
        // rustfmt.toml is the source of truth — don't override it.
        assert!(effective_args(&f, &root).is_empty());
    }

    #[test]
    fn effective_args_noop_for_non_rustfmt() {
        let root = fresh_tmp("other");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let f = FormatterConfig {
            command: "gofmt".into(),
            args: vec![],
        };
        assert!(effective_args(&f, &root).is_empty());
    }
}
