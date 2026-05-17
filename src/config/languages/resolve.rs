//! User-config × built-in-defaults merge step. Field-level overlay for
//! `[lsp]` and `[languages]`, then resolution of each language's `lsp`
//! reference list against the merged server table.

use std::collections::HashMap;

use anyhow::{Result, anyhow};

use super::builtins::{builtin_languages, builtin_lsp};
use super::{FormatterConfig, Language, LanguageConfig, LspConfig, LspToml};

/// Merge user `[lsp]` over built-in defaults. Fields the user supplied
/// replace ours; the rest survive. New entries (user-only) require
/// `command`.
pub(super) fn resolve_lsp_table(
    user: HashMap<String, LspToml>,
) -> Result<HashMap<String, LspConfig>> {
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
pub(super) fn resolve(
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
pub(super) fn build_extension_index(langs: &HashMap<String, Language>) -> HashMap<String, String> {
    let mut idx = HashMap::new();
    for (name, lang) in langs {
        for ext in &lang.extensions {
            idx.insert(ext.clone(), name.clone());
        }
    }
    idx
}
