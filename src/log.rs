//! Append-only debug log.
//!
//! `init()` is called once at startup; afterwards the [`vlog!`] macro
//! writes a timestamped line to the resolved path. If path resolution
//! or the initial open fails, logging is silently disabled — callers
//! never have to check.
//!
//! Path resolution mirrors [`crate::config`]: honor `$VORTO_LOG` first,
//! then `$XDG_STATE_HOME/vorto/vorto.log`, then
//! `$HOME/.local/state/vorto/vorto.log`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init() {
    let Some(path) = default_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = LOG_FILE.set(Mutex::new(f));
}

pub fn default_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VORTO_LOG") {
        return Some(PathBuf::from(p));
    }
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        let mut p = PathBuf::from(xdg);
        p.push("vorto");
        p.push("vorto.log");
        return Some(p);
    }
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".local");
    p.push("state");
    p.push("vorto");
    p.push("vorto.log");
    Some(p)
}

pub fn write(args: std::fmt::Arguments<'_>) {
    let Some(lock) = LOG_FILE.get() else {
        return;
    };
    let Ok(mut f) = lock.lock() else {
        return;
    };
    // `now_local` can fail in multithreaded programs on some platforms
    // (it refuses to read the system tz if other threads might be
    // mutating env). Fall back to UTC so we always get a timestamp.
    let ts = OffsetDateTime::now_local()
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::from("?"));
    let _ = writeln!(f, "[{ts}] {args}");
}

#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => {
        $crate::log::write(::std::format_args!($($arg)*))
    };
}
