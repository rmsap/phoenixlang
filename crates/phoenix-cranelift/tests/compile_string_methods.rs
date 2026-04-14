//! Integration tests for compiled String builtin methods.
//!
//! Each test compiles a Phoenix program via Cranelift and verifies
//! the output matches the IR interpreter (see [`common::roundtrip`]).
//! The `split` method is not tested here because it returns a
//! `List<String>`, which is not yet supported in compiled mode.

mod common;
use common::roundtrip;

// ── length ──────────────────────────────────────────────────────────

#[test]
fn string_length() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.length())
}
"#,
    );
}

#[test]
fn string_length_empty() {
    roundtrip(
        r#"
function main() {
    let s: String = ""
    print(s.length())
}
"#,
    );
}

#[test]
fn string_length_unicode() {
    roundtrip(
        r#"
function main() {
    let s: String = "h\u{00e9}llo"
    print(s.length())
}
"#,
    );
}

// ── contains ────────────────────────────────────────────────────────

#[test]
fn string_contains_true() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.contains("world"))
}
"#,
    );
}

#[test]
fn string_contains_false() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.contains("xyz"))
}
"#,
    );
}

#[test]
fn string_contains_empty_substring() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.contains(""))
}
"#,
    );
}

// ── startsWith ──────────────────────────────────────────────────────

#[test]
fn string_starts_with_true() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.startsWith("hello"))
}
"#,
    );
}

#[test]
fn string_starts_with_false() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.startsWith("world"))
}
"#,
    );
}

// ── endsWith ────────────────────────────────────────────────────────

#[test]
fn string_ends_with_true() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.endsWith("world"))
}
"#,
    );
}

#[test]
fn string_ends_with_false() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.endsWith("hello"))
}
"#,
    );
}

// ── trim ────────────────────────────────────────────────────────────

#[test]
fn string_trim() {
    roundtrip(
        r#"
function main() {
    let s: String = "  hello  "
    print(s.trim())
}
"#,
    );
}

#[test]
fn string_trim_no_whitespace() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.trim())
}
"#,
    );
}

#[test]
fn string_trim_empty() {
    roundtrip(
        r#"
function main() {
    let s: String = "   "
    print(s.trim())
}
"#,
    );
}

// ── toLowerCase ─────────────────────────────────────────────────────

#[test]
fn string_to_lower_case() {
    roundtrip(
        r#"
function main() {
    let s: String = "Hello World"
    print(s.toLowerCase())
}
"#,
    );
}

#[test]
fn string_to_lower_case_already_lower() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.toLowerCase())
}
"#,
    );
}

// ── toUpperCase ─────────────────────────────────────────────────────

#[test]
fn string_to_upper_case() {
    roundtrip(
        r#"
function main() {
    let s: String = "Hello World"
    print(s.toUpperCase())
}
"#,
    );
}

#[test]
fn string_to_upper_case_already_upper() {
    roundtrip(
        r#"
function main() {
    let s: String = "HELLO"
    print(s.toUpperCase())
}
"#,
    );
}

// ── indexOf ─────────────────────────────────────────────────────────

#[test]
fn string_index_of_found() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.indexOf("world"))
}
"#,
    );
}

#[test]
fn string_index_of_not_found() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.indexOf("xyz"))
}
"#,
    );
}

#[test]
fn string_index_of_at_start() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.indexOf("hel"))
}
"#,
    );
}

#[test]
fn string_index_of_empty_substring() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.indexOf(""))
}
"#,
    );
}

// ── replace ─────────────────────────────────────────────────────────

#[test]
fn string_replace() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.replace("world", "phoenix"))
}
"#,
    );
}

#[test]
fn string_replace_multiple() {
    roundtrip(
        r#"
function main() {
    let s: String = "aaa"
    print(s.replace("a", "bb"))
}
"#,
    );
}

#[test]
fn string_replace_not_found() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.replace("xyz", "abc"))
}
"#,
    );
}

// ── substring ───────────────────────────────────────────────────────

#[test]
fn string_substring() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.substring(0, 5))
}
"#,
    );
}

#[test]
fn string_substring_middle() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    print(s.substring(6, 11))
}
"#,
    );
}

#[test]
fn string_substring_empty_range() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.substring(2, 2))
}
"#,
    );
}

// ── chained methods ─────────────────────────────────────────────────

#[test]
fn string_method_chain() {
    roundtrip(
        r#"
function main() {
    let s: String = "  Hello World  "
    print(s.trim().toLowerCase())
}
"#,
    );
}

#[test]
fn string_replace_and_length() {
    roundtrip(
        r#"
function main() {
    let s: String = "aabbcc"
    let replaced: String = s.replace("bb", "x")
    print(replaced)
    print(replaced.length())
}
"#,
    );
}

// ── additional edge cases ──────────────────────────────────────────

