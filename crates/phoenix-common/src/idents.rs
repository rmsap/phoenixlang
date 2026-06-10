//! Identifier-casing helpers shared between sema and the codegen backends.

/// Capitalizes the first character of a string, leaving the tail unchanged.
///
/// Returns an empty string for empty input.
///
/// This is the casing rule every codegen backend uses to build generated
/// type names from endpoint names — notably the envelope types
/// `<Endpoint>Result` / `<Endpoint>Page` / `<Endpoint>Response` — and the
/// rule sema's envelope-collision check mirrors when predicting those names.
/// It lives here, in the one crate both depend on, so the check and the
/// generators cannot silently diverge.
pub fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capitalize_handles_empty_and_unicode() {
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("post"), "Post");
        assert_eq!(capitalize("Post"), "Post");
        // Non-ASCII: `char::to_uppercase` uppercases the first scalar value
        // and may expand it to multiple chars (`ß` → `SS`). Sema's collision
        // check and the codegen backends agree on these outputs only because
        // they share this one function.
        assert_eq!(capitalize("über"), "Über");
        assert_eq!(capitalize("ßeta"), "SSeta");
    }
}
