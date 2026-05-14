//! Compile-time-embedded query files.
//!
//! `assets/queries/<lang>/*.scm` is vendored in the repo and pulled into
//! the binary via [`include_dir!`]. Doing it at compile time means a
//! shipped `vorto` binary owns its query set — no network access, no
//! tmp-clone gymnastics, no "works on my machine because I have the
//! grammar repo checked out" failures. The trade-off is that refreshing
//! upstream queries requires re-running the scaffolder script and
//! rebuilding.

use include_dir::{Dir, File, include_dir};

/// All vendored query files, organized as `<lang>/<file>.scm`.
pub static QUERIES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets/queries");

/// Return every `.scm` file vendored for `lang`, or an empty slice if
/// the language has no bundled queries (perfectly fine — caller just
/// reports "none shipped").
pub fn files_for(lang: &str) -> Vec<&'static File<'static>> {
    QUERIES
        .get_dir(lang)
        .map(|d| d.files().collect())
        .unwrap_or_default()
}

/// File-stem list (without `.scm`) for a language, sorted. Used by the
/// `list` UI to display which query kinds are bundled.
pub fn bundled_query_names(lang: &str) -> Vec<String> {
    let mut names: Vec<String> = files_for(lang)
        .iter()
        .filter_map(|f| {
            let path = f.path();
            if path.extension().and_then(|s| s.to_str()) != Some("scm") {
                return None;
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_has_bundled_highlights() {
        let names = bundled_query_names("rust");
        assert!(
            names.iter().any(|n| n == "highlights"),
            "expected highlights.scm in bundled rust queries, got {:?}",
            names
        );
    }

    #[test]
    fn unknown_language_returns_empty() {
        assert!(files_for("definitely-no-such-language").is_empty());
        assert!(bundled_query_names("definitely-no-such-language").is_empty());
    }
}
