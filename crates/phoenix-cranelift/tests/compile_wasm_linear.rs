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
use phoenix_ir::instruction::{FuncId, Op};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;

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
    compile_ir_or_skip(&ir_module, label, body);
}

/// Same skip-or-compile contract as [`compile_or_skip`] but takes a
/// pre-built [`IrModule`] rather than lowering from source. Used by
/// tests that hand-construct IR shapes the source front-end can't
/// produce today (e.g. the parallel-copy back-edge fixture, which
/// requires a `Jump` passing block params in shuffled order — Phoenix's
/// `let mut` / `while` lowers via `Op::Alloca` and never emits this
/// shape from source).
fn compile_ir_or_skip(ir_module: &IrModule, label: &str, body: impl FnOnce(&[u8])) {
    let bytes = match phoenix_cranelift::compile(ir_module, Target::Wasm32Linear) {
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

/// Count active data segments in `bytes`. Used by structural
/// assertions on string-literal fixtures: each `Op::ConstString`
/// reservation emits one active data segment in addition to the
/// runtime's own segments, so the per-fixture data-segment count is
/// "runtime baseline + N literals". A regression that silently
/// dropped a reservation would show up as a count miss before the
/// wasmtime tier even ran.
fn active_data_segment_count(bytes: &[u8]) -> Result<usize, String> {
    use wasmparser::{DataKind, Parser, Payload};
    let mut active = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| format!("parsing payload header: {e}"))?;
        if let Payload::DataSection(rdr) = payload {
            for data in rdr {
                let data = data.map_err(|e| format!("parsing data segment: {e}"))?;
                if matches!(data.kind, DataKind::Active { .. }) {
                    active += 1;
                }
            }
        }
    }
    Ok(active)
}

/// Count `(GlobalGet, GlobalSet)` operator pairs targeting
/// `__stack_pointer` across *every* function body in `bytes`.
/// Module-wide variant of [`sp_global_access_count_in_export`]; the
/// fizzbuzz fixture's *sret* sequence lives in the user `fizzbuzz`
/// function rather than `_start`, so a per-export check would target
/// the wrong body. Resolves `__stack_pointer` from the one-entry
/// `name` custom section the merger always emits in the output, so
/// this works regardless of whether the runtime build itself
/// retained names.
fn sp_global_access_count_in_module(bytes: &[u8]) -> Result<(usize, usize), String> {
    use wasmparser::{Operator, Parser, Payload};
    let mut sp_global_idx: Option<u32> = None;
    let mut bodies: Vec<wasmparser::FunctionBody<'_>> = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| format!("parsing payload header: {e}"))?;
        match payload {
            Payload::CustomSection(reader) if reader.name() == "name" => {
                if let wasmparser::KnownCustom::Name(name_reader) = reader.as_known() {
                    for subsection in name_reader {
                        let subsection = subsection.map_err(|e| format!("name subsection: {e}"))?;
                        if let wasmparser::Name::Global(map) = subsection {
                            for entry in map {
                                let entry = entry.map_err(|e| format!("name entry: {e}"))?;
                                if entry.name == "__stack_pointer" {
                                    sp_global_idx = Some(entry.index);
                                }
                            }
                        }
                    }
                }
            }
            Payload::CodeSectionEntry(body) => bodies.push(body),
            _ => {}
        }
    }
    let sp_idx = sp_global_idx
        .ok_or_else(|| "no `__stack_pointer` entry in the name section".to_string())?;
    let mut gets = 0;
    let mut sets = 0;
    for body in bodies {
        let mut reader = body
            .get_operators_reader()
            .map_err(|e| format!("function-body operators: {e}"))?;
        while !reader.eof() {
            match reader
                .read()
                .map_err(|e| format!("function-body operator: {e}"))?
            {
                Operator::GlobalGet { global_index } if global_index == sp_idx => gets += 1,
                Operator::GlobalSet { global_index } if global_index == sp_idx => sets += 1,
                _ => {}
            }
        }
    }
    Ok((gets, sets))
}

/// Count *occurrences* of the post-call `(I32Load@0, I32Load@4)` sret
/// shape across every body, not just bodies-that-contain-the-pattern.
/// Used to anchor multi-concat fixtures (e.g. `chained_string_concat`)
/// where one body contains two distinct sret call sites; the
/// body-count helper saturates at 1 per body and would miss the
/// composition. State machine: `Idle → AfterCall → SawLoad0 → +1 →
/// Idle`. A new `Call` while waiting for the loads restarts the
/// window — abandoning the previous Call's pending pair.
fn module_sret_call_site_occurrence_count(bytes: &[u8]) -> Result<usize, String> {
    use wasmparser::{Operator, Parser, Payload};
    let mut bodies: Vec<wasmparser::FunctionBody<'_>> = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| format!("parsing payload header: {e}"))?;
        if let Payload::CodeSectionEntry(body) = payload {
            bodies.push(body);
        }
    }
    enum State {
        Idle,
        AfterCall,
        SawLoad0,
    }
    let mut count = 0;
    for body in bodies {
        let mut reader = body
            .get_operators_reader()
            .map_err(|e| format!("function-body operators: {e}"))?;
        let mut state = State::Idle;
        while !reader.eof() {
            let op = reader
                .read()
                .map_err(|e| format!("function-body operator: {e}"))?;
            match (&state, &op) {
                (_, Operator::Call { .. }) => state = State::AfterCall,
                (State::AfterCall, Operator::I32Load { memarg }) if memarg.offset == 0 => {
                    state = State::SawLoad0;
                }
                (State::SawLoad0, Operator::I32Load { memarg }) if memarg.offset == 4 => {
                    count += 1;
                    state = State::Idle;
                }
                _ => {}
            }
        }
    }
    Ok(count)
}

