//! Language and LSP-server registry.
//!
//! Two top-level TOML sections live here:
//!
//! - `[lsp.<server-name>]` — server definitions (command, args, root
//!   markers). Built-in defaults are seeded from Rust; user entries
//!   field-level overlay onto the built-ins (so users can tweak just
//!   `args` without re-typing `command`). Entirely new servers can
//!   also be defined.
//! - `[languages.<lang-name>]` — language definitions (extensions,
//!   grammar, comment token, editor overrides) plus an `lsp` field
//!   that *references* server names from the `[lsp]` table.
//!
//! ```toml
//! [lsp.vtsls]
//! command = "vtsls"
//! args = ["--stdio"]
//!
//! [languages.typescript]
//! lsp = ["vtsls", "typescript-language-server"]
//! ```
//!
//! Overlay rule (field-level): a user-supplied `Some(_)` field
//! replaces the default; `None` (= unset) leaves the default in place.
//! `lsp` (the reference list on a language) is replaced whole, not
//! merged — users who want to add to the default set must re-state
//! the names they want.
//!
//! File layout: this module file owns the data types and the
//! `LanguageRegistry` entry point. The two big jobs are split off:
//!
//! - [`builtins`] — out-of-the-box `[lsp]` and `[languages]` defaults.
//! - [`resolve`] — merge user TOML onto the built-ins and expand
//!   each language's `lsp` reference list against the server table.

mod builtins;
mod resolve;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use super::editor::EditorToml;

// ────────────────────────────────────────────────────────────────────────
// LSP server schema
// ────────────────────────────────────────────────────────────────────────

/// Raw `[lsp.<name>]` entry as it appears in user TOML. Every field is
/// `Option<T>` so partial overrides can field-level overlay onto the
/// built-in defaults.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct LspToml {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub language_id: Option<String>,
    pub root_markers: Option<Vec<String>>,
}

/// Fully-resolved LSP-server record. The key from the `[lsp]` table
/// becomes `name`; downstream code uses `(<lang>, name)` to identify
/// each spawned client.
#[derive(Debug, Clone)]
pub struct LspConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// LSP `languageId` sent in `didOpen`. `None` falls back to the
    /// language name — typically the right thing when a server is
    /// dedicated to one language, and also fine when a server serves
    /// multiple langs (each `didOpen` then carries that file's own
    /// language name).
    pub language_id: Option<String>,
    pub root_markers: Vec<String>,
}

impl LspConfig {
    /// Apply a partial user override. Only `Some` fields on `user`
    /// replace this entry's values; the rest survive.
    fn overlay(&mut self, user: LspToml) {
        if let Some(c) = user.command {
            self.command = c;
        }
        if let Some(a) = user.args {
            self.args = a;
        }
        if user.language_id.is_some() {
            self.language_id = user.language_id;
        }
        if let Some(r) = user.root_markers {
            self.root_markers = r;
        }
    }

    /// Promote a user-only TOML entry (no built-in to overlay onto)
    /// into a full record. `command` is required for new entries —
    /// without it there's nothing to spawn.
    fn from_user(name: &str, user: LspToml) -> Result<Self> {
        let command = user.command.ok_or_else(|| {
            anyhow!(
                "[lsp.{}] is a new server (no built-in to overlay onto) and \
                 must define `command`",
                name
            )
        })?;
        Ok(Self {
            name: name.to_string(),
            command,
            args: user.args.unwrap_or_default(),
            language_id: user.language_id,
            root_markers: user.root_markers.unwrap_or_default(),
        })
    }
}

// ────────────────────────────────────────────────────────────────────────
// Formatter schema
// ────────────────────────────────────────────────────────────────────────

/// Raw `[languages.<name>.formatter]` entry as it appears in TOML.
/// `command` is required when the user defines a new entry; both fields
/// land here as `Option` so a missing `formatter` table on the language
/// can be distinguished from an explicitly-cleared one.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct FormatterToml {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
}

