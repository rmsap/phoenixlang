//! Shared test helpers for IR interpreter roundtrip tests.

#![allow(dead_code)]

use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// Run source through the AST interpreter and capture print() output.
pub fn ast_run(source: &str) -> Vec<String> {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "type errors: {:?}",
        result.diagnostics
    );
    phoenix_interp::run_and_capture(&program, result.lambda_captures).expect("AST runtime error")
}

/// Run source through the IR interpreter and capture print() output.
pub fn ir_run(source: &str) -> Vec<String> {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "type errors: {:?}",
        result.diagnostics
    );
    let module = phoenix_ir::lower(&program, &result);
    let errors = phoenix_ir::verify::verify(&module);
    assert!(errors.is_empty(), "IR verification errors: {:?}", errors);
    phoenix_ir_interp::run_and_capture(&module).expect("IR runtime error")
}

/// Assert that both interpreters produce the same output.
pub fn roundtrip(source: &str) {
    let ast_out = ast_run(source);
    let ir_out = ir_run(source);
    assert_eq!(
        ast_out, ir_out,
        "output mismatch\n  AST: {:?}\n  IR:  {:?}",
        ast_out, ir_out
    );
}

/// Compile source and run only the IR interpreter, returning the result.
pub fn ir_run_result(
    source: &str,
) -> std::result::Result<Vec<String>, phoenix_ir_interp::error::IrRuntimeError> {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "type errors: {:?}",
        result.diagnostics
    );
    let module = phoenix_ir::lower(&program, &result);
    let errors = phoenix_ir::verify::verify(&module);
    assert!(errors.is_empty(), "IR verification errors: {:?}", errors);
    phoenix_ir_interp::run_and_capture(&module)
}
