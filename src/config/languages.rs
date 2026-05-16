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
// Built-in LSP defaults
// ────────────────────────────────────────────────────────────────────────

/// Built-in `[lsp.<name>]` defaults. Users overlay onto these by
/// re-declaring `[lsp.<name>]` in their config; entirely new servers
/// can also be added.
pub fn builtin_lsp() -> HashMap<String, LspConfig> {
    let mut m = HashMap::new();
    let add = |m: &mut HashMap<String, LspConfig>,
               name: &str,
               command: &str,
               args: &[&str],
               language_id: Option<&str>,
               root_markers: &[&str]| {
        m.insert(
            name.to_string(),
            LspConfig {
                name: name.to_string(),
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                language_id: language_id.map(|s| s.to_string()),
                root_markers: root_markers.iter().map(|s| s.to_string()).collect(),
            },
        );
    };

    add(
        &mut m,
        "rust-analyzer",
        "rust-analyzer",
        &[],
        None,
        &["Cargo.toml", "rust-project.json"],
    );
    add(
        &mut m,
        "pyright",
        "pyright-langserver",
        &["--stdio"],
        None,
        &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
    );
    add(&mut m, "taplo", "taplo", &["lsp", "stdio"], None, &[]);
    add(
        &mut m,
        "vtsls",
        "vtsls",
        &["--stdio"],
        None,
        &["package.json", "tsconfig.json"],
    );
    add(
        &mut m,
        "typescript-language-server",
        "typescript-language-server",
        &["--stdio"],
        None,
        &["package.json", "tsconfig.json", "jsconfig.json"],
    );
    add(&mut m, "gopls", "gopls", &[], None, &["go.mod", "go.work"]);
    add(
        &mut m,
        "kotlin-language-server",
        "kotlin-language-server",
        &[],
        None,
        &[
            "settings.gradle.kts",
            "settings.gradle",
            "build.gradle.kts",
            "build.gradle",
            "pom.xml",
        ],
    );
    add(
        &mut m,
        "clangd",
        "clangd",
        &[],
        None,
        &[
            "compile_commands.json",
            ".clangd",
            "Makefile",
            "CMakeLists.txt",
        ],
    );
    add(
        &mut m,
        "jdtls",
        "jdtls",
        &[],
        None,
        &["pom.xml", "build.gradle", "build.gradle.kts", ".project"],
    );
    // bash-language-server expects `languageId: "shellscript"`; the
    // `bash` language name wouldn't match.
    add(
        &mut m,
        "bash-language-server",
        "bash-language-server",
        &["start"],
        Some("shellscript"),
        &[],
    );
    add(
        &mut m,
        "vscode-json-language-server",
        "vscode-json-language-server",
        &["--stdio"],
        None,
        &[],
    );
    add(
        &mut m,
        "yaml-language-server",
        "yaml-language-server",
        &["--stdio"],
        None,
        &[],
    );
    add(
        &mut m,
        "marksman",
        "marksman",
        &["server"],
        None,
        &[".marksman.toml"],
    );
    add(
        &mut m,
        "vscode-html-language-server",
        "vscode-html-language-server",
        &["--stdio"],
        None,
        &[],
    );
    add(
        &mut m,
        "vscode-css-language-server",
        "vscode-css-language-server",
        &["--stdio"],
        None,
        &[],
    );
    add(
        &mut m,
        "lua-language-server",
        "lua-language-server",
        &[],
        None,
        &[".luarc.json", ".luarc.jsonc", "stylua.toml"],
    );
    add(
        &mut m,
        "ruby-lsp",
        "ruby-lsp",
        &[],
        None,
        &["Gemfile", ".rubocop.yml"],
    );
    add(&mut m, "zls", "zls", &[], None, &["build.zig"]);
    m
}

// ────────────────────────────────────────────────────────────────────────
// Built-in language defaults
// ────────────────────────────────────────────────────────────────────────