/// Count function bodies that contain a *post-call* pair of
/// `i32.load offset=0` and `i32.load offset=4` — the literal shape
/// the *sret* sequence in `translate_to_string_builtin` emits to
/// extract `PhxFatPtr.ptr` and `PhxFatPtr.len` from the caller-
/// allocated result area. A body is counted iff it contains, in
/// order: a `Call`, then (within the same body) at least one
/// `I32Load { offset: 0 }` and at least one `I32Load { offset: 4 }`.
///
/// Returning a count rather than a bool lets the fizzbuzz assertion
/// anchor against the runtime-only baseline (from `hello.phx`): a
/// runtime body might happen to emit the same operator pattern, so
/// "fizzbuzz has strictly more such bodies than hello does" is
/// stronger than "fizzbuzz has at least one." A regression that
/// drops the user-emitted loads still trips even if the runtime's
/// own bodies match the pattern.
///
/// This is a tripwire for the sret loads' offset choice: a regression
/// to e.g. `offset: 8` (reading garbage past the fat pointer) wouldn't
/// fail wasm validation and might pass an end-to-end fixture if the
/// garbage happened to look like a valid `len`. Asserting the exact
/// offset shape catches that class of regression before wasmtime even
/// runs.
///
/// The loose "anywhere after a Call in the same body" framing — rather
/// than demanding the loads be *immediately* after — keeps the check
/// stable against future codegen changes that interleave other ops
/// (e.g. a shadow-stack-rooting push between the call and the loads).
fn module_sret_load_shape_body_count(bytes: &[u8]) -> Result<usize, String> {
    use wasmparser::{Operator, Parser, Payload};
    let mut bodies: Vec<wasmparser::FunctionBody<'_>> = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| format!("parsing payload header: {e}"))?;
        if let Payload::CodeSectionEntry(body) = payload {
            bodies.push(body);
        }
    }
    let mut count = 0;
    for body in bodies {
        let mut reader = body
            .get_operators_reader()
            .map_err(|e| format!("function-body operators: {e}"))?;
        let mut seen_call = false;
        let mut load_at_0 = false;
        let mut load_at_4 = false;
        while !reader.eof() {
            let op = reader
                .read()
                .map_err(|e| format!("function-body operator: {e}"))?;
            match op {
                // Direct `Call` only — the user-emitted sret sequence
                // uses `Operator::Call`. Counting `CallIndirect` would
                // inflate the runtime baseline (the runtime's own
                // bodies have plenty of CallIndirect / load patterns),
                // weakening the `count > base_count` assertion in
                // fizzbuzz without catching any additional regression.
                Operator::Call { .. } => seen_call = true,
                Operator::I32Load { memarg } if seen_call => {
                    if memarg.offset == 0 {
                        load_at_0 = true;
                    } else if memarg.offset == 4 {
                        load_at_4 = true;
                    }
                }
                _ => {}
            }
        }
        if load_at_0 && load_at_4 {
            count += 1;
        }
    }
    Ok(count)
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

/// `defer_basic.phx` fixture — PR 3c's gate for the string-literal +
/// `print(String)` surface. Defers are pre-linearized at IR lowering
/// time (they become sequential `Op::BuiltinCall("print", ConstString)`
/// instructions in the entry block), so this fixture exercises
/// decision H end-to-end without needing PR 3c's `defer` exit-path
/// machinery.
const DEFER_BASIC_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/defer_basic.phx"
));

/// `fizzbuzz.phx` — PR 3c's gate for the *sret* call sequence. The
/// `toString(Int)` call goes through `phx_i64_to_str`, which returns a
/// `PhxFatPtr` struct via the wasm32-wasip1 C ABI's implicit struct-
/// return pointer — codegen reserves stack space via
/// `__stack_pointer`, passes the pointer, then loads the fat-pointer
/// fields back out. Pinning this fixture pins the SP-manipulation
/// dance; a regression that misaligns the stack or fails to restore
/// the SP shows up as wrong output (or a wasmtime trap) on this
/// fixture before manifesting in less obvious ways elsewhere.
const FIZZBUZZ_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/fizzbuzz.phx"
));

/// Count the `Op::ConstString` occurrences in `source`'s lowered IR.
/// This is what the structural data-segment assertions actually need
/// to predict — each `Op::ConstString` reservation emits one active
/// data segment, regardless of whether the source-level form was a
/// `"..."` literal, a multi-line string, or (in the future) a
/// constant-folded expression. Deriving from the IR rather than
/// scanning the source for `"` characters means comments, escape
/// sequences, and future syntax that doesn't map one-to-one to
/// `Op::ConstString` can't silently throw the count off.
fn const_string_op_count(source: &str) -> usize {
    let ir_module = lower_to_ir(source);
    ir_module
        .concrete_functions()
        .flat_map(|f| f.blocks.iter())
        .flat_map(|b| b.instructions.iter())
        .filter(|i| matches!(i.op, Op::ConstString(_)))
        .count()
}

