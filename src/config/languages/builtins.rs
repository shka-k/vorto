//! Out-of-the-box LSP server and language definitions. Users overlay
//! their own `[lsp.<name>]` / `[languages.<name>]` tables onto these
//! at startup ([`super::resolve::resolve_lsp_table`] /
//! [`super::resolve::resolve`]).

use std::collections::HashMap;

use super::{FormatterToml, LanguageConfig, LspConfig};
use crate::config::editor::EditorToml;

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
                indent_guides: None,
                indent_guides_skip_levels: None,
                indent_guide_style: None,
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
