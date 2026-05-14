//! Built-in catalog of tree-sitter grammar repos.
//!
//! A "recipe" is the minimum info needed to fetch a grammar's source and
//! point `tree-sitter build` at it: the git repo URL, an optional
//! subdirectory for monorepos (e.g. tree-sitter-typescript holds both
//! `typescript/` and `tsx/`), and an optional pinned revision.
//!
//! Adding a new built-in language is a one-line addition to
//! [`builtin_recipes`]. Users who want a grammar that's not built in can
//! still install it manually by dropping the `.so` into `grammar_dir`.

/// Static description of how to fetch and build one grammar.
#[derive(Debug, Clone)]
pub struct GrammarRecipe {
    /// Logical name — the filename stem the loader looks for
    /// (`<name>.{so,dylib,dll}`) and the symbol root
    /// (`tree_sitter_<name>`).
    pub name: &'static str,
    /// Git URL to clone.
    pub repo: &'static str,
    /// Optional subdirectory inside the cloned repo to build from. Used
    /// for monorepos like tree-sitter-typescript that ship multiple
    /// grammars side-by-side.
    pub subpath: Option<&'static str>,
    /// Optional pinned git revision (tag, branch, or commit). When
    /// `None`, the default branch is shallow-cloned. When `Some`, a full
    /// clone is performed and the rev checked out.
    pub rev: Option<&'static str>,
}

/// The built-in catalog. Names here line up with the language entries in
/// [`crate::config::languages::builtin_languages`] so `vorto grammar
/// install <lang>` "just works" for the languages that ship out of the
/// box.
pub fn builtin_recipes() -> Vec<GrammarRecipe> {
    vec![
        GrammarRecipe {
            name: "rust",
            repo: "https://github.com/tree-sitter/tree-sitter-rust",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "python",
            repo: "https://github.com/tree-sitter/tree-sitter-python",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "go",
            repo: "https://github.com/tree-sitter/tree-sitter-go",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "javascript",
            repo: "https://github.com/tree-sitter/tree-sitter-javascript",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "typescript",
            repo: "https://github.com/tree-sitter/tree-sitter-typescript",
            subpath: Some("typescript"),
            rev: None,
        },
        GrammarRecipe {
            name: "tsx",
            repo: "https://github.com/tree-sitter/tree-sitter-typescript",
            subpath: Some("tsx"),
            rev: None,
        },
        GrammarRecipe {
            name: "toml",
            repo: "https://github.com/tree-sitter-grammars/tree-sitter-toml",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "kotlin",
            repo: "https://github.com/fwcd/tree-sitter-kotlin",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "c",
            repo: "https://github.com/tree-sitter/tree-sitter-c",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "cpp",
            repo: "https://github.com/tree-sitter/tree-sitter-cpp",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "java",
            repo: "https://github.com/tree-sitter/tree-sitter-java",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "bash",
            repo: "https://github.com/tree-sitter/tree-sitter-bash",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "json",
            repo: "https://github.com/tree-sitter/tree-sitter-json",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "yaml",
            repo: "https://github.com/tree-sitter-grammars/tree-sitter-yaml",
            subpath: None,
            rev: None,
        },
        // `tree-sitter-grammars/tree-sitter-markdown` ships two grammars in
        // one repo: a block-level one (`tree-sitter-markdown/`) and an
        // inline one (`tree-sitter-markdown-inline/`) intended to be used
        // via injection. We install only the block grammar here — the
        // inline grammar requires editor-side injection plumbing that
        // doesn't exist yet, and installing it standalone would just be
        // dead weight.
        GrammarRecipe {
            name: "markdown",
            repo: "https://github.com/tree-sitter-grammars/tree-sitter-markdown",
            subpath: Some("tree-sitter-markdown"),
            rev: None,
        },
        GrammarRecipe {
            name: "html",
            repo: "https://github.com/tree-sitter/tree-sitter-html",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "css",
            repo: "https://github.com/tree-sitter/tree-sitter-css",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "lua",
            repo: "https://github.com/tree-sitter-grammars/tree-sitter-lua",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "ruby",
            repo: "https://github.com/tree-sitter/tree-sitter-ruby",
            subpath: None,
            rev: None,
        },
        GrammarRecipe {
            name: "zig",
            repo: "https://github.com/tree-sitter-grammars/tree-sitter-zig",
            subpath: None,
            rev: None,
        },
    ]
}

/// Look up a recipe by name. Returns `None` when no built-in recipe
/// matches — callers should report the available names to the user.
pub fn find_recipe(name: &str) -> Option<GrammarRecipe> {
    builtin_recipes().into_iter().find(|r| r.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_contains_rust() {
        assert!(find_recipe("rust").is_some());
    }

    #[test]
    fn typescript_and_tsx_share_repo_with_subpaths() {
        let ts = find_recipe("typescript").unwrap();
        let tsx = find_recipe("tsx").unwrap();
        assert_eq!(ts.repo, tsx.repo);
        assert_eq!(ts.subpath, Some("typescript"));
        assert_eq!(tsx.subpath, Some("tsx"));
    }

    #[test]
    fn unknown_recipe_is_none() {
        assert!(find_recipe("does-not-exist").is_none());
    }
}
