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
use std::sync::Once;

use phoenix_common::span::SourceId;
use phoenix_cranelift::{CompileErrorKind, Target};
use phoenix_ir::module::IrModule;

/// Print the "phoenix_runtime.wasm not built" warning at most once
/// per test process. A test binary with N skipped fixtures otherwise
/// emits N copies of the same instructions to stderr, which buries
/// any genuinely useful test failures.
static MISSING_RUNTIME_WARNING: Once = Once::new();

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

/// Compile `source` via the wasm32-linear backend and pass the bytes
/// to `body`. When the runtime artifact is missing, prints a one-time
/// warning and skips `body` entirely (the calling `#[test]` returns
/// success without running the body) — same skip-or-fail-loud shape
/// as the native runtime-lib gate (`PHOENIX_REQUIRE_RUNTIME_LIB`),
/// here gated by `PHOENIX_REQUIRE_RUNTIME_WASM=1`. Branching on
/// [`CompileErrorKind::RuntimeWasmNotFound`] (rather than substring-
/// matching the diagnostic) keeps the skip gate stable across copy-
/// edits to the error message text. Any other compile error panics
/// with a contextual message so failures point at the right backend
/// stage.
///
/// Single chokepoint for the compile-and-skip pattern: every
/// integration test routes through this helper so the skip plumbing
/// doesn't drift across call sites.
fn compile_or_skip(source: &str, label: &str, body: impl FnOnce(&[u8])) {
    let ir_module = lower_to_ir(source);
    let bytes = match phoenix_cranelift::compile(&ir_module, Target::Wasm32Linear) {
        Ok(bytes) => bytes,
        Err(e) if e.kind == CompileErrorKind::RuntimeWasmNotFound => {
            if std::env::var("PHOENIX_REQUIRE_RUNTIME_WASM").as_deref() == Ok("1") {
                panic!(
                    "PHOENIX_REQUIRE_RUNTIME_WASM=1 set but phoenix_runtime.wasm \
                     is not on any search path — run `cargo build -p phoenix-runtime \
                     --target wasm32-wasip1 --release` first"
                );
            }
            // Per-test trace, plus a one-shot full instructions line so
            // a 10-test skip storm doesn't bury the rest of stderr.
            eprintln!("warning: skipping {label} — phoenix_runtime.wasm not built");
            MISSING_RUNTIME_WARNING.call_once(|| {
                eprintln!(
                    "         (set PHOENIX_REQUIRE_RUNTIME_WASM=1 to fail instead; \
                     `cargo build -p phoenix-runtime --target wasm32-wasip1 --release` \
                     to fix)"
                );
            });
            return;
        }
        Err(e) => panic!("wasm32-linear compile failed: {e}"),
    };
    body(&bytes);
}

/// Spill `bytes` into a self-cleaning temp `.wasm` file and hand its
/// path to `body`. Centralizing this keeps every "compile, write,
/// invoke wasmtime" test path from re-deriving the same scrap of
/// I/O plumbing — and ensures every site uses a self-deleting
/// `NamedTempFile` rather than leaving `/tmp` debris.
fn with_temp_wasm(label: &str, bytes: &[u8], body: impl FnOnce(&std::path::Path)) {
    use std::io::Write;
    let mut file = tempfile::Builder::new()
        .prefix(&format!("phx-wasm-{label}-"))
        .suffix(".wasm")
        .tempfile()
        .expect("create temp .wasm");
    file.write_all(bytes).expect("write temp .wasm");
    file.as_file().sync_all().expect("flush temp .wasm");
    body(file.path());
}

/// Truncate a WAT dump for use inside assertion messages. Keeping
/// every "first 4 KiB of WAT" sentinel uniform across call sites so a
/// future bump only needs to touch one constant.
fn wat_excerpt(wat: &str) -> &str {
    &wat[..wat.len().min(4096)]
}

