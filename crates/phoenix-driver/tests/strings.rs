mod common;
use common::*;

/// String escape sequences work correctly end-to-end.
#[test]
fn string_escapes() {
    run_expect(
        r#"
function main() {
  let nl: String = "hello\nworld"
  print(nl)
  let tab: String = "a\tb"
  print(tab)
  let bs: String = "a\\b"
  print(bs)
  let quote: String = "say \"hi\""
  print(quote)
}
"#,
        &["hello", "world", "a\tb", "a\\b", "say \"hi\""],
    );
}

// ── 1.8.2: String Interpolation ────────────────────────────────────

#[test]
fn string_interpolation_basic() {
    run_expect(
        r#"
function main() {
  let name: String = "world"
  print("hello {name}")
}
"#,
        &["hello world"],
    );
}

#[test]
fn string_interpolation_expression() {
    run_expect(
        r#"
function main() {
  let x: Int = 5
  let y: Int = 3
  print("{x} + {y} = {x + y}")
}
"#,
        &["5 + 3 = 8"],
    );
}

#[test]
fn string_interpolation_multiple_segments() {
    run_expect(
        r#"
function main() {
  let first: String = "Alice"
  let age: Int = 30
  print("Name: {first}, Age: {age}")
}
"#,
        &["Name: Alice, Age: 30"],
    );
}

#[test]
fn string_interpolation_escaped_braces() {
    run_expect(
        r#"
function main() {
  print("literal {{braces}}")
}
"#,
        &["literal {braces}"],
    );
}

#[test]
fn string_interpolation_no_interpolation() {
    run_expect(
        r#"
function main() {
  print("just a plain string")
}
"#,
        &["just a plain string"],
    );
}

#[test]
fn string_interpolation_with_method_call() {
    run_expect(
        r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  print("length is {nums.length()}")
}
"#,
        &["length is 3"],
    );
}

#[test]
fn string_interpolation_in_variable() {
    run_expect(
        r#"
function main() {
  let val: Int = 42
  let msg: String = "the answer is {val}"
  print(msg)
}
"#,
        &["the answer is 42"],
    );
}

#[test]
fn string_interpolation_nested_function_call() {
    run_expect(
        r#"
function double(x: Int) -> Int {
  return x * 2
}
function main() {
  print("double of 5 is {double(5)}")
}
"#,
        &["double of 5 is 10"],
    );
}

#[test]
fn string_interpolation_bool_and_float() {
    run_expect(
        r#"
function main() {
  let flag: Bool = true
  let pi: Float = 3.14
  print("flag={flag}, pi={pi}")
}
"#,
        &["flag=true, pi=3.14"],
    );
}

#[test]
fn string_interpolation_with_field_access() {
    run_expect(
        r#"
struct User {
  String name
  Int age
}
function main() {
  let u: User = User("Bob", 25)
  print("User: {u.name}, Age: {u.age}")
}
"#,
        &["User: Bob, Age: 25"],
    );
}

#[test]
fn implicit_return_with_string_interpolation() {
    run_expect(
        r#"
function greet(name: String, age: Int) -> String {
  "Hello {name}, you are {age} years old"
}
function main() {
  print(greet("Alice", 30))
}
"#,
        &["Hello Alice, you are 30 years old"],
    );
}

// ── 1.8 Edge cases: String Interpolation ───────────────────────────

#[test]
fn string_interpolation_adjacent() {
    run_expect(
        r#"
function main() {
  let a: Int = 1
  let b: Int = 2
  print("{a}{b}")
}
"#,
        &["12"],
    );
}

#[test]
fn string_interpolation_only_expression() {
    run_expect(
        r#"
function main() {
  let x: Int = 42
  print("{x}")
}
"#,
        &["42"],
    );
}

#[test]
fn string_interpolation_escaped_and_real_mixed() {
    run_expect(
        r#"
function main() {
  let x: Int = 5
  print("{{literal}} {x}")
}
"#,
        &["{literal} 5"],
    );
}