/// Fully-resolved external formatter. The buffer's text is piped on
/// stdin; stdout becomes the new text. A missing `formatter` on the
/// resolved `Language` means "fall back to `textDocument/formatting`
/// against the first attached LSP".
#[derive(Debug, Clone)]
pub struct FormatterConfig {
    pub command: String,
    pub args: Vec<String>,
}

// ────────────────────────────────────────────────────────────────────────
// Language schema
// ────────────────────────────────────────────────────────────────────────

/// Raw `[languages.<name>]` entry as it appears in TOML / built-in
/// defaults. `Option<T>` everywhere so the overlay step can
/// distinguish "user wrote nothing" from a meaningful empty value.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct LanguageConfig {
    pub extensions: Option<Vec<String>>,
    /// Grammar filename stem (without the `.so` / `.dylib` / `.dll`
    /// extension). Defaults to the language name itself.
    pub grammar: Option<String>,
    /// Override directory for `<grammar>.{so,dylib,dll}` — overrides
    /// the global grammar dir for just this language.
    pub grammar_dir: Option<PathBuf>,
    /// Override directory for `<lang>/highlights.scm` — overrides the
    /// global query dir for just this language.
    pub query_dir: Option<PathBuf>,
    /// Single-line comment prefix used by the `<space>c` toggle (e.g.
    /// `"//"` for Rust, `"#"` for Python). Unset disables commenting
    /// for the language.
    pub comment_token: Option<String>,
    /// Editor-setting overrides for this language. Fields are
    /// flattened into `[languages.<name>]` (e.g. `tab_width = 8` sits
    /// directly on the language table, not under `[…].editor`).
    /// Field-level overlay onto the global `[editor]` defaults.
    #[serde(default, flatten)]
    pub editor: EditorToml,
    /// Server names (keys from the `[lsp]` table) to attach to buffers
    /// of this language. Replaced whole on overlay, not merged.
    pub lsp: Option<Vec<String>>,
    /// External formatter invoked on save. The buffer's text is piped
    /// on stdin and stdout becomes the new buffer text. Unset means
    /// "fall back to `textDocument/formatting` against the attached
    /// LSP". Replaced whole on overlay, not merged — `command` and
    /// `args` go together, partial overrides don't pay rent here.
    pub formatter: Option<FormatterToml>,
}

impl LanguageConfig {
    /// Overlay `user` onto `self`. Any `Some` field in `user` wins;
    /// the rest of `self` survives.
    pub fn overlay(&mut self, user: LanguageConfig) {
        if user.extensions.is_some() {
            self.extensions = user.extensions;
        }
        if user.grammar.is_some() {
            self.grammar = user.grammar;
        }
        if user.grammar_dir.is_some() {
            self.grammar_dir = user.grammar_dir;
        }
        if user.query_dir.is_some() {
            self.query_dir = user.query_dir;
        }
        if user.comment_token.is_some() {
            self.comment_token = user.comment_token;
        }
        // Editor settings are field-level overlay so users can flip
        // just one knob (typically `tab_width`) without re-stating the
        // others.
        if user.editor.indent_width.is_some() {
            self.editor.indent_width = user.editor.indent_width;
        }
        if user.editor.tab_width.is_some() {
            self.editor.tab_width = user.editor.tab_width;
        }
        if user.editor.use_tabs.is_some() {
            self.editor.use_tabs = user.editor.use_tabs;
        }
        if user.editor.show_whitespace.is_some() {
            self.editor.show_whitespace = user.editor.show_whitespace;
        }
        if user.lsp.is_some() {
            self.lsp = user.lsp;
        }
        if user.formatter.is_some() {
            self.formatter = user.formatter;
        }
    }
}