/// Collect the function-index targets of every `Call` operator inside
/// the body of the function exported under `export_name`, in the
/// order they appear. Returns an `Err` with a specific message if
/// the export, its function index, or its body can't be located, or
/// if any wasmparser step fails — so a regression in the test helper
/// surfaces as "couldn't find the export" rather than collapsing into
/// a count mismatch the caller has to diagnose. Used to verify
/// structural commitments of `_start` (call count *and* uniqueness of
/// targets, so a regression that doubles a call shows up even when
/// the resulting count still falls in the expected band) independently
/// of the runtime's `name` custom section (which release builds strip).
fn call_targets_in_export(bytes: &[u8], export_name: &str) -> Result<Vec<u32>, String> {
    use wasmparser::{Operator, Parser, Payload};

    let mut target_func_idx: Option<u32> = None;
    let mut import_func_count: u32 = 0;
    let mut bodies: Vec<wasmparser::FunctionBody<'_>> = Vec::new();

    let parse_payload = |label: &str, e: wasmparser::BinaryReaderError| -> String {
        format!("wasmparser failed while reading {label} for `{export_name}`: {e}")
    };

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| parse_payload("payload header", e))?;
        match payload {
            Payload::ImportSection(rdr) => {
                for group in rdr {
                    let group = group.map_err(|e| parse_payload("import section", e))?;
                    match group {
                        wasmparser::Imports::Single(_, imp) => {
                            if matches!(imp.ty, wasmparser::TypeRef::Func(_)) {
                                import_func_count += 1;
                            }
                        }
                        wasmparser::Imports::Compact1 { items, .. } => {
                            for item in items {
                                let item = item.map_err(|e| parse_payload("import items", e))?;
                                if matches!(item.ty, wasmparser::TypeRef::Func(_)) {
                                    import_func_count += 1;
                                }
                            }
                        }
                        wasmparser::Imports::Compact2 { ty, names, .. } => {
                            if matches!(ty, wasmparser::TypeRef::Func(_)) {
                                for name in names {
                                    name.map_err(|e| parse_payload("import names", e))?;
                                    import_func_count += 1;
                                }
                            }
                        }
                    }
                }
            }
            Payload::ExportSection(rdr) => {
                for export in rdr {
                    let export = export.map_err(|e| parse_payload("export section", e))?;
                    if export.name == export_name
                        && matches!(export.kind, wasmparser::ExternalKind::Func)
                    {
                        target_func_idx = Some(export.index);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => bodies.push(body),
            _ => {}
        }
    }

    let target = target_func_idx
        .ok_or_else(|| format!("module has no function export named `{export_name}`"))?;
    if target < import_func_count {
        return Err(format!(
            "export `{export_name}` resolves to imported function {target} \
             (no local body exists; this helper only counts locally-defined Call ops)"
        ));
    }
    let local_idx = (target - import_func_count) as usize;
    let body = bodies.get(local_idx).ok_or_else(|| {
        format!(
            "export `{export_name}` points at local function {local_idx} but the \
             code section has only {} bodies",
            bodies.len()
        )
    })?;

    let mut reader = body
        .get_operators_reader()
        .map_err(|e| parse_payload("function-body operators", e))?;
    let mut targets = Vec::new();
    while !reader.eof() {
        if let Operator::Call { function_index } = reader
            .read()
            .map_err(|e| parse_payload("function-body operator", e))?
        {
            targets.push(function_index);
        }
    }
    Ok(targets)
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
    compile_or_skip(source, label, |bytes| {
        wasmparser::validate(bytes)
            .unwrap_or_else(|e| panic!("wasmparser rejected the module for {label}: {e}"));

        with_temp_wasm(label, bytes, |path| {
            let Some(actual) = run_with_wasmtime(path, label) else {
                return; // skipped — warning already printed
            };
            let expected = run_ast_interp(source);
            assert_eq!(
                actual, expected,
                "wasmtime stdout disagrees with AST interp for {label}\n\
                 expected: {expected:?}\nactual:   {actual:?}"
            );
        });
    });
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
    compile_or_skip(
        HELLO_SOURCE,
        "hello_world_emits_valid_wasm_module",
        |bytes| {
            // Tier 1: structural validation. Any failure here is an emitter
            // bug — fix the wasm-encoder usage, not the test.
            wasmparser::validate(bytes).unwrap_or_else(|e| {
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
            let wat = wasmprinter::print_bytes(bytes)
                .unwrap_or_else(|e| panic!("wasmprinter failed: {e}"));
            hello_world_check_body(bytes, &wat);
        },
    );
}

/// Body of `hello_world_emits_valid_wasm_module`, factored out so the
/// closure passed to `compile_or_skip` stays focused on flow control.
fn hello_world_check_body(bytes: &[u8], wat: &str) {
    // Spot-check the WAT for PR 3a's structural commitments. The
    // merged module contains both our user-side scaffolding (the
    // `_start` entry, the exported memory) and the merged
    // `phoenix-runtime` (WASI imports it needs, every `phx_*`
    // function). Loose substring checks are intentional — exact WAT
    // formatting can shift with wasmprinter versions — but these
    // features have to be present for the module to be functionally
    // correct.
    let expectations: &[(&str, &str)] = &[
        (
            "(import \"wasi_snapshot_preview1\" \"fd_write\"",
            "merged runtime must import WASI fd_write (stdio path)",
        ),
        (
            "(import \"wasi_snapshot_preview1\" \"proc_exit\"",
            "merged runtime must import WASI proc_exit (panic exit path)",
        ),
        (
            "(export \"_start\"",
            "WASI requires the `_start` export for runnable modules",
        ),
        (
            "(export \"memory\"",
            "memory must be exported so WASI hosts can read it",
        ),
        ("(memory ", "module must declare a memory section"),
    ];
    for (needle, reason) in expectations {
        assert!(
            wat.contains(needle),
            "emitted WAT missing `{needle}` — {reason}\n\
             (first 4 KiB of WAT shown for diagnostic;\n\
             search the full WAT to confirm the section's absence)\n\
             {}",
            wat_excerpt(wat),
        );
    }

    // Positive merge assertion. The WASI-import / `_start` / memory
    // expectations above don't actually prove the merge ran — a
    // regression where `merge_runtime` early-returned but the user-side
    // scaffolding still declared the WASI imports would pass them all.
    // PR 3a user code emits zero data segments and zero globals (hello.phx
    // has no string constants); the runtime contributes both. Asserting
    // their presence is a strict superset of "merge ran" without depending
    // on the runtime's `name` custom section (which `--release` may strip).
    assert!(
        wat.contains("(data "),
        "emitted WAT contains no `(data ` section — the runtime \
         embed-and-merge step did not contribute any data segments, \
         which means `merge_runtime` was skipped or `merge_data` \
         silently dropped its input. PR 3a user code emits no data \
         segments, so this section can only come from the runtime.\n\
         (first 4 KiB of WAT shown for diagnostic:)\n{}",
        wat_excerpt(wat),
    );
    assert!(
        wat.contains("(global "),
        "emitted WAT contains no `(global ` section — the merged \
         runtime must contribute at least `__stack_pointer`. \
         A missing global section means `merge_runtime` skipped the \
         global section or `merge_global` is broken.\n\
         (first 4 KiB of WAT shown for diagnostic:)\n{}",
        wat_excerpt(wat),
    );

    // GC lifecycle assertion. `_start` must call `phx_gc_enable`
    // before `main` and `phx_gc_shutdown` after — a regression in
    // `emit_start_body` that drops either call would otherwise pass
    // every assertion above (the symbols still merge, the module
    // still validates) and only manifest as a runtime leak or
    // uninitialized-allocator trap. The release runtime strips the
    // `name` section so we can't reach this through WAT substring
    // matching; instead, walk the binary directly: locate `_start`
    // by export and decode its body's `Call` targets.
    //
    // We check two structural commitments separately:
    //
    // 1. Call count is 3 or 4. 3 = no static ctors (common
    //    wasm32-wasip1 cdylib shape: `phx_gc_enable` / `phx_main` /
    //    `phx_gc_shutdown`); 4 = runtime exports `_initialize` or
    //    `__wasm_call_ctors`, in which case `emit_start_body` calls
    //    that first. The merged module doesn't re-export `_initialize`,
    //    so we accept either count rather than trying to re-derive.
    //
    // 2. *All call targets are distinct.* This catches regressions
    //    where `emit_start_body` accidentally doubles a call
    //    (e.g. emitting `phx_gc_enable` twice) — that would still
    //    land a count in {3,4} but is a real bug. Every site in
    //    `emit_start_body` calls a different runtime function, so
    //    distinct targets is a tight invariant.
    let call_targets = call_targets_in_export(bytes, "_start").unwrap_or_else(|e| {
        panic!("collecting calls in `_start` failed: {e}");
    });
    let call_count = call_targets.len();
    assert!(
        call_count == 3 || call_count == 4,
        "the `_start` function must contain 3 `Call` instructions \
         (phx_gc_enable / phx_main / phx_gc_shutdown) or 4 \
         (with `_initialize` / `__wasm_call_ctors` first); got {call_count} \
         (targets={call_targets:?}). \
         A different count means `emit_start_body` was changed \
         (or the GC wiring was dropped) without updating this \
         assertion. See `module_builder::emit_start_body`."
    );
    let mut sorted = call_targets.clone();
    sorted.sort_unstable();
    let len_before = sorted.len();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        len_before,
        "the `_start` function has duplicate `Call` targets ({call_targets:?}); \
         every step in `emit_start_body` calls a distinct runtime function, \
         so a duplicate means a call site was inadvertently emitted twice. \
         See `module_builder::emit_start_body`."
    );

    // Behavioral check folded in: hello.phx is
    // `function main() { let x: Int = 42; print(x) }`, so the WASI
    // stdout must contain "42". Pinning the exact substring guards
    // against the case where the AST interp and the WASM backend
    // agree on a *wrong* output (e.g. both print "0" because some
    // shared lowering bug zeroes the literal — `assert_wasm_matches_interp`
    // wouldn't catch that). Runs as the wasmtime tier of this test
    // rather than a separate fixture so we don't re-compile hello.phx
    // twice.
    with_temp_wasm("hello-42", bytes, |path| {
        if let Some(stdout) = run_with_wasmtime(path, "hello_world_emits_valid_wasm_module") {
            assert!(
                stdout.contains("42"),
                "wasmtime stdout does not contain `42` for hello.phx \
                 (expected `print(42)` to route through merged `phx_print_i64`); \
                 stdout was: {stdout:?}"
            );
        }
        // (None = wasmtime not available; the skip path is already
        // logged by `run_with_wasmtime`.)
    });
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
/// exit. With PR 3a the `_start` body still runs `phx_gc_enable` →
/// user `main` → `phx_gc_shutdown` regardless of whether `main`
/// has any prints, so this fixture pins the "no-op main" path
/// through the GC lifecycle.
#[test]
fn empty_main_runs_under_wasmtime() {
    let src = "function main() {}\n";
    assert_wasm_matches_interp(src, "empty_main");
}

/// `print(bool)` routes through the merged runtime's `phx_print_bool`
/// (and its `"true\n"` / `"false\n"` static strings, which live in
/// the runtime's data segments). Both arms in one fixture keeps both
/// boolean values covered.
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
