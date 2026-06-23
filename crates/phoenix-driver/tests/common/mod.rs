//! Shared test helpers for Phoenix integration tests.
//!
//! Provides convenience functions that feed Phoenix source code through
//! the full pipeline (lex -> parse -> check -> interpret) and verify results.

#![allow(dead_code)]

pub mod compiled_fixtures;
pub mod matrix_harness;
#[cfg(target_os = "linux")]
pub mod rlimit;

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
    interpreter::run(&program, result.module.lambda_captures).expect("runtime error");
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
    interpreter::run_and_capture(&program, result.module.lambda_captures).expect("runtime error")
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
    let result = interpreter::run(&program, check_result.module.lambda_captures);
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

// ---------------------------------------------------------------------------
// Runtime-artifact skip gates.
//
// Shared by every CLI test whose `build` tier links a native binary or emits a
// wasm module — both need a prebuilt runtime artifact the bare `cargo test`
// invocation doesn't produce. Soft-skip when it's missing (with a one-line
// warning) so `cargo test` is green out of the box, and turn the skip into a
// hard failure under `PHOENIX_REQUIRE_RUNTIME_*=1` so provisioned CI can't
// silently stop exercising the path. Kept here so `build_cli.rs`,
// `extern_js_cli.rs`, and any future native/wasm CLI test share one copy.
// ---------------------------------------------------------------------------

/// `$VAR` is set to exactly `1`. The opt-in shape shared by every
/// `PHOENIX_REQUIRE_*` gate in the repo.
pub fn require(var: &str) -> bool {
    std::env::var(var).as_deref() == Ok("1")
}

/// Whether the `phoenix` binary under test will find the runtime static
/// library. Probes `$PHOENIX_RUNTIME_LIB` (which the binary honors) and
/// otherwise the binary's *own* exe-relative search — NOT this test
/// binary's. The test binary lives in `target/<profile>/deps/`, so a
/// `find_runtime_lib()` here would see a `deps/libphoenix_runtime.a` that
/// the spawned binary (in `target/<profile>/`) never searches — making the
/// skip decision disagree with the build the test then runs.
pub fn runtime_lib_visible_to_phoenix() -> bool {
    if std::env::var_os("PHOENIX_RUNTIME_LIB").is_some() {
        return true;
    }
    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_phoenix"));
    phoenix_cranelift::find_runtime_lib_near(&bin).is_some()
}

/// Soft-skip the native build tier when `libphoenix_runtime.a` isn't
/// on any search path: the CLI link step would hard-error, but linking
/// is profile-independent and is already gated in the debug `check` job
/// (which builds the lib and sets `PHOENIX_REQUIRE_RUNTIME_LIB=1`) and
/// proven end-to-end by release.yml's install smoke test. Here the
/// `release-test` / release `test` jobs run `cargo test --release`
/// without building the lib, so without this gate they fail spuriously.
/// `PHOENIX_REQUIRE_RUNTIME_LIB=1` turns the skip into a hard failure —
/// same shape as `link.rs`'s in-crate `precheck` gate. Returns `true`
/// when the caller should early-return (skip).
#[must_use]
pub fn skip_if_no_runtime_lib(label: &str) -> bool {
    if runtime_lib_visible_to_phoenix() {
        return false;
    }
    assert!(
        !require("PHOENIX_REQUIRE_RUNTIME_LIB"),
        "PHOENIX_REQUIRE_RUNTIME_LIB=1 set but libphoenix_runtime.a is not on any \
         search path — run `cargo build -p phoenix-runtime` or set $PHOENIX_RUNTIME_LIB"
    );
    eprintln!(
        "warning: skipping {label} — libphoenix_runtime.a not built \
         (set PHOENIX_REQUIRE_RUNTIME_LIB=1 to fail instead; \
         `cargo build -p phoenix-runtime` to fix)"
    );
    true
}

/// Wasm-tier counterpart of [`skip_if_no_runtime_lib`], gated by
/// `PHOENIX_REQUIRE_RUNTIME_WASM=1`. Mirrors the skip plumbing in
/// `phoenix-cranelift/tests/compile_wasm_linear.rs`. Returns `true`
/// when the caller should early-return (skip).
#[must_use]
pub fn skip_if_no_runtime_wasm(label: &str) -> bool {
    if phoenix_cranelift::runtime_wasm_available() {
        return false;
    }
    assert!(
        !require("PHOENIX_REQUIRE_RUNTIME_WASM"),
        "PHOENIX_REQUIRE_RUNTIME_WASM=1 set but phoenix_runtime.wasm is not on any \
         search path — run `cargo build -p phoenix-runtime --target wasm32-wasip1 --release` first"
    );
    eprintln!(
        "warning: skipping {label} — phoenix_runtime.wasm not built \
         (set PHOENIX_REQUIRE_RUNTIME_WASM=1 to fail instead; \
         `cargo build -p phoenix-runtime --target wasm32-wasip1 --release` to fix)"
    );
    true
}

