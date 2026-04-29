//! Shared test helpers for the `check_modules_*.rs` integration tests.
//!
//! Each `tests/check_modules_*.rs` file at this level is its own test
//! binary; this module is included via `mod common;` in each one so
//! the parsing/wrapping helpers are written once.

#![allow(dead_code)]

use std::path::PathBuf;

use phoenix_common::module_path::ModulePath;
use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_modules::ResolvedSourceModule;
use phoenix_parser::parser;

pub fn parse(source: &str, source_id: SourceId) -> phoenix_parser::ast::Program {
    let tokens = tokenize(source, source_id);
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    program
}

pub fn entry_only(source: &str) -> ResolvedSourceModule {
    ResolvedSourceModule {
        module_path: ModulePath::entry(),
        source_id: SourceId(0),
        program: parse(source, SourceId(0)),
        is_entry: true,
        file_path: PathBuf::from("<test>"),
    }
}

pub fn non_entry(name: &str, source: &str, source_id: SourceId) -> ResolvedSourceModule {
    ResolvedSourceModule {
        module_path: ModulePath(vec![name.to_string()]),
        source_id,
        program: parse(source, source_id),
        is_entry: false,
        file_path: PathBuf::from(format!("<test:{}>", name)),
    }
}
