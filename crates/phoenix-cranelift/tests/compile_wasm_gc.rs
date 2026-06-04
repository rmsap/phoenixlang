//! wasm32-gc backend integration tests (Phase 2.4 PR 5).
//!
//! These tests exercise `phoenix-cranelift`'s `Target::Wasm32Gc`
//! pipeline end-to-end. The structural tier always runs (it just
//! asks `wasmparser` whether the module parses with the GC proposal
//! enabled); the execution tier runs whenever `wasmtime` is on
//! `$PATH`, invoking it with `-W gc=y` to enable the GC proposal.
//!
//! `PHOENIX_REQUIRE_WASMTIME=1` turns the soft-skip on missing
//! wasmtime into a hard failure — same gating shape as the
//! wasm32-linear integration tests in [`compile_wasm_linear.rs`].

use std::process::{Command, Stdio};

use phoenix_common::SourceId;
use phoenix_cranelift::{Target, compile};
use phoenix_ir::instruction::{FuncId, Op};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;

/// Lower a Phoenix source string through lexer → parser → sema → IR
/// to produce an `IrModule` for the codegen pipeline to operate on.
/// Same recipe the wasm32-linear tests use; if any front-end stage
/// rejects the program, this panics with the diagnostics.
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

/// `PHOENIX_REQUIRE_WASMTIME=1` turns the soft skip on missing
/// wasmtime into a hard failure. Same shape as the wasm32-linear
/// integration tests.
fn require_wasmtime() -> bool {
    std::env::var("PHOENIX_REQUIRE_WASMTIME").as_deref() == Ok("1")
}

/// Compile `source` through `Target::Wasm32Gc` and return the
/// resulting WASM bytes. Panics with the codegen diagnostic on
/// compile failure.
fn compile_to_wasm_gc(source: &str) -> Vec<u8> {
    let ir_module = lower_to_ir(source);
    compile(&ir_module, Target::Wasm32Gc)
        .unwrap_or_else(|e| panic!("wasm32-gc compile failed: {e}"))
}

/// Validate `bytes` with `wasmparser`, GC proposal enabled. Panics
/// with the parse diagnostic on rejection. Always runs (no wasmtime
/// dependency), so it guards module structure even when the execution
/// tier is skipped.
///
/// Note on what this currently proves: slices 1–2 emit **no** WASM-GC
/// types or instructions — they are structurally plain linear modules.
/// Enabling `WasmFeatures::GC` here validates against a superset, so
/// today this asserts only "is a structurally valid module," not "uses
/// the GC proposal correctly." The GC-specific coverage arrives with
/// the struct slice (decision J slice 3), which is the first to declare
/// `struct.new` types; the feature flag is enabled now so that fixture
/// needs no test-harness change when it lands.
fn validate_gc_module(bytes: &[u8], label: &str) {
    let mut features = wasmparser::WasmFeatures::default();
    features.insert(wasmparser::WasmFeatures::GC);
    let mut validator = wasmparser::Validator::new_with_features(features);
    validator
        .validate_all(bytes)
        .unwrap_or_else(|e| panic!("wasmparser rejected wasm32-gc {label}: {e}"));
}

/// Compile `source`, structurally validate it, and — when wasmtime is
/// available — assert its stdout equals `expected`. Shared by the
/// `print(Int)` digit-conversion cases.
fn assert_prints(source: &str, label: &str, expected: &[u8]) {
    let bytes = compile_to_wasm_gc(source);
    assert_wasm_prints(&bytes, label, expected);
}

/// Structurally validate `bytes` and — when wasmtime is available —
/// assert its stdout equals `expected`. Shared by the source-driven
/// [`assert_prints`] and the hand-built-IR negative-path test, which has
/// no Phoenix source to lower (see [`print_int_module`]).
fn assert_wasm_prints(bytes: &[u8], label: &str, expected: &[u8]) {
    validate_gc_module(bytes, label);
    if let Some(stdout) = run_under_wasmtime_gc(bytes, label) {
        assert_eq!(
            stdout,
            expected,
            "{label} stdout mismatch:\n  got: {:?}\n  want: {:?}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(expected),
        );
    }
}

