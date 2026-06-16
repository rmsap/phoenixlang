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

use std::path::{Path, PathBuf};
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

/// Whether a Go module version is present in the local module cache
/// (`$GOMODCACHE/<module@version>`). Gates the chi tests: the `go-chi` scaffold
/// pins chi in `go.mod`/`go.sum` but does NOT vendor it, so `go build`/`go test`
/// resolve chi from the cache (offline) or the proxy (network). Without a cached
/// copy and without `PHOENIX_GEN_E2E` (which permits the network), the chi checks
/// skip rather than fail trying to download in a sandboxed/offline run.
pub fn go_module_cached(module_at_version: &str) -> bool {
    let Ok(output) = Command::new("go").args(["env", "GOMODCACHE"]).output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let cache = String::from_utf8_lossy(&output.stdout);
    let cache = cache.trim();
    !cache.is_empty() && Path::new(cache).join(module_at_version).is_dir()
}

/// The committed `go-chi` scaffold directory (`tests/scaffold/go-chi/`), whose
/// `go.mod`/`go.sum` are the single pinned source of truth for the chi version
/// both Go suites resolve against. Shared so the compile-lint and round-trip
/// suites can't construct divergent paths.
pub fn chi_scaffold_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scaffold")
        .join("go-chi")
}

/// The chi `require` directive (`"module version"`, e.g.
/// `"github.com/go-chi/chi/v5 v5.3.0"`) read from the scaffold's `go.mod`. Both
/// Go suites derive the pinned chi version from here rather than hardcoding a
/// copy, so a `go get …@<version>` bump in the scaffold can't drift from the
/// tests' cache gate. Handles both the single-line (`require mod ver`) and block
/// (`require (\n\tmod ver\n)`) forms and drops any trailing `// indirect`
/// marker. Panics if the scaffold `go.mod` has no chi `require` line (a
/// structural breakage worth failing loudly).
pub fn chi_require_from_scaffold(scaffold: &Path) -> String {
    let go_mod = scaffold.join("go.mod");
    let contents = std::fs::read_to_string(&go_mod)
        .unwrap_or_else(|e| panic!("read {}: {e}", go_mod.display()));
    contents
        .lines()
        .find_map(|line| {
            // A single-line `require mod ver` and an in-block `mod ver` line both
            // reduce to a `mod ver …` token stream once an optional `require `
            // prefix is stripped; `split_whitespace` then drops any `// indirect`.
            let line = line.trim();
            let line = line.strip_prefix("require ").map(str::trim).unwrap_or(line);
            let mut tokens = line.split_whitespace();
            let module = tokens.next()?;
            let version = tokens.next()?;
            module
                .starts_with("github.com/go-chi/chi/")
                .then(|| format!("{module} {version}"))
        })
        .unwrap_or_else(|| panic!("no chi `require` line in {}", go_mod.display()))
}

/// The chi module in `module@version` form (e.g.
/// `"github.com/go-chi/chi/v5@v5.3.0"`) for the module-cache gate, derived from
/// the scaffold's `go.mod` via [`chi_require_from_scaffold`].
pub fn chi_module_at_version(scaffold: &Path) -> String {
    chi_require_from_scaffold(scaffold).replacen(' ', "@", 1)
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