#[test]
fn string_contains_empty_receiver() {
    roundtrip(
        r#"
function main() {
    let s: String = ""
    print(s.contains("a"))
}
"#,
    );
}

#[test]
fn string_starts_with_empty_receiver() {
    roundtrip(
        r#"
function main() {
    let s: String = ""
    print(s.startsWith("a"))
}
"#,
    );
}

#[test]
fn string_ends_with_empty_receiver() {
    roundtrip(
        r#"
function main() {
    let s: String = ""
    print(s.endsWith("a"))
}
"#,
    );
}

#[test]
fn string_index_of_unicode() {
    roundtrip(
        r#"
function main() {
    let s: String = "h\u{00e9}llo"
    print(s.indexOf("llo"))
}
"#,
    );
}

#[test]
fn string_replace_empty_replacement() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.replace("l", ""))
}
"#,
    );
}

#[test]
fn string_to_lower_case_unicode() {
    roundtrip(
        r#"
function main() {
    let s: String = "\u{00dc}BER"
    print(s.toLowerCase())
}
"#,
    );
}

#[test]
fn string_to_upper_case_unicode() {
    roundtrip(
        r#"
function main() {
    let s: String = "\u{00fc}ber"
    print(s.toUpperCase())
}
"#,
    );
}

// ── additional coverage ───────────────────────────────────────────

#[test]
fn string_trim_leading_only() {
    roundtrip(
        r#"
function main() {
    let s: String = "   hello"
    print(s.trim())
}
"#,
    );
}

#[test]
fn string_trim_trailing_only() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello   "
    print(s.trim())
}
"#,
    );
}

#[test]
fn string_substring_full_range() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.substring(0, 5))
}
"#,
    );
}

#[test]
fn string_substring_unicode() {
    roundtrip(
        r#"
function main() {
    let s: String = "h\u{00e9}llo"
    print(s.substring(1, 3))
}
"#,
    );
}

#[test]
fn string_replace_empty_from() {
    roundtrip(
        r#"
function main() {
    let s: String = "ab"
    print(s.replace("", "x"))
}
"#,
    );
}

#[test]
fn string_index_of_empty_haystack() {
    roundtrip(
        r#"
function main() {
    let s: String = ""
    print(s.indexOf("a"))
}
"#,
    );
}

#[test]
fn string_starts_with_empty_prefix() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.startsWith(""))
}
"#,
    );
}

#[test]
fn string_ends_with_empty_suffix() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.endsWith(""))
}
"#,
    );
}

#[test]
fn string_method_on_function_result() {
    roundtrip(
        r#"
function greet(name: String) -> String {
    "hello {name}"
}

function main() {
    print(greet("world").toUpperCase())
}
"#,
    );
}

#[test]
fn string_method_in_if_condition() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello world"
    if s.contains("world") {
        print("yes")
    } else {
        print("no")
    }
}
"#,
    );
}

#[test]
fn string_method_result_in_interpolation() {
    roundtrip(
        r#"
function main() {
    let s: String = "  hello  "
    print("trimmed: {s.trim()}")
}
"#,
    );
}

// ── additional gap coverage ───────────────────────────────────────

#[test]
fn string_length_emoji() {
    roundtrip(
        r#"
function main() {
    let s: String = "\u{1F600}"
    print(s.length())
}
"#,
    );
}

#[test]
fn string_contains_identity() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.contains("hello"))
}
"#,
    );
}

#[test]
fn string_index_of_empty_haystack_nonempty_needle() {
    roundtrip(
        r#"
function main() {
    let s: String = ""
    print(s.indexOf("a"))
}
"#,
    );
}

#[test]
fn string_index_of_full_match() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.indexOf("hello"))
}
"#,
    );
}

#[test]
fn string_replace_unicode() {
    roundtrip(
        r#"
function main() {
    let s: String = "h\u{00e9}llo"
    print(s.replace("\u{00e9}", "e"))
}
"#,
    );
}

#[test]
fn string_replace_identity() {
    roundtrip(
        r#"
function main() {
    let s: String = "hello"
    print(s.replace("l", "l"))
}
"#,
    );
}

#[test]
fn string_trim_tabs_and_newlines() {
    roundtrip(
        "function main() {\n    let s: String = \"\\t\\nhello\\r\\n\"\n    print(s.trim())\n}\n",
    );
}

#[test]
fn string_trim_inner_whitespace_preserved() {
    roundtrip(
        r#"
function main() {
    let s: String = "a b c"
    print(s.trim())
}
"#,
    );
}

#[test]
fn string_method_chain_three() {
    roundtrip(
        r#"
function main() {
    let s: String = "  Hello World  "
    print(s.trim().toLowerCase().replace("world", "phoenix"))
}
"#,
    );
}

#[test]
fn string_method_on_interpolation_result() {
    roundtrip(
        r#"
function main() {
    let name: String = "World"
    let greeting: String = "hello {name}"
    print(greeting.toUpperCase())
}
"#,
    );
}