/// Fully-resolved language record. This is what the runtime works with.
#[derive(Debug, Clone)]
pub struct Language {
    pub name: String,
    pub extensions: Vec<String>,
    pub grammar: String,
    pub grammar_dir: Option<PathBuf>,
    pub query_dir: Option<PathBuf>,
    pub comment_token: Option<String>,
    /// Per-language editor-setting overrides.
    pub editor: EditorToml,
    /// LSP servers attached to this language, expanded from the name
    /// references on `LanguageConfig.lsp`. Empty when the language has
    /// no LSP configured.
    pub lsp: Vec<LspConfig>,
    /// External formatter, if configured. `None` means save-time
    /// formatting falls through to `textDocument/formatting` against
    /// the first attached LSP — or is a no-op when no LSP is attached.
    pub formatter: Option<FormatterConfig>,
}

// ────────────────────────────────────────────────────────────────────────
// Registry
// ────────────────────────────────────────────────────────────────────────

/// Catalog of resolved languages with name and extension lookups.
/// Built once at startup from the user's TOML overlaid on the built-in
/// defaults.
#[derive(Debug, Clone, Default)]
pub struct LanguageRegistry {
    by_name: HashMap<String, Language>,
    extension_to_name: HashMap<String, String>,
    /// LSP `languageId` per file extension. Distinct from the language
    /// name because the LSP protocol's id space is fixed by spec —
    /// `.tsx` must announce `"typescriptreact"` even though we route
    /// it through our own `tsx` language entry. Extensions missing
    /// here fall back to the language name at `didOpen` time.
    extension_to_language_id: HashMap<String, String>,
}

impl LanguageRegistry {
    /// Build the catalog from the user's `[languages]` and `[lsp]`
    /// tables. Returns an error when a language references an unknown
    /// LSP server name, or when a user-defined `[lsp.<name>]` entry
    /// lacks `command`.
    pub fn build(
        user_languages: HashMap<String, LanguageConfig>,
        user_lsp: HashMap<String, LspToml>,
    ) -> Result<Self> {
        let lsp_table = resolve::resolve_lsp_table(user_lsp)?;
        let by_name = resolve::resolve(user_languages, &lsp_table)?;
        let extension_to_name = resolve::build_extension_index(&by_name);
        Ok(Self {
            by_name,
            extension_to_name,
            extension_to_language_id: builtins::builtin_extension_language_ids(),
        })
    }

    /// Resolve a file extension (without the leading `.`) to its
    /// language entry, if any.
    pub fn by_extension(&self, ext: &str) -> Option<&Language> {
        let name = self.extension_to_name.get(ext)?;
        self.by_name.get(name)
    }

    /// LSP `languageId` for this file extension. `None` means "no
    /// override — let the LSP layer default to the language name."
    pub fn language_id_for_extension(&self, ext: &str) -> Option<&str> {
        self.extension_to_language_id.get(ext).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::builtins::{builtin_languages, builtin_lsp};
    use super::resolve::{build_extension_index, resolve, resolve_lsp_table};
    use super::*;

    fn empty_lsp() -> HashMap<String, LspConfig> {
        resolve_lsp_table(HashMap::new()).unwrap()
    }

    #[test]
    fn builtins_include_rust() {
        let m = builtin_languages();
        assert!(m.contains_key("rust"));
        assert_eq!(
            m["rust"].extensions.as_deref(),
            Some(&["rs".to_string()][..])
        );
    }

    #[test]
    fn builtin_lsp_includes_vtsls_and_tsserver() {
        let m = builtin_lsp();
        assert!(m.contains_key("vtsls"));
        assert!(m.contains_key("typescript-language-server"));
    }

    #[test]
    fn overlay_replaces_only_provided_fields() {
        let mut base = LanguageConfig {
            extensions: Some(vec!["rs".into()]),
            grammar: Some("rust".into()),
            ..Default::default()
        };
        let user = LanguageConfig {
            grammar: Some("rust-tree-sitter".into()),
            ..Default::default()
        };
        base.overlay(user);
        assert_eq!(base.grammar.as_deref(), Some("rust-tree-sitter"));
        assert_eq!(base.extensions.as_deref(), Some(&["rs".to_string()][..]));
    }

    #[test]
    fn resolve_adds_user_only_language() {
        let mut user = HashMap::new();
        user.insert(
            "fish".into(),
            LanguageConfig {
                extensions: Some(vec!["fish".into()]),
                ..Default::default()
            },
        );
        let langs = resolve(user, &empty_lsp()).unwrap();
        assert!(langs.contains_key("fish"));
        assert_eq!(langs["fish"].grammar, "fish");
        assert!(langs["fish"].lsp.is_empty());
    }

    #[test]
    fn resolve_falls_back_to_default_when_user_omits_field() {
        let mut user = HashMap::new();
        user.insert(
            "rust".into(),
            LanguageConfig {
                grammar: Some("rust-custom".into()),
                ..Default::default()
            },
        );
        let langs = resolve(user, &empty_lsp()).unwrap();
        assert_eq!(langs["rust"].grammar, "rust-custom");
        assert_eq!(langs["rust"].extensions, vec!["rs"]);
        // Built-in LSP ref survives the partial override.
        assert_eq!(langs["rust"].lsp.len(), 1);
        assert_eq!(langs["rust"].lsp[0].name, "rust-analyzer");
    }

    #[test]
    fn extension_index_routes_to_language_name() {
        let langs = resolve(HashMap::new(), &empty_lsp()).unwrap();
        let idx = build_extension_index(&langs);
        assert_eq!(idx.get("rs"), Some(&"rust".to_string()));
        assert_eq!(idx.get("py"), Some(&"python".to_string()));
    }

    #[test]
    fn typescript_resolves_to_two_servers() {
        let langs = resolve(HashMap::new(), &empty_lsp()).unwrap();
        let ts = &langs["typescript"];
        let names: Vec<&str> = ts.lsp.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["vtsls", "typescript-language-server"]);
    }

