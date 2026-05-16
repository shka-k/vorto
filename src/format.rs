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
    let mut child = Command::new(&formatter.command)
        .args(&formatter.args)
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
}