/// Built-in `[languages.<name>]` defaults. To support a new language
/// out-of-the-box, add it here. Users can override every field via
/// `[languages.<name>]` in their config, and they can add entirely new
/// languages with the same syntax.
pub fn builtin_languages() -> HashMap<String, LanguageConfig> {
    let mut m = HashMap::new();
    let lsp = |names: &[&str]| Some(names.iter().map(|s| s.to_string()).collect());

    // rustfmt with no path argument reads stdin and writes stdout —
    // the shape `run_external_formatter` expects.
    m.insert(
        "rust".into(),
        LanguageConfig {
            extensions: Some(vec!["rs".into()]),
            comment_token: Some("//".into()),
            lsp: lsp(&["rust-analyzer"]),
            formatter: Some(FormatterToml {
                command: Some("rustfmt".into()),
                args: None,
            }),
            ..Default::default()
        },
    );
    m.insert(
        "python".into(),
        LanguageConfig {
            extensions: Some(vec!["py".into()]),
            comment_token: Some("#".into()),
            lsp: lsp(&["pyright"]),
            ..Default::default()
        },
    );
    m.insert(
        "toml".into(),
        LanguageConfig {
            extensions: Some(vec!["toml".into()]),
            comment_token: Some("#".into()),
            lsp: lsp(&["taplo"]),
            ..Default::default()
        },
    );
    // TypeScript ships with both vtsls and typescript-language-server
    // — whichever is installed will spawn, the other is silently
    // skipped (`is_command_not_found`). Users who want only one can
    // re-declare `lsp = [...]` in their config.
    m.insert(
        "typescript".into(),
        LanguageConfig {
            extensions: Some(vec!["ts".into(), "tsx".into()]),
            comment_token: Some("//".into()),
            editor: EditorToml {
                indent_width: Some(2),
                tab_width: Some(2),
                ..Default::default()
            },
            lsp: lsp(&["vtsls", "typescript-language-server"]),
            ..Default::default()
        },
    );
    m.insert(
        "javascript".into(),
        LanguageConfig {
            extensions: Some(vec!["js".into(), "jsx".into(), "mjs".into(), "cjs".into()]),
            comment_token: Some("//".into()),
            lsp: lsp(&["typescript-language-server"]),
            ..Default::default()
        },
    );
    // Go is canonically tab-indented (gofmt enforces it).
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
                format_on_save: None,
            },
            lsp: lsp(&["gopls"]),
            formatter: Some(FormatterToml {
                command: Some("gofmt".into()),
                args: None,
            }),
            ..Default::default()
        },
    );
    m.insert(
        "kotlin".into(),
        LanguageConfig {
            extensions: Some(vec!["kt".into(), "kts".into()]),
            comment_token: Some("//".into()),
            lsp: lsp(&["kotlin-language-server"]),
            ..Default::default()
        },
    );
    // `.h` is ambiguous (C or C++); routed to C by default. C++-specific
    // headers (`.hpp`, `.hh`, `.hxx`) go to C++.
    m.insert(
        "c".into(),
        LanguageConfig {
            extensions: Some(vec!["c".into(), "h".into()]),
            comment_token: Some("//".into()),
            lsp: lsp(&["clangd"]),
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
            lsp: lsp(&["clangd"]),
            ..Default::default()
        },
    );
    m.insert(
        "java".into(),
        LanguageConfig {
            extensions: Some(vec!["java".into()]),
            comment_token: Some("//".into()),
            lsp: lsp(&["jdtls"]),
            ..Default::default()
        },
    );
    m.insert(
        "bash".into(),
        LanguageConfig {
            extensions: Some(vec!["sh".into(), "bash".into()]),
            comment_token: Some("#".into()),
            lsp: lsp(&["bash-language-server"]),
            ..Default::default()
        },
    );
    // JSON has no native single-line comment; leaving `comment_token`
    // unset disables the `<space>c` toggle (correct).
    m.insert(
        "json".into(),
        LanguageConfig {
            extensions: Some(vec!["json".into()]),
            comment_token: None,
            lsp: lsp(&["vscode-json-language-server"]),
            ..Default::default()
        },
    );
    m.insert(
        "yaml".into(),
        LanguageConfig {
            extensions: Some(vec!["yaml".into(), "yml".into()]),
            comment_token: Some("#".into()),
            lsp: lsp(&["yaml-language-server"]),
            ..Default::default()
        },
    );
    m.insert(
        "markdown".into(),
        LanguageConfig {
            extensions: Some(vec!["md".into(), "markdown".into()]),
            comment_token: None,
            lsp: lsp(&["marksman"]),
            ..Default::default()
        },
    );
    m.insert(
        "html".into(),
        LanguageConfig {
            extensions: Some(vec!["html".into(), "htm".into()]),
            comment_token: None,
            lsp: lsp(&["vscode-html-language-server"]),
            ..Default::default()
        },
    );
    m.insert(
        "css".into(),
        LanguageConfig {
            extensions: Some(vec!["css".into()]),
            comment_token: None,
            lsp: lsp(&["vscode-css-language-server"]),
            ..Default::default()
        },
    );
    m.insert(
        "lua".into(),
        LanguageConfig {
            extensions: Some(vec!["lua".into()]),
            comment_token: Some("--".into()),
            lsp: lsp(&["lua-language-server"]),
            ..Default::default()
        },
    );
    m.insert(
        "ruby".into(),
        LanguageConfig {
            extensions: Some(vec!["rb".into()]),
            comment_token: Some("#".into()),
            lsp: lsp(&["ruby-lsp"]),
            ..Default::default()
        },
    );
    m.insert(
        "zig".into(),
        LanguageConfig {
            extensions: Some(vec!["zig".into(), "zon".into()]),
            comment_token: Some("//".into()),
            lsp: lsp(&["zls"]),
            formatter: Some(FormatterToml {
                command: Some("zig".into()),
                args: Some(vec!["fmt".into(), "--stdin".into()]),
            }),
            ..Default::default()
        },
    );
    m
}

