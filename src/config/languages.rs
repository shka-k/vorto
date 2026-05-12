//! Language registry.
//!
//! Per-language configuration (extensions, grammar, query paths) is
//! seeded from built-in Rust defaults and then overlaid with whatever
//! `[languages.<name>]` tables the user supplies in `config.toml`.
//!
//! Overlay rule (field-level): a user-supplied `Some(_)` field replaces
//! the default; `None` (= unset) leaves the default in place. Subtables
//! — when we add them later (LSP, formatter) — are replaced whole, not
//! deep-merged.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// Raw, partially-filled language entry as it appears in TOML or in the
/// built-in defaults table. Every field is `Option<T>` so the overlay
/// step can distinguish "user wrote nothing" from a meaningful empty
/// value. Resolves into [`Language`] after merging.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct LanguageConfig {
    pub extensions: Option<Vec<String>>,
    /// Grammar filename stem (without the `.so` / `.dylib` / `.dll`
    /// extension). Defaults to the language name itself.
    pub grammar: Option<String>,
    /// Override directory for `<grammar>.{so,dylib,dll}` — overrides the
    /// global grammar dir for just this language.
    pub grammar_dir: Option<PathBuf>,
    /// Override directory for `<lang>/highlights.scm` — overrides the
    /// global query dir for just this language.
    pub query_dir: Option<PathBuf>,
    /// `[languages.<name>.lsp]` subtable — replaced whole, not deep-merged.
    pub lsp: Option<LspConfig>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct LspConfig {
    /// Server executable (e.g. "rust-analyzer", "pyright-langserver").
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// LSP `languageId` sent in `didOpen`. Falls back to the language
    /// name when unset.
    pub language_id: Option<String>,
    /// Filenames that mark the project root (e.g. "Cargo.toml",
    /// "pyproject.toml"). The first match walking up from the opened
    /// file becomes `rootUri`; defaults to the file's parent directory.
    #[serde(default)]
    pub root_markers: Vec<String>,
}

impl LanguageConfig {
    /// Overlay `user` onto `self`. Any `Some` field in `user` wins; the
    /// rest of `self` survives.
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
        if user.lsp.is_some() {
            self.lsp = user.lsp;
        }
    }
}

/// Fully-resolved language record after `resolve()` has merged user
/// config over built-ins. This is what the runtime works with.
#[derive(Debug, Clone)]
pub struct Language {
    pub name: String,
    pub extensions: Vec<String>,
    pub grammar: String,
    pub grammar_dir: Option<PathBuf>,
    pub query_dir: Option<PathBuf>,
    pub lsp: Option<LspConfig>,
}

impl Language {
    fn from_config(name: &str, c: LanguageConfig) -> Self {
        Self {
            name: name.to_string(),
            extensions: c.extensions.unwrap_or_default(),
            grammar: c.grammar.unwrap_or_else(|| name.to_string()),
            grammar_dir: c.grammar_dir,
            query_dir: c.query_dir,
            lsp: c.lsp,
        }
    }
}

