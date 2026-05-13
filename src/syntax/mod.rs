//! Syntax highlighting — tree-sitter grammar loading, per-buffer
//! highlighting, and capture-name → terminal-style mapping.

mod highlight;
mod theme;

pub use highlight::{Capture, Highlighter, Loader};
pub use theme::style_for;
