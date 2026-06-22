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

/// Converts a `PascalCase`/`camelCase` name to `SCREAMING_SNAKE_CASE`
/// (`TicketStatus` → `TICKET_STATUS`).
///
/// Shared by the codegen backends so the Python enum-value casing and the
/// TypeScript `<ENUM>_VALUES` const name cannot drift. A `_` marks each word
/// boundary, then every scalar is uppercased. A boundary is a lower→upper
/// transition (`aB`) or an acronym→word transition (the last cap of a run when
/// the next char is lowercase, so `HTTPError` → `HTTP_ERROR`, not `H_T_T_P…`).
/// An all-caps run with no following word stays intact (`RED` → `RED`).
pub fn to_screaming_snake(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            let next_starts_word = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            // Split when leaving a lowercase/digit run (`a|B`) or when this cap is
            // the start of a new word after an acronym run (`HTTP|Error`).
            if !prev.is_uppercase() || next_starts_word {
                result.push('_');
            }
        }
        result.extend(c.to_uppercase());
    }
    result
}

/// Converts a `camelCase`/`PascalCase` name to `snake_case` (`avatarUrl` →
/// `avatar_url`): a `_` before each non-leading uppercase char, then everything
/// lowercased. The pure casing rule with no language-keyword escaping — the Python
/// backend wraps this to escape keywords, and sema uses it to predict when two
/// distinct field names would collide as one Python attribute. Shared so the
/// collision check and the generator cannot diverge.
pub fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(c.to_lowercase().next().unwrap_or(c));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_snake_case_lowers_and_splits_camel() {
        assert_eq!(to_snake_case("avatarUrl"), "avatar_url");
        assert_eq!(to_snake_case("postId"), "post_id");
        assert_eq!(to_snake_case("id"), "id");
        assert_eq!(to_snake_case("foo_bar"), "foo_bar");
        // `fooBar` and `foo_bar` both collapse to `foo_bar` — the collision sema
        // rejects.
        assert_eq!(to_snake_case("fooBar"), "foo_bar");
        assert_eq!(to_snake_case(""), "");
    }

    #[test]
    fn to_screaming_snake_splits_on_word_boundaries() {
        assert_eq!(to_screaming_snake("TicketStatus"), "TICKET_STATUS");
        assert_eq!(to_screaming_snake("Color"), "COLOR");
        assert_eq!(to_screaming_snake("camelCase"), "CAMEL_CASE");
        // An all-caps variant is a single word, not one letter per `_`.
        assert_eq!(to_screaming_snake("RED"), "RED");
        // An acronym run splits only where the next word begins.
        assert_eq!(to_screaming_snake("HTTPError"), "HTTP_ERROR");
        assert_eq!(to_screaming_snake(""), "");
    }

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