    #[test]
    fn user_lsp_overlay_replaces_only_provided_fields() {
        let mut user_lsp: HashMap<String, LspToml> = HashMap::new();
        user_lsp.insert(
            "vtsls".into(),
            LspToml {
                args: Some(vec!["--my-flag".into()]),
                ..Default::default()
            },
        );
        let table = resolve_lsp_table(user_lsp).unwrap();
        let entry = &table["vtsls"];
        assert_eq!(entry.command, "vtsls"); // built-in survived
        assert_eq!(entry.args, vec!["--my-flag"]); // user replaced
    }

    #[test]
    fn user_lsp_new_entry_requires_command() {
        let mut user_lsp: HashMap<String, LspToml> = HashMap::new();
        user_lsp.insert(
            "my-server".into(),
            LspToml {
                args: Some(vec!["--stdio".into()]),
                ..Default::default()
            },
        );
        assert!(resolve_lsp_table(user_lsp).is_err());
    }

    #[test]
    fn user_lsp_new_entry_with_command_succeeds() {
        let mut user_lsp: HashMap<String, LspToml> = HashMap::new();
        user_lsp.insert(
            "my-server".into(),
            LspToml {
                command: Some("my-bin".into()),
                args: Some(vec!["--stdio".into()]),
                ..Default::default()
            },
        );
        let table = resolve_lsp_table(user_lsp).unwrap();
        assert_eq!(table["my-server"].command, "my-bin");
    }

    #[test]
    fn language_ref_to_unknown_server_errors() {
        let mut user_langs: HashMap<String, LanguageConfig> = HashMap::new();
        user_langs.insert(
            "rust".into(),
            LanguageConfig {
                lsp: Some(vec!["nonexistent".into()]),
                ..Default::default()
            },
        );
        let err = resolve(user_langs, &empty_lsp()).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn user_can_pick_subset_of_servers() {
        // User keeps only one server for typescript.
        let mut user_langs: HashMap<String, LanguageConfig> = HashMap::new();
        user_langs.insert(
            "typescript".into(),
            LanguageConfig {
                lsp: Some(vec!["typescript-language-server".into()]),
                ..Default::default()
            },
        );
        let langs = resolve(user_langs, &empty_lsp()).unwrap();
        assert_eq!(langs["typescript"].lsp.len(), 1);
        assert_eq!(
            langs["typescript"].lsp[0].name,
            "typescript-language-server"
        );
    }
}