/// Fibonacci fixture, pulled directly from `tests/fixtures/fibonacci.phx`.
/// PR 3b's headline gain — exercises `Int` arith (`isub`, `iadd`),
/// comparison (`ile`), multi-block control flow via the loop+switch
/// dispatcher (decision G), and direct user-function recursion
/// (`Op::Call`). All four pieces compose for `fib(10) == 55` to print.
const FIBONACCI_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/fibonacci.phx"
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

/// Fibonacci end-to-end: PR 3b's headline test. Pins the loop+switch
/// dispatcher (multi-block control flow), `Int` arith / comparison,
/// and direct user-function recursion all working together. The fixture
/// prints `fib(0)`, `fib(1)`, `fib(5)`, `fib(10)` — covers both the
/// base-case branch (returns `n` directly) and the recursive branch
/// (two recursive calls + `iadd`).
#[test]
fn fibonacci_runs_under_wasmtime() {
    assert_wasm_matches_interp(FIBONACCI_SOURCE, "fibonacci");
}

/// Compile `hello.phx` and return its bytes, or `None` when the
/// runtime isn't built. `hello.phx` has no string literals and no
/// *sret* calls, so it serves as the "runtime contribution only"
/// baseline for data-segment, SP-traffic, and sret-load counts.
///
/// Cached via `OnceLock` so the multiple per-test baseline helpers
/// share a single compile pass even across the full test binary —
/// the cost was ~one compile per fixture test before, now it's one
/// for the whole run.
///
/// Critically: only `RuntimeWasmNotFound` returns `None` (the
/// expected skip condition matching `compile_or_skip`). Any *other*
/// compile error from `hello.phx` panics — that's a regression in
/// the baseline fixture itself, not a runtime-availability skip, and
/// silently swallowing it would let every structural assertion that
/// depends on the baseline silently skip with a misleading "baseline
/// must resolve" message at the call site.
fn hello_compiled_bytes() -> Option<&'static [u8]> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let ir_module = lower_to_ir(HELLO_SOURCE);
            match phoenix_cranelift::compile(&ir_module, Target::Wasm32Linear) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind == CompileErrorKind::RuntimeWasmNotFound => None,
                Err(e) => panic!(
                    "hello.phx baseline compile failed for a reason other than \
                     missing runtime: {e}. This is a regression in the baseline \
                     fixture itself — the structural test suite cannot proceed."
                ),
            }
        })
        .as_deref()
}

/// Active-data-segment count contributed by the runtime alone (via
/// `hello.phx`, which adds zero user-data segments).
fn hello_data_segment_baseline() -> Option<usize> {
    active_data_segment_count(hello_compiled_bytes()?).ok()
}

/// `(gets, sets)` of `__stack_pointer` contributed by the runtime
/// alone. `hello.phx` doesn't emit any user-side *sret* calls — the
/// only SP traffic comes from runtime function bodies — so any
/// fixture with `gets > hello_baseline.0` or `sets >
/// hello_baseline.1` provably exercised user-emitted SP manipulation.
/// `None` only when the runtime artifact itself isn't present
/// (`hello_compiled_bytes()` returns `None`); the merger always
/// emits a `name` section identifying the SP global, so the
/// resolution path doesn't itself produce skips.
fn hello_sp_baseline() -> Option<(usize, usize)> {
    sp_global_access_count_in_module(hello_compiled_bytes()?).ok()
}

/// Count of function bodies that contain the sret-load shape in
/// `hello.phx` — the runtime-only contribution. Used to scope the
/// fizzbuzz sret-load assertion to "fizzbuzz has strictly more
/// sret-shape bodies than the runtime alone." `None` when the
/// runtime isn't built or the scan fails.
fn hello_sret_load_shape_body_count() -> Option<usize> {
    module_sret_load_shape_body_count(hello_compiled_bytes()?).ok()
}

/// Same hello-baseline shape as [`hello_sret_load_shape_body_count`],
/// but counting *occurrences* of the sret shape (one per call site)
/// rather than bodies-containing-the-pattern. Used by the multi-concat
/// fixtures where one body contains more than one sret call site.
fn hello_sret_call_site_occurrence_count() -> Option<usize> {
    module_sret_call_site_occurrence_count(hello_compiled_bytes()?).ok()
}