/// Built-in defaults. To support a new language out-of-the-box, add it
/// here. Users can still override every field via `[languages.<name>]`
/// in their config, and they can add entirely new languages with the
/// same syntax.
pub fn builtin_languages() -> HashMap<String, LanguageConfig> {
    let mut m = HashMap::new();
    m.insert(
        "rust".into(),
        LanguageConfig {
            extensions: Some(vec!["rs".into()]),
            lsp: Some(LspConfig {
                command: "rust-analyzer".into(),
                args: vec![],
                language_id: Some("rust".into()),
                root_markers: vec!["Cargo.toml".into(), "rust-project.json".into()],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "python".into(),
        LanguageConfig {
            extensions: Some(vec!["py".into()]),
            lsp: Some(LspConfig {
                command: "pyright-langserver".into(),
                args: vec!["--stdio".into()],
                language_id: Some("python".into()),
                root_markers: vec![
                    "pyproject.toml".into(),
                    "setup.py".into(),
                    "setup.cfg".into(),
                    "requirements.txt".into(),
                ],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "toml".into(),
        LanguageConfig {
            extensions: Some(vec!["toml".into()]),
            lsp: Some(LspConfig {
                command: "taplo".into(),
                args: vec!["lsp".into(), "stdio".into()],
                language_id: Some("toml".into()),
                root_markers: vec![],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "typescript".into(),
        LanguageConfig {
            extensions: Some(vec!["ts".into(), "tsx".into()]),
            lsp: Some(LspConfig {
                command: "typescript-language-server".into(),
                args: vec!["--stdio".into()],
                language_id: Some("typescript".into()),
                root_markers: vec!["package.json".into(), "tsconfig.json".into()],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "javascript".into(),
        LanguageConfig {
            extensions: Some(vec!["js".into(), "jsx".into(), "mjs".into(), "cjs".into()]),
            lsp: Some(LspConfig {
                command: "typescript-language-server".into(),
                args: vec!["--stdio".into()],
                language_id: Some("javascript".into()),
                root_markers: vec!["package.json".into(), "jsconfig.json".into()],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "go".into(),
        LanguageConfig {
            extensions: Some(vec!["go".into()]),
            lsp: Some(LspConfig {
                command: "gopls".into(),
                args: vec![],
                language_id: Some("go".into()),
                root_markers: vec!["go.mod".into(), "go.work".into()],
            }),
            ..Default::default()
        },
    );
    m
}

/// Merge user TOML over built-in defaults and turn each entry into a
/// resolved [`Language`]. User-defined languages absent from the
/// defaults are added as new entries.
pub fn resolve(user: HashMap<String, LanguageConfig>) -> HashMap<String, Language> {
    let mut merged = builtin_languages();
    for (name, user_lang) in user {
        merged
            .entry(name)
            .and_modify(|d| d.overlay(user_lang.clone()))
            .or_insert(user_lang);
    }
    merged
        .into_iter()
        .map(|(name, cfg)| {
            let lang = Language::from_config(&name, cfg);
            (name, lang)
        })
        .collect()
}

/// Build an `extension → language name` lookup index. The mapping is
/// many-to-one (multiple extensions can resolve to the same language).
/// Last-wins on collisions; collisions across languages should be rare
/// enough that we don't bother surfacing them.
fn build_extension_index(langs: &HashMap<String, Language>) -> HashMap<String, String> {
    let mut idx = HashMap::new();
    for (name, lang) in langs {
        for ext in &lang.extensions {
            idx.insert(ext.clone(), name.clone());
        }
    }
    idx
}

/// Catalog of resolved languages with both name and extension lookups.
/// Built once at startup from the user's TOML overlaid on the built-in
/// defaults; consumed by `attach_lsp` / `attach_highlighter` to decide
/// what tooling to spawn for a given file.
#[derive(Debug, Clone, Default)]
pub struct LanguageRegistry {
    by_name: HashMap<String, Language>,
    extension_to_name: HashMap<String, String>,
}

impl LanguageRegistry {
    /// Build the catalog from the user's `[languages.<name>]` table,
    /// overlaying onto the built-in defaults.
    pub fn build(user: HashMap<String, LanguageConfig>) -> Self {
        let by_name = resolve(user);
        let extension_to_name = build_extension_index(&by_name);
        Self {
            by_name,
            extension_to_name,
        }
    }

    /// Resolve a file extension (without the leading `.`) to its
    /// language entry, if any.
    pub fn by_extension(&self, ext: &str) -> Option<&Language> {
        let name = self.extension_to_name.get(ext)?;
        self.by_name.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // grammar overridden
        assert_eq!(base.grammar.as_deref(), Some("rust-tree-sitter"));
        // extensions survived
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
        let langs = resolve(user);
        assert!(langs.contains_key("fish"));
        assert_eq!(langs["fish"].grammar, "fish"); // grammar defaults to name
    }

    #[test]
    fn resolve_user_overrides_default_extensions() {
        let mut user = HashMap::new();
        user.insert(
            "rust".into(),
            LanguageConfig {
                extensions: Some(vec!["rs".into(), "rlib".into()]),
                ..Default::default()
            },
        );
        let langs = resolve(user);
        assert_eq!(langs["rust"].extensions, vec!["rs", "rlib"]);
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
        let langs = resolve(user);
        // grammar overridden
        assert_eq!(langs["rust"].grammar, "rust-custom");
        // extensions fell back to default
        assert_eq!(langs["rust"].extensions, vec!["rs"]);
    }

    #[test]
    fn extension_index_routes_to_language_name() {
        let langs = resolve(HashMap::new());
        let idx = build_extension_index(&langs);
        assert_eq!(idx.get("rs"), Some(&"rust".to_string()));
        assert_eq!(idx.get("py"), Some(&"python".to_string()));
    }
}