// ────────────────────────────────────────────────────────────────────────
// Resolve
// ────────────────────────────────────────────────────────────────────────

/// Merge user `[lsp]` over built-in defaults. Fields the user supplied
/// replace ours; the rest survive. New entries (user-only) require
/// `command`.
fn resolve_lsp_table(user: HashMap<String, LspToml>) -> Result<HashMap<String, LspConfig>> {
    let mut merged = builtin_lsp();
    for (name, user_entry) in user {
        if let Some(existing) = merged.get_mut(&name) {
            existing.overlay(user_entry);
        } else {
            merged.insert(name.clone(), LspConfig::from_user(&name, user_entry)?);
        }
    }
    Ok(merged)
}

/// Merge user `[languages]` over built-in defaults and resolve each
/// entry's `lsp` name references against `lsp_table`. Unknown names
/// surface as errors so config typos don't degrade silently.
pub fn resolve(
    user_languages: HashMap<String, LanguageConfig>,
    lsp_table: &HashMap<String, LspConfig>,
) -> Result<HashMap<String, Language>> {
    let mut merged = builtin_languages();
    for (name, user_lang) in user_languages {
        merged
            .entry(name)
            .and_modify(|d| d.overlay(user_lang.clone()))
            .or_insert(user_lang);
    }

    let mut out = HashMap::new();
    for (name, cfg) in merged {
        let lang = build_language(&name, cfg, lsp_table)?;
        out.insert(name, lang);
    }
    Ok(out)
}

fn build_language(
    name: &str,
    c: LanguageConfig,
    lsp_table: &HashMap<String, LspConfig>,
) -> Result<Language> {
    let mut lsp = Vec::new();
    if let Some(refs) = c.lsp {
        for server_name in refs {
            let entry = lsp_table.get(&server_name).ok_or_else(|| {
                anyhow!(
                    "[languages.{}] references unknown server `{}` — add a \
                     `[lsp.{}]` table or use one of the built-in names",
                    name,
                    server_name,
                    server_name
                )
            })?;
            lsp.push(entry.clone());
        }
    }
    let formatter = match c.formatter {
        Some(f) => Some(FormatterConfig {
            command: f.command.ok_or_else(|| {
                anyhow!("[languages.{}.formatter] requires a `command` field", name)
            })?,
            args: f.args.unwrap_or_default(),
        }),
        None => None,
    };
    Ok(Language {
        name: name.to_string(),
        extensions: c.extensions.unwrap_or_default(),
        grammar: c.grammar.unwrap_or_else(|| name.to_string()),
        grammar_dir: c.grammar_dir,
        query_dir: c.query_dir,
        comment_token: c.comment_token,
        editor: c.editor,
        lsp,
        formatter,
    })
}

/// Build an `extension → language name` lookup index. Many-to-one;
/// last-wins on collisions (rare enough that we don't surface them).
fn build_extension_index(langs: &HashMap<String, Language>) -> HashMap<String, String> {
    let mut idx = HashMap::new();
    for (name, lang) in langs {
        for ext in &lang.extensions {
            idx.insert(ext.clone(), name.clone());
        }
    }
    idx
}

/// Catalog of resolved languages with name and extension lookups.
/// Built once at startup from the user's TOML overlaid on the built-in
/// defaults.
#[derive(Debug, Clone, Default)]
pub struct LanguageRegistry {
    by_name: HashMap<String, Language>,
    extension_to_name: HashMap<String, String>,
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
        let lsp_table = resolve_lsp_table(user_lsp)?;
        let by_name = resolve(user_languages, &lsp_table)?;
        let extension_to_name = build_extension_index(&by_name);
        Ok(Self {
            by_name,
            extension_to_name,
        })
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
