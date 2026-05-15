//! `file://` ↔ `Path` conversion plus a minimal percent-decoder.

use std::path::{Path, PathBuf};

/// Inverse of [`path_to_uri`]: strip the `file://` scheme and decode
/// percent-escapes. Anything else (`http://`, `untitled:`) returns
/// `None` — we don't try to round-trip those.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // On Windows the spec is `file:///C:/...`; on Unix it's `file:///abs`.
    // Either way the byte after the scheme is `/` and we hand off the
    // remainder as-is (decoded).
    let decoded = percent_decode(rest);
    Some(PathBuf::from(decoded))
}

/// Best-effort `file://` URI for a path. Non-UTF-8 paths fall back to a
/// lossy conversion — we don't need bit-perfect roundtrip, just something
/// the server can match against.
pub fn path_to_uri(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let s = abs.to_string_lossy();
    if s.starts_with('/') {
        format!("file://{}", s)
    } else {
        format!("file:///{}", s)
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_spaces() {
        assert_eq!(percent_decode("foo%20bar"), "foo bar");
        assert_eq!(percent_decode("plain"), "plain");
        // Truncated escape stays literal.
        assert_eq!(percent_decode("foo%"), "foo%");
    }

    #[test]
    fn uri_to_path_strips_scheme_and_decodes() {
        let p = uri_to_path("file:///tmp/with%20space.rs").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/with space.rs"));
        assert!(uri_to_path("https://example.com/x").is_none());
    }
}