/// Run `wasmtime -W gc=y <wasm_path>` and return its stdout. Returns
/// `None` with a stderr warning when `wasmtime` isn't on `$PATH`;
/// panics if `PHOENIX_REQUIRE_WASMTIME=1`.
fn run_under_wasmtime_gc(bytes: &[u8], label: &str) -> Option<Vec<u8>> {
    // Quick liveness probe so the helpful error fires here rather
    // than as a confusing "could not spawn" further down.
    if Command::new("wasmtime").arg("--version").output().is_err() {
        if require_wasmtime() {
            panic!("PHOENIX_REQUIRE_WASMTIME=1 set but `wasmtime` is not on PATH");
        }
        eprintln!("warning: skipping wasmtime execution for {label} — `wasmtime` not on PATH");
        return None;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{label}.wasm"));
    std::fs::write(&path, bytes).expect("write wasm");
    let out = Command::new("wasmtime")
        .args(["-W", "gc=y"])
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("invoke wasmtime");
    assert!(
        out.status.success(),
        "wasmtime exited non-zero for {label}: status={:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    Some(out.stdout)
}

/// `hello.phx` — PR 5 slice 1's gate. The fixture binds an `Int`
/// literal to an immutable local and prints it. Exercises the full
/// minimal pipeline:
///
/// - `Op::ConstI64` lowering to `i64.const + local.set`. The `let x`
///   here is immutable, so it binds the `ConstI64` SSA value directly
///   — no `Op::Alloca` / `Store` / `Load` is emitted (that trio is the
///   *mutable* `let mut` shape, covered by
///   [`mutable_let_runs_under_wasmtime_gc`]).
/// - `Op::BuiltinCall("print", Int)` routed to the synthesized
///   `phx_print_i64` helper.
/// - `_start` → user `main` plumbing and the WASI `fd_write` import.
///   (No `proc_exit` import yet — `_start` returns normally; panic
///   routing through `proc_exit` lands in a later slice.)
/// - The codegen-synthesized `phx_print_i64` digit-conversion path:
///   write `\n`, then digits backward, then assemble the iovec entry
///   and call `fd_write`.
///
/// Expected stdout: `42\n`.
#[test]
fn hello_runs_under_wasmtime_gc() {
    // Structural validation (inside `assert_prints`) always runs. The
    // GC proposal is enabled so any GC-typed declarations parse
    // correctly (today's MVP doesn't declare any, but the validator is
    // forward-compatible).
    assert_prints(
        "function main() {\n  let x: Int = 42\n  print(x)\n}\n",
        "hello_wasm_gc",
        b"42\n",
    );
}

/// `print(0)` exercises the `phx_print_i64` zero branch (`n == 0 →
/// '0'`), which the multi-digit `hello` case never reaches.
#[test]
fn print_zero_runs_under_wasmtime_gc() {
    assert_prints(
        "function main() {\n  let x: Int = 0\n  print(x)\n}\n",
        "print_zero_wasm_gc",
        b"0\n",
    );
}

/// A long multi-digit value exercises many iterations of the
/// digit-conversion loop and the `i64`→ASCII remainder math, well past
/// `hello`'s two digits.
#[test]
fn print_large_runs_under_wasmtime_gc() {
    assert_prints(
        "function main() {\n  let x: Int = 1234567890\n  print(x)\n}\n",
        "print_large_wasm_gc",
        b"1234567890\n",
    );
}

/// A `main` that never calls `print` exercises the no-print compile
/// path, which is *structurally distinct* from every other success case:
/// `module_calls_print` returns `false`, so `declare_imports` and
/// `declare_print_helper` are both skipped. That leaves `import_func_count`
/// at `0`, which shifts every Phoenix function's WASM index down by one
/// (no `fd_write` import sits at index 0) and changes the `_start → main`
/// call target. All the other passing tests print, so they only ever
/// cover the *with*-import index arithmetic; this case locks the
/// no-import branch so a regression in `add_local_function` /
/// `declare_start` can't slip through. Empty stdout: the body is a bare
/// `Return(None)`.
#[test]
fn empty_main_with_no_print_compiles_and_runs() {
    assert_prints("function main() {\n}\n", "empty_main_wasm_gc", b"");
}

/// A *mutable* `let mut` is the only shape that emits the `Op::Alloca` /
/// `Op::Store` / `Op::Load` trio — an immutable `let` (as in `hello.phx`)
/// binds its initializer's SSA value directly and never reaches it. This
/// fixture allocates a mutable `Int` slot, stores an initial value,
/// reassigns it (a second `Op::Store`), then reads it back (`Op::Load`)
/// to print — so it drives every arm of that trio, which `hello.phx`
/// leaves unexercised. The reassignment to `42` (distinct from the
/// initial `1`) proves the store-then-load round-trips the new value
/// rather than the initializer.
///
/// Expected stdout: `42\n`.
#[test]
fn mutable_let_runs_under_wasmtime_gc() {
    assert_prints(
        "function main() {\n  let mut x: Int = 1\n  x = 42\n  print(x)\n}\n",
        "mutable_let_wasm_gc",
        b"42\n",
    );
}

/// `print(Bool)` is accepted by sema (the `print` built-in is
/// type-unconstrained) but slice 1 only synthesizes `phx_print_i64`.
/// The backend must reject a non-`Int` argument with a clear diagnostic
/// rather than emit a `Call` against a mismatched signature — which
/// would produce a structurally invalid module. Locks in the
/// `translate_print` type guard.
#[test]
fn print_bool_is_rejected_until_a_later_slice() {
    let ir_module = lower_to_ir("function main() {\n  print(true)\n}\n");
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("print(Bool) should not compile under wasm32-gc slice 1");
    let msg = err.to_string();
    assert!(
        msg.contains("print") && msg.contains("Int"),
        "expected a print/Int diagnostic, got: {msg}"
    );
}

/// Control flow (here, an `if`) lowers `main` into more than one IR
/// block. Slice 1 is single-block only, so `translate_function` must
/// reject it with a clear "multi-block lands in a later slice"
/// diagnostic rather than silently dropping the extra blocks. Locks in
/// the slice-1/slice-2 boundary so the guard isn't quietly removed when
/// control flow lands.
#[test]
fn multi_block_function_is_rejected_until_a_later_slice() {
    let ir_module = lower_to_ir("function main() {\n  if true {\n    print(1)\n  }\n}\n");
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("multi-block `main` should not compile under wasm32-gc slice 1");
    let msg = err.to_string();
    assert!(
        msg.contains("multi-block"),
        "expected a multi-block diagnostic, got: {msg}"
    );
}

/// A program with no `main` reaches the backend: sema does not *require*
/// an entry point (it only constrains where `main` may be declared), so
/// the wasm32-gc module builder is the layer that must reject it. Locks in
/// the `declare_phoenix_functions` "no `main`" guard, which is reachable
/// from ordinary user source.
#[test]
fn missing_main_is_rejected() {
    let ir_module = lower_to_ir("function helper() {\n}\n");
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("a program with no `main` should not compile under wasm32-gc");
    let msg = err.to_string();
    assert!(
        msg.contains("main"),
        "expected a missing-`main` diagnostic, got: {msg}"
    );
}

/// `main` with a non-`Void` return type passes sema but cannot be wired to
/// the synthesized `_start` (typed `[] -> []`, which discards no value).
/// The backend must reject it before emitting a structurally invalid
/// `_start` that leaves an operand on the stack. Locks in the
/// `declare_phoenix_functions` return-type guard — also reachable from
/// ordinary user source.
#[test]
fn main_returning_non_void_is_rejected() {
    let ir_module = lower_to_ir("function main() -> Int {\n  return 0\n}\n");
    let err = compile(&ir_module, Target::Wasm32Gc)
        .expect_err("`main` returning non-Void should not compile under wasm32-gc");
    let msg = err.to_string();
    assert!(
        msg.contains("main") && msg.contains("Void"),
        "expected a `main`/`Void` diagnostic, got: {msg}"
    );
}

/// Hand-build the IR for `function main() { print(n) }` with a literal
/// `Op::ConstI64(n)`, bypassing the front end.
///
/// Why hand-build: a source-level negative integer lowers to `Op::INeg`
/// of a positive `ConstI64`, and `INeg` (like all arithmetic) is outside
/// slice 1's op surface — so the only way to route a negative `Int` into
/// `phx_print_i64` today is to mint the `ConstI64(n)` directly. An
/// immutable `let` binds its initializer's SSA value directly (no
/// `Alloca`/`Store`/`Load`), so this two-instruction body is exactly what
/// `let x: Int = n; print(x)` would lower to if `n` could be negative.
fn print_int_module(n: i64) -> IrModule {
    let mut func = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = func.create_block();
    let value = func.emit_value(entry, Op::ConstI64(n), IrType::I64, None);
    func.emit(
        entry,
        Op::BuiltinCall("print".to_string(), vec![value]),
        IrType::Void,
        None,
    );
    func.set_terminator(entry, Terminator::Return(None));

    let mut module = IrModule::new();
    module.push_concrete(func);
    module
}

/// The negative / `is_neg` branch of `phx_print_i64`: write the digits
/// backward, then prepend `'-'`. Unreachable from slice-1 *source* (a
/// source `-7` needs `Op::INeg`, deferred to slice 2), so the body is
/// built straight from IR via [`print_int_module`]. Covers a single-digit
/// negative (the minimal sign path) and a long negative (sign path plus
/// many digit-loop iterations) so the hand-written sign emission doesn't
/// ship unexercised until arithmetic lands.
#[test]
fn print_negative_runs_under_wasmtime_gc() {
    for (n, want) in [
        (-7_i64, &b"-7\n"[..]),
        (-1234567890_i64, &b"-1234567890\n"[..]),
    ] {
        let module = print_int_module(n);
        let bytes = compile(&module, Target::Wasm32Gc)
            .unwrap_or_else(|e| panic!("wasm32-gc compile of print({n}) failed: {e}"));
        assert_wasm_prints(&bytes, &format!("print_negative_{n}_wasm_gc"), want);
    }
}
