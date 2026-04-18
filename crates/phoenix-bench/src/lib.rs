//! Benchmarks and validation tests for the Phoenix compiler pipeline.
//!
//! This crate measures the performance of each compilation stage (lex, parse,
//! sema, IR lowering, Cranelift native code generation) and both interpreters
//! (tree-walk and IR) across fixture programs of increasing complexity.  It
//! also contains integration tests that verify each fixture produces the
//! expected output.
//!
//! # Running benchmarks
//!
//! ```sh
//! cargo bench -p phoenix-bench
//! ```
//!
//! To save a baseline for later regression comparison:
//!
//! ```sh
//! cargo bench -p phoenix-bench -- --save-baseline <name>
//! ```
//!
//! To compare against a saved baseline:
//!
//! ```sh
//! cargo bench -p phoenix-bench -- --baseline <name>
//! ```
//!
//! # Running fixture validity tests
//!
//! ```sh
//! cargo test -p phoenix-bench
//! ```

#![warn(missing_docs)]

use phoenix_common::span::SourceId;

/// Source ID used for all benchmark fixtures (no real file backing).
pub const BENCH_SOURCE_ID: SourceId = SourceId(0);

// ---------------------------------------------------------------------------
// Fixture sources
// ---------------------------------------------------------------------------

/// Minimal fixture: recursion, if/else, arithmetic (8 lines).
pub const SMALL: &str = include_str!("../benches/fixtures/small.phx");

/// Moderate fixture: structs, enums, pattern matching (27 lines).
pub const MEDIUM: &str = include_str!("../benches/fixtures/medium.phx");

/// Mid-size fixture: structs with methods, closures, higher-order functions,
/// loops, mutable variables (~60 lines).
pub const MEDIUM_LARGE: &str = include_str!("../benches/fixtures/medium_large.phx");

/// Broad-coverage fixture: traits, generics, closures, lists, Option/Result,
/// string interpolation, loops, fizzbuzz (155 lines).
pub const LARGE: &str = include_str!("../benches/fixtures/large.phx");

/// Empty program — minimal valid fixture.
pub const EMPTY: &str = include_str!("../benches/fixtures/empty.phx");

/// Fixture that contains a deliberate parse error.
pub const PARSE_ERROR: &str = include_str!("../benches/fixtures/parse_error.phx");

/// Fixture that parses successfully but fails type checking.
pub const TYPE_ERROR: &str = include_str!("../benches/fixtures/type_error.phx");

// ---------------------------------------------------------------------------
// Shared pipeline helpers
// ---------------------------------------------------------------------------

/// Lex → parse → sema, panicking on errors.  Returns the checked program and
/// sema result so callers can feed them to an interpreter or IR lowering.
fn check_fixture(name: &str, source: &str) -> (phoenix_parser::Program, phoenix_sema::CheckResult) {
    let tokens = phoenix_lexer::tokenize(source, BENCH_SOURCE_ID);
    let (program, parse_diags) = phoenix_parser::parse(&tokens);
    assert!(
        parse_diags.is_empty(),
        "{name} has parse errors: {parse_diags:?}"
    );

    let check_result = phoenix_sema::check(&program);
    assert!(
        check_result.diagnostics.is_empty(),
        "{name} has sema errors: {:?}",
        check_result.diagnostics
    );

    (program, check_result)
}

/// Compile a fixture through lex → parse → sema → IR lowering, panicking on
/// any errors.  Returns the number of functions in the resulting IR module.
pub fn compile(name: &str, source: &str) -> usize {
    let (program, check_result) = check_fixture(name, source);
    let ir_module = phoenix_ir::lower(&program, &check_result);
    ir_module.functions.len()
}

/// Run a fixture through the full compilation pipeline and tree-walk
/// interpreter.  Returns the captured output lines.
pub fn run_tree_walk(name: &str, source: &str) -> Vec<String> {
    let (program, check_result) = check_fixture(name, source);
    phoenix_interp::run_and_capture(&program, check_result.lambda_captures)
        .unwrap_or_else(|e| panic!("{name} failed in tree-walk interpreter: {e:?}"))
}

/// Run a fixture through the full compilation pipeline and IR interpreter.
/// Returns the captured output lines.
pub fn run_ir(name: &str, source: &str) -> Vec<String> {
    let (program, check_result) = check_fixture(name, source);
    let ir_module = phoenix_ir::lower(&program, &check_result);
    phoenix_ir_interp::run_and_capture(&ir_module)
        .unwrap_or_else(|e| panic!("{name} failed in IR interpreter: {e:?}"))
}

/// Assert that a fixture source has parse errors (does not reach sema).
pub fn assert_parse_error(name: &str, source: &str) {
    let tokens = phoenix_lexer::tokenize(source, BENCH_SOURCE_ID);
    let (_program, parse_diags) = phoenix_parser::parse(&tokens);
    assert!(
        !parse_diags.is_empty(),
        "{name} was expected to have parse errors but parsed successfully"
    );
}

/// Assert that a fixture source parses successfully but fails type checking.
pub fn assert_type_error(name: &str, source: &str) {
    let tokens = phoenix_lexer::tokenize(source, BENCH_SOURCE_ID);
    let (program, parse_diags) = phoenix_parser::parse(&tokens);
    assert!(
        parse_diags.is_empty(),
        "{name} was expected to parse cleanly but had errors: {parse_diags:?}"
    );

    let check_result = phoenix_sema::check(&program);
    assert!(
        !check_result.diagnostics.is_empty(),
        "{name} was expected to have type errors but passed sema cleanly"
    );
}