#[test]
fn string_interpolation_escaped_closing_brace() {
    run_expect(
        r#"
function main() {
  let x: Int = 5
  print("{x}}}")
}
"#,
        &["5}"],
    );
}

#[test]
fn string_interpolation_to_string_call() {
    run_expect(
        r#"
function main() {
  let x: Int = 42
  print("value: {toString(x)}")
}
"#,
        &["value: 42"],
    );
}

#[test]
fn unknown_string_escape_passthrough() {
    // Unknown escape sequences are preserved literally (backslash + char)
    run_expect(
        r#"
function main() {
  let s: String = "hello\x41world"
  print(s)
}
"#,
        &["hello\\x41world"],
    );
}

// --- String interpolation edge cases ---

#[test]
fn string_interpolation_complex_expression() {
    run_expect(
        r#"
function main() {
    let x: Int = 5
    print("{x * x + 1}")
}
"#,
        &["26"],
    );
}

#[test]
fn string_concatenation() {
    run_expect(
        r#"
function main() {
    let greeting: String = "hello" + " " + "world"
    print(greeting)
}
"#,
        &["hello world"],
    );
}

#[test]
fn string_interpolation_with_match() {
    run_expect(
        r#"
enum Color { Red Green Blue }
function describe(c: Color) -> String {
    let name: String = match c {
        Red -> "red"
        Green -> "green"
        Blue -> "blue"
    }
    "the color is {name}"
}
function main() {
    print(describe(Red))
    print(describe(Blue))
}
"#,
        &["the color is red", "the color is blue"],
    );
}

#[test]
fn chained_string_concatenation() {
    run_expect(
        r#"
function main() {
    let a: String = "a"
    let b: String = "b"
    let c: String = "c"
    let result: String = a + b + c
    print(result)
}
"#,
        &["abc"],
    );
}

// ── String interpolation with complex nested expressions ──────────────

#[test]
fn string_interpolation_with_arithmetic_and_bool() {
    run_expect(
        r#"
function main() {
    let x: Int = 5
    let msg: String = "value is {x}, doubled is {x * 2}, even: {x % 2 == 0}"
    print(msg)
}
"#,
        &["value is 5, doubled is 10, even: false"],
    );
}

#[test]
fn output_string_interpolation() {
    run_expect(
        r#"
function main() {
  let name: String = "world"
  print("hello {name}")
  let x: Int = 5
  let y: Int = 3
  print("{x} + {y} = {x + y}")
}
"#,
        &["hello world", "5 + 3 = 8"],
    );
}

// ── String methods ──────────────────────────────────────────────────────

#[test]
fn string_method_length() {
    run_expect(
        r#"
function main() {
    print("hello".length())
    print("".length())
}
"#,
        &["5", "0"],
    );
}

#[test]
fn string_method_contains() {
    run_expect(
        r#"
function main() {
    print("hello world".contains("world"))
    print("hello world".contains("xyz"))
}
"#,
        &["true", "false"],
    );
}

#[test]
fn string_method_starts_ends_with() {
    run_expect(
        r#"
function main() {
    print("hello world".startsWith("hello"))
    print("hello world".endsWith("world"))
    print("hello world".startsWith("world"))
}
"#,
        &["true", "true", "false"],
    );
}

#[test]
fn string_method_trim() {
    run_expect(
        r#"
function main() {
    print("  hello  ".trim())
}
"#,
        &["hello"],
    );
}

#[test]
fn string_method_case() {
    run_expect(
        r#"
function main() {
    print("Hello World".toLowerCase())
    print("Hello World".toUpperCase())
}
"#,
        &["hello world", "HELLO WORLD"],
    );
}

#[test]
fn string_method_split() {
    run_expect(
        r#"
function main() {
    let parts: List<String> = "a,b,c".split(",")
    print(parts.length())
    print(parts.get(0))
    print(parts.get(1))
    print(parts.get(2))
}
"#,
        &["3", "a", "b", "c"],
    );
}

