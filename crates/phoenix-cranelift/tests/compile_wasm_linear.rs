//! Integration tests for the wasm32-linear backend.
//!
//! PR 2 scope: exercise the end-to-end pipeline from a Phoenix source
//! string through to a `.wasm` module. Verification has two tiers:
//!
//! 1. **Structural** (always-on): the emitted bytes pass
//!    `wasmparser::validate` and `wasmprinter` round-trips them to
//!    human-readable WAT. Any CI runner with the test dependencies
//!    installed exercises this tier.
//! 2. **Execution** (best-effort): if `wasmtime` is on `$PATH`, the
//!    test executes the module and asserts stdout against the AST
//!    interp's output (`phoenix run`). Otherwise it prints a visible
//!    skip warning. `PHOENIX_REQUIRE_WASMTIME=1` turns the skip into a
//!    hard failure so CI runners with wasmtime provisioned can't
//!    silently lose the check — same gating shape as the §2.3 valgrind
//!    gate (`PHOENIX_REQUIRE_VALGRIND`).
//!
//! Multi-fixture matrix coverage lands in PR 4
//! (`phoenix-driver/tests/three_backend_matrix.rs` adds a
//! `wasm32-linear` column). This file pins the hello-world and a
//! handful of corner-case fixtures that gate PR 2 — a regression
//! here means the WASM emitter is broken at the structural or
//! per-helper level, which the matrix couldn't isolate cleanly.

use std::process::{Command, Stdio};

use phoenix_common::span::SourceId;
use phoenix_cranelift::Target;
use phoenix_ir::module::IrModule;

