//! Shared test helpers for Phoenix integration tests.
//!
//! Provides convenience functions that feed Phoenix source code through
//! the full pipeline (lex -> parse -> check -> interpret) and verify results.

#![allow(dead_code)]

use phoenix_common::span::SourceId;
use phoenix_interp::interpreter;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// Run source through the full pipeline. Panics on parse or type errors.
pub fn run(source: &str) {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "type errors: {:?}",
        result.diagnostics
    );
    interpreter::run(&program, result.lambda_captures).expect("runtime error");
}

/// Run source through the full pipeline and capture `print()` output.
/// Returns the captured lines.
pub fn run_capturing(source: &str) -> Vec<String> {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "type errors: {:?}",
        result.diagnostics
    );
    interpreter::run_and_capture(&program, result.lambda_captures).expect("runtime error")
}

/// Run source and assert that `print()` output matches expected lines exactly.
pub fn run_expect(source: &str, expected: &[&str]) {
    let output = run_capturing(source);
    assert_eq!(
        output, expected,
        "output mismatch\n  got:      {:?}\n  expected: {:?}",
        output, expected
    );
}

/// Run source and expect a type error containing the given substring.
pub fn expect_type_error(source: &str, expected: &str) {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = checker::check(&program);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|e| e.message.contains(expected)),
        "expected type error containing '{}', got: {:?}",
        expected,
        result.diagnostics
    );
}

/// Run source through lexing and parsing and expect a parse error containing the given substring.
pub fn expect_parse_error(source: &str, expected: &str) {
    let tokens = tokenize(source, SourceId(0));
    let (_program, parse_errors) = parser::parse(&tokens);
    assert!(
        parse_errors.iter().any(|e| e.message.contains(expected)),
        "expected parse error containing '{}', got: {:?}",
        expected,
        parse_errors
    );
}

/// Run source through the full pipeline and expect a runtime error containing the given substring.
pub fn expect_runtime_error(source: &str, expected: &str) {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let check_result = checker::check(&program);
    assert!(
        check_result.diagnostics.is_empty(),
        "type errors: {:?}",
        check_result.diagnostics
    );
    let result = interpreter::run(&program, check_result.lambda_captures);
    assert!(
        result.is_err(),
        "expected runtime error containing '{}'",
        expected
    );
    assert!(
        result.unwrap_err().to_string().contains(expected),
        "expected runtime error containing '{}'",
        expected,
    );
}
