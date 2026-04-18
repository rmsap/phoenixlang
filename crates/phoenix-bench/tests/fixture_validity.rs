//! Integration tests that verify each benchmark fixture produces the expected
//! output through both the tree-walk interpreter and the IR interpreter.
//!
//! Run with:
//! ```sh
//! cargo test -p phoenix-bench
//! ```

use phoenix_bench::{
    EMPTY, LARGE, MEDIUM, MEDIUM_LARGE, PARSE_ERROR, SMALL, TYPE_ERROR, assert_parse_error,
    assert_type_error, compile, run_ir, run_tree_walk,
};

// ---------------------------------------------------------------------------
// Compilation (IR well-formedness)
// ---------------------------------------------------------------------------

#[test]
fn empty_fixture_compiles() {
    let fn_count = compile("empty", EMPTY);
    assert!(fn_count >= 1, "IR module should contain at least main");
}

#[test]
fn small_fixture_compiles() {
    let fn_count = compile("small", SMALL);
    assert!(fn_count >= 2, "IR module should contain fib and main");
}

#[test]
fn medium_fixture_compiles() {
    let fn_count = compile("medium", MEDIUM);
    assert!(fn_count >= 2, "IR module should contain area and main");
}

#[test]
fn medium_large_fixture_compiles() {
    let fn_count = compile("medium_large", MEDIUM_LARGE);
    assert!(fn_count >= 1, "IR module should contain at least main");
}

#[test]
fn large_fixture_compiles() {
    let fn_count = compile("large", LARGE);
    assert!(fn_count >= 5, "IR module should contain multiple functions");
}

// ---------------------------------------------------------------------------
// Negative-path tests
// ---------------------------------------------------------------------------

#[test]
fn parse_error_fixture_has_parse_errors() {
    assert_parse_error("parse_error", PARSE_ERROR);
}

#[test]
fn type_error_fixture_has_type_errors() {
    assert_type_error("type_error", TYPE_ERROR);
}

// ---------------------------------------------------------------------------
// Tree-walk interpreter tests
// ---------------------------------------------------------------------------

#[test]
fn empty_fixture_tree_walk() {
    let output = run_tree_walk("empty", EMPTY);
    assert!(output.is_empty());
}

#[test]
fn small_fixture_tree_walk() {
    let output = run_tree_walk("small", SMALL);
    assert_eq!(output, vec!["55"]);
}

#[test]
fn medium_fixture_tree_walk() {
    let output = run_tree_walk("medium", MEDIUM);
    assert_eq!(output, vec!["3", "78.53975"]);
}

#[test]
fn medium_large_fixture_tree_walk() {
    let output = run_tree_walk("medium_large", MEDIUM_LARGE);
    assert_eq!(output, vec!["(3, 7)", "25", "120", "[4, 8, 12, 16, 20]"]);
}

#[test]
fn large_fixture_tree_walk() {
    let output = run_tree_walk("large", LARGE);
    let expected = vec![
        "(4, 6)",
        "(8, 12)",
        "circle with radius 5: area = 78.53975",
        "rectangle 3x4: area = 12",
        "triangle base=6 height=3: area = 9",
        "42",
        "Hello, Phoenix!",
        "60",
        "first: 1",
        "success: 42",
        "1",
        "2",
        "Fizz",
        "4",
        "Buzz",
        "Fizz",
        "7",
        "8",
        "Fizz",
        "Buzz",
        "11",
        "Fizz",
        "13",
        "14",
        "FizzBuzz",
        "10",
        "99",
        "45",
    ];
    assert_eq!(output, expected);
}

// ---------------------------------------------------------------------------
// IR interpreter tests
//
// These are #[ignore]-d when the IR interpreter does not yet support the
// required features.  Remove #[ignore] as the IR interpreter gains coverage;
// see the IR interpreter's known-limitations section in its crate docs for
// the current status.
// ---------------------------------------------------------------------------

#[test]
fn empty_fixture_ir_interp() {
    let output = run_ir("empty", EMPTY);
    assert!(output.is_empty());
}

#[test]
fn small_fixture_ir_interp() {
    let output = run_ir("small", SMALL);
    assert_eq!(output, vec!["55"]);
}

#[test]
fn medium_fixture_ir_interp() {
    let output = run_ir("medium", MEDIUM);
    assert_eq!(output, vec!["3", "78.53975"]);
}

#[test]
fn medium_large_fixture_ir_interp() {
    let output = run_ir("medium_large", MEDIUM_LARGE);
    assert_eq!(output, vec!["(3, 7)", "25", "120", "[4, 8, 12, 16, 20]"]);
}

#[test]
#[ignore = "IR interpreter does not yet support string methods — needed for describe()"]
fn large_fixture_ir_interp() {
    let output = run_ir("large", LARGE);
    let expected = vec![
        "(4, 6)",
        "(8, 12)",
        "circle with radius 5: area = 78.53975",
        "rectangle 3x4: area = 12",
        "triangle base=6 height=3: area = 9",
        "42",
        "Hello, Phoenix!",
        "60",
        "first: 1",
        "success: 42",
        "1",
        "2",
        "Fizz",
        "4",
        "Buzz",
        "Fizz",
        "7",
        "8",
        "Fizz",
        "Buzz",
        "11",
        "Fizz",
        "13",
        "14",
        "FizzBuzz",
        "10",
        "99",
        "45",
    ];
    assert_eq!(output, expected);
}