#[test]
fn string_method_replace() {
    run_expect(
        r#"
function main() {
    print("hello world".replace("world", "phoenix"))
}
"#,
        &["hello phoenix"],
    );
}

#[test]
fn string_method_substring() {
    run_expect(
        r#"
function main() {
    print("hello world".substring(0, 5))
    print("hello world".substring(6, 11))
}
"#,
        &["hello", "world"],
    );
}

#[test]
fn string_method_index_of() {
    run_expect(
        r#"
function main() {
    print("hello world".indexOf("world"))
    print("hello world".indexOf("xyz"))
}
"#,
        &["6", "-1"],
    );
}

#[test]
fn string_ordering_comparisons() {
    run_expect(
        r#"
function main() {
    print("apple" < "banana")
    print("banana" > "apple")
    print("abc" <= "abc")
    print("abc" >= "abd")
}
"#,
        &["true", "true", "true", "false"],
    );
}

// ════════════════════════════════════════════════════════════════════════
// Edge case tests
// ════════════════════════════════════════════════════════════════════════

// ── String method edge cases ────────────────────────────────────────────

#[test]
fn string_empty_edge_cases() {
    run_expect(
        r#"
function main() {
    print("".trim())
    print("".toLowerCase())
    print("".toUpperCase())
    let parts: List<String> = "".split(",")
    print(parts.length())
    print("".contains(""))
    print("".indexOf(""))
}
"#,
        &["", "", "", "1", "true", "0"],
    );
}

#[test]
fn string_methods_on_variables() {
    run_expect(
        r#"
function main() {
    let s: String = "Hello World"
    let lower: String = s.toLowerCase()
    print(lower)
    print(s.contains("World"))
    print(s.length())
}
"#,
        &["hello world", "true", "11"],
    );
}

#[test]
fn string_substring_out_of_bounds() {
    expect_runtime_error(
        r#"
function main() {
    let s: String = "hello"
    print(s.substring(0, 100))
}
"#,
        "out of bounds",
    );
}

#[test]
fn string_method_wrong_arg_type() {
    expect_type_error(
        r#"
function main() {
    let s: String = "hello"
    print(s.contains(42))
}
"#,
        "expected String but got Int",
    );
}

#[test]
fn string_ordering_equal_and_empty() {
    run_expect(
        r#"
function main() {
    print("abc" <= "abc")
    print("abc" >= "abc")
    print("" < "a")
    print("" <= "")
}
"#,
        &["true", "true", "true", "true"],
    );
}

#[test]
fn map_in_string_interpolation() {
    run_expect(
        r#"
function main() {
    let m: Map<String, Int> = {"a": 1}
    print("map: {m}")
}
"#,
        &["map: {a: 1}"],
    );
}

// ── String interpolation with complex types ─────────────────────────────

#[test]
fn string_interpolation_list_and_option() {
    run_expect(
        r#"
function main() {
    let xs: List<Int> = [1, 2, 3]
    print("list: {xs}")
    let opt: Option<Int> = Some(42)
    print("opt: {opt}")
}
"#,
        &["list: [1, 2, 3]", "opt: Some(42)"],
    );
}

// ── Substring start > end ───────────────────────────────────────────────

#[test]
fn string_substring_start_greater_than_end() {
    expect_runtime_error(
        r#"
function main() {
    print("hello".substring(3, 1))
}
"#,
        "out of bounds",
    );
}

// ── Cross-feature: map + string methods ─────────────────────────────────

#[test]
fn map_with_string_method_values() {
    run_expect(
        r#"
function main() {
    let words: List<String> = ["hello", "WORLD", "  foo  "]
    let processed: List<String> = words.map(function(s: String) -> String { s.trim().toLowerCase() })
    for w in processed {
        print(w)
    }
}
"#,
        &["hello", "world", "foo"],
    );
}

// ── Tier B: Built-in method wrong arg count (type errors) ────────────────

#[test]
fn string_contains_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    print("hello".contains())
}
"#,
        "takes 1 argument",
    );
}