/// `defer_basic` end-to-end: PR 3c's gate for `Op::ConstString` +
/// `print(String)` via decision H's data-section borrowed pointers.
/// Five sequential string-literal prints; the defer ordering in the
/// fixture is observable as LIFO output ("defer 3 / defer 2 / defer 1")
/// — verified through the AST interp comparison.
///
/// Structural assertion: each `Op::ConstString` reserves one active
/// data segment, so this fixture's segment count must be exactly
/// `hello-baseline + N` where N is the literal count derived from
/// the source. A regression that dropped a reservation (or aliased
/// two literals into one segment) shows up here before the wasmtime
/// tier even runs.
#[test]
fn defer_basic_runs_under_wasmtime() {
    compile_or_skip(DEFER_BASIC_SOURCE, "defer_basic", |bytes| {
        wasmparser::validate(bytes)
            .unwrap_or_else(|e| panic!("wasmparser rejected the module for defer_basic: {e}"));

        // We're inside `compile_or_skip`, so the runtime artifact is
        // present and `hello_data_segment_baseline` must resolve — a
        // `None` here would mean the baseline helper itself is broken
        // (not a skip condition), so panic rather than silently bypass
        // the assertion.
        let baseline = hello_data_segment_baseline()
            .expect("hello.phx baseline must resolve when the runtime artifact is available");
        let count = active_data_segment_count(bytes)
            .unwrap_or_else(|e| panic!("counting data segments: {e}"));
        let literals = const_string_op_count(DEFER_BASIC_SOURCE);
        let expected = baseline + literals;
        assert_eq!(
            count, expected,
            "defer_basic lowers to {literals} `Op::ConstString` instructions \
             so the compiled module must have exactly `hello-baseline \
             (= {baseline}) + {literals} = {expected}` active data segments; \
             got {count}. A miss means `Op::ConstString` lowering dropped a \
             reservation or the runtime data-segment count drifted from \
             hello.phx's.",
        );

        with_temp_wasm("defer_basic", bytes, |path| {
            let Some(actual) = run_with_wasmtime(path, "defer_basic") else {
                return;
            };
            let expected = run_ast_interp(DEFER_BASIC_SOURCE);
            assert_eq!(
                actual, expected,
                "wasmtime stdout disagrees with AST interp for defer_basic\n\
                 expected: {expected:?}\nactual:   {actual:?}"
            );
        });
    });
}

/// `fizzbuzz` end-to-end: PR 3c's gate for the *sret* call sequence +
/// value-returning multi-block functions of `StringRef` return type +
/// cross-function `Op::Call` returning a fat pointer. Exercises
/// `toString(Int)` (via `phx_i64_to_str` + stack-pointer manipulation),
/// string-literal returns from a function (multi-value WASM return),
/// and `imod` / `ieq` chained Branch terminators.
///
/// Structural assertions:
///
/// 1. Three string literals (`"FizzBuzz"`, `"Fizz"`, `"Buzz"`) → three
///    active data segments above the runtime baseline.
/// 2. SP-traffic count exceeds `hello.phx`'s baseline. Just "any SP
///    traffic in the module" would be ~always true (the runtime's own
///    bodies do plenty of SP work), so we anchor against the runtime-
///    only baseline from `hello.phx` and assert fizzbuzz adds strictly
///    more. The merged module always emits a one-entry `name` custom
///    section identifying `__stack_pointer` (regardless of whether
///    the runtime was built with names retained), so this assertion
///    is unconditional.
/// 3. The exact sret-load shape: the count of function bodies
///    containing a `(call …; i32.load offset=0; i32.load offset=4)`
///    pattern must strictly exceed the hello-baseline count. A
///    regression to e.g. `offset: 8` (reading garbage past the fat
///    pointer) would still pass wasm validation; anchoring against
///    the baseline catches it even if the runtime's own bodies happen
///    to match the same shape.
#[test]
fn fizzbuzz_runs_under_wasmtime() {
    compile_or_skip(FIZZBUZZ_SOURCE, "fizzbuzz", |bytes| {
        wasmparser::validate(bytes)
            .unwrap_or_else(|e| panic!("wasmparser rejected the module for fizzbuzz: {e}"));

        // Inside `compile_or_skip` → runtime is present → every
        // baseline helper must resolve. A failure to resolve is a
        // helper-side regression, not a skip condition, so panic.
        let baseline = hello_data_segment_baseline()
            .expect("hello.phx baseline must resolve when the runtime artifact is available");
        let count = active_data_segment_count(bytes)
            .unwrap_or_else(|e| panic!("counting data segments: {e}"));
        let literals = const_string_op_count(FIZZBUZZ_SOURCE);
        let expected = baseline + literals;
        assert_eq!(
            count, expected,
            "fizzbuzz lowers to {literals} `Op::ConstString` instructions \
             so the compiled module must have exactly `hello-baseline \
             (= {baseline}) + {literals} = {expected}` active data segments; \
             got {count}. The `toString` result is heap-allocated, so it \
             doesn't add a data segment.",
        );

        // SP usage anchored against the hello.phx baseline (runtime-only
        // contribution). Fizzbuzz's `toString(Int)` is the one user-side
        // site that emits SP traffic in PR 3c, so the count *must*
        // exceed the baseline; a regression that dropped the sret
        // sequence (or emitted it as a no-op) shows up here even if
        // gets/sets are nonzero from the runtime's own bodies.
        let (base_gets, base_sets) = hello_sp_baseline().expect(
            "hello.phx SP baseline must resolve — the merger emits a `name` \
             section for `__stack_pointer` unconditionally",
        );
        let (gets, sets) = sp_global_access_count_in_module(bytes).expect(
            "fizzbuzz's merged module must expose `__stack_pointer` via its `name` section",
        );
        assert!(
            gets > base_gets && sets > base_sets,
            "fizzbuzz's *sret* sequence must add at least one \
             GlobalGet/GlobalSet pair on `__stack_pointer` beyond what \
             the runtime itself contributes; got gets={gets} (baseline \
             {base_gets}), sets={sets} (baseline {base_sets}). A miss \
             means the sret sequence was reached but emitted no SP \
             manipulation, or it was bypassed entirely."
        );

        // Structural shape of the sret loads, anchored against the
        // hello baseline: even if a runtime body coincidentally emits
        // the same (call; load@0; load@4) pattern, fizzbuzz must
        // *add* at least one such body via the user-side
        // `toString(Int)` call. A regression that dropped or relocated
        // the user loads would fail to widen the count past the
        // baseline.
        let base_count = hello_sret_load_shape_body_count()
            .expect("hello.phx sret-shape baseline must resolve when the runtime is available");
        let count = module_sret_load_shape_body_count(bytes)
            .unwrap_or_else(|e| panic!("scanning for sret-load shape: {e}"));
        assert!(
            count > base_count,
            "fizzbuzz must add at least one body with a post-call \
             `(i32.load offset=0, i32.load offset=4)` pair beyond what \
             hello.phx contributes; got count={count} (baseline \
             {base_count}). A miss means the offsets drifted in the \
             user-emitted sret sequence (regression hazard: the same \
             drift would silently miscompile any future sret-returning \
             builtin)."
        );

        with_temp_wasm("fizzbuzz", bytes, |path| {
            let Some(actual) = run_with_wasmtime(path, "fizzbuzz") else {
                return;
            };
            let expected = run_ast_interp(FIZZBUZZ_SOURCE);
            assert_eq!(
                actual, expected,
                "wasmtime stdout disagrees with AST interp for fizzbuzz\n\
                 expected: {expected:?}\nactual:   {actual:?}"
            );
        });
    });
}

