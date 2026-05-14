//! Tree-sitter grammar installer.
//!
//! Provides the `vorto grammar` subcommand, which fetches grammar
//! sources from upstream repositories and runs the external
//! `tree-sitter` CLI to produce `<name>.{so,dylib,dll}` libraries the
//! [`crate::syntax::Loader`] can `dlopen`. Built libraries land in the
//! configured `grammar_dir` (defaults to `~/.config/vorto/grammars`).
//!
//! Queries (`highlights.scm`, `textobjects.scm`, …) are **not** installed
//! by this module — they live under `query_dir` and remain a manual
//! responsibility, since most users want to pick a query set
//! (helix-editor's, nvim-treesitter's, the grammar repo's own) on a
//! per-language basis.

pub mod assets;
pub mod build;
pub mod cli;
pub mod recipe;