#[test]
fn string_replace_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    print("hello".replace("a"))
}
"#,
        "takes 2 argument",
    );
}

#[test]
fn string_substring_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    print("hello".substring(0))
}
"#,
        "takes 2 argument",
    );
}

#[test]
fn string_split_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    let parts: List<String> = "hello".split()
}
"#,
        "takes 1 argument",
    );
}

#[test]
fn string_index_of_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    print("hello".indexOf())
}
"#,
        "takes 1 argument",
    );
}

#[test]
fn string_starts_with_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    print("hello".startsWith())
}
"#,
        "takes 1 argument",
    );
}

#[test]
fn string_ends_with_wrong_arg_count() {
    expect_type_error(
        r#"
function main() {
    print("hello".endsWith())
}
"#,
        "takes 1 argument",
    );
}

#[test]
fn string_concat_type_mismatch() {
    expect_type_error(
        r#"
function main() {
    print("hello" + 42)
}
"#,
        "cannot apply",
    );
}

/// Escape sequences inside string interpolation.
#[test]
fn escape_sequences_in_string_interpolation() {
    run_expect(
        r#"
function main() {
  let name: String = "world"
  print("hello\n{name}\tend")
}
"#,
        &["hello", "world\tend"],
    );
}

/// String methods on empty string.
#[test]
fn string_methods_on_empty_string() {
    run_expect(
        r#"
function main() {
  let s: String = ""
  print(s.length())
  print(s.trim())
  print(s.contains(""))
  print(s.startsWith(""))
  print(s.endsWith(""))
}
"#,
        &["0", "", "true", "true", "true"],
    );
}

// ── Bug fix: escaped braces in non-interpolated strings ────────────

/// Escaped braces `{{` and `}}` in a string without interpolation should
/// produce literal `{` and `}` characters.
#[test]
fn escaped_braces_no_interpolation() {
    run_expect(
        r#"
function main() {
  let s: String = "use {{braces}} here"
  print(s)
}
"#,
        &["use {braces} here"],
    );
}

/// Only opening escaped brace `{{`.
#[test]
fn escaped_opening_brace_only() {
    run_expect(
        r#"
function main() {
  print("open {{")
}
"#,
        &["open {"],
    );
}

/// Only closing escaped brace `}}`.
#[test]
fn escaped_closing_brace_only() {
    run_expect(
        r#"
function main() {
  print("close }}")
}
"#,
        &["close }"],
    );
}

/// Mixed: escaped braces alongside actual interpolation.
#[test]
fn escaped_braces_with_interpolation() {
    run_expect(
        r#"
function main() {
  let x: Int = 42
  let s: String = "value: {x} and {{literal}}"
  print(s)
}
"#,
        &["value: 42 and {literal}"],
    );
}

// --- index_of character index regression tests ---

/// `index_of` must return a character index, not a byte offset.
/// For ASCII strings both are the same, but for multi-byte UTF-8 they differ.
#[test]
fn index_of_returns_character_index_for_multibyte_string() {
    // "café" has 4 characters but 5 bytes (é is 2 bytes in UTF-8).
    // index_of("é") should return 3 (character index), not 3 (happens to be
    // the same for this case). A more discriminating test uses index_of on
    // a character that appears after the multi-byte character.
    run_expect(
        r#"
function main() {
  let s: String = "café!"
  print(toString(s.indexOf("!")))
}
"#,
        &["4"],
    );
}

// ── Empty string interpolation ─────────────────────────────────────────

#[test]
fn empty_string_interpolation_is_parse_error() {
    expect_parse_error(
        r#"
function main() {
  print("{}")
}
"#,
        "expected expression",
    );
}

/// Verify index_of + substring round-trips correctly on multi-byte strings.
#[test]
fn index_of_and_substring_roundtrip_multibyte() {
    run_expect(
        r#"
function main() {
  let s: String = "über cool"
  let idx: Int = s.indexOf("cool")
  print(s.substring(idx, idx + 4))
}
"#,
        &["cool"],
    );
}