/// `let mut` + integer-arith + while loop: PR 3c slice 2's gate for
/// `Op::Alloca(I64)` + `Op::Load` + `Op::Store` routed through the
/// loop+switch dispatcher's while-loop shape (bb_header / bb_body /
/// bb_after). Pinning the sum of 1..=10 = 55 catches off-by-one
/// regressions in the load-modify-store sequence and any local-index
/// drift between the slot's binding and the loaded-value's binding.
#[test]
fn mutable_int_while_loop_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let mut sum: Int = 0\n  \
                 let mut i: Int = 1\n  \
                 while i <= 10 {\n    \
                   sum = sum + i\n    \
                   i = i + 1\n  \
                 }\n  \
                 print(sum)\n\
               }\n";
    assert_wasm_matches_interp(src, "mutable_int_while_loop");
}

/// Mutable `String` accumulator + while + `Op::StringConcat`: PR 3c
/// slice 2's gate for the multi-slot `Op::Alloca(StringRef)` path plus
/// the `phx_str_concat` sret call. The fixture loops three times,
/// appending `"x"` each iteration; `Op::Load` / `Op::Store` on a 2-slot
/// (i32 ptr, i32 len) fat-pointer slot must read/write *both* locals
/// in the right order. A regression that read only one slot would
/// surface as garbage output (or a wasmtime trap on the bogus pointer)
/// rather than the expected `xxx`.
#[test]
fn mutable_string_concat_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let mut s: String = \"\"\n  \
                 let mut i: Int = 0\n  \
                 while i < 3 {\n    \
                   s = s + \"x\"\n    \
                   i = i + 1\n  \
                 }\n  \
                 print(s)\n\
               }\n";
    assert_wasm_matches_interp(src, "mutable_string_concat");
}

/// Three-way `Op::StringConcat` chain (`a + b + c`): pins that nested
/// sret calls compose. Each concat allocates a new GC string; the
/// chain `((a + b) + c)` runs `phx_str_concat` twice, with the
/// intermediate result feeding the second call. Without shadow-stack
/// rooting (the rest of PR 3c), this works because no GC-triggering
/// allocation happens between the two concat calls — the second
/// concat's allocation is the only intervening allocation, and it
/// consumes the intermediate before triggering any sweep.
///
/// Structural anchor: pin that *two* distinct sret call sites land in
/// the user code beyond what the runtime baseline contributes. A
/// regression that collapsed the chain into one call (or that emitted
/// the second call but dropped its `(I32Load@0, I32Load@4)` pair)
/// would lose the `+2` margin even if stdout happened to match.
#[test]
fn chained_string_concat_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let a: String = \"hello\"\n  \
                 let b: String = \", \"\n  \
                 let c: String = \"world\"\n  \
                 print(a + b + c)\n\
               }\n";
    let label = "chained_string_concat";
    compile_or_skip(src, label, |bytes| {
        wasmparser::validate(bytes)
            .unwrap_or_else(|e| panic!("wasmparser rejected the module for {label}: {e}"));

        let base_count = hello_sret_call_site_occurrence_count().expect(
            "hello.phx sret-occurrence baseline must resolve when the runtime is available",
        );
        let count = module_sret_call_site_occurrence_count(bytes)
            .unwrap_or_else(|e| panic!("scanning for sret call sites: {e}"));
        assert!(
            count >= base_count + 2,
            "chained `a + b + c` must add at least 2 sret call sites \
             beyond the hello baseline (one per `phx_str_concat` in \
             the chain); got count={count} (baseline {base_count}). A \
             regression that collapsed or dropped one of the concat \
             calls would surface here before the wasmtime tier ran."
        );

        with_temp_wasm(label, bytes, |path| {
            let Some(actual) = run_with_wasmtime(path, label) else {
                return;
            };
            let expected = run_ast_interp(src);
            assert_eq!(
                actual, expected,
                "wasmtime stdout disagrees with AST interp for {label}\n\
                 expected: {expected:?}\nactual:   {actual:?}"
            );
        });
    });
}

