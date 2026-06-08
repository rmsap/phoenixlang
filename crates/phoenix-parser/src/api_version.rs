//! Shared validation and normalization for `api version "..."` prefix strings.
//!
//! The version string named in an `api version "vX" { ... }` block is spliced
//! into every contained endpoint's path as one or more leading segments. Two
//! crates touch it: the parser validates the block header (one diagnostic per
//! block, where block structure still exists), and sema normalizes it again
//! when building the resolved path. Both go through this module so the
//! path-safety rules and the trimming can never drift apart.

/// Normalizes a version string to its canonical leading-path-segment form:
/// surrounding whitespace and slashes stripped. The author may write a version
/// with or without a leading/trailing slash (`"v1"` ≡ `"/v1"` ≡ `" /v1/ "`);
/// they all normalize to the same inner segments.
///
/// ```text
/// "v1"      -> "v1"
/// "/v1/"    -> "v1"
/// " /v1/ "  -> "v1"
/// "v1/beta" -> "v1/beta"   (internal separators are preserved)
/// ```
pub fn normalize_api_version(version: &str) -> &str {
    version.trim().trim_matches('/')
}

/// Validates a version string for path-safety, returning an error message
/// (suitable for a diagnostic) when it would malform or escape the route.
///
/// The string is spliced in literally as leading path segment(s), so once
/// [`normalize_api_version`] has stripped the surrounding slashes/whitespace it
/// must be non-empty, contain only path-segment-safe characters, and have no
/// degenerate segment:
///
/// - **Empty after trimming** (`""`, `"/"`, `"//"`, `" "`) would produce a
///   malformed path like `//posts`.
/// - **Unsafe characters** — anything outside `[A-Za-z0-9-._~/]` could break
///   out of the path (`?`, `#`), inject a phantom path param (`{`/`}`), or
///   malform the route (whitespace, control chars). `/` is permitted for
///   multi-segment prefixes.
/// - **Bad segments** — an internal empty segment (`v1//beta`), a `.`, or a
///   `..` each slip past the char allowlist yet still malform the route
///   (`/v1//posts`) or inject a traversal (`/v1/../posts`). `.` remains legal
///   *inside* a segment (e.g. `v1.0`), so this is a segment-level check.
pub fn validate_api_version(version: &str) -> Result<(), &'static str> {
    let trimmed = normalize_api_version(version);
    if trimmed.is_empty() {
        return Err("`api version` string cannot be empty");
    }
    let has_unsafe_char = trimmed
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/')));
    if has_unsafe_char {
        return Err("`api version` string may contain only letters, digits, and `-._~/`");
    }
    let has_bad_segment = trimmed
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if has_bad_segment {
        return Err("`api version` string may not contain empty, `.`, or `..` path segments");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{normalize_api_version, validate_api_version};

    #[test]
    fn normalize_strips_surrounding_slashes_and_whitespace() {
        assert_eq!(normalize_api_version("v1"), "v1");
        assert_eq!(normalize_api_version("/v1"), "v1");
        assert_eq!(normalize_api_version("/v1/"), "v1");
        assert_eq!(normalize_api_version(" /v1/ "), "v1");
        // Internal separators of a multi-segment prefix are preserved.
        assert_eq!(normalize_api_version("v1/beta"), "v1/beta");
        assert_eq!(normalize_api_version("/v1/beta/"), "v1/beta");
    }

    #[test]
    fn valid_versions_accepted() {
        for ok in [
            "v1", "/v1", "/v1/", " v1 ", "v1.0", "v1/beta", "v1_beta", "v1-beta",
        ] {
            assert!(
                validate_api_version(ok).is_ok(),
                "expected {ok:?} to be valid"
            );
        }
    }

    #[test]
    fn empty_after_trimming_rejected() {
        for bad in ["", "/", "//", " "] {
            assert!(
                validate_api_version(bad).is_err(),
                "expected {bad:?} to be rejected as empty"
            );
        }
    }

    #[test]
    fn unsafe_chars_rejected() {
        for bad in ["v 1", "v1/{id}", "{tenant}", "v1?x=1", "v1#frag"] {
            assert!(
                validate_api_version(bad).is_err(),
                "expected {bad:?} to be rejected for unsafe characters"
            );
        }
    }

    #[test]
    fn bad_segments_rejected() {
        for bad in ["v1//beta", "v1/./beta", "v1/../beta", "v1/.", "v1/../v2"] {
            assert!(
                validate_api_version(bad).is_err(),
                "expected {bad:?} to be rejected for a bad segment"
            );
        }
    }
}
