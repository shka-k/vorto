//! Syntax highlighting — tree-sitter grammar loading, per-buffer
//! highlighting, and capture-name → terminal-style mapping.

mod highlight;
mod loader;
mod theme;

pub use highlight::{Capture, Highlighter};
pub use loader::Loader;
pub use theme::style_for;
