//! Shared helpers for the `phoenix-codegen` integration test suites
//! (`compiles_and_lints.rs` and `roundtrip.rs`).
//!
//! Both suites generate target code from a schema, then shell out to that
//! target's toolchain. The toolchain-gating logic (skip locally / hard-fail
//! under `PHOENIX_GEN_E2E=1`), the subprocess runner, and the schema → AST +
//! analysis pipeline are identical between them and live here so the two stay in
//! lock-step rather than drifting as copy-pasted twins.
//!
//! Each integration test binary compiles this module independently via
//! `mod common;`, so a helper a given binary doesn't use shows up as dead code
//! there — hence the crate-level `allow`.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::Program;
use phoenix_parser::parser;
use phoenix_sema::Analysis;
use phoenix_sema::checker;

// ── Toolchain gating ────────────────────────────────────────────────────────

/// Returns true if `name` is an invokable program on `PATH`.
pub fn tool_available(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether missing toolchains should hard-fail (CI) rather than skip (local).
pub fn e2e_required() -> bool {
    std::env::var("PHOENIX_GEN_E2E").as_deref() == Ok("1")
}

/// Skips or fails depending on `PHOENIX_GEN_E2E`. Returns true if the caller
/// should bail out (tools missing, skip allowed).
pub fn gate(missing: &[&str]) -> bool {
    if missing.is_empty() {
        return false;
    }
    let msg = format!(
        "required toolchain not found on PATH: {}",
        missing.join(", ")
    );
    if e2e_required() {
        panic!("PHOENIX_GEN_E2E=1 but {msg}");
    }
    eprintln!("SKIP (set PHOENIX_GEN_E2E=1 to enforce): {msg}");
    true
}

/// Of `needed`, returns those not found on `PATH` (the input to [`gate`]).
pub fn missing_tools<'a>(needed: &[&'a str]) -> Vec<&'a str> {
    needed
        .iter()
        .copied()
        .filter(|t| !tool_available(t))
        .collect()
}

/// Runs a command in `dir`, returning (success, combined stdout+stderr).
pub fn run(dir: &Path, program: &str, args: &[&str]) -> (bool, String) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{program}`: {e}"));
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), combined)
}

// ── Schema pipeline ──────────────────────────────────────────────────────────

/// Tokenizes, parses, and type-checks `schema`, asserting there are no parse or
/// check errors, and returns the AST + analysis ready to hand to a `generate_*`
/// entry point.
pub fn parse_and_check(schema: &str) -> (Program, Analysis) {
    let tokens = tokenize(schema, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    (program, result)
}