/// Negative integer: `-123` lowers to `INeg(ConstI64(123))`. PR 3b
/// adds `Op::INeg` (emitted as `0 - x` because WASM MVP has no
/// `i64.neg`), so this fixture now reaches `phx_print_i64`'s
/// `is_negative` branch — exercising both the magnitude-negation step
/// and the sign-store path inside the runtime's itoa loop. PR 2
/// couldn't express this from any source-level fixture.
#[test]
fn negative_int_runs_under_wasmtime() {
    let src = "function main() {\n  print(0 - 123)\n}\n";
    assert_wasm_matches_interp(src, "negative_int");
}

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

// `toString(Float)` end-to-end coverage is deferred until `Op::ConstF64`
// lowering lands (later in PR 3c — float arithmetic / literals follow
// the heap surface). The dispatch arm for `IrType::F64 →
// "phx_f64_to_str"` exists in `translate_to_string_builtin` today, but
// there's no source-level way to construct a `Float` binding yet, so
// any test would fail at `Op::ConstF64`'s "not yet supported"
// diagnostic before reaching the *sret* call sequence. The bool test
// below catches dispatch-table regressions in the meantime.

/// `toString(Bool)` exercises the third *sret*-returning variant —
/// dispatches to `phx_bool_to_str`, which takes an `i8` rather than
/// the i64/f64 the other two take. A regression in the per-type
/// arg-width handling (e.g. emitting an i64 arg for the bool case
/// because the dispatch fell through) would show up here as a
/// wasmtime trap or wrong output.
#[test]
fn to_string_bool_runs_under_wasmtime() {
    let src = "function main() {\n  print(toString(true))\n  print(toString(false))\n}\n";
    assert_wasm_matches_interp(src, "to_string_bool");
}

