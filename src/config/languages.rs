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

use super::editor::EditorToml;

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
    /// Single-line comment prefix used by the `<space>c` toggle (e.g.
    /// `"//"` for Rust, `"#"` for Python). Unset means commenting is
    /// disabled for the language.
    pub comment_token: Option<String>,
    /// Editor-setting overrides for this language. Fields are flattened
    /// into `[languages.<name>]` (e.g. `tab_width = 8` sits directly on
    /// the language table, not under `[…].editor`). Field-level overlay
    /// onto the global `[editor]` defaults.
    #[serde(default, flatten)]
    pub editor: EditorToml,
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
        if user.comment_token.is_some() {
            self.comment_token = user.comment_token;
        }
        // Editor settings are field-level overlay so users can flip
        // just one knob (typically `tab_width`) without re-stating the
        // other.
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
    pub comment_token: Option<String>,
    /// Per-language editor-setting overrides. Each field is optional;
    /// at use time, overlay this onto the global `[editor]` to get the
    /// effective values for the buffer.
    pub editor: EditorToml,
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
            comment_token: c.comment_token,
            editor: c.editor,
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
            comment_token: Some("//".into()),
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
            comment_token: Some("#".into()),
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
            comment_token: Some("#".into()),
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
            comment_token: Some("//".into()),
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
            comment_token: Some("//".into()),
            lsp: Some(LspConfig {
                command: "typescript-language-server".into(),
                args: vec!["--stdio".into()],
                language_id: Some("javascript".into()),
                root_markers: vec!["package.json".into(), "jsconfig.json".into()],
            }),
            ..Default::default()
        },
    );
    // Go is canonically tab-indented (gofmt enforces it). We use a
    // tab stop of 4 to match what most repos in this codebase's
    // ecosystem ship with; users can override via `[languages.go]`.
    m.insert(
        "go".into(),
        LanguageConfig {
            extensions: Some(vec!["go".into()]),
            comment_token: Some("//".into()),
            editor: EditorToml {
                indent_width: Some(4),
                tab_width: Some(4),
                use_tabs: Some(true),
                show_whitespace: None,
            },
            lsp: Some(LspConfig {
                command: "gopls".into(),
                args: vec![],
                language_id: Some("go".into()),
                root_markers: vec!["go.mod".into(), "go.work".into()],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "kotlin".into(),
        LanguageConfig {
            extensions: Some(vec!["kt".into(), "kts".into()]),
            comment_token: Some("//".into()),
            lsp: Some(LspConfig {
                command: "kotlin-language-server".into(),
                args: vec![],
                language_id: Some("kotlin".into()),
                root_markers: vec![
                    "settings.gradle.kts".into(),
                    "settings.gradle".into(),
                    "build.gradle.kts".into(),
                    "build.gradle".into(),
                    "pom.xml".into(),
                ],
            }),
            ..Default::default()
        },
    );
    // `.h` is ambiguous (C or C++); we route it to C by default and
    // assume mixed projects override via `[languages.cpp]` in user
    // config. C++-specific headers (`.hpp`, `.hh`, `.hxx`) go to C++.
    m.insert(
        "c".into(),
        LanguageConfig {
            extensions: Some(vec!["c".into(), "h".into()]),
            comment_token: Some("//".into()),
            lsp: Some(LspConfig {
                command: "clangd".into(),
                args: vec![],
                language_id: Some("c".into()),
                root_markers: vec![
                    "compile_commands.json".into(),
                    ".clangd".into(),
                    "Makefile".into(),
                    "CMakeLists.txt".into(),
                ],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "cpp".into(),
        LanguageConfig {
            extensions: Some(vec![
                "cpp".into(),
                "cc".into(),
                "cxx".into(),
                "hpp".into(),
                "hh".into(),
                "hxx".into(),
            ]),
            comment_token: Some("//".into()),
            lsp: Some(LspConfig {
                command: "clangd".into(),
                args: vec![],
                language_id: Some("cpp".into()),
                root_markers: vec![
                    "compile_commands.json".into(),
                    ".clangd".into(),
                    "CMakeLists.txt".into(),
                ],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "java".into(),
        LanguageConfig {
            extensions: Some(vec!["java".into()]),
            comment_token: Some("//".into()),
            lsp: Some(LspConfig {
                command: "jdtls".into(),
                args: vec![],
                language_id: Some("java".into()),
                root_markers: vec![
                    "pom.xml".into(),
                    "build.gradle".into(),
                    "build.gradle.kts".into(),
                    ".project".into(),
                ],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "bash".into(),
        LanguageConfig {
            extensions: Some(vec!["sh".into(), "bash".into()]),
            comment_token: Some("#".into()),
            lsp: Some(LspConfig {
                command: "bash-language-server".into(),
                args: vec!["start".into()],
                language_id: Some("shellscript".into()),
                root_markers: vec![],
            }),
            ..Default::default()
        },
    );
    // JSON has no native single-line comment; leaving `comment_token`
    // unset disables the `<space>c` toggle for the language (correct).
    m.insert(
        "json".into(),
        LanguageConfig {
            extensions: Some(vec!["json".into()]),
            comment_token: None,
            lsp: Some(LspConfig {
                command: "vscode-json-language-server".into(),
                args: vec!["--stdio".into()],
                language_id: Some("json".into()),
                root_markers: vec![],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "yaml".into(),
        LanguageConfig {
            extensions: Some(vec!["yaml".into(), "yml".into()]),
            comment_token: Some("#".into()),
            lsp: Some(LspConfig {
                command: "yaml-language-server".into(),
                args: vec!["--stdio".into()],
                language_id: Some("yaml".into()),
                root_markers: vec![],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "markdown".into(),
        LanguageConfig {
            extensions: Some(vec!["md".into(), "markdown".into()]),
            comment_token: None,
            lsp: Some(LspConfig {
                command: "marksman".into(),
                args: vec!["server".into()],
                language_id: Some("markdown".into()),
                root_markers: vec![".marksman.toml".into()],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "html".into(),
        LanguageConfig {
            extensions: Some(vec!["html".into(), "htm".into()]),
            comment_token: None,
            lsp: Some(LspConfig {
                command: "vscode-html-language-server".into(),
                args: vec!["--stdio".into()],
                language_id: Some("html".into()),
                root_markers: vec![],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "css".into(),
        LanguageConfig {
            extensions: Some(vec!["css".into()]),
            comment_token: None,
            lsp: Some(LspConfig {
                command: "vscode-css-language-server".into(),
                args: vec!["--stdio".into()],
                language_id: Some("css".into()),
                root_markers: vec![],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "lua".into(),
        LanguageConfig {
            extensions: Some(vec!["lua".into()]),
            comment_token: Some("--".into()),
            lsp: Some(LspConfig {
                command: "lua-language-server".into(),
                args: vec![],
                language_id: Some("lua".into()),
                root_markers: vec![
                    ".luarc.json".into(),
                    ".luarc.jsonc".into(),
                    "stylua.toml".into(),
                ],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "ruby".into(),
        LanguageConfig {
            extensions: Some(vec!["rb".into()]),
            comment_token: Some("#".into()),
            lsp: Some(LspConfig {
                command: "ruby-lsp".into(),
                args: vec![],
                language_id: Some("ruby".into()),
                root_markers: vec!["Gemfile".into(), ".rubocop.yml".into()],
            }),
            ..Default::default()
        },
    );
    m.insert(
        "zig".into(),
        LanguageConfig {
            extensions: Some(vec!["zig".into(), "zon".into()]),
            comment_token: Some("//".into()),
            lsp: Some(LspConfig {
                command: "zls".into(),
                args: vec![],
                language_id: Some("zig".into()),
                root_markers: vec!["build.zig".into()],
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
