//! Integration tests that verify each benchmark fixture produces the expected
//! output through both the tree-walk interpreter and the IR interpreter.
//!
//! Run with:
//! ```sh
//! cargo test -p phoenix-bench
//! ```

use phoenix_bench::{
    CompileLinkError, EMPTY, LARGE, MEDIUM, MEDIUM_LARGE, PARSE_ERROR, SMALL, TYPE_ERROR,
    assert_parse_error, assert_type_error, compile, compile_and_link, run_ir, run_native,
    run_tree_walk,
};

/// Native-binary output must match both interpreters and be non-empty.
/// IR-interp is a third witness so a regression in two backends that
/// agrees on the same wrong answer still surfaces; the non-empty
/// checks defend against `[] == [] == []` passing vacuously.
///
/// Complements (does not replace) the IR-only and tree-walk-only
/// fixture tests below — those stay as the canonical reference for
/// each fixture's expected output.
///
/// Runtime lib missing is a visible-skip environmental condition;
/// `PHOENIX_REQUIRE_RUNTIME_LIB=1` turns the skip into a hard fail.
fn assert_native_matches_interps(name: &str, source: &str) {
    let exe = match compile_and_link(name, source) {
        Ok(p) => p,
        Err(CompileLinkError::RuntimeLibMissing) => {
            if std::env::var("PHOENIX_REQUIRE_RUNTIME_LIB").as_deref() == Ok("1") {
                panic!(
                    "PHOENIX_REQUIRE_RUNTIME_LIB=1 set but the runtime static library \
                     is not on any search path — run `cargo build -p phoenix-runtime` \
                     or set $PHOENIX_RUNTIME_LIB"
                );
            }
            eprintln!(
                "warning: skipping {name}_fixture_native — runtime lib not built \
                 (set PHOENIX_REQUIRE_RUNTIME_LIB=1 to fail instead; \
                 `cargo build -p phoenix-runtime` to fix)"
            );
            return;
        }
        Err(e) => panic!("{name} compile/link failed: {e}"),
    };
    let native = run_native(&exe);
    let ir = run_ir(name, source);
    let tree_walk = run_tree_walk(name, source);
    assert!(!native.is_empty(), "{name} native output was empty");
    assert!(!ir.is_empty(), "{name} IR-interp output was empty");
    assert!(!tree_walk.is_empty(), "{name} tree-walk output was empty");
    assert!(
        native == tree_walk && tree_walk == ir,
        "{name}: backends diverged\n  native:    {native:?}\n  IR:        {ir:?}\n  tree-walk: {tree_walk:?}",
    );
}

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

// ---------------------------------------------------------------------------
// Native compile-and-run tests. Same `compile_and_link` + `run_native`
// path the `compile_and_run` bench group exercises — catches
// codegen / linker / runtime regressions the interpreter tests miss.
// The tree-walk fixture tests above stay as the canonical expected
// output; equality is checked transitively through
// `assert_native_matches_interps`.
// ---------------------------------------------------------------------------

#[test]
fn medium_fixture_native() {
    assert_native_matches_interps("medium", MEDIUM);
}

// `medium_large` and `large` are blocked on outstanding Cranelift
// codegen gaps. Drop the matching `#[ignore]` once the capability
// lands; the pipeline bench's `COMPILE_AND_RUN_FIXTURES` will
// auto-enable the matching `compile_and_run` group at the same time.

#[test]
#[ignore = "blocked on phoenix-cranelift: print() of list<i64> not yet lowered"]
fn medium_large_fixture_native() {
    assert_native_matches_interps("medium_large", MEDIUM_LARGE);
}

#[test]
#[ignore = "blocked on phoenix-cranelift: string methods used by describe() not yet lowered"]
fn large_fixture_native() {
    assert_native_matches_interps("large", LARGE);
}