/// Lower a Phoenix source string to an `IrModule`, panicking on any
/// front-end failure. Shared by the success-path `compile_to_wasm`
/// helper and the negative-path tests below, which want a valid IR
/// but expect the backend to refuse it.
fn lower_to_ir(source: &str) -> IrModule {
    let tokens = phoenix_lexer::tokenize(source, SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parser errors: {parse_errors:?}");
    let analysis = phoenix_sema::checker::check(&program);
    assert!(
        analysis.diagnostics.is_empty(),
        "sema errors: {:?}",
        analysis.diagnostics
    );
    let ir_module = phoenix_ir::lower(&program, &analysis.module);
    let verify_errors = phoenix_ir::verify::verify(&ir_module);
    assert!(
        verify_errors.is_empty(),
        "IR verification errors: {verify_errors:?}"
    );
    ir_module
}

/// Compile a Phoenix source string to a `wasm32-linear` `.wasm` byte
/// vector via the same pipeline `phoenix build --target wasm32-linear`
/// uses. Panics with a contextual message on any compile error so
/// failures point at the right backend stage.
fn compile_to_wasm(source: &str) -> Vec<u8> {
    let ir_module = lower_to_ir(source);
    phoenix_cranelift::compile(&ir_module, Target::Wasm32Linear)
        .unwrap_or_else(|e| panic!("wasm32-linear compile failed: {e}"))
}

/// Run the AST interpreter on `source` and return its stdout. Used
/// to build the expected-output baseline for the wasmtime comparison.
///
/// Empty-capture handling matters: a `main` with no `print` calls
/// produces an empty `captured` Vec, and the naive `join("\n") +
/// "\n"` would yield `"\n"` — a single newline that wasmtime's stdout
/// (genuinely empty) would never produce. Empty in, empty out keeps
/// the comparison honest for empty-output fixtures.
fn run_ast_interp(source: &str) -> String {
    let tokens = phoenix_lexer::tokenize(source, SourceId(0));
    let (program, _) = phoenix_parser::parser::parse(&tokens);
    let analysis = phoenix_sema::checker::check(&program);
    let captures = analysis.module.lambda_captures.clone();
    let captured = phoenix_interp::run_and_capture(&program, captures)
        .unwrap_or_else(|e| panic!("ast interp failed: {e:?}"));
    if captured.is_empty() {
        String::new()
    } else {
        captured.join("\n") + "\n"
    }
}

/// Treat an env var as set only when its value is literally `"1"`.
/// Avoids the footgun where `PHOENIX_REQUIRE_WASMTIME=0` (or `=""`)
/// would trip a `.is_ok()` check and quietly enter strict mode. Same
/// shape as the §2.3 valgrind gate's `env_flag_enabled`.
fn require_wasmtime() -> bool {
    std::env::var("PHOENIX_REQUIRE_WASMTIME").as_deref() == Ok("1")
}

/// Run `wasmtime <wasm_path>` and capture stdout. Returns `None`
/// (with a visible warning) when `wasmtime` is not on `$PATH`,
/// honoring the [`require_wasmtime`] env-var gate by panicking
/// instead. Mirrors the §2.3 valgrind helper's skip/fail shape.
fn run_with_wasmtime(wasm_path: &std::path::Path, label: &str) -> Option<String> {
    let spawn_result = Command::new("wasmtime")
        .arg(wasm_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let output = match spawn_result {
        Ok(o) => o,
        Err(_) => {
            eprintln!(
                "warning: skipping {label} — `wasmtime` not available on PATH \
                 (set PHOENIX_REQUIRE_WASMTIME=1 to fail instead; \
                 see docs/design-decisions.md §Phase 2.4 decision B)"
            );
            if require_wasmtime() {
                panic!("PHOENIX_REQUIRE_WASMTIME=1 set but `wasmtime` is not available on PATH");
            }
            return None;
        }
    };
    assert!(
        output.status.success(),
        "wasmtime exited non-zero for {label}: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Compile `source`, validate it structurally, then (if wasmtime is
/// available) execute it and assert stdout matches the AST interp's
/// output. Skips the execution tier on hosts without wasmtime unless
/// `PHOENIX_REQUIRE_WASMTIME=1` is set.
///
/// `tempfile::NamedTempFile` owns the on-disk `.wasm`, so an
/// assertion-failure unwind cleans the file up automatically rather
/// than leaving `/tmp` debris that a future test or developer would
/// have to track down.
fn assert_wasm_matches_interp(source: &str, label: &str) {
    let bytes = compile_to_wasm(source);
    wasmparser::validate(&bytes)
        .unwrap_or_else(|e| panic!("wasmparser rejected the module for {label}: {e}"));

    let mut file = tempfile::Builder::new()
        .prefix(&format!("phx-wasm-{label}-"))
        .suffix(".wasm")
        .tempfile()
        .expect("create temp .wasm");
    use std::io::Write;
    file.write_all(&bytes).expect("write temp .wasm");
    file.as_file().sync_all().expect("flush temp .wasm");

    let Some(actual) = run_with_wasmtime(file.path(), label) else {
        return; // skipped — warning already printed
    };

    let expected = run_ast_interp(source);
    assert_eq!(
        actual, expected,
        "wasmtime stdout disagrees with AST interp for {label}\n\
         expected: {expected:?}\nactual:   {actual:?}"
    );
}

/// Hello-world fixture, pulled directly from `tests/fixtures/hello.phx`
/// at compile time so the test stays locked to whatever the canonical
/// fixture says. The multi-fixture matrix in PR 4 is still the
/// natural home for path-walked coverage; this single `include_str!`
/// just pins the gate fixture for PR 2.
const HELLO_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/hello.phx"
));

#[test]
fn hello_world_emits_valid_wasm_module() {
    let bytes = compile_to_wasm(HELLO_SOURCE);

    // Tier 1: structural validation. Any failure here is an emitter
    // bug — fix the wasm-encoder usage, not the test.
    wasmparser::validate(&bytes).unwrap_or_else(|e| {
        panic!(
            "wasmparser rejected the emitted module: {e}\n\
             (the binary should be a valid WebAssembly module; check \
             section ordering, function-index space, and import vs \
             local function-index handoff in `src/wasm/mod.rs`)"
        )
    });

    // Sanity: bytes start with the WASM magic + MVP version. Catches
    // truncated / empty emission before deeper assertions.
    assert!(
        bytes.len() >= 8,
        "wasm output too small ({} bytes) to even contain the header",
        bytes.len()
    );
    assert_eq!(&bytes[..4], b"\0asm", "wrong wasm magic");

    // Disassemble to WAT to make later assertions human-readable —
    // also a regression check that wasmprinter accepts our output.
    let wat =
        wasmprinter::print_bytes(&bytes).unwrap_or_else(|e| panic!("wasmprinter failed: {e}"));

    // Spot-check the WAT for the structural commitments listed in
    // `src/wasm/mod.rs`'s top-level docs. Loose substring checks are
    // intentional — exact WAT formatting can shift with wasmprinter
    // versions, but these features have to be present for the module
    // to be functionally correct.
    let expectations: &[(&str, &str)] = &[
        (
            "(import \"wasi_snapshot_preview1\" \"fd_write\"",
            "PR 2 must import WASI fd_write",
        ),
        (
            "(import \"wasi_snapshot_preview1\" \"proc_exit\"",
            "PR 2 must import WASI proc_exit (now wired for fd_write errno)",
        ),
        (
            "(export \"_start\"",
            "WASI requires the `_start` export for runnable modules",
        ),
        (
            "(export \"memory\"",
            "memory must be exported so WASI hosts can read iovec staging area",
        ),
        ("(memory ", "module must declare a memory section"),
    ];
    for (needle, reason) in expectations {
        assert!(
            wat.contains(needle),
            "emitted WAT missing `{needle}` — {reason}\nfull WAT:\n{wat}"
        );
    }

    // hello.phx only `print(int)`s — no `print(bool)`, so the
    // `phx_print_bool` helper and its `"true\n"` / `"false\n"`
    // literals must be absent. A regression that always emits the
    // bool data segments (e.g. collapsing the `usage.print_bool`
    // guard in `module_builder::declare_runtime_helpers` or
    // `emit_runtime_bodies`) would silently bloat every module; this
    // assertion is the only PR-2-level guard for the
    // `HelperUsage::scan` omit-when-unused branch. (The bool-print
    // test below covers the present-when-needed branch.)
    assert!(
        !wat.contains("(data "),
        "hello.phx prints no booleans, so no data segments should \
         be emitted, but the WAT contains a `(data` section:\n{wat}"
    );
}

#[test]
fn hello_world_runs_under_wasmtime() {
    assert_wasm_matches_interp(HELLO_SOURCE, "hello_world");
}

// Negative-integer coverage of `phx_print_i64`'s `is_negative` branch
// is deferred to PR 3. Phoenix lowers `-123` to `INeg(ConstI64(123))`,
// and PR 2's translator-op surface is intentionally narrow enough to
// reject `INeg` — there's no source-level expression of a negative
// integer that doesn't route through arithmetic. Once PR 3 lifts
// `INeg`, add these tests here so the sign-store path and the
// `i64::MIN` two's-complement wrap documented in `runtime.rs` get
// end-to-end coverage:
//
//   - `negative_int_runs_under_wasmtime` — fixture `print(-123)`,
//     exercises the sign-write branch and the magnitude-negation
//     `0 - mag` step.
//   - `i64_min_runs_under_wasmtime` — fixture `print(-9223372036854775808)`,
//     exercises the `i64::MIN` two's-complement-wrap path where
//     `0 - mag` overflows back to `i64::MIN` and the `I64DivU` /
//     `I64RemU` loop reads the bit pattern as 2^63.
//
// Both targets are unreachable from a PR-2 source-level fixture; PR 3
// is the natural home because lifting `INeg` is on its critical path.

/// Zero is the do-while edge of the itoa loop: the magnitude is 0
/// on entry, but the loop body still has to emit a single `'0'`
/// digit. A `while` (rather than `loop`) shape would print an empty
/// string here — pin the correct output explicitly.
#[test]
fn zero_int_runs_under_wasmtime() {
    let src = "function main() {\n  let x: Int = 0\n  print(x)\n}\n";
    assert_wasm_matches_interp(src, "zero_int");
}

/// Empty `main` — no IR ops, no prints — should still produce a
/// well-formed module that wasmtime accepts and runs to a clean
/// exit. Exercises the all-helpers-disabled branch of
/// [`HelperUsage::scan`] end-to-end: `phx_print_*` declarations
/// are skipped, the data section is empty, and only `_start` +
/// `main` end up in the code section. A regression here would
/// indicate the runtime-helper plumbing is inadvertently required
/// even when no `print` call appears.
#[test]
fn empty_main_runs_under_wasmtime() {
    let src = "function main() {}\n";
    assert_wasm_matches_interp(src, "empty_main");
}

/// `print(bool)` exercises the `phx_print_bool` helper, the
/// associated `"true\n"` / `"false\n"` data segments, and the
/// `HelperUsage` pre-scan's bool branch (which gates whether those
/// segments are emitted at all). Both arms in one fixture keeps the
/// data-section layout under coverage.
#[test]
fn bool_print_runs_under_wasmtime() {
    let src = "function main() {\n  print(true)\n  print(false)\n}\n";
    assert_wasm_matches_interp(src, "bool_print");
}

/// Run the backend on `source` and return the error message. Panics
/// if compilation unexpectedly succeeds — the caller passes IR the
/// backend is expected to reject. Locks the error-message shape so
/// PR 3 lifting these restrictions has a visible diff point rather
/// than silently changing wording downstream tooling may grep.
fn expect_wasm_compile_error(source: &str) -> String {
    let ir_module = lower_to_ir(source);
    match phoenix_cranelift::compile(&ir_module, Target::Wasm32Linear) {
        Ok(_) => panic!(
            "expected wasm32-linear compile to fail for source:\n{source}\n\
             (PR 3 may have lifted the restriction — relocate this test \
             or replace the fixture)"
        ),
        Err(e) => e.to_string(),
    }
}

#[test]
fn rejects_module_without_main() {
    // PR 2 hard-requires a `main` function; the WASI `_start` shim
    // can't call anything else. Lock the error wording so PR 3+
    // multi-entry-point work surfaces here.
    let src = "function notmain() {\n  let x: Int = 1\n  print(x)\n}\n";
    let err = expect_wasm_compile_error(src);
    assert!(
        err.contains("no main function found"),
        "expected `no main function found`, got: {err}"
    );
}

#[test]
fn rejects_multi_block_control_flow() {
    // An `if` introduces a second basic block. PR 2's translator only
    // visits block 0; PR 3 lifts this.
    let src = "function main() {\n  if true {\n    let x: Int = 1\n    print(x)\n  }\n}\n";
    let err = expect_wasm_compile_error(src);
    assert!(
        err.contains("multi-block control flow"),
        "expected multi-block-control-flow error, got: {err}"
    );
}

#[test]
fn rejects_unsupported_ir_op() {
    // Arithmetic is not in PR 2's op surface. The error must point at
    // PR 3 so a future regression in the deferred-error wording is
    // visible.
    let src = "function main() {\n  let a: Int = 1\n  let b: Int = 2\n  let c: Int = a + b\n  print(c)\n}\n";
    let err = expect_wasm_compile_error(src);
    assert!(
        err.contains("not yet supported"),
        "expected `not yet supported` op error, got: {err}"
    );
    assert!(
        err.contains("Phase 2.4 PR 3"),
        "expected error to cite the PR 3 follow-up, got: {err}"
    );
}

#[test]
fn rejects_unrepresentable_param_type() {
    // A `String` parameter lowers to `IrType::StringRef`, which has
    // no single-slot WASM ValType — PR 2's translator rejects it at
    // entry-block parameter binding time. PR 3 lifts this once the
    // fat-pointer (ptr, len) representation lands. The fixture keeps
    // `main` minimal and never calls `greet`, so the only path that
    // can fire is the param-rejection one — a previous shape that
    // called `greet("hello")` would fail at `Op::ConstString` /
    // `Op::Call` before hitting the param check, masking regressions
    // in the path we actually want to lock down.
    let src = "function main() {}\n\
               function greet(msg: String) {\n  print(true)\n}\n";
    let err = expect_wasm_compile_error(src);
    assert!(
        err.contains("StringRef"),
        "expected `StringRef` in unsupported-param-type error, got: {err}"
    );
}

#[test]
fn rejects_main_with_params() {
    // WASI `_start` calls `main` with no arguments; a param-taking
    // `main` would produce an operand-stack mismatch at the `Call`
    // site that `wasmparser` would reject. The backend catches this
    // upfront so the diagnostic points at the source rather than at
    // wasmparser's bytes-level rejection.
    let src = "function main(x: Int) {\n  print(x)\n}\n";
    let err = expect_wasm_compile_error(src);
    assert!(
        err.contains("`main` must take no parameters"),
        "expected main-with-params diagnostic, got: {err}"
    );
}

#[test]
fn rejects_unrepresentable_return_type() {
    // `function greet() -> String { ... }` lowers to an IR function
    // whose `return_type` is `IrType::StringRef`, which has no single-
    // slot WASM `ValType` — `wasm_return_valtypes` rejects it during
    // signature construction in `declare_phoenix_functions`. Pinning
    // this branch separately from the param-side rejection in
    // `rejects_unrepresentable_param_type` covers both halves of
    // `wasm_valtype_for`'s value-representation gate; without this,
    // a regression on the return path would only surface once a
    // String-returning function was actually called from `main`.
    let src = "function main() {}\n\
               function greet() -> String {\n  return \"hi\"\n}\n";
    let err = expect_wasm_compile_error(src);
    assert!(
        err.contains("StringRef"),
        "expected `StringRef` in unsupported-return-type error, got: {err}"
    );
}

#[test]
fn rejects_main_with_return_value() {
    // Same shape as `rejects_main_with_params`, but for the
    // return-type side of the `main`/`_start` contract.
    let src = "function main() -> Int {\n  return 0\n}\n";
    let err = expect_wasm_compile_error(src);
    assert!(
        err.contains("`main` must return void"),
        "expected main-with-return diagnostic, got: {err}"
    );
}