/// Whether a `node` interpreter is on `PATH` (probed via `node --version`).
/// The `extern js` glue tiers run the generated `.js` under Node, so they gate
/// on this.
pub fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Soft-skip a Node-dependent tier when `node` isn't on `PATH`, gated by
/// `PHOENIX_REQUIRE_NODE=1` (the same `== "1"` opt-in shape as the runtime
/// gates above — not a bare "is set" check). Returns `true` when the caller
/// should early-return (skip).
#[must_use]
pub fn skip_if_no_node(label: &str) -> bool {
    if node_available() {
        return false;
    }
    assert!(
        !require("PHOENIX_REQUIRE_NODE"),
        "PHOENIX_REQUIRE_NODE=1 set but `node` is not on PATH"
    );
    eprintln!(
        "warning: skipping {label} — `node` not on PATH \
         (set PHOENIX_REQUIRE_NODE=1 to fail instead)"
    );
    true
}

/// Whether the `node` on `PATH` confirms support for the WasmGC proposal
/// (struct/array types) that the `wasm32-gc` glue tier instantiates. Probes by
/// asking Node to `WebAssembly.validate` a minimal module whose type section
/// declares a GC `struct`: a GC-capable engine exits `0`, an engine without GC
/// support exits `1` (`validate` returns `false`, it does *not* throw). WasmGC is
/// enabled by default in Node ≈22+; an older Node validates the bytes as `false`.
///
/// The probe checks only a GC struct, not `externref` directly — but `externref`
/// (reference-types) long predates the GC proposal, so any engine that validates
/// a GC struct necessarily has the `externref` the glue also uses. The struct is
/// the strictly-newer feature, hence the sufficient gate.
///
/// Returns `true` only on a clean exit `0`. Any other outcome — a definitive
/// exit `1`, *or* a probe that couldn't run cleanly (a node crash, a signal) —
/// returns `false`: this is a soft capability gate, so "not confirmed" is treated
/// the same as "unsupported". Callers that need the gc engine set
/// `PHOENIX_REQUIRE_WASM_GC=1` (see [`skip_if_no_wasm_gc`]). Distinguishing "no
/// node" from "node without GC" is the caller's job: this is only reached after
/// [`skip_if_no_node`].
pub fn node_supports_wasm_gc() -> bool {
    // Memoized: this is called once per gc test (via `skip_if_no_wasm_gc`), so the
    // `node -e` subprocess runs only on the first probe and every later call reads
    // the cached verdict.
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        // 00 61 73 6d  wasm magic | 01 00 00 00  version 1
        // 01 03        type section (id 1), 3-byte payload
        // 01 5f 00     one type: struct (0x5f = 95) with zero fields
        let probe = "process.exit(WebAssembly.validate(\
                     new Uint8Array([0,97,115,109,1,0,0,0,1,3,1,95,0])) ? 0 : 1);";
        // Only a clean exit `0` confirms support. `status.success()` is already
        // exactly "exited 0", so an exit `1` (no GC) and a non-exit failure (crash,
        // signal) both fall through to `false` — the messages below are worded to
        // not over-claim *which* of those it was.
        std::process::Command::new("node")
            .arg("-e")
            .arg(probe)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Soft-skip a wasm32-gc glue tier when the `node` on `PATH` can't run WasmGC,
/// gated by `PHOENIX_REQUIRE_WASM_GC=1`. Call *after* [`skip_if_no_node`]: a gc
/// module fails to even compile on a pre-GC engine, so without this probe those
/// tiers would hard-fail rather than soft-skip the way every other capability
/// gate does. Returns `true` when the caller should early-return (skip).
#[must_use]
pub fn skip_if_no_wasm_gc(label: &str) -> bool {
    if node_supports_wasm_gc() {
        return false;
    }
    assert!(
        !require("PHOENIX_REQUIRE_WASM_GC"),
        "PHOENIX_REQUIRE_WASM_GC=1 set but the `node` on PATH did not pass the WasmGC \
         probe — it lacks the GC proposal, or the probe failed to run \
         (needs Node ≈22+, where the GC proposal is on by default)"
    );
    eprintln!(
        "warning: skipping {label} — the `node` on PATH did not pass the WasmGC probe \
         (lacks the GC proposal, or the probe failed to run; set \
         PHOENIX_REQUIRE_WASM_GC=1 to fail instead; needs Node ≈22+)"
    );
    true
}
