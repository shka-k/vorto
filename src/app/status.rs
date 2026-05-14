//! Transient message surfaced as a top-right toast. Each constructor
//! stamps `shown_at = Instant::now()`, so the renderer and main-loop
//! scheduler can age toasts out without any call site having to manage
//! timestamps.

use std::time::Instant;

/// Severity of a toast — drives only the foreground color the UI picks.
/// `Warn` is exposed for callers but not used in-tree yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    #[allow(dead_code)]
    Warn,
    Error,
}

pub struct Status {
    text: String,
    level: Level,
    shown_at: Instant,
}

impl Status {
    pub fn info(s: impl Into<String>) -> Self {
        Self::new(s, Level::Info)
    }
    #[allow(dead_code)]
    pub fn warn(s: impl Into<String>) -> Self {
        Self::new(s, Level::Warn)
    }
    pub fn error(s: impl Into<String>) -> Self {
        Self::new(s, Level::Error)
    }
    fn new(s: impl Into<String>, level: Level) -> Self {
        Self {
            text: s.into(),
            level,
            shown_at: Instant::now(),
        }
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn level(&self) -> Level {
        self.level
    }
    pub fn shown_at(&self) -> Instant {
        self.shown_at
    }
}