/// `toString(String)` is the source-level identity — `translate_to_string_builtin`
/// short-circuits it to a 2-slot local-copy with no runtime call and
/// no *sret* plumbing. The other `toString` tests would still pass if
/// this branch silently fell through to the default-arm error
/// diagnostic; this fixture pins the identity arm specifically so a
/// regression there surfaces as a compile failure rather than a
/// "looks like it worked because the other arms still do" silence.
#[test]
fn to_string_string_runs_under_wasmtime() {
    let src = "function main() {\n  print(toString(\"hi\"))\n}\n";
    assert_wasm_matches_interp(src, "to_string_string");
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

/// Bool-producing comparison: integer-compare ops (`Op::IEq` and
/// friends) land their `i32 0/1` result in a `Bool` binding, which
/// then routes through `phx_print_bool`. `bool_print_runs_under_wasmtime`
/// only exercises bool *literals* — without this fixture, a regression
/// that miscompiled `IEq` (e.g. wrong WASM op, or stored to an `i64`
/// local) would still let `print(true)` pass cleanly. The two cases
/// (true and false) cover both `i64.eq` outcomes.
#[test]
fn bool_from_int_cmp_runs_under_wasmtime() {
    let src = "function main() {\n  print(1 == 1)\n  print(1 == 2)\n}\n";
    assert_wasm_matches_interp(src, "bool_from_int_cmp");
}

/// If-as-expression: both arms compute a value and jump to a merge
/// block, where the merged value is bound via a block parameter. This
/// exercises the **Jump-with-args** path in `emit_block_param_copies`
/// that fibonacci's branch-then-return shape never reaches — both
/// arms of the conditional terminate with `Jump merge(value)` rather
/// than `Return`, so the dispatcher copies the SSA-value local into
/// the merge block's param local before re-entering the dispatch.
///
/// Phoenix lowers `let mut` / `while` mutable state through
/// `Op::Alloca` rather than block-param threading (deferred to PR 3c
/// alongside the rest of the heap-aware op surface), so loop-shaped
/// back-edges with SSA params can't be expressed at source level
/// here. The if-as-expression shape is the simplest source-level
/// fixture that produces a `Jump { args: [..] }` IR terminator and
/// thus locks down the param-copy path against regression.
#[test]
fn if_as_expression_runs_under_wasmtime() {
    let src = "function abs2(x: Int) -> Int {\n  \
        if x > 0 { x * 2 } else { 0 - x }\n\
        }\n\
        function main() {\n  \
        print(abs2(5))\n  \
        print(abs2(0 - 3))\n\
        }\n";
    assert_wasm_matches_interp(src, "if_as_expression");
}

/// Build an IR module whose `main` function exercises the parallel-
/// copy semantics of `emit_block_param_copies` (translate.rs). The
/// shape is a 3-iteration loop whose back-edge passes the loop
/// header's own block params in *shuffled* order — every iteration
/// swaps `x` and `y` while incrementing `i`. After the loop exits, we
/// print `x` then `y`.
///
/// ```text
/// entry: c0=0, a=10, b=20; jump loop(c0, a, b)
/// loop (i, x, y):
///   print(i)
///   cond = i < 3
///   if cond { body } else { exit }
/// body:
///   i' = i + 1
///   jump loop(i', y, x)        ;; SHUFFLED — x↔y on every iter
/// exit:
///   print(x); print(y); return
/// ```
///
/// Expected stdout with the correct parallel-copy semantics:
/// `0 1 2 3 20 10`. A naive sequential copy (set in declaration order
/// while reading from sources) would clobber `x` before reading it on
/// the swap, collapsing `y` to whatever the prior `x` held — yielding
/// `0 1 2 3 20 20` instead. The two diverge on the *first* swap, so a
/// regression surfaces immediately rather than depending on iteration
/// depth.
///
/// Phoenix's source surface lowers `let mut` / `while` mutable state
/// through `Op::Alloca` (deferred to PR 3c) rather than block-param
/// threading, so this Jump-with-shuffled-args shape is unreachable
/// from any source-level fixture today. Hand-building the IR is the
/// only way to pin the parallel-copy path before PR 3c lands.
fn build_shuffled_back_edge_ir() -> IrModule {
    let mut module = IrModule::new();
    let mut f = IrFunction::new(
        FuncId(0), // overwritten by push_concrete
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );

    let entry = f.create_block();
    let loop_blk = f.create_block();
    let body = f.create_block();
    let exit_blk = f.create_block();

    // `loop` block parameters: (i, x, y). The back-edge from `body`
    // will pass [i+1, y, x] — the swap is what exercises the
    // parallel-copy path, since `y_local` and `x_local` are both in
    // the dest-locals set and read as sources at the same time.
    let i = f.add_block_param(loop_blk, IrType::I64);
    let x = f.add_block_param(loop_blk, IrType::I64);
    let y = f.add_block_param(loop_blk, IrType::I64);

    // entry: jump loop(0, 10, 20)
    let c0 = f.emit_value(entry, Op::ConstI64(0), IrType::I64, None);
    let a = f.emit_value(entry, Op::ConstI64(10), IrType::I64, None);
    let b = f.emit_value(entry, Op::ConstI64(20), IrType::I64, None);
    f.set_terminator(
        entry,
        Terminator::Jump {
            target: loop_blk,
            args: vec![c0, a, b],
        },
    );

    // loop: print(i); cond = i < 3; branch
    f.emit(
        loop_blk,
        Op::BuiltinCall("print".to_string(), vec![i]),
        IrType::Void,
        None,
    );
    let three = f.emit_value(loop_blk, Op::ConstI64(3), IrType::I64, None);
    let cond = f.emit_value(loop_blk, Op::ILt(i, three), IrType::Bool, None);
    f.set_terminator(
        loop_blk,
        Terminator::Branch {
            condition: cond,
            true_block: body,
            true_args: vec![],
            false_block: exit_blk,
            false_args: vec![],
        },
    );

    // body: i' = i + 1; jump loop(i', y, x)  -- the SHUFFLE
    let one = f.emit_value(body, Op::ConstI64(1), IrType::I64, None);
    let i_plus = f.emit_value(body, Op::IAdd(i, one), IrType::I64, None);
    f.set_terminator(
        body,
        Terminator::Jump {
            target: loop_blk,
            args: vec![i_plus, y, x],
        },
    );

    // exit: print(x); print(y); return
    f.emit(
        exit_blk,
        Op::BuiltinCall("print".to_string(), vec![x]),
        IrType::Void,
        None,
    );
    f.emit(
        exit_blk,
        Op::BuiltinCall("print".to_string(), vec![y]),
        IrType::Void,
        None,
    );
    f.set_terminator(exit_blk, Terminator::Return(None));

    module.push_concrete(f);
    module
}

/// Parallel-copy semantics regression test. Constructs an IR module
/// with a loop whose back-edge passes block params in shuffled order
/// (see [`build_shuffled_back_edge_ir`] for the precise shape and the
/// expected-vs-buggy stdout difference). A naive sequential
/// `local.get src; local.set dest` per-pair lowering would clobber
/// one of the swap targets — this test would then print
/// `0 1 2 3 20 20` instead of `0 1 2 3 20 10`.
///
/// IR is hand-built because no source-level Phoenix construct produces
/// a `Jump { args: [..] }` with shuffled block-param order today
/// (mutable loop state lives on `Op::Alloca`, deferred to PR 3c).
#[test]
fn parallel_copy_back_edge_swap_runs_under_wasmtime() {
    let ir_module = build_shuffled_back_edge_ir();
    let verify_errors = phoenix_ir::verify::verify(&ir_module);
    assert!(
        verify_errors.is_empty(),
        "hand-built IR fixture failed verification: {verify_errors:?}"
    );

    compile_ir_or_skip(&ir_module, "parallel_copy_back_edge_swap", |bytes| {
        wasmparser::validate(bytes)
            .unwrap_or_else(|e| panic!("wasmparser rejected parallel-copy fixture: {e}"));

        with_temp_wasm("parallel_copy_back_edge_swap", bytes, |path| {
            let Some(actual) = run_with_wasmtime(path, "parallel_copy_back_edge_swap") else {
                return;
            };
            // Iterations print i=0,1,2,3 (the print fires *before* the
            // i<3 check, so i=3 is printed too on the last iteration).
            // Then exit prints x then y. After 3 swaps (one per back-
            // edge), x and y are swapped from their initial (10, 20)
            // ordering: x=20, y=10.
            let expected = "0\n1\n2\n3\n20\n10\n";
            assert_eq!(
                actual, expected,
                "parallel-copy regression: a naive sequential copy would \
                 produce `0 1 2 3 20 20` (y clobbered to match x). \
                 expected: {expected:?}\nactual:   {actual:?}"
            );
        });
    });
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
fn accepts_multi_block_control_flow_with_branch() {
    // PR 3b adds multi-block control flow via the loop+switch
    // dispatcher (decision G). An `if` introduces a Branch terminator
    // and two non-entry blocks; codegen must accept it. Before PR 3b
    // this was `rejects_multi_block_control_flow`; the inversion pins
    // the lifted restriction so a future regression resurfaces here
    // rather than only surfacing at execution time. Routing through
    // `assert_wasm_matches_interp` (rather than just asserting compile
    // success) additionally locks down the dispatcher's *behavior* in
    // this minimal "single-arm if, no merge" shape — a regression that
    // emitted structurally-valid WASM but jumped to the wrong block
    // would still pass a compile-only check.
    //
    // The condition is `n > 0` (computed at runtime) rather than a
    // literal `true`. A future IR-builder pass that constant-folded
    // `if true { ... }` to a single straight-line block would
    // otherwise silently drain this test of multi-block coverage —
    // pinning a non-constant predicate makes that regression visible
    // here rather than in some downstream perf test.
    let src = "function main() {\n  \
        let n: Int = 1\n  \
        if n > 0 {\n    \
        print(n)\n  \
        }\n\
        }\n";
    assert_wasm_matches_interp(src, "accepts_multi_block_control_flow_with_branch");
}

#[test]
fn rejects_unsupported_ir_op() {
    // Float arithmetic is not in PR 3b's op surface — only `Int`
    // arith (`IAdd` / `ISub` / `IMul` / `IDiv` / `IMod` / `INeg`)
    // and comparisons land here; `FAdd` / `ConstF64` / friends defer
    // to PR 3c alongside the rest of the wider numeric surface. The
    // diagnostic must cite the PR-3 follow-up so a regression in the
    // deferred-error wording is visible.
    let src = "function main() {\n  let a: Float = 1.5\n  let b: Float = 2.5\n  let c: Float = a + b\n  print(c)\n}\n";
    let err = expect_wasm_compile_error(src);
    // Pin the *path* — `IR op` only appears in the per-op rejection
    // arm of `translate_instruction`, so this rules out a spurious
    // type-rep rejection (`IR type \`F64\``) firing first and masking
    // a regression in the op-coverage check.
    assert!(
        err.contains("IR op") && err.contains("not yet supported"),
        "expected `IR op ... not yet supported` op error, got: {err}"
    );
    assert!(
        err.contains("PR 3"),
        "expected error to cite the PR 3 follow-up, got: {err}"
    );
}

#[test]
fn rejects_unrepresentable_param_type() {
    // A `List<Int>` parameter has no WASM value representation in
    // PR 3b — collection types land in PR 3c with shadow-stack root
    // emission and the `phx_list_alloc`-based ABI. The fixture keeps
    // `main` minimal and never calls `sink`, so the only path that
    // can fire is the param-rejection one: a previous shape that
    // called `sink([1,2,3])` would fail at `Op::ListAlloc` /
    // `Op::Call` before hitting the param check, masking regressions
    // in the param-rejection path we want to lock down.
    let src = "function main() {}\n\
               function sink(xs: List<Int>) {\n  print(true)\n}\n";
    let err = expect_wasm_compile_error(src);
    // Pin both the type (so a regression that swapped `ListRef` for
    // a different fallthrough type would surface) and the path
    // (`value representation` only comes from `wasm_valtypes_for`'s
    // `unsupported(...)`, ruling out an op-side rejection that
    // happened to mention `not yet supported` for unrelated reasons).
    assert!(
        err.contains("IR type `ListRef") && err.contains("value representation"),
        "expected `IR type \\`ListRef ...\\` ... value representation` error, got: {err}"
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
    // `function returns_list() -> List<Int> { ... }` lowers to an IR
    // function whose `return_type` is `IrType::ListRef`, which has no
    // PR 3b WASM value representation. Pinning this branch separately
    // from the param-side rejection in
    // `rejects_unrepresentable_param_type` covers both halves of
    // `wasm_valtypes_for`'s value-representation gate; without this,
    // a regression on the return path would only surface once a
    // list-returning function was actually called from `main`.
    let src = "function main() {}\n\
               function returns_list() -> List<Int> {\n  return [1, 2, 3]\n}\n";
    let err = expect_wasm_compile_error(src);
    // Same shape as `rejects_unrepresentable_param_type`'s tightened
    // assertion: pin `ListRef` and the `value representation` path so
    // a future regression that moved the rejection elsewhere (e.g.
    // an op-side check firing first) doesn't pass silently.
    assert!(
        err.contains("IR type `ListRef") && err.contains("value representation"),
        "expected `IR type \\`ListRef ...\\` ... value representation` error \
         on return-position list, got: {err}"
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
