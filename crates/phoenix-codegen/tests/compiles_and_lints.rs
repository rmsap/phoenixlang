//! End-to-end "the generated code actually compiles and lints" harness.
//!
//! Unlike the snapshot/string tests, this runs the real toolchain for each
//! target against generated output:
//!   * Go:      `go build ./...`, `gofmt -l` (must be empty), `golangci-lint run`
//!
//! Toolchain gating: if a required tool is missing from `PATH`, each target test
//! SKIPS with a printed message — UNLESS `PHOENIX_GEN_E2E=1` is set, in which
//! case a missing toolchain is a hard failure so CI cannot silently skip.
//!
//! Adding TypeScript/Python/OpenAPI later: generate the target's files, then call
//! `run_target_checks` with the appropriate scaffold + build/lint commands. The
//! Go target below is the reference implementation; TS/Python are left as TODO
//! stubs because their toolchains are not installed in this environment.

use std::path::Path;
use std::process::Command;

use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// The representative schema exercised end-to-end. Mirrors
/// `tests/fixtures/gen_api.phx` (structs with constraints, an enum, optional
/// query params, omit/pick/partial bodies, error mappings, void responses).
const SCHEMA: &str = include_str!("../../../tests/fixtures/gen_api.phx");

// ── Toolchain gating ────────────────────────────────────────────────────

/// Returns true if `name` is an invokable program on `PATH`.
fn tool_available(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether missing toolchains should hard-fail (CI) rather than skip (local).
fn e2e_required() -> bool {
    std::env::var("PHOENIX_GEN_E2E").as_deref() == Ok("1")
}

/// Skips or fails depending on `PHOENIX_GEN_E2E`. Returns true if the caller
/// should bail out (tools missing, skip allowed).
fn gate(missing: &[&str]) -> bool {
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

// ── Pipeline ────────────────────────────────────────────────────────────

fn generate_go_files() -> phoenix_codegen::GoFiles {
    let tokens = tokenize(SCHEMA, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    phoenix_codegen::generate_go(&program, &result)
}

/// Runs a command in `dir`, returning (success, combined stdout+stderr).
fn run(dir: &Path, program: &str, args: &[&str]) -> (bool, String) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{program}`: {e}"));
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), combined)
}

// ── Go target ───────────────────────────────────────────────────────────

#[test]
fn go_output_compiles_and_lints() {
    let needed = ["go", "gofmt"];
    let missing: Vec<&str> = needed
        .iter()
        .copied()
        .filter(|t| !tool_available(t))
        .collect();
    if gate(&missing) {
        return;
    }

    let files = generate_go_files();

    // Scaffold: <tmp>/go.mod + <tmp>/api/*.go (package is `api`).
    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    let api_dir = root.join("api");
    std::fs::create_dir(&api_dir).expect("create api dir");
    std::fs::write(root.join("go.mod"), "module gencheck\n\ngo 1.23\n").expect("write go.mod");
    std::fs::write(api_dir.join("types.go"), &files.types).expect("write types.go");
    std::fs::write(api_dir.join("client.go"), &files.client).expect("write client.go");
    std::fs::write(api_dir.join("handlers.go"), &files.handlers).expect("write handlers.go");
    std::fs::write(api_dir.join("server.go"), &files.server).expect("write server.go");

    // 1. `go build ./...` must succeed.
    let (built, build_out) = run(root, "go", &["build", "./..."]);
    assert!(built, "go build failed:\n{build_out}");

    // 2. `gofmt -l` over the generated files must report NOTHING.
    let go_files = [
        api_dir.join("types.go"),
        api_dir.join("client.go"),
        api_dir.join("handlers.go"),
        api_dir.join("server.go"),
    ];
    let mut gofmt_args = vec!["-l".to_string()];
    gofmt_args.extend(go_files.iter().map(|p| p.to_string_lossy().into_owned()));
    let gofmt_arg_refs: Vec<&str> = gofmt_args.iter().map(String::as_str).collect();
    let (_, gofmt_out) = run(root, "gofmt", &gofmt_arg_refs);
    assert!(
        gofmt_out.trim().is_empty(),
        "gofmt -l reported files needing formatting:\n{gofmt_out}"
    );

    // 3. `golangci-lint run ./...` must exit 0 (skip only this step if absent).
    if tool_available("golangci-lint") {
        let (linted, lint_out) = run(root, "golangci-lint", &["run", "./..."]);
        assert!(linted, "golangci-lint failed:\n{lint_out}");
    } else if e2e_required() {
        panic!("PHOENIX_GEN_E2E=1 but golangci-lint not found on PATH");
    } else {
        eprintln!("SKIP golangci-lint step (not installed)");
    }
}

// ── OpenAPI target ───────────────────────────────────────────────────────

/// The redocly config used to lint generated specs. It extends `recommended`
/// but disables rules that conflict with Phoenix Gen's documented design (auth
/// deferred, no license, optional 4xx). See the file for per-rule rationale.
const REDOCLY_CONFIG: &str = include_str!("scaffold/openapi/redocly.yaml");

fn generate_openapi_spec() -> String {
    let tokens = tokenize(SCHEMA, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "check errors: {:?}",
        result.diagnostics
    );
    phoenix_codegen::generate_openapi(&program, &result)
}

#[test]
fn openapi_output_lints() {
    // `npx` fetches `@redocly/cli` on first use; gate on `npx` being present.
    let missing: Vec<&str> = ["npx"]
        .iter()
        .copied()
        .filter(|t| !tool_available(t))
        .collect();
    if gate(&missing) {
        return;
    }

    let spec = generate_openapi_spec();

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path();
    std::fs::write(root.join("openapi.json"), &spec).expect("write openapi.json");
    // redocly auto-discovers `redocly.yaml` in the working directory.
    std::fs::write(root.join("redocly.yaml"), REDOCLY_CONFIG).expect("write redocly.yaml");

    let (linted, lint_out) = run(
        root,
        "npx",
        &["--yes", "@redocly/cli", "lint", "openapi.json"],
    );
    assert!(linted, "redocly lint failed:\n{lint_out}");
}

// ── Future targets (stubs) ──────────────────────────────────────────────
//
// TODO(typescript): generate via `generate_typescript`, scaffold a package.json +
//   tsconfig, then run `tsc --noEmit`, `eslint`, `prettier --check`. Toolchain
//   (tsc/eslint/prettier) is not installed in this environment.
//
// TODO(python): generate via `generate_python`, scaffold, then run `mypy`,
//   `ruff check`, `black --check`. Toolchain not installed here.
