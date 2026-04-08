//! The Phoenix CLI driver.
//!
//! This binary wires together the lexer, parser, semantic checker, and
//! tree-walk interpreter to provide a unified command-line interface for
//! working with Phoenix source files.
//!
//! # Subcommands
//!
//! | Command | Description |
//! |---------|-------------|
//! | `lex`   | Tokenize a source file and print the token stream |
//! | `parse` | Parse a source file and dump the AST as JSON |
//! | `check` | Type-check a source file and report errors |
//! | `run`   | Execute a Phoenix program via the tree-walk interpreter |

use clap::{Parser, Subcommand};
use std::fs;
use std::process;

use phoenix_common::source::SourceMap;
use phoenix_common::span::SourceId;
use phoenix_interp::interpreter;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

#[derive(Parser)]
#[command(name = "phoenix")]
#[command(about = "The Phoenix programming language")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Tokenize a source file and print the token stream
    Lex {
        /// Path to the source file
        file: String,
    },
    /// Parse a source file and print the AST as JSON
    Parse {
        /// Path to the source file
        file: String,
    },
    /// Type-check a source file
    Check {
        /// Path to the source file
        file: String,
    },
    /// Run a Phoenix program
    Run {
        /// Path to the source file
        file: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Lex { file } => cmd_lex(&file),
        Commands::Parse { file } => cmd_parse(&file),
        Commands::Check { file } => cmd_check(&file),
        Commands::Run { file } => cmd_run(&file),
    }
}

/// Reads a source file from disk and registers it in a fresh [`SourceMap`].
///
/// Returns the source map, the file's [`SourceId`], and the raw contents.
/// Exits the process with an error message if the file cannot be read.
fn read_source(path: &str) -> (SourceMap, SourceId, String) {
    let contents = fs::read_to_string(path).unwrap_or_else(|err| {
        eprintln!("error: could not read file '{}': {}", path, err);
        process::exit(1);
    });
    let mut source_map = SourceMap::new();
    let source_id = source_map.add(path, &contents);
    (source_map, source_id, contents)
}

/// Prints a slice of diagnostics to stderr with `file:line:col` prefixes.
fn report_diagnostics(
    diagnostics: &[phoenix_common::diagnostics::Diagnostic],
    source_map: &SourceMap,
    source_id: SourceId,
) {
    for diag in diagnostics {
        let loc = source_map.line_col(source_id, diag.span.start);
        eprintln!(
            "{}:{}:{}: {}",
            source_map.name(source_id),
            loc.line,
            loc.col,
            diag,
        );
    }
}

fn cmd_lex(path: &str) {
    let (_source_map, source_id, contents) = read_source(path);
    let tokens = tokenize(&contents, source_id);

    for token in &tokens {
        println!(
            "{:?}\t{:?}\t[{}..{}]",
            token.kind, token.text, token.span.start, token.span.end
        );
    }
}

fn cmd_parse(path: &str) {
    let (source_map, source_id, contents) = read_source(path);
    let tokens = tokenize(&contents, source_id);
    let (program, diagnostics) = parser::parse(&tokens);

    if !diagnostics.is_empty() {
        report_diagnostics(&diagnostics, &source_map, source_id);
        process::exit(1);
    }

    let json = serde_json::to_string_pretty(&program).unwrap_or_else(|err| {
        eprintln!("error: failed to serialize AST: {}", err);
        process::exit(1);
    });
    println!("{}", json);
}

/// Parses and type-checks a source file, exiting on errors.
/// Returns the program AST and the semantic check result.
fn parse_and_check(path: &str) -> (phoenix_parser::ast::Program, phoenix_sema::CheckResult) {
    let (source_map, source_id, contents) = read_source(path);
    let tokens = tokenize(&contents, source_id);
    let (program, parse_errors) = parser::parse(&tokens);

    if !parse_errors.is_empty() {
        report_diagnostics(&parse_errors, &source_map, source_id);
        process::exit(1);
    }

    let result = checker::check(&program);
    if !result.diagnostics.is_empty() {
        report_diagnostics(&result.diagnostics, &source_map, source_id);
        process::exit(1);
    }

    (program, result)
}

fn cmd_check(path: &str) {
    parse_and_check(path);
    println!("No errors found.");
}

fn cmd_run(path: &str) {
    let (program, check_result) = parse_and_check(path);
    if let Err(err) = interpreter::run(&program, check_result.lambda_captures) {
        eprintln!("runtime error: {}", err);
        process::exit(1);
    }
}
