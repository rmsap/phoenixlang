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

/// Single-pass walk of the wasm bytes collecting the bookkeeping
/// every phx_main / shadow-stack assertion needs:
/// * `import_func_count` so absolute function indices can be biased
///   into code-section-local indices;
/// * the code-section `FunctionBody`s;
/// * the function index of the `_start` export (used to locate the
///   user `main` function — see [`call_targets_in_phx_main`]).
///
/// Centralizing this in one helper means callers never have to re-parse
/// the same bytes; each new structural assertion gets the lookups it
/// needs from the returned bundle.
struct WasmInspect<'a> {
    import_func_count: u32,
    bodies: Vec<wasmparser::FunctionBody<'a>>,
    start_func_idx: Option<u32>,
}

fn inspect_wasm(bytes: &[u8]) -> Result<WasmInspect<'_>, String> {
    use wasmparser::{Parser, Payload};

    let mut import_func_count: u32 = 0;
    let mut bodies: Vec<wasmparser::FunctionBody<'_>> = Vec::new();
    let mut start_func_idx: Option<u32> = None;

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| format!("parsing payload header: {e}"))?;
        match payload {
            Payload::ImportSection(rdr) => {
                for group in rdr {
                    let group = group.map_err(|e| format!("import section: {e}"))?;
                    match group {
                        wasmparser::Imports::Single(_, imp) => {
                            if matches!(imp.ty, wasmparser::TypeRef::Func(_)) {
                                import_func_count += 1;
                            }
                        }
                        wasmparser::Imports::Compact1 { items, .. } => {
                            for item in items {
                                let item = item.map_err(|e| format!("import items: {e}"))?;
                                if matches!(item.ty, wasmparser::TypeRef::Func(_)) {
                                    import_func_count += 1;
                                }
                            }
                        }
                        wasmparser::Imports::Compact2 { ty, names, .. } => {
                            if matches!(ty, wasmparser::TypeRef::Func(_)) {
                                for name in names {
                                    name.map_err(|e| format!("import names: {e}"))?;
                                    import_func_count += 1;
                                }
                            }
                        }
                    }
                }
            }
            Payload::ExportSection(rdr) => {
                for export in rdr {
                    let export = export.map_err(|e| format!("export section: {e}"))?;
                    if export.name == "_start"
                        && matches!(export.kind, wasmparser::ExternalKind::Func)
                    {
                        start_func_idx = Some(export.index);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => bodies.push(body),
            _ => {}
        }
    }

    Ok(WasmInspect {
        import_func_count,
        bodies,
        start_func_idx,
    })
}

/// Walk a single function body's instructions and return every
/// `Call`-target function index in the order they appear. Used by
/// per-function structural assertions (no parsing of the whole module
/// is implied — that lives in [`inspect_wasm`]).
fn call_targets_in_body(body: &wasmparser::FunctionBody<'_>) -> Result<Vec<u32>, String> {
    use wasmparser::Operator;
    let mut reader = body
        .get_operators_reader()
        .map_err(|e| format!("function-body operators: {e}"))?;
    let mut targets = Vec::new();
    while !reader.eof() {
        if let Operator::Call { function_index } = reader
            .read()
            .map_err(|e| format!("function-body operator: {e}"))?
        {
            targets.push(function_index);
        }
    }
    Ok(targets)
}

/// Same as [`call_targets_in_body`], but pairs each `Call` target with
/// the immediately preceding `I32Const` literal (if any). Used by tests
/// that need to identify a specific `phx_gc_push_frame(N)` site by its
/// `N` argument — the ad-hoc 1-slot key frame in `List.sortBy`, for
/// example, is the only push preceded by `I32Const(1)` whose target
/// also matches the function-level push.
fn call_targets_with_preceding_i32const(
    body: &wasmparser::FunctionBody<'_>,
) -> Result<Vec<(u32, Option<i32>)>, String> {
    use wasmparser::Operator;
    let mut reader = body
        .get_operators_reader()
        .map_err(|e| format!("function-body operators: {e}"))?;
    let mut targets = Vec::new();
    let mut last_const: Option<i32> = None;
    while !reader.eof() {
        match reader
            .read()
            .map_err(|e| format!("function-body operator: {e}"))?
        {
            Operator::I32Const { value } => last_const = Some(value),
            Operator::Call { function_index } => {
                targets.push((function_index, last_const));
                last_const = None;
            }
            _ => last_const = None,
        }
    }
    Ok(targets)
}

/// Return the list of `Call`-target indices in the body of the user
/// `main` function (`phx_main`). The merged module doesn't re-export
/// `phx_main` by name, but `_start` always calls it as the
/// second-to-last call — `emit_start_body` lays out
/// `[ctors?, phx_gc_enable, phx_main, phx_gc_shutdown]`, so phx_main
/// is at index `len - 2`. We re-derive its function index from
/// `_start`'s call list and walk its body for `Call` opcodes.
///
/// Used by the shadow-stack assertions on fibonacci (which has no
/// ref bindings → must contain no `phx_gc_push_frame` /
/// `phx_gc_pop_frame` calls; the surface call count is a direct
/// regression tripwire) and on string fixtures (which gain at least
/// one frame push + pop pair, so the count strictly exceeds the
/// fibonacci baseline).
fn call_targets_in_phx_main(bytes: &[u8]) -> Result<Vec<u32>, String> {
    let inspect = inspect_wasm(bytes)?;
    let start_idx = inspect
        .start_func_idx
        .ok_or_else(|| "module has no `_start` export".to_string())?;
    let start_body = body_for_func_idx(&inspect, start_idx)
        .map_err(|e| format!("locating `_start` body: {e}"))?;
    let start_calls = call_targets_in_body(start_body)?;
    if start_calls.len() < 2 {
        return Err(format!(
            "`_start` body has {} `Call` ops; phx_main lookup expects at least 2 \
             (phx_gc_enable + phx_main + phx_gc_shutdown, optionally preceded by \
             a ctor call)",
            start_calls.len(),
        ));
    }
    // `emit_start_body` always emits phx_main second-to-last (the
    // final call is phx_gc_shutdown). That holds with or without a
    // ctor call leading the body.
    let phx_main_idx = start_calls[start_calls.len() - 2];
    let phx_main_body = body_for_func_idx(&inspect, phx_main_idx)
        .map_err(|e| format!("locating phx_main body: {e}"))?;
    call_targets_in_body(phx_main_body)
}

/// Same as [`call_targets_in_phx_main`] but pairs each Call with its
/// immediately preceding `I32Const` literal. Used by tests that
/// distinguish push-frame sites by their frame-size argument.
fn call_targets_with_const_in_phx_main(bytes: &[u8]) -> Result<Vec<(u32, Option<i32>)>, String> {
    let inspect = inspect_wasm(bytes)?;
    let start_idx = inspect
        .start_func_idx
        .ok_or_else(|| "module has no `_start` export".to_string())?;
    let start_body = body_for_func_idx(&inspect, start_idx)
        .map_err(|e| format!("locating `_start` body: {e}"))?;
    let start_calls = call_targets_in_body(start_body)?;
    if start_calls.len() < 2 {
        return Err(format!(
            "`_start` body has {} `Call` ops; phx_main lookup expects at least 2",
            start_calls.len(),
        ));
    }
    let phx_main_idx = start_calls[start_calls.len() - 2];
    let phx_main_body = body_for_func_idx(&inspect, phx_main_idx)
        .map_err(|e| format!("locating phx_main body: {e}"))?;
    call_targets_with_preceding_i32const(phx_main_body)
}

/// Resolve an absolute WASM function index (the kind that appears in a
/// `Call` operand) to its code-section `FunctionBody` reference.
/// Returns an error if the index points at an imported function (no
/// body) or past the end of the code section.
fn body_for_func_idx<'a, 'b>(
    inspect: &'a WasmInspect<'b>,
    func_idx: u32,
) -> Result<&'a wasmparser::FunctionBody<'b>, String> {
    if func_idx < inspect.import_func_count {
        return Err(format!(
            "function {func_idx} is imported (import_func_count={}); no local body exists",
            inspect.import_func_count,
        ));
    }
    let local_idx = (func_idx - inspect.import_func_count) as usize;
    inspect.bodies.get(local_idx).ok_or_else(|| {
        format!(
            "function {func_idx} resolves to local body {local_idx} but the code \
             section has only {} bodies",
            inspect.bodies.len(),
        )
    })
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

/// Shadow-stack rooting must NOT add any `phx_gc_push_frame` /
/// `phx_gc_set_root` / `phx_gc_pop_frame` calls to a function whose
/// bindings are all value types. Fibonacci is the canonical ref-free
/// gate: every binding is `Int` or `Bool`, so `assign_gc_root_slots`
/// returns an empty map and `setup_gc_frame` short-circuits before
/// allocating the frame local. `phx_main`'s call list should be
/// exactly `[fib, phx_print_i64] × 4`.
///
/// The exact call count (8) anchors against two regression shapes:
/// (a) accidentally emitting `phx_gc_push_frame(0)` for ref-free
/// functions (would push call_count to ≥9), and (b) accidentally
/// emitting `emit_gc_set_root` for value-typed Alloca bindings (would
/// push call_count higher still). Both are silent miscompiles — the
/// resulting program would still print 0, 1, 5, 55, but waste GC
/// runtime on no-op frame churn.
///
/// **Fixture coupling.** The exact count is keyed to `FIBONACCI_SOURCE`
/// printing four values (`fib(0)`, `fib(1)`, `fib(5)`, `fib(10)`). If
/// that fixture changes (extra print, fewer prints, switching to a
/// loop), update the `assert_eq!(calls.len(), 8, …)` to match — the
/// shape-of-bytecode invariant the test pins is "ref-free → no `phx_gc_*`
/// calls in `phx_main`", which the distinct-target check below already
/// captures independently of the exact count.
#[test]
fn fibonacci_emits_no_shadow_stack_frame_calls() {
    compile_or_skip(
        FIBONACCI_SOURCE,
        "fibonacci_no_shadow_stack_frame",
        |bytes| {
            let calls = call_targets_in_phx_main(bytes)
                .unwrap_or_else(|e| panic!("locating phx_main and decoding its calls failed: {e}"));
            assert_eq!(
                calls.len(),
                8,
                "fibonacci's `main` should contain exactly 8 `Call` opcodes \
                 (4× fib + 4× phx_print_i64) — any other count means the \
                 shadow-stack pass started emitting `phx_gc_push_frame` / \
                 `phx_gc_set_root` / `phx_gc_pop_frame` for a function with \
                 no ref-typed bindings, which `assign_gc_root_slots` is \
                 supposed to short-circuit. got call targets={calls:?}"
            );
            // Distinct-target check: `fib` and `phx_print_i64` are the only
            // two callees in `main`. A regression that wedged a GC frame
            // call into the function would land a third distinct index.
            let mut distinct = calls.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert_eq!(
                distinct.len(),
                2,
                "fibonacci's `main` should call exactly 2 distinct \
                 functions (`fib` and `phx_print_i64`); got {} distinct \
                 targets ({distinct:?}) — a new distinct target most \
                 likely means a `phx_gc_*` call leaked into a ref-free \
                 function body.",
                distinct.len(),
            );
        },
    );
}

/// Counterpart to [`fibonacci_emits_no_shadow_stack_frame_calls`]: a
/// function with ref-typed bindings *must* gain shadow-stack frame
/// machinery in its body. The fixture is `let mut s: String = "x"; s
/// = s + "y"; print(s)` — the `Op::Alloca(StringRef)` creates a
/// ref-typed slot, `Op::Store` re-roots it after the concat write,
/// and the function ends with a `Return(None)` that must pop the
/// frame.
///
/// Asserts the call count strictly exceeds fibonacci's 8: at least
/// one `phx_gc_push_frame` + one `phx_gc_pop_frame` + at least one
/// `phx_gc_set_root` land in `phx_main` on top of the concat / print
/// calls the source already requires. A regression that dropped any
/// of the three would show up as a count miss before wasmtime even
/// ran (and the end-to-end `mutable_string_concat_runs_under_wasmtime`
/// catches the corresponding *behavioral* regression at the
/// runtime tier).
#[test]
fn string_mut_fixture_emits_shadow_stack_frame_calls() {
    let src = "function main() {\n  \
                 let mut s: String = \"x\"\n  \
                 s = s + \"y\"\n  \
                 print(s)\n\
               }\n";
    compile_or_skip(src, "string_mut_shadow_stack_frame", |bytes| {
        let calls = call_targets_in_phx_main(bytes)
            .unwrap_or_else(|e| panic!("locating phx_main and decoding its calls failed: {e}"));
        // Frame-balance tripwire: every `phx_gc_push_frame` call must
        // be matched by exactly one `phx_gc_pop_frame`. A regression
        // that wired up the push side but skipped the pop on some
        // terminator path would leak frame entries — the runtime's
        // per-thread frame counter would drift, eventually tripping a
        // debug-assert or (in release) silently misrooting future
        // allocations.
        //
        // Resolving these function indices by name isn't free in the
        // merged module: the merger doesn't re-export the runtime's
        // `phx_gc_*` symbols and doesn't preserve a Function name
        // subsection in the output. The codegen *contract* pins them
        // structurally instead — `setup_gc_frame` emits the push as
        // the very first Call in the function body (no other Call can
        // run before it), and `emit_gc_pop_frame` runs before every
        // `Return`, so for a single-Return fixture like this one the
        // pop is the very last Call. We anchor on those positions and
        // count their occurrences across the body, which catches a
        // dropped pop without needing the name resolution.
        let push_idx = *calls
            .first()
            .expect("ref-using `main` should emit at least one Call (phx_gc_push_frame)");
        let pop_idx = *calls
            .last()
            .expect("ref-using `main` should emit at least one Call (phx_gc_pop_frame)");
        assert_ne!(
            push_idx, pop_idx,
            "first and last `Call` opcodes in `phx_main` should target different \
             runtime functions (push_frame ≠ pop_frame); both came back as {push_idx} \
             — most likely the pop emission was dropped and the last call now resolves \
             to whatever the previous emission was. calls={calls:?}",
        );
        let push_count = calls.iter().filter(|&&t| t == push_idx).count();
        let pop_count = calls.iter().filter(|&&t| t == pop_idx).count();
        assert_eq!(
            push_count, pop_count,
            "phx_gc_push_frame (idx {push_idx}, by codegen-contract the first Call) \
             and phx_gc_pop_frame (idx {pop_idx}, by codegen-contract the last Call) \
             must appear an equal number of times in `phx_main` — got push={push_count}, \
             pop={pop_count}. A mismatch means some Return path skipped the pop and \
             the runtime's frame counter will drift. calls={calls:?}",
        );
        assert_eq!(
            push_count, 1,
            "single-Return `main` fixture should emit exactly one push/pop pair; got \
             push_count={push_count} — either codegen started emitting per-block frames \
             or the fixture grew a second control-flow path. calls={calls:?}",
        );
        // Lower bound counting (each emit_gc_set_root that the shadow-
        // stack pass adds):
        //   1× phx_gc_push_frame
        //   1× phx_gc_set_root after `Op::Store` re-roots `s`
        //   1× phx_gc_set_root after the `Op::Load` that re-binds `s`
        //   1× phx_gc_set_root after `Op::StringConcat`'s result
        //   1× phx_str_concat
        //   1× phx_print_str
        //   1× phx_gc_pop_frame
        // = 7 calls floor. Without shadow-stack rooting the fixture
        // would emit just 2 (`phx_str_concat` + `phx_print_str`), so a
        // floor of 5 — well above the no-shadow-stack baseline of 2 —
        // is a robust regression tripwire even if a future codegen
        // change drops one of the set_root sites.
        assert!(
            calls.len() >= 5,
            "ref-using `main` should contain at least 5 `Call` opcodes \
             (push_frame + ≥1 set_root + str_concat + print_str + \
             pop_frame); got {} (targets={calls:?}). A regression that \
             dropped the shadow-stack frame setup would collapse this \
             count back to the pre-shadow-stack baseline of 2 (concat \
             + print only).",
            calls.len(),
        );
        // Distinct-target tripwire: pre-shadow-stack code path would
        // call exactly 2 distinct functions (`phx_str_concat` and
        // `phx_print_str`). Shadow-stack rooting adds 3 more distinct
        // targets (push_frame, set_root, pop_frame). Asserting ≥ 3
        // distinct targets rules out the regression where the frame
        // machinery is silently elided for ref-using functions (e.g.
        // an `is_ref_type` predicate that started returning `false`
        // for `StringRef`).
        let mut distinct = calls.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() >= 3,
            "ref-using `main` should call at least 3 distinct functions \
             (phx_gc_push_frame, phx_gc_set_root, phx_gc_pop_frame are \
             three of them; phx_str_concat / phx_print_str add more); \
             got {} distinct targets ({distinct:?}). A regression that \
             collapsed shadow-stack rooting back to a no-op would land \
             near 2 distinct targets (concat + print).",
            distinct.len(),
        );
    });
}

/// End-to-end gate for a ref-typed function *return* — exercises
/// `translate_terminator`'s `Return(Some(v))` arm: a helper function
/// allocates a string, returns it, and `main` prints it. The wasmtime
/// run compared against the AST interpreter confirms that rooting the
/// return value across the callee's frame pop produces the expected
/// output.
///
/// **What this test does NOT pin.** It does not catch a swapped
/// pop-then-load vs. load-then-pop ordering inside `Return(Some(v))`,
/// because the current emit sequence has no GC-triggering op between
/// `emit_load_all(*v)` and `Return` (the load is `local.get` only).
/// Either order produces a behaviorally identical program today; a
/// dedicated structural assertion (e.g. "the `Call(phx_gc_pop_frame)`
/// opcode in `greet`'s body appears at a byte offset BEFORE any
/// `local.get` for the return value's slots") would be needed to pin
/// the ordering itself. The original comment overstated the
/// guarantee — leaving this note in so a future regression doesn't
/// look surprising when it slips past this test.
#[test]
fn ref_typed_return_runs_under_wasmtime() {
    let src = "function greet(name: String) -> String {\n  \
                 return \"hi, \" + name\n\
               }\n\
               function main() {\n  \
                 print(greet(\"world\"))\n\
               }\n";
    assert_wasm_matches_interp(src, "ref_typed_return");
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

/// `List<Int>` literal + length + for-loop iteration: PR 3d slice 1's
/// gate for `Op::ListAlloc`, `BuiltinCall("List.length")`, and
/// `BuiltinCall("List.get")`. The for-loop in Phoenix lowers to a
/// multi-block CFG with an i64 counter alloca, `List.length` once,
/// `List.get` per iteration, and an `ILt` against the cached length;
/// this fixture exercises the full trio plus the existing
/// loop+switch dispatcher. `List.length()` is also called as a
/// standalone method to pin the BuiltinCall path independent of the
/// for-loop's lowering.
#[test]
fn list_int_iteration_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [10, 20, 30]\n  \
                 print(xs.length())\n  \
                 for x in xs {\n    \
                   print(x)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_int_iteration");
}

/// Empty `List<Int>` literal: pins that `Op::ListAlloc` with zero
/// initial elements emits a valid `phx_list_alloc(elem_size, 0)`
/// call and that `phx_list_length` reads back zero. A for-loop over
/// the empty list would invoke `List.get` zero times — print just
/// the length so the test stays in slice 1's surface even if the
/// for-loop body were never reached on a real regression.
#[test]
fn empty_list_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = []\n  \
                 print(xs.length())\n\
               }\n";
    assert_wasm_matches_interp(src, "empty_list");
}

/// `List<String>` literal + for-loop iteration: pins the *multi-slot*
/// element path that the `List<Int>` fixtures never reach. A
/// `StringRef` element is an 8-byte fat pointer (i32 ptr + i32 len) on
/// wasm32, so `Op::ListAlloc` exercises the two-`i32.store` `StringRef`
/// arm of `emit_field_store` at a per-element `LIST_HEADER + i * 8`
/// offset, and `List.get` exercises the `ptr` + `len@+4` arm of
/// `emit_field_load`. The loaded string pointer is GC-managed, so this
/// also covers the blanket `emit_gc_set_root` rooting the `List.get`
/// result.
#[test]
fn list_string_iteration_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<String> = [\"alpha\", \"beta\", \"gamma\"]\n  \
                 print(xs.length())\n  \
                 for s in xs {\n    \
                   print(s)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_string_iteration");
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

/// Focused gate for `BuiltinCall("String.length")` lowering: the
/// receiver is a 2-slot `StringRef`, so this pins that both slots
/// (`[ptr, len]`) are pushed in declaration order into
/// `phx_str_length` and that the `i64` result flows back. Covers the
/// method on a literal, on a concatenation result (whose `len` is
/// computed at runtime rather than baked in), and on the empty string.
/// The backend matrix exercises `String.length` only transitively
/// (and only when `wasmtime` is on `$PATH`); this keeps the lowering
/// pinned independent of those fixtures.
#[test]
fn string_length_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let s: String = \"hello\"\n  \
                 print(s.length())\n  \
                 let joined: String = s + \", world\"\n  \
                 print(joined.length())\n  \
                 let empty: String = \"\"\n  \
                 print(empty.length())\n\
               }\n";
    assert_wasm_matches_interp(src, "string_length");
}

/// `defaults.phx` end-to-end gate for struct alloc + field access
/// (Counter struct) + StringConcat in a while loop + method dispatch
/// via `Op::Call(method_func_id)`. Combines every piece of the
/// surface in one fixture — a regression in `Op::StructAlloc` /
/// `StructGetField` / multi-slot Alloca / String concat would surface
/// here as either a wasmtime trap or a stdout mismatch.
const DEFAULTS_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/defaults.phx"
));

#[test]
fn defaults_runs_under_wasmtime() {
    assert_wasm_matches_interp(DEFAULTS_SOURCE, "defaults");
}

/// `features.phx` end-to-end gate for enum alloc + discriminant +
/// variant-field access (Shape) + struct alloc with method dispatch
/// (Point) + match (lowered to chained Branch terminators) + Float
/// constants and storage. Exercises the concrete-typed enum-variant
/// path (Shape's Circle(Float) and Rect(Float, Float) have no
/// placeholders) so the layout-shared-across-variants invariant is
/// pinned.
const FEATURES_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/features.phx"
));

#[test]
fn features_runs_under_wasmtime() {
    assert_wasm_matches_interp(FEATURES_SOURCE, "features");
}

/// `alloc_loop.phx` — first real GC-pressure stress test under
/// wasmtime. 100k iterations, each allocating a 3-element `List<Int>`
/// plus a `toString` + `StringConcat` chain producing a fresh heap
/// string; cumulative allocation ≈ 8 MB which comfortably exceeds the
/// runtime's 1 MB auto-collect threshold so the threshold-driven
/// sweep path runs many times during the test rather than not at all.
///
/// **This is what PR 3c slice 4's shadow-stack root emission gates.**
/// Without precise rooting, the GC's conservative scan happens to
/// keep current fixtures alive — but a 100k-allocation loop forces
/// the sweep through every register/local boundary, so any missed
/// root would surface as either:
///
/// - Wasmtime trap on a use-after-free (sweep zeroed a still-live
///   heap pointer, subsequent load reads through the dangling
///   reference).
/// - OOM (no sweep ever frees the per-iteration allocs because they
///   *all* root as conservative-scan hits in the data segment).
/// - Wrong sum (the `total` accumulator gets clobbered).
///
/// Expected stdout: `300000` (100000 iterations × 3 elements per list).
const ALLOC_LOOP_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/alloc_loop.phx"
));

#[test]
fn alloc_loop_runs_under_wasmtime() {
    assert_wasm_matches_interp(ALLOC_LOOP_SOURCE, "alloc_loop");
}

/// `closures.phx` — gate for `Op::ClosureAlloc`, `Op::CallIndirect`,
/// `Op::ClosureLoadCapture`. Exercises:
///
/// - A capture-less closure (`let double = function(x: Int) -> Int { x * 2 }`)
///   passed to `applyTwice(f, x)` as a function-typed parameter,
///   reached via `Op::CallIndirect` from inside `applyTwice`.
/// - A `String`-capturing closure (`let say = function(name) -> { prefix + name }`)
///   where `prefix` is a multi-slot `StringRef` capture, requiring
///   the alloc-side `StringRef` two-`i32.store` path plus the load
///   side's two-`i32.load` `emit_field_load` arm to land at the
///   matching computed offset (4-byte fn-idx + natural alignment).
/// - The env-pointer calling convention: the closure pointer arrives
///   as the WASM function's first param and `Op::ClosureLoadCapture`
///   reads from it.
/// - A closure capturing BOTH an `Int` and a `String` (`combine`),
///   exercising non-trivial capture-layout alignment: the 8-byte-
///   aligned `Int` forces `align_up` past the 4-byte fn-table-idx,
///   and the two-capture record drives the offset-accumulation walk
///   in `capture_offset` / the `Op::ClosureAlloc` arm beyond the
///   single-already-aligned-capture case the other closures cover.
///
/// Expected stdout:
///   result: alice
///   result: bob
///   12
///   combined: 15
const CLOSURES_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/closures.phx"
));

#[test]
fn closures_runs_under_wasmtime() {
    assert_wasm_matches_interp(CLOSURES_SOURCE, "closures");
}

/// `List.map` → `List.filter` → `List.reduce` chain: PR 3d slice 6's
/// gate for the single-loop functional methods. Each lowers to an
/// inline WASM `block`/`loop` with a `call_indirect` per element:
///
/// - `map` allocates an output list sized to the input, applies the
///   closure per element, stores results at matching indices.
/// - `filter` allocates an output list (capacity = input length),
///   writes matching elements contiguously, then patches the output
///   list's i64 length field to the kept count.
/// - `reduce` folds with the accumulator held in the result vid's
///   locals (seeded from `init`), re-rooted on the shadow stack each
///   iteration.
///
/// The `for x in evens` loop afterward confirms the filtered list's
/// length field was patched correctly (iteration reads the patched
/// length, not the over-allocated capacity).
///
/// `xs = [1,2,3,4,5]` → `doubled = [2,4,6,8,10]` →
/// `evens (>4) = [6,8,10]` → `sum = 24`.
///
/// Expected stdout:
///   24
///   6
///   8
///   10
#[test]
fn list_map_filter_reduce_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [1, 2, 3, 4, 5]\n  \
                 let doubled = xs.map(function(x: Int) -> Int { x * 2 })\n  \
                 let evens = doubled.filter(function(x: Int) -> Bool { x > 4 })\n  \
                 let sum = evens.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })\n  \
                 print(sum)\n  \
                 for x in evens {\n    \
                   print(x)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_map_filter_reduce");
}

/// `List.flatMap` — gate for the nested-loop functional method.
/// Each element's closure returns a `List<Int>`; the results are
/// concatenated into one output list via repeated
/// `phx_list_push_raw` (immutable-append: each push allocates a fresh
/// list, so the output pointer is re-stored and re-rooted every
/// iteration). The closure here *allocates* (`[n, n * 10]` builds a
/// fresh list per element), so the inner list returned by the closure
/// must stay rooted in its ad-hoc shadow frame across the push loop's
/// allocations. This test pins only the *functional* result — three
/// elements never approach the 1 MB auto-collect threshold, so a
/// regression that dropped the inner-list root would still pass it. The
/// presence of that root is gated deterministically by
/// `list_flatmap_emits_adhoc_shadow_frame` below, which pins the ad-hoc
/// shadow frame structurally (the runtime tier can't reliably surface
/// the use-after-free — see that test's comment for why).
///
/// `[1,2,3].flatMap(\n -> [n, n*10])` → `[1,10,2,20,3,30]`.
///
/// Expected stdout:
///   1
///   10
///   2
///   20
///   3
///   30
#[test]
fn list_flatmap_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let nested: List<Int> = [1, 2, 3]\n  \
                 let flat = nested.flatMap(function(n: Int) -> List<Int> { [n, n * 10] })\n  \
                 for x in flat {\n    \
                   print(x)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_flatmap");
}

/// `List.sortBy` — gate for the stable insertion-sort lowering with a
/// custom nested `block`/`loop` (decreasing inner counter `j`, compound
/// stop condition `j < 0 || cmp(copy[j], key) <= 0`). The comparator
/// `a - b` gives ascending order; the unsorted input with a duplicate
/// (`1` appears twice) confirms the sort is total and stable (the two
/// `1`s are indistinguishable, so output is deterministic either way).
///
/// `[3,1,4,1,5,9,2,6].sortBy(\a b -> a - b)` → `[1,1,2,3,4,5,6,9]`.
#[test]
fn list_sortby_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let unsorted: List<Int> = [3, 1, 4, 1, 5, 9, 2, 6]\n  \
                 let sorted = unsorted.sortBy(function(a: Int, b: Int) -> Int { a - b })\n  \
                 for x in sorted {\n    \
                   print(x)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_sortby");
}

/// `List.sortBy` at **n=50** — pin correctness past the 8-element
/// fixture above. A buggy inner-loop termination (off-by-one,
/// signed/unsigned mix on `j < 0`, wrong `LOOP_BREAK` depth) that
/// happened to give the right answer for small inputs would diverge
/// from the interpreter's merge sort here. The input is the LCG
/// `x_{i+1} = (1103515245 · x_i + 12345) mod 2^15` seeded at 42 —
/// chosen so the test source is self-contained (no large literal
/// list) and the sequence is deterministic across runs.
///
/// The byte-identical match against the interpreter is the assertion:
/// wasm32's insertion sort and the interpreter's merge sort are both
/// stable, so they must agree on the full sorted output (including
/// duplicates, which the LCG produces).
#[test]
fn list_sortby_n50_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let mut xs: List<Int> = []\n  \
                 let mut seed: Int = 42\n  \
                 let mut i: Int = 0\n  \
                 while i < 50 {\n    \
                   seed = (1103515245 * seed + 12345) % 32768\n    \
                   xs = xs.push(seed)\n    \
                   i = i + 1\n  \
                 }\n  \
                 let sorted = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })\n  \
                 for x in sorted {\n    \
                   print(x)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_sortby_n50");
}

/// Structural gate for sortBy's ref-element ad-hoc `key` frame:
/// `phx_main` must emit exactly two `phx_gc_push_frame` calls (the
/// function-level frame + sortBy's `key` frame). Push/pop indices come
/// from `calls.first()` / `calls.last()` (`setup_gc_frame` invariant);
/// the `key` frame is the unique `push_idx` preceded by `I32Const(1)`.
#[test]
fn list_sortby_ref_elem_emits_adhoc_shadow_frame() {
    let src = "function main() {\n  \
                 let nested: List<List<Int>> = [[3, 0], [1, 0], [2, 0]]\n  \
                 let sorted = nested.sortBy(function(a: List<Int>, b: List<Int>) -> Int { a.get(0) - b.get(0) })\n  \
                 print(sorted.length())\n\
               }\n";
    compile_or_skip(src, "list_sortby_ref_adhoc_frame", |bytes| {
        let calls_with_const = call_targets_with_const_in_phx_main(bytes)
            .unwrap_or_else(|e| panic!("locating phx_main and decoding its calls failed: {e}"));
        let calls: Vec<u32> = calls_with_const.iter().map(|(t, _)| *t).collect();
        // Brittle invariant: the function-level `phx_gc_push_frame` is
        // assumed to be the **first** Call in `phx_main` and its
        // matching `pop_frame` the **last**. That holds today because
        // the prologue / epilogue bracket every other runtime call, but
        // a future codegen change that inserted a different Call ahead
        // of the prologue push would mis-identify `push_idx` here. The
        // `pushes_with_size_1 == 1` follow-up assertion below is what
        // would catch such a mis-identification — the wrong index
        // wouldn't be preceded by an `I32Const(1)` in this fixture,
        // since sortBy's ad-hoc push is the only size-1 push.
        let push_idx = *calls
            .first()
            .expect("sortBy `main` should emit at least one Call (phx_gc_push_frame)");
        let pop_idx = *calls
            .last()
            .expect("sortBy `main` should emit at least one Call (phx_gc_pop_frame)");
        assert_ne!(
            push_idx, pop_idx,
            "first and last `Call` in `phx_main` should differ (push_frame ≠ pop_frame); \
             both came back as {push_idx}. calls={calls:?}",
        );
        let push_count = calls.iter().filter(|&&t| t == push_idx).count();
        let pop_count = calls.iter().filter(|&&t| t == pop_idx).count();
        assert_eq!(
            push_count, pop_count,
            "phx_gc_push_frame (idx {push_idx}) and phx_gc_pop_frame (idx {pop_idx}) must \
             appear an equal number of times in `phx_main` — got push={push_count}, \
             pop={pop_count}. A mismatch means sortBy's ad-hoc key frame push/pop pair is \
             unbalanced (the runtime's frame counter would drift). calls={calls:?}",
        );
        assert_eq!(
            push_count, 2,
            "sortBy `main` with a ref-typed element should emit exactly two \
             phx_gc_push_frame calls — the function-level frame plus sortBy's ad-hoc \
             `key` frame. Got {push_count}: a count of 1 means the frame that roots the \
             `key` element across the comparator call was dropped (a latent \
             use-after-free under GC pressure). calls={calls:?}",
        );

        // Confirm we really identified `phx_gc_push_frame` (not some
        // unrelated `Call` that happened to be the first opcode): the
        // ad-hoc `key` frame uses `frame_size = 1`, so exactly one of
        // the two `push_idx` calls must be preceded by `I32Const(1)`.
        // A future codegen change that put a different `Call` first
        // (mis-identifying push_idx) would fail this — the wrong index
        // wouldn't have a matching `I32Const(1)` predecessor at all,
        // since sortBy's ad-hoc push is the *only* size-1 push in this
        // fixture.
        let pushes_with_size_1 = calls_with_const
            .iter()
            .filter(|(t, c)| *t == push_idx && *c == Some(1))
            .count();
        assert_eq!(
            pushes_with_size_1, 1,
            "expected exactly one `phx_gc_push_frame(1)` site (sortBy's ad-hoc `key` frame); \
             got {pushes_with_size_1}. Either push_idx is not actually phx_gc_push_frame \
             (a different `Call` is now the first in the body), or sortBy stopped emitting \
             the size-1 ad-hoc frame. calls_with_const={calls_with_const:?}",
        );
    });
}

/// `List<String>.sortBy` — sortBy over a 2-slot ref element. Pins
/// that the multi-slot key load/store round-trip both slots of the fat
/// pointer faithfully and that the ad-hoc frame survives an allocating
/// comparator call. Comparator returns `0` so the inner shift body
/// never fires (companion test below covers the shift path).
#[test]
fn list_string_sortby_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<String> = [\"banana\", \"fig\", \"cherry\", \"date\"]\n  \
                 let sorted = xs.sortBy(function(a: String, b: String) -> Int {\n    \
                   let _scratch: String = a + \"_\" + b\n    \
                   0\n  \
                 })\n  \
                 for s in sorted {\n    \
                   print(s)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_string_sortby");
}

/// `List<String>.sortBy` 2-slot shift path, under a real total order.
/// The comparator sorts by `String.length()` and the input is in
/// reverse length order (longest first), so every element inserts ahead
/// of all its predecessors — forcing the maximum number of shifts and
/// exercising the multi-slot key store across each one.
///
/// Unlike a constant-`1` comparator, `a.length() - b.length()` is a
/// genuine total order over these distinct-length inputs, so sortBy's
/// "byte-identical to interp for any total order" guarantee actually
/// covers this test rather than passing by coincidence of matching sort
/// algorithms. It also pins `BuiltinCall("String.length")` lowering
/// inside a closure body on wasm32-linear.
#[test]
fn list_string_sortby_shifts_runs_under_wasmtime() {
    // Lengths: elderberry=10, banana=6, apple=5, kiwi=4, fig=3 — all
    // distinct and in descending order, so the ascending-by-length sort
    // reverses the list and every insertion shifts every prior element.
    let src = "function main() {\n  \
                 let xs: List<String> = [\"elderberry\", \"banana\", \"apple\", \"kiwi\", \"fig\"]\n  \
                 let sorted = xs.sortBy(function(a: String, b: String) -> Int {\n    \
                   a.length() - b.length()\n  \
                 })\n  \
                 for s in sorted {\n    \
                   print(s)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_string_sortby_shifts");
}

/// `collections.phx` end-to-end — the full functional-collections
/// fixture: `map` → `filter` → `reduce`, `sortBy`, `flatMap`, plus a
/// `Map` literal with `length` / `contains` / `remove`. This is the
/// fixture PR 3d's list/map/closure slices were built toward; it
/// passing byte-identical with the interpreter confirms the whole
/// functional-collections surface composes.
const COLLECTIONS_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/collections.phx"
));

#[test]
fn collections_runs_under_wasmtime() {
    assert_wasm_matches_interp(COLLECTIONS_SOURCE, "collections");
}

/// Deterministic gate for `flatMap`'s inner-list rooting — the part the
/// functional test above can't pin. The closure's per-element inner
/// list has no IR `ValueId`, so it's rooted in a dedicated *ad-hoc*
/// shadow frame (`emit_gc_push_frame_at` / `emit_gc_set_root_at` /
/// `emit_gc_pop_frame_at`) held across the push loop's allocations.
///
/// An end-to-end "run it under GC pressure" test can't reliably catch a
/// regression that drops this frame: the runtime is mark-sweep over the
/// *system* allocator with no freed-memory poisoning (see
/// `gc/heap.rs::sweep_phase`'s bare `dealloc`), and within one inner
/// push loop the only intervening allocations are the *growing* output
/// buffers — never the inner list's size — so a swept-but-unrooted
/// inner list is never reused/clobbered before its remaining elements
/// are copied. The use-after-free reads stale-but-intact bytes and the
/// program prints the right answer anyway. (Confirmed empirically: with
/// the `emit_gc_set_root_at` call commented out, a 30k-iteration
/// flatMap loop forcing ~12 collections still matched the interpreter.)
///
/// So we pin the frame *structurally* instead, exactly as
/// [`string_mut_fixture_emits_shadow_stack_frame_calls`] pins the
/// function-level frame. `phx_main` here emits two `phx_gc_push_frame`
/// calls — the function-level frame (always the first `Call`, per
/// `setup_gc_frame`) plus `flatMap`'s ad-hoc inner-list frame — and the
/// two matching `phx_gc_pop_frame` calls (the function-level pop is the
/// last `Call`). A regression that dropped the ad-hoc frame collapses
/// `push_count` from 2 to 1; the balance check catches a dropped pop.
/// `flatMap` is currently the only construct that emits a nested frame,
/// so the count of 2 is unambiguous.
#[test]
fn list_flatmap_emits_adhoc_shadow_frame() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [1, 2, 3]\n  \
                 let flat = xs.flatMap(function(n: Int) -> List<Int> { [n, n * 10] })\n  \
                 print(flat.length())\n\
               }\n";
    compile_or_skip(src, "list_flatmap_adhoc_frame", |bytes| {
        let calls = call_targets_in_phx_main(bytes)
            .unwrap_or_else(|e| panic!("locating phx_main and decoding its calls failed: {e}"));
        // Same structural anchoring as the string-mut fixture: the
        // function-level `phx_gc_push_frame` is the first `Call` and the
        // function-level `phx_gc_pop_frame` is the last. Both ad-hoc
        // frame calls resolve to those same two runtime function
        // indices, so counting occurrences of each tallies *all* the
        // pushes/pops in the body without name resolution (the merger
        // strips the runtime's Function name subsection).
        let push_idx = *calls
            .first()
            .expect("flatMap `main` should emit at least one Call (phx_gc_push_frame)");
        let pop_idx = *calls
            .last()
            .expect("flatMap `main` should emit at least one Call (phx_gc_pop_frame)");
        assert_ne!(
            push_idx, pop_idx,
            "first and last `Call` in `phx_main` should differ (push_frame ≠ pop_frame); \
             both came back as {push_idx}. calls={calls:?}",
        );
        let push_count = calls.iter().filter(|&&t| t == push_idx).count();
        let pop_count = calls.iter().filter(|&&t| t == pop_idx).count();
        assert_eq!(
            push_count, pop_count,
            "phx_gc_push_frame (idx {push_idx}) and phx_gc_pop_frame (idx {pop_idx}) must \
             appear an equal number of times in `phx_main` — got push={push_count}, \
             pop={pop_count}. A mismatch means flatMap's ad-hoc frame push/pop pair is \
             unbalanced (the runtime's frame counter would drift). calls={calls:?}",
        );
        assert_eq!(
            push_count, 2,
            "flatMap `main` should emit exactly two phx_gc_push_frame calls — the \
             function-level frame plus flatMap's ad-hoc inner-list frame. Got \
             {push_count}: a count of 1 means the ad-hoc frame that roots the closure's \
             per-element inner list across the push loop was dropped (a latent \
             use-after-free under GC pressure that the runtime tier can't reliably \
             surface). calls={calls:?}",
        );
    });
}

/// `List.reduce` building a `String` accumulator — exercises the
/// ref-typed-accumulator rooting path. Each `acc + x` allocates a
/// fresh heap string via `phx_str_concat`; the running accumulator
/// must stay rooted on the shadow stack (via the result vid's slot,
/// re-rooted each iteration) across those allocations. A regression
/// that dropped the re-root would risk the accumulator being swept
/// mid-fold under GC pressure.
///
/// `["a","b","c"].reduce("", \acc x -> acc + x)` → `"abc"`.
#[test]
fn list_reduce_string_acc_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<String> = [\"a\", \"b\", \"c\"]\n  \
                 let joined = xs.reduce(\"\", function(acc: String, x: String) -> String { acc + x })\n  \
                 print(joined)\n\
               }\n";
    assert_wasm_matches_interp(src, "list_reduce_string_acc");
}

/// `List.map` / `List.filter` over `String` (fat-pointer) elements —
/// the ref-typed-*element* counterpart to the ref-typed-*accumulator*
/// coverage in [`list_reduce_string_acc_runs_under_wasmtime`]. Where
/// the `Int` chain test exercises single-slot element load/store, this
/// drives the 2-slot `StringRef` path through both loops:
///
/// - `map(s -> s + "!")` loads each input element via the 2-slot
///   `emit_field_load`, runs a closure that **allocates** a fresh heap
///   string (`phx_str_concat`) every iteration, and stores the result
///   via the 2-slot `emit_field_store`. Because the output list is
///   filled progressively (index 0, 1, 2, …) while each closure call
///   allocates, a GC triggered mid-loop scans the rooted output list
///   when it is *partially* filled — slots `[0, i]` hold live fat
///   pointers and `[i+1, len)` are still the zeroed bytes from
///   `phx_list_alloc`. This is the partial-fill fat-pointer scan path;
///   a regression that dropped the output-list root (or mis-sized the
///   element store) would surface as a swept/garbled string here.
/// - `filter(_ -> false)` keeps nothing: `out_count` stays 0, the
///   length field is patched to 0, and the over-allocated capacity
///   slots remain zeroed (GC-safe null fat pointers).
/// - `filter(_ -> true)` keeps everything: `out_count == capacity`,
///   exercising the 2-slot `StringRef` store for every element with no
///   trailing slack.
///
/// The WASM backend does not lower `String` equality or `String`
/// methods yet, so the predicate can't vary on element *content* — a
/// genuinely partial `filter` of strings waits on that slice. The
/// keep-none / keep-all pair plus `map`'s progressive fill still cover
/// every fat-pointer element path the loops emit today.
///
/// `["a","bb","ccc"]` → `map` → `["a!","bb!","ccc!"]`;
/// `filter(false)` → `[]` (length 0); `filter(true)` → `["a","bb","ccc"]`.
///
/// Expected stdout:
///   a!
///   bb!
///   ccc!
///   0
///   a
///   bb
///   ccc
#[test]
fn list_map_filter_string_elems_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let words: List<String> = [\"a\", \"bb\", \"ccc\"]\n  \
                 let shouted = words.map(function(s: String) -> String { s + \"!\" })\n  \
                 for w in shouted {\n    \
                   print(w)\n  \
                 }\n  \
                 let none = words.filter(function(s: String) -> Bool { false })\n  \
                 print(none.length())\n  \
                 let all = words.filter(function(s: String) -> Bool { true })\n  \
                 for w in all {\n    \
                   print(w)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_map_filter_string_elems");
}

/// Empty-list and `filter` boundary coverage for the single-loop
/// functional methods. The other tests only hit the genuinely-partial
/// case (`[6,8,10]` kept out of five); this pins the degenerate ends:
///
/// - **Empty input** (`[]`): `map` / `filter` / `reduce` each take the
///   zero-iteration path — the `i >= len` guard fails on the first
///   check, so the loop body never runs. `map` / `filter` allocate a
///   zero-length output list; `reduce` returns its seed (`100`)
///   untouched. A guard that ran one iteration on an empty list (e.g.
///   `i > len` instead of `i >= len`, or a signedness slip) would read
///   out of bounds and diverge from the interp here.
/// - **`filter` keeps nothing** (`x > 100`): `out_count` stays 0 and
///   the length field is patched to 0.
/// - **`filter` keeps everything** (`x > 0`): `out_count == capacity`,
///   so the contiguous-write fills the output list exactly with no
///   length shrink.
///
/// `[]` → all empty (lengths 0, 0; reduce seed 100);
/// `[1,2,3]` → `filter(>100)` → `[]` (length 0);
/// `[1,2,3]` → `filter(>0)` → `[1,2,3]` (length 3).
///
/// Expected stdout:
///   0
///   0
///   100
///   0
///   3
///   1
///   2
///   3
#[test]
fn list_methods_empty_and_filter_bounds_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let empty: List<Int> = []\n  \
                 let em = empty.map(function(x: Int) -> Int { x + 1 })\n  \
                 let ef = empty.filter(function(x: Int) -> Bool { x > 0 })\n  \
                 let es = empty.reduce(100, function(acc: Int, x: Int) -> Int { acc + x })\n  \
                 print(em.length())\n  \
                 print(ef.length())\n  \
                 print(es)\n  \
                 let nums: List<Int> = [1, 2, 3]\n  \
                 let keepNone = nums.filter(function(x: Int) -> Bool { x > 100 })\n  \
                 let keepAll = nums.filter(function(x: Int) -> Bool { x > 0 })\n  \
                 print(keepNone.length())\n  \
                 print(keepAll.length())\n  \
                 for x in keepAll {\n    \
                   print(x)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_methods_empty_and_filter_bounds");
}

/// `List.map` that **changes element width** — `Int` (8-byte `i64`
/// stride) → `Bool` (4-byte `i32` stride). Every other map test is
/// width-preserving (`Int`→`Int`, `String`→`String`), so a bug that
/// addressed the *output* store with the *input* element size (or vice
/// versa) would pass them all. Here the input is read at stride 8 while
/// the result is written at stride 4: `translate_list_map` allocates
/// the output list with `out_elem_size` and tracks `in_elem_size` /
/// `out_elem_size` separately across the two `emit_list_elem_addr`
/// calls. The trailing `for b in bigs` reads the output list back at
/// its own header-recorded element size (4), so any stride mismatch
/// surfaces as garbled bools or an out-of-bounds trap rather than a
/// silently-correct result.
///
/// `[1,2,3,4].map(x -> x > 2)` → `[false, false, true, true]`.
///
/// Expected stdout:
///   false
///   false
///   true
///   true
#[test]
fn list_map_changes_elem_width_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let nums: List<Int> = [1, 2, 3, 4]\n  \
                 let bigs = nums.map(function(x: Int) -> Bool { x > 2 })\n  \
                 for b in bigs {\n    \
                   print(b)\n  \
                 }\n\
               }\n";
    assert_wasm_matches_interp(src, "list_map_changes_elem_width");
}

/// `closures_over_generic.phx` — gate for closures defined inside a
/// generic function. The closure `makeBox<T>` captures `x: T`; after
/// monomorphization to `makeBox<Int>`, the inner closure function is
/// *shared* (not cloned per specialization), so its declared
/// `capture_types` retains an unsubstituted `TypeVar("T")`.
///
/// The WASM backend resolves this exactly as the native backend does:
///
/// - `Op::ClosureAlloc` derives the capture layout from the *alloc-
///   site value types* (the capture vids' bindings in the enclosing
///   *monomorphized* `makeBox<Int>`, which are concrete `Int`), not
///   from the shared closure's declared `capture_types`.
/// - `Op::ClosureLoadCapture` derives the load offset using the
///   instruction's substituted `result_type` (concrete `Int`) as the
///   target capture type, walking `current_capture_types` only for
///   the preceding captures (empty for the single-capture shape here).
///
/// A regression that reverted either side to reading the closure's
/// declared `capture_types` would fail compilation with a `TypeVar`
/// reaching `phx_field_align_bytes`.
///
/// Expected stdout:
///   42
///   12
const CLOSURES_OVER_GENERIC_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/closures_over_generic.phx"
));

#[test]
fn closures_over_generic_runs_under_wasmtime() {
    assert_wasm_matches_interp(CLOSURES_OVER_GENERIC_SOURCE, "closures_over_generic");
}

/// `Map<String, Int>` literal + length + contains + remove: PR 3d
/// slice 4's gate for `Op::MapAlloc`, `BuiltinCall("Map.length")`,
/// `BuiltinCall("Map.contains")`, and `BuiltinCall("Map.remove")`.
/// The fixture exercises:
///
/// - `Op::MapAlloc` with three `(String, Int)` pairs — uses the WASM
///   shadow-stack pair-buffer staging dance: reserve `3 * (ks + vs)
///   = 48` bytes, write keys (multi-slot `StringRef`) and values
///   (`i64`) at densely packed offsets, then call
///   `phx_map_from_pairs`.
/// - `Map.contains` with a key buffer staged on the shadow stack —
///   write the `String` key into a 8-byte frame, pass pointer + size
///   to `phx_map_contains`, restore SP after.
/// - `Map.remove` returning a fresh map (`m2`) — same staging shape
///   as `contains`, but the result is a GC-tracked map pointer rooted
///   by the blanket post-instruction `emit_gc_set_root`.
///
/// Pinned to fixed scalars (counts) rather than iterating the map's
/// contents so the test stays order-insensitive — `Map` iteration
/// order is unspecified and would diverge from the interpreter on
/// any rebuild.
#[test]
fn map_basic_ops_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<String, Int> = {\"alice\": 1, \"bob\": 2, \"carol\": 3}\n  \
                 print(m.length())\n  \
                 print(m.contains(\"bob\"))\n  \
                 print(m.contains(\"dan\"))\n  \
                 let m2 = m.remove(\"bob\")\n  \
                 print(m2.length())\n  \
                 print(m2.contains(\"bob\"))\n\
               }\n";
    assert_wasm_matches_interp(src, "map_basic_ops");
}

/// Empty `Map<String, Int>` literal: pins the `n_pairs == 0` carve-
/// out path in `Op::MapAlloc` — codegen skips the stack-buffer
/// reservation entirely and passes a null `pair_data` pointer, per
/// the runtime's documented safety contract. The runtime returns a
/// fresh empty map; `Map.length` reads back zero.
#[test]
fn empty_map_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<String, Int> = {}\n  \
                 print(m.length())\n\
               }\n";
    assert_wasm_matches_interp(src, "empty_map");
}

/// `Map.keys` / `Map.values` returning fresh `List<K>` / `List<V>`.
/// Iterating the keys list directly would be order-sensitive (Map
/// iteration order is unspecified), so this fixture only pins
/// `.length()` on the returned lists — the keys/values pointer must
/// be a valid list whose length matches the source map's.
#[test]
fn map_keys_values_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<String, Int> = {\"a\": 1, \"b\": 2}\n  \
                 let ks: List<String> = m.keys()\n  \
                 let vs: List<Int> = m.values()\n  \
                 print(ks.length())\n  \
                 print(vs.length())\n\
               }\n";
    assert_wasm_matches_interp(src, "map_keys_values");
}

/// `Map<String, String>` literal: exercises the multi-slot `StringRef`
/// *value* store inside `Op::MapAlloc`'s densely-packed pair buffer.
/// The `Map<String, Int>` fixtures above only write single-slot i64
/// values; here each value writes a fat pointer instead (`ptr` at
/// `pair_off + ks`, `len` at `pair_off + ks + 4`). A wrong value
/// offset or size would clobber the adjacent pair's key bytes, so
/// pinning `.length()` plus `.contains()` on every key (and a missing
/// key) catches buffer-integrity regressions even though the value
/// bytes themselves can't be read back by key yet (`Map.get` ships in
/// a later slice). `.values().length()` confirms the value list builds.
///
/// Values are deliberately different lengths (`"x"` / `"yy"` / `"zzz"`)
/// so a stale or fixed-width `len` slot would diverge from the
/// interpreter. Order-insensitive by construction — no iteration over
/// the map's contents, since `Map` iteration order is unspecified.
#[test]
fn map_string_values_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<String, String> = {\"alice\": \"x\", \"bob\": \"yy\", \"carol\": \"zzz\"}\n  \
                 print(m.length())\n  \
                 print(m.contains(\"alice\"))\n  \
                 print(m.contains(\"bob\"))\n  \
                 print(m.contains(\"carol\"))\n  \
                 print(m.contains(\"dan\"))\n  \
                 let vs: List<String> = m.values()\n  \
                 print(vs.length())\n\
               }\n";
    assert_wasm_matches_interp(src, "map_string_values");
}

/// `Map<String, Int>.contains` pins string-content comparison on
/// wasm32. The lookup key is built via a helper so it's a fresh heap
/// allocation distinct from the literal in the map — pointer-identity
/// comparison (the only thing the size fallback could give on wasm32,
/// where `StringRef` is 8 bytes) would answer `false`. Routing through
/// a function call also defends against future string interning /
/// constant folding. Counterpart to
/// [`list_string_contains_by_content_runs_under_wasmtime`].
#[test]
fn map_string_keys_contains_by_content_runs_under_wasmtime() {
    let src = "function tail(prefix: String) -> String { prefix + \"ob\" }\n\
               function tail2(prefix: String) -> String { prefix + \"an\" }\n\
               function main() {\n  \
                 let m: Map<String, Int> = {\"alice\": 1, \"bob\": 2, \"carol\": 3}\n  \
                 let needle_hit: String = tail(\"b\")\n  \
                 let needle_miss: String = tail2(\"d\")\n  \
                 print(m.contains(needle_hit))\n  \
                 print(m.contains(needle_miss))\n\
               }\n";
    assert_wasm_matches_interp(src, "map_string_keys_contains_by_content");
}

/// `defer_try.phx` — gate for `Result.isErr` in conditional dispatch
/// plus a `defer` that fires on the early-`?`-return path. The fixture
/// exercises:
///
/// - `Op::EnumAlloc` with stdlib-`Result` (Ok/Err variants).
/// - `BuiltinCall("Result.isErr")` returning a Bool from a
///   discriminant-equality check.
/// - The `?` operator's early-return lowering — `lower_try` emits a
///   discriminant branch, payload extraction on the Ok arm, and a
///   `Terminator::Return` of the original Err value on the Err arm.
/// - `defer print("cleanup")` from the entry block runs before the
///   early Return fires (the defer pre-linearization pass inserts
///   the print before *every* exit terminator).
///
/// Expected stdout:
///   cleanup
///   failed
const DEFER_TRY_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/defer_try.phx"
));

#[test]
fn defer_try_runs_under_wasmtime() {
    assert_wasm_matches_interp(DEFER_TRY_SOURCE, "defer_try");
}

/// `enum_predicates.phx` — exhaustive gate for all four stdlib-enum
/// discriminant predicates (`Result.isOk` / `isErr`, `Option.isSome` /
/// `isNone`). `defer_try` only exercises `isErr` (the `i32.ne`
/// negative branch); this fixture prints every predicate against both
/// variants of its enum, so `translate_enum_is_variant_builtin`'s
/// `i32.eq` positive branch (`isOk` / `isSome`) and the
/// "positive variant ⇒ discriminant 0" invariant — which the wasm path
/// hard-codes rather than deriving from a layout — are both pinned.
///
/// Expected stdout (per the fixture): the eight booleans
///   true false false true   (Result: ok/err × isOk/isErr)
///   true false false true   (Option: some/none × isSome/isNone)
const ENUM_PREDICATES_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/enum_predicates.phx"
));

#[test]
fn enum_predicates_runs_under_wasmtime() {
    assert_wasm_matches_interp(ENUM_PREDICATES_SOURCE, "enum_predicates");
}

/// Focused gate for the multi-slot `StringRef` field path inside
/// `Op::StructAlloc` / `Op::StructGetField`. `defaults.phx` and
/// `features.phx` exercise struct-with-Int-field codegen, but neither
/// constructs a struct whose declared field type is `String`. A
/// regression in `emit_field_store`'s two-`i32.store` `StringRef` arm
/// (or in `emit_field_load`'s symmetric two-`i32.load` arm) would
/// store/read only the `ptr` half of the fat pointer and miscompile
/// silently — this test pins both halves by routing a literal through
/// alloc, then read-back, then `print(String)`.
#[test]
fn struct_with_string_field_roundtrips() {
    // `print(p.name)` exercises field-load → multi-slot result binding
    // → `phx_print_str(ptr, len)`. `print(p.value)` rules out a
    // field-order miscompile by pinning the Int field after the String
    // — a swapped offset/size would surface as a wrong integer.
    let src = "struct Pair {\n  String name\n  Int value\n}\n\
               function main() {\n  \
                 let p = Pair(\"hello\", 42)\n  \
                 print(p.name)\n  \
                 print(p.value)\n\
               }\n";
    assert_wasm_matches_interp(src, "struct_with_string_field");
}

/// Focused gate for `Op::StructSetField`. Phoenix source today
/// reaches `StructAlloc` + `StructGetField` from `defaults.phx` /
/// `features.phx` / `struct_with_string_field_roundtrips`, but no
/// existing fixture emits `Op::StructSetField` directly — so a
/// regression in `emit_field_store`'s non-StringRef arm at a
/// non-initial offset would only surface once a future fixture
/// landed. Hand-build the op shape to pin the path now.
#[test]
fn struct_set_field_compiles_and_validates() {
    // Build `struct Pair { Int a; Int b }` and a `main` that
    // allocates a Pair, overwrites field 0, then reads it back so
    // both store and load are wired through StructSetField/GetField.
    let mut module = IrModule::new();
    module.struct_layouts.insert(
        "Pair".to_string(),
        vec![
            ("a".to_string(), IrType::I64),
            ("b".to_string(), IrType::I64),
        ],
    );

    let mut f = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = f.create_block();
    let v_a = f.emit_value(entry, Op::ConstI64(1), IrType::I64, None);
    let v_b = f.emit_value(entry, Op::ConstI64(2), IrType::I64, None);
    let pair = f.emit_value(
        entry,
        Op::StructAlloc("Pair".to_string(), vec![v_a, v_b]),
        IrType::StructRef("Pair".to_string(), Vec::new()),
        None,
    );
    let v_new = f.emit_value(entry, Op::ConstI64(99), IrType::I64, None);
    // The op under test: overwrite field 0.
    f.emit(
        entry,
        Op::StructSetField(pair, 0, v_new),
        IrType::Void,
        None,
    );
    // Read it back so the store isn't dead-code-eliminated by any
    // future pass.
    let _ = f.emit_value(entry, Op::StructGetField(pair, 0), IrType::I64, None);
    f.set_terminator(entry, Terminator::Return(None));
    module.push_concrete(f);

    compile_ir_or_skip(&module, "struct_set_field", |bytes| {
        wasmparser::validate(bytes)
            .unwrap_or_else(|e| panic!("wasmparser rejected the StructSetField module: {e}"));
    });
}

/// Focused gate for `Op::ConstF64` codegen: a Float literal must
/// emit an `f64.const` instruction whose 8-byte LE bit pattern matches
/// the source literal's `f64::to_le_bytes()`. Isolates the instruction
/// from `features_runs_under_wasmtime` (which exercises F64 via enum
/// payloads + match lowering + StringConcat all at once) so a single-
/// bit regression surfaces here unambiguously.
#[test]
fn f64_const_emits_f64_const_instruction() {
    // `7.25` is exactly representable in f64 and far from any
    // `f64::consts::*` (keeps `clippy::approx_constant` quiet).
    let src = "function main() {\n  let _x: Float = 7.25\n  print(true)\n}\n";
    compile_or_skip(src, "f64_const", |bytes| {
        wasmparser::validate(bytes)
            .unwrap_or_else(|e| panic!("wasmparser rejected the module: {e}"));
        let wat =
            wasmprinter::print_bytes(bytes).unwrap_or_else(|e| panic!("wasmprinter failed: {e}"));
        assert!(
            wat.contains("f64.const"),
            "emitted WAT must contain at least one `f64.const` for the \
             Float literal, but none was found.\n\
             (first 4 KiB of WAT shown for diagnostic:)\n{}",
            wat_excerpt(&wat),
        );
        // WASM encodes `f64.const` as opcode `0x44` followed by the
        // 8 little-endian bytes of the IEEE-754 pattern. Scanning the
        // raw bytes (rather than re-parsing) pins the actual literal
        // that landed and isn't tied to `wasmprinter`'s hex-float
        // rendering.
        let pattern = 7.25_f64.to_le_bytes();
        assert!(
            bytes.windows(pattern.len()).any(|w| w == pattern),
            "emitted module must contain `7.25_f64`'s little-endian \
             bit pattern ({pattern:02x?}) in its byte stream; a \
             regression that fed the wrong value through `Ieee64::from` \
             would surface here.",
        );
    });
}

/// Sister check to [`f64_const_emits_f64_const_instruction`] that pins
/// the *sign bit*: `-0.0` and `0.0` differ only in bit 63, so a
/// regression that canonicalized the input through an arithmetic or
/// `if n == 0.0 { 0.0 }` short-circuit would silently flip `-0.0` to
/// `+0.0`. Verifying the negative-zero bit pattern lands in the
/// emitted bytes is the cheapest way to catch that.
///
/// Also re-checks a non-zero negative literal (`-3.5`) to pin the
/// general sign-bit-bearing path: a regression that masked the high
/// bit on every f64 emission would still leak past the `-0.0` check
/// alone, since `0.0` and `-0.0` share every bit but bit 63.
#[test]
fn f64_const_preserves_negative_zero_and_sign() {
    // Phoenix's parser accepts negative float literals via unary
    // negation, so `-0.0` lowers to `FNeg(ConstF64(0.0))` rather than
    // a direct `ConstF64(-0.0)` in some lowerings. Skip that pre-arg
    // by constructing IR by hand: `Op::ConstF64(-0.0)` and
    // `Op::ConstF64(-3.5)` route straight into the F64Const arm we're
    // here to verify.
    let mut module = IrModule::new();
    let mut f = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = f.create_block();
    let _ = f.emit_value(entry, Op::ConstF64(-0.0), IrType::F64, None);
    let _ = f.emit_value(entry, Op::ConstF64(-3.5), IrType::F64, None);
    f.set_terminator(entry, Terminator::Return(None));
    module.push_concrete(f);

    compile_ir_or_skip(&module, "f64_const_neg_zero", |bytes| {
        wasmparser::validate(bytes)
            .unwrap_or_else(|e| panic!("wasmparser rejected the module: {e}"));
        let neg_zero = (-0.0_f64).to_le_bytes();
        let pos_zero = 0.0_f64.to_le_bytes();
        assert_ne!(
            neg_zero, pos_zero,
            "test premise broken: `-0.0` and `+0.0` should differ in their \
             little-endian byte patterns (sign bit)"
        );
        assert!(
            bytes.windows(neg_zero.len()).any(|w| w == neg_zero),
            "emitted module must contain `-0.0_f64`'s little-endian bit \
             pattern ({neg_zero:02x?}); a regression that canonicalized \
             the value to `+0.0` (or stripped the sign bit) would surface \
             here. (`+0.0` pattern for contrast: {pos_zero:02x?})"
        );
        let neg_three_five = (-3.5_f64).to_le_bytes();
        assert!(
            bytes
                .windows(neg_three_five.len())
                .any(|w| w == neg_three_five),
            "emitted module must contain `-3.5_f64`'s little-endian bit \
             pattern ({neg_three_five:02x?})"
        );
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

/// Shared "compile from source and expect Ok" helper used by the
/// signature-acceptance tests. Treats `RuntimeWasmNotFound` the same
/// way `compile_or_skip` does (skip with a warning) and panics with
/// `failure_msg` on any other error.
fn compile_src_or_assert_ok(src: &str, label: &str, failure_msg: &str) {
    let ir_module = lower_to_ir(src);
    match phoenix_cranelift::compile(&ir_module, Target::Wasm32Linear) {
        Ok(_) => {}
        Err(e) if e.kind == CompileErrorKind::RuntimeWasmNotFound => {
            eprintln!("warning: skipping {label} — phoenix_runtime.wasm not built");
        }
        Err(e) => panic!("{failure_msg} — got: {e}"),
    }
}

#[test]
fn accepts_ref_typed_param_signature() {
    // `wasm_valtypes_for` flattens every GC-pointer reference type
    // (`StructRef` / `EnumRef` / `ListRef` / `MapRef` / `ClosureRef`)
    // to a single i32 slot, so function signatures with these types
    // in param position declare cleanly even when the body translator
    // hasn't landed support for the matching *alloc / method* ops
    // yet. This positive test pins the signature-level acceptance —
    // a regression that tightened the gate back to "single-slot
    // primitives only" would surface here before manifesting as a
    // confusing signature-level error on collection fixtures whose
    // op support lands later.
    //
    // `main` doesn't call `sink`, so the only path exercised is the
    // signature declaration. Bodies that try to *use* a ref-typed
    // value through a still-unsupported surface (a builtin without
    // a WASM lowering, today) end up at builtin-level rejections
    // covered by `rejects_unsupported_list_method` below.
    let src = "function main() {}\n\
               function sink(xs: List<Int>) {\n  print(true)\n}\n";
    compile_src_or_assert_ok(
        src,
        "accepts_ref_typed_param_signature",
        "wasm32-linear must accept `List<Int>` in param position \
         (ref-type flattening produces a single i32 GC pointer)",
    );
}

/// Mirror of [`accepts_ref_typed_param_signature`] for *return*
/// position. `wasm_valtypes_for` should accept `StructRef` as a
/// declared return type and flatten it to a single i32 GC pointer
/// — even when the function body itself never reaches codegen
/// (here `main` doesn't call `make`, so the only path exercised is
/// the signature declaration). Without this test, a regression that
/// re-tightened the gate on return-position ref types would only
/// surface once a struct-returning function got called.
#[test]
fn accepts_ref_typed_return_signature() {
    // Phoenix structs declare fields type-first (`Int value`) and
    // construct positionally (`Box(1)`). `main` doesn't call `make`,
    // so only the signature is exercised — the body still lowers,
    // but its `Op::StructAlloc` is a supported op so codegen succeeds.
    let src = "struct Box {\n  Int value\n}\n\
               function main() {}\n\
               function make() -> Box {\n  return Box(1)\n}\n";
    compile_src_or_assert_ok(
        src,
        "accepts_ref_typed_return_signature",
        "wasm32-linear must accept `StructRef` in return position \
         (single-i32 GC pointer flattening)",
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

/// Boundary cases for `take` / `drop` / `push`: zero counts, counts
/// past `length`, and `push` onto an empty list. Pins the wasm32 ABI
/// of the runtime calls across each boundary; the interpreter is the
/// authoritative oracle.
#[test]
fn list_take_drop_push_edge_cases_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [1, 2, 3]\n  \
                 let empty: List<Int> = []\n  \
                 print(xs.take(0).length())\n  \
                 print(xs.take(99).length())\n  \
                 print(xs.drop(0).length())\n  \
                 print(xs.drop(99).length())\n  \
                 let pushed: List<Int> = empty.push(42)\n  \
                 print(pushed.length())\n  \
                 print(pushed.get(0))\n\
               }\n";
    assert_wasm_matches_interp(src, "list_take_drop_push_edge_cases");
}

#[test]
fn list_take_drop_push_contains_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [10, 20, 30]\n  \
                 print(xs.take(2).length())\n  \
                 let d: List<Int> = xs.drop(1)\n  \
                 print(d.get(0))\n  \
                 print(xs.push(40).length())\n  \
                 print(xs.contains(20))\n  \
                 print(xs.contains(99))\n\
               }\n";
    assert_wasm_matches_interp(src, "list_take_drop_push_contains");
}

/// `List<String>.contains` pins string-content comparison on wasm32.
/// The needle is built via a helper function so it's a fresh heap
/// allocation distinct from the literal in the list — pointer-identity
/// comparison would answer `false`. Routing through a function call
/// also defends against future string interning / constant folding.
#[test]
fn list_string_contains_by_content_runs_under_wasmtime() {
    let src = "function tail(prefix: String) -> String { prefix + \"ob\" }\n\
               function tail2(prefix: String) -> String { prefix + \"an\" }\n\
               function main() {\n  \
                 let xs: List<String> = [\"alice\", \"bob\", \"carol\"]\n  \
                 let needle_hit: String = tail(\"b\")\n  \
                 let needle_miss: String = tail2(\"d\")\n  \
                 print(xs.contains(needle_hit))\n  \
                 print(xs.contains(needle_miss))\n\
               }\n";
    assert_wasm_matches_interp(src, "list_string_contains_by_content");
}

/// `List.any` / `List.all` — value cases. Short-circuit behavior is
/// pinned separately by [`list_any_all_short_circuits_like_interp`].
#[test]
fn list_any_all_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [2, 4, 6]\n  \
                 print(xs.any(function(x: Int) -> Bool { x > 5 }))\n  \
                 print(xs.any(function(x: Int) -> Bool { x > 10 }))\n  \
                 print(xs.all(function(x: Int) -> Bool { x % 2 == 0 }))\n  \
                 print(xs.all(function(x: Int) -> Bool { x > 2 }))\n\
               }\n";
    assert_wasm_matches_interp(src, "list_any_all");
}

/// `List.any` / `List.all` short-circuit like the interpreter — each
/// predicate `print`s its element so the printed count reveals exactly
/// how far the fold ran. A lowering that evaluated every element would
/// print extra values and fail the byte-identical match.
#[test]
fn list_any_all_short_circuits_like_interp() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [1, 3, 5]\n  \
                 print(xs.any(function(x: Int) -> Bool {\n    \
                   print(x)\n    \
                   x > 2\n  \
                 }))\n  \
                 let ys: List<Int> = [4, 1, 6]\n  \
                 print(ys.all(function(x: Int) -> Bool {\n    \
                   print(x)\n    \
                   x > 2\n  \
                 }))\n\
               }\n";
    assert_wasm_matches_interp(src, "list_any_all_short_circuit");
}

/// `List.any` / `List.all` on an **empty** list — boundary case for the
/// hand-rolled loop's seed. With `len == 0` the body never runs, the
/// `i >= len` guard immediately branches to `$exit`, and `result_local`
/// retains its initial seed: `0` (false) for `any`, `1` (true) for
/// `all`. Matches the interpreter's mathematical convention
/// (`any([]) = false`, `all([]) = true`).
#[test]
fn list_any_all_on_empty_list_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = []\n  \
                 print(xs.any(function(x: Int) -> Bool { x > 0 }))\n  \
                 print(xs.all(function(x: Int) -> Bool { x > 0 }))\n\
               }\n";
    assert_wasm_matches_interp(src, "list_any_all_empty");
}

/// `List.first` / `List.last` / `List.find` — `Option<T>`-returning
/// lookups built inline. Each branches on length / found-flag and
/// constructs `Some(elem)` or `None` via the shared
/// `emit_option_some` / `emit_option_none` helpers (one
/// `phx_gc_alloc(size, TypeTag::Enum)` per construction). Tested via
/// `isSome` / `unwrapOr` since `unwrap` ships in a later slice.
///
/// `xs = [10, 20, 30]`:
///   first  ⇒ Some(10), last ⇒ Some(30),
///   find(x > 15) ⇒ Some(20), find(x > 100) ⇒ None.
///
/// Empty list: first ⇒ None (pins the `len == 0` branch); find ⇒ None
/// (pins the loop-never-runs path, where the `found` flag stays 0).
#[test]
fn list_first_last_find_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [10, 20, 30]\n  \
                 print(xs.first().isSome())\n  \
                 print(xs.first().unwrapOr(0))\n  \
                 print(xs.last().isSome())\n  \
                 print(xs.last().unwrapOr(0))\n  \
                 let found = xs.find(function(x: Int) -> Bool { x > 15 })\n  \
                 print(found.isSome())\n  \
                 print(found.unwrapOr(0))\n  \
                 let empty: List<Int> = []\n  \
                 print(empty.first().isSome())\n  \
                 print(empty.find(function(x: Int) -> Bool { x > 0 }).isSome())\n  \
                 print(xs.find(function(x: Int) -> Bool { x > 100 }).isSome())\n\
               }\n";
    assert_wasm_matches_interp(src, "list_first_last_find");
}

/// `List.find` **short-circuits** at the first match — the predicate is
/// not evaluated on elements past the match, matching the interpreter's
/// first-match-and-return. Pins this against a regression to a
/// full-iteration `found`-flag loop.
///
/// The predicate divides by the element, so it traps on `0`. The list
/// is `[5, 0]`: a short-circuiting `find` matches at `5` (index 0) and
/// never touches the `0`, so both wasm and the interpreter return
/// `Some(5)` and print `5`. A full-iteration implementation would
/// evaluate the predicate on `0`, trapping under wasmtime (`i64.div_s`
/// by zero) — a divergence this test would catch.
#[test]
fn list_find_short_circuits_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [5, 0]\n  \
                 let found = xs.find(function(x: Int) -> Bool { return (100 / x) > 0 })\n  \
                 print(found.unwrapOr(0 - 1))\n\
               }\n";
    assert_wasm_matches_interp(src, "list_find_short_circuits");
}

/// `Map.get` / `Map.set` — Option-returning lookup + immutable
/// insertion via copy-on-write. `set` stages key and value back-to-
/// back in one shadow-stack frame and passes the value pointer as
/// `frame + ks` so a single SP restore covers both; `get` stages
/// just the key, branches on the runtime's null return, and
/// constructs `None` or `Some(value)`.
///
/// `{"alice": 1, "bob": 2}`: get("alice") = Some(1), get("dan") = None.
/// `set("carol", 3)` → new map of length 3, get("carol") = Some(3).
/// `set("alice", 99)` → length stays 2 (update, not insert), and the
/// updated value reads back as 99 while the original map is unchanged
/// (copy-on-write).
#[test]
fn map_get_set_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<String, Int> = {\"alice\": 1, \"bob\": 2}\n  \
                 print(m.get(\"alice\").isSome())\n  \
                 print(m.get(\"alice\").unwrapOr(0))\n  \
                 print(m.get(\"dan\").isSome())\n  \
                 let m2 = m.set(\"carol\", 3)\n  \
                 print(m2.length())\n  \
                 print(m2.get(\"carol\").unwrapOr(0))\n  \
                 let m3 = m.set(\"alice\", 99)\n  \
                 print(m3.length())\n  \
                 print(m3.get(\"alice\").unwrapOr(0))\n  \
                 print(m.get(\"alice\").unwrapOr(0))\n\
               }\n";
    assert_wasm_matches_interp(src, "map_get_set");
}

/// `Option.unwrap` / `Result.unwrap` — positive-variant payload
/// extraction. The negative arm panics via `phx_panic` and never
/// returns, so we only test the positive path here. The negative
/// path is exercised by `panics_on_option_unwrap_of_none` below.
///
/// Expected: 10 / 7 / hello.
#[test]
fn enum_unwrap_positive_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let s: Option<Int> = Some(10)\n  \
                 print(s.unwrap())\n  \
                 let r: Result<Int, String> = Ok(7)\n  \
                 print(r.unwrap())\n  \
                 let so: Option<String> = Some(\"hello\")\n  \
                 print(so.unwrap())\n\
               }\n";
    assert_wasm_matches_interp(src, "enum_unwrap_positive");
}

/// `Option.unwrap` on `None` traps via `phx_panic`. The compiled
/// program exits with a non-zero status; assert that the wasmtime
/// invocation fails rather than producing matching stdout. (Match
/// against the AST interpreter would be wrong here — the interp
/// raises a `RuntimeError`, the compiled binary aborts.)
#[test]
fn enum_unwrap_negative_panics_under_wasmtime() {
    let src = "function main() {\n  \
                 let n: Option<Int> = None\n  \
                 print(n.unwrap())\n\
               }\n";
    let label = "enum_unwrap_panic";
    compile_or_skip(src, label, |bytes| {
        let spawn = Command::new("wasmtime").arg("--version").output();
        if spawn.is_err() {
            if require_wasmtime() {
                panic!("PHOENIX_REQUIRE_WASMTIME=1 but wasmtime not on PATH");
            }
            eprintln!("warning: skipping {label} — wasmtime not on PATH");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panic.wasm");
        std::fs::write(&path, bytes).expect("write wasm");
        let out = Command::new("wasmtime")
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("invoke wasmtime");
        assert!(
            !out.status.success(),
            "expected wasmtime to abort on Option.unwrap of None, got success: \
             stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    });
}

/// `Result.ok` / `Result.err` — convert `Result<T, E>` into
/// `Option<T>` / `Option<E>` respectively. The matching arm extracts
/// the variant's payload and wraps it as `Some`; the mismatching arm
/// yields `None`.
///
/// Expected: 1 / true / true / false / true (for the bool flags).
#[test]
fn result_ok_err_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let ok: Result<Int, String> = Ok(1)\n  \
                 let er: Result<Int, String> = Err(\"oops\")\n  \
                 print(ok.ok().unwrapOr(0))\n  \
                 print(ok.ok().isSome())\n  \
                 print(ok.err().isSome())\n  \
                 print(er.ok().isSome())\n  \
                 print(er.err().isSome())\n  \
                 print(er.err().unwrapOr(\"none\"))\n\
               }\n";
    assert_wasm_matches_interp(src, "result_ok_err");
}

/// `Option.okOr` — convert `Option<T>` to `Result<T, E>` by tagging
/// `None` with a caller-supplied `Err` value. Matches the
/// interpreter's behavior across both variants.
///
/// Expected stdout: true / 5 / false / err.
#[test]
fn option_okor_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let s: Option<Int> = Some(5)\n  \
                 let n: Option<Int> = None\n  \
                 let r1: Result<Int, String> = s.okOr(\"missing\")\n  \
                 let r2: Result<Int, String> = n.okOr(\"err\")\n  \
                 print(r1.isOk())\n  \
                 print(r1.unwrapOr(0))\n  \
                 print(r2.isOk())\n  \
                 print(r2.err().unwrapOr(\"none\"))\n\
               }\n";
    assert_wasm_matches_interp(src, "option_okor");
}

/// Option closure-payload transforms — `map`, `andThen`, `filter`,
/// `orElse`, `unwrapOrElse`. Each branches on the discriminant and
/// either runs a user closure on the matching variant's payload (for
/// `map`/`andThen`/`filter`) or on the negative variant (for
/// `orElse`/`unwrapOrElse`). Mixed `Int` (single-slot) and `String`
/// (multi-slot) payload types exercise the rooting-on-ref-result
/// path in `emit_option_some` after an allocating closure, and a
/// `filter` over an `Option<String>` drives the ref-payload rooting
/// branch in `translate_option_filter` that the `Int` cases skip.
#[test]
fn option_payload_transforms_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let s: Option<Int> = Some(3)\n  \
                 let n: Option<Int> = None\n  \
                 // map\n  \
                 print(s.map(function(x: Int) -> Int { x * 10 }).unwrapOr(0))\n  \
                 print(n.map(function(x: Int) -> Int { x * 10 }).isSome())\n  \
                 // andThen\n  \
                 print(s.andThen(function(x: Int) -> Option<Int> { Some(x + 1) }).unwrapOr(0))\n  \
                 print(s.andThen(function(x: Int) -> Option<Int> { None }).isSome())\n  \
                 // filter\n  \
                 print(s.filter(function(x: Int) -> Bool { x > 0 }).isSome())\n  \
                 print(s.filter(function(x: Int) -> Bool { x > 100 }).isSome())\n  \
                 // orElse\n  \
                 print(n.orElse(function() -> Option<Int> { Some(42) }).unwrapOr(0))\n  \
                 print(s.orElse(function() -> Option<Int> { Some(42) }).unwrapOr(0))\n  \
                 // unwrapOrElse\n  \
                 print(n.unwrapOrElse(function() -> Int { 99 }))\n  \
                 print(s.unwrapOrElse(function() -> Int { 99 }))\n  \
                 // unwrapOrElse with a multi-slot (String) payload — drives\n  \
                 // the BlockType::Empty + fat-pointer result-locals path in\n  \
                 // translate_enum_unwrap_or_else (the Int cases above only\n  \
                 // exercise the single-slot store/reload).\n  \
                 let ns: Option<String> = None\n  \
                 let ks: Option<String> = Some(\"have\")\n  \
                 print(ns.unwrapOrElse(function() -> String { \"made\" }))\n  \
                 print(ks.unwrapOrElse(function() -> String { \"made\" }))\n  \
                 // map across ref payload (closure allocates) — pins the\n  \
                 // ad-hoc ref-rooting path around emit_option_some.\n  \
                 let si: Option<Int> = Some(7)\n  \
                 print(si.map(function(x: Int) -> String { \"v=\" + toString(x) }).unwrapOr(\"none\"))\n  \
                 // filter across a ref payload (String) — drives the\n  \
                 // maybe_root_ref_payload branch in translate_option_filter\n  \
                 // (the Int cases above only hit the value-typed no-op).\n  \
                 let ss: Option<String> = Some(\"keep\")\n  \
                 print(ss.filter(function(x: String) -> Bool { true }).unwrapOr(\"dropped\"))\n  \
                 print(ss.filter(function(x: String) -> Bool { false }).unwrapOr(\"dropped\"))\n\
               }\n";
    assert_wasm_matches_interp(src, "option_payload_transforms");
}

/// Result closure-payload transforms — `map`, `mapErr`, `andThen`,
/// `orElse`, `unwrapOrElse`. The pass-through pointer trick (sending
/// the receiver pointer down the non-matching arm because the
/// preserved variant's layout is identical between input and output
/// `Result` types) is exercised for both `map` (Err pass-through) and
/// `mapErr` (Ok pass-through).
///
/// Two extra cases pin paths the Int→Int fixtures above leave
/// uncovered:
///   - `map` returning a ref-typed `U` that allocates (`Int -> String`)
///     drives the `maybe_root_ref_payload` + `emit_enum_construct`
///     rooting branch in `translate_result_map` (the Ok→Int cases only
///     hit the value-typed branch).
///   - the *passthrough with a payload-size change*: `map(Int ->
///     String)` reading the Err arm and `mapErr(String -> Int)` reading
///     the Ok arm both alias a receiver whose preserved-variant offset
///     stays put while the *other* variant's payload size changes —
///     the exact assumption the passthrough trick rests on.
#[test]
fn result_payload_transforms_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let ok: Result<Int, String> = Ok(5)\n  \
                 let er: Result<Int, String> = Err(\"oops\")\n  \
                 // map (Ok → Ok(f), Err passthrough)\n  \
                 print(ok.map(function(x: Int) -> Int { x + 1 }).unwrapOr(0))\n  \
                 print(er.map(function(x: Int) -> Int { x + 1 }).isOk())\n  \
                 // map with a ref-typed (allocating) U — roots Ok(U) across construct\n  \
                 print(ok.map(function(x: Int) -> String { \"v=\" + toString(x) }).unwrapOr(\"none\"))\n  \
                 // map Int → String, then read the Err passthrough (Ok payload\n  \
                 // size grew Int → String, Err offset must be unchanged)\n  \
                 print(er.map(function(x: Int) -> String { \"v=\" + toString(x) }).err().unwrapOr(\"none\"))\n  \
                 // mapErr (Ok passthrough, Err → Err(f))\n  \
                 print(ok.mapErr(function(e: String) -> String { e + \"!\" }).isOk())\n  \
                 print(er.mapErr(function(e: String) -> String { e + \"!\" }).err().unwrapOr(\"none\"))\n  \
                 // mapErr String → Int, then read the Ok passthrough (Err payload\n  \
                 // size shrank String → Int, Ok offset must be unchanged)\n  \
                 print(ok.mapErr(function(e: String) -> Int { 0 }).unwrapOr(-1))\n  \
                 // andThen\n  \
                 print(ok.andThen(function(x: Int) -> Result<Int, String> { Ok(x * 2) }).unwrapOr(0))\n  \
                 print(ok.andThen(function(x: Int) -> Result<Int, String> { Err(\"nope\") }).isOk())\n  \
                 print(er.andThen(function(x: Int) -> Result<Int, String> { Ok(x * 2) }).isOk())\n  \
                 // orElse\n  \
                 print(er.orElse(function(e: String) -> Result<Int, String> { Ok(0) }).unwrapOr(-1))\n  \
                 print(ok.orElse(function(e: String) -> Result<Int, String> { Ok(0) }).unwrapOr(-1))\n  \
                 // unwrapOrElse\n  \
                 print(ok.unwrapOrElse(function(e: String) -> Int { 0 }))\n  \
                 print(er.unwrapOrElse(function(e: String) -> Int { 100 }))\n  \
                 // unwrapOrElse with a multi-slot (String) Ok payload —\n  \
                 // drives the BlockType::Empty + fat-pointer result-locals\n  \
                 // path in translate_enum_unwrap_or_else, and passes the Err\n  \
                 // payload into the recovery closure (the Int cases above\n  \
                 // only exercise the single-slot store/reload).\n  \
                 let oks: Result<String, String> = Ok(\"good\")\n  \
                 let ers: Result<String, String> = Err(\"bad\")\n  \
                 print(oks.unwrapOrElse(function(e: String) -> String { e + \"!\" }))\n  \
                 print(ers.unwrapOrElse(function(e: String) -> String { e + \"!\" }))\n\
               }\n";
    assert_wasm_matches_interp(src, "result_payload_transforms");
}

/// `Map.get` / `Map.set` with **`Int` keys** — exercises the
/// `key_is_string == false` branch in `translate_map_set` and the
/// fixed-width (non-fat-pointer) key staging in both helpers. The
/// `Map<String, _>` fixtures above only cover content-hashed string
/// keys, so without this the integer-key path is unrun.
///
/// `{1: 10, 2: 20}`: get(1) = Some(10), get(9) = None.
/// `set(3, 30)` → length 3, get(3) = Some(30).
#[test]
fn map_get_set_int_keys_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<Int, Int> = {1: 10, 2: 20}\n  \
                 print(m.get(1).isSome())\n  \
                 print(m.get(1).unwrapOr(0))\n  \
                 print(m.get(9).isSome())\n  \
                 let m2 = m.set(3, 30)\n  \
                 print(m2.length())\n  \
                 print(m2.get(3).unwrapOr(0))\n\
               }\n";
    assert_wasm_matches_interp(src, "map_get_set_int_keys");
}

/// `Map.get` / `Map.set` with a **GC-ref value type**
/// (`Map<String, List<Int>>`) — exercises the rooting claim in
/// `translate_map_get`: the value loaded from the map's data region is
/// a heap pointer that must stay reachable across the `phx_gc_alloc`
/// that builds the `Some`. The `Int`-valued fixtures above only load a
/// scalar, so this is the only test where the loaded value is itself a
/// collectible object. `set` likewise stages a list pointer as the
/// value bytes.
///
/// `{"a": [1, 2], "b": [3]}`: get("a") = Some([1,2]) (length 2),
/// get("b") = Some([3]) (length 1), get("z") = None.
/// `set("c", [4, 5, 6])` → new map of length 3, get("c") length 3,
/// while the original map still lacks "c" (copy-on-write).
#[test]
fn map_get_set_ref_values_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<String, List<Int>> = {\"a\": [1, 2], \"b\": [3]}\n  \
                 print(m.get(\"a\").unwrapOr([99]).length())\n  \
                 print(m.get(\"b\").unwrapOr([99]).length())\n  \
                 print(m.get(\"z\").isSome())\n  \
                 let m2 = m.set(\"c\", [4, 5, 6])\n  \
                 print(m2.length())\n  \
                 print(m2.get(\"c\").unwrapOr([99]).length())\n  \
                 print(m.get(\"c\").isSome())\n\
               }\n";
    assert_wasm_matches_interp(src, "map_get_set_ref_values");
}

/// `Result.unwrapOr` — the `unwrapOr` dispatch is generic over the
/// stdlib enum, but the `List` / `Map` fixtures only ever reach it
/// through `Option`. This pins the `Result` arm: `Ok(v).unwrapOr(d)`
/// extracts the Ok payload at variant index 0, `Err(_).unwrapOr(d)`
/// yields the default.
///
/// `Ok(7).unwrapOr(0)` = 7; `Err("boom").unwrapOr(99)` = 99.
#[test]
fn result_unwrap_or_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let ok: Result<Int, String> = Ok(7)\n  \
                 print(ok.unwrapOr(0))\n  \
                 let err: Result<Int, String> = Err(\"boom\")\n  \
                 print(err.unwrapOr(99))\n\
               }\n";
    assert_wasm_matches_interp(src, "result_unwrap_or");
}

/// `Option<String>.unwrapOr` — multi-slot (`StringRef` fat-pointer)
/// payload. The `Int` `unwrapOr` fixtures above only exercise the
/// single-slot case; this pins the multi-slot local copy in both the
/// positive (`emit_field_load` of the 2-slot payload) and negative
/// (zip-copy of the default's 2 locals) branches, where a slot-count
/// mismatch would truncate the fat pointer.
///
/// `["hi", "yo"].first().unwrapOr("none")` = "hi" (Some path);
/// `[].first().unwrapOr("none")` = "none" (None path).
#[test]
fn option_string_unwrap_or_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let names: List<String> = [\"hi\", \"yo\"]\n  \
                 print(names.first().unwrapOr(\"none\"))\n  \
                 let empty: List<String> = []\n  \
                 print(empty.first().unwrapOr(\"none\"))\n\
               }\n";
    assert_wasm_matches_interp(src, "option_string_unwrap_or");
}

/// `List.find` with a **GC-ref element type** (`List<List<Int>>`) —
/// `first`/`last`/`find` all share `emit_option_some`, but `find` is
/// the one that captures the matched element into `found_elem_locals`
/// and carries it across the post-loop `phx_gc_alloc` that builds the
/// `Some`. The `Int`-element fixtures above only capture a scalar; this
/// pins that a *collectible* element survives that alloc — it stays
/// reachable through the rooted input list during the carry.
///
/// `[[1], [2, 3], [4, 5, 6]]`: find(len == 2) ⇒ Some([2, 3]) (length 2);
/// find(len == 9) ⇒ None.
#[test]
fn list_find_ref_element_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let lol: List<List<Int>> = [[1], [2, 3], [4, 5, 6]]\n  \
                 let found = lol.find(function(xs: List<Int>) -> Bool { xs.length() == 2 })\n  \
                 print(found.isSome())\n  \
                 print(found.unwrapOr([]).length())\n  \
                 let missing = lol.find(function(xs: List<Int>) -> Bool { xs.length() == 9 })\n  \
                 print(missing.isSome())\n\
               }\n";
    assert_wasm_matches_interp(src, "list_find_ref_element");
}

/// `Map.set` with a **multi-slot value** (`Map<String, String>`) —
/// the `Int` / `List`-valued fixtures above store a single-slot value,
/// so this is the only case where `translate_map_set`'s
/// `emit_field_store` writes a 2-slot `StringRef` fat pointer at the
/// non-zero `ks` frame offset (`[key bytes | value fat pointer]`). A
/// slot-count or offset bug in the value store would truncate the
/// stored string here.
///
/// `{"k": "v"}`: get("k") = Some("v"), get("x") = None.
/// `set("k2", "v2")` → length 2, get("k2") = "v2", get("k") = "v".
/// `set("k", "vv")` → length stays 1 (update), get("k") = "vv".
#[test]
fn map_set_string_value_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<String, String> = {\"k\": \"v\"}\n  \
                 print(m.get(\"k\").unwrapOr(\"none\"))\n  \
                 print(m.get(\"x\").isSome())\n  \
                 let m2 = m.set(\"k2\", \"v2\")\n  \
                 print(m2.length())\n  \
                 print(m2.get(\"k2\").unwrapOr(\"none\"))\n  \
                 print(m2.get(\"k\").unwrapOr(\"none\"))\n  \
                 let m3 = m.set(\"k\", \"vv\")\n  \
                 print(m3.length())\n  \
                 print(m3.get(\"k\").unwrapOr(\"none\"))\n\
               }\n";
    assert_wasm_matches_interp(src, "map_set_string_value");
}

/// **GC-pressure** tier for the ref-returning `Option` lookups
/// (`Map.get` and `List.find` on collectible value/element types). The
/// `map_get_set_ref_values` / `list_find_ref_element` fixtures above pin
/// the *functional* result, but their handful of allocations never
/// approach the 1 MB auto-collect threshold — so a regression that
/// dropped the rooting (the loaded value/element must stay reachable
/// through its rooted container across the `Some`-construction
/// `phx_gc_alloc`) would still pass them, exactly as the `flatMap`
/// runtime test documents for its inner-list root. This test closes that
/// gap the same way `alloc_loop` does: a ~100k-iteration loop whose
/// per-iteration `Some(List)` constructions plus `[99]` defaults sum to
/// several MB, so the threshold-driven sweep runs many times *while*
/// `get` / `find` hold an interior pointer into a rooted collection. A
/// missed root would surface as a wasmtime use-after-free trap or a
/// clobbered `total`.
///
/// `m = {"a": [1, 2], "b": [3]}`, `lol = [[1], [2, 3], [4, 5, 6]]`. Each
/// iteration: `m.get("a")` ⇒ Some([1, 2]) (length 2) and
/// `lol.find(len == 2)` ⇒ Some([2, 3]) (length 2), so `total += 4` per
/// iteration ⇒ `4 * 100000 = 400000`.
#[test]
fn ref_option_lookups_under_gc_pressure_run_under_wasmtime() {
    let src = "function main() {\n  \
                 let m: Map<String, List<Int>> = {\"a\": [1, 2], \"b\": [3]}\n  \
                 let lol: List<List<Int>> = [[1], [2, 3], [4, 5, 6]]\n  \
                 let n: Int = 100000\n  \
                 let mut total: Int = 0\n  \
                 let mut i: Int = 0\n  \
                 while (i < n) {\n    \
                   let got: List<Int> = m.get(\"a\").unwrapOr([99])\n    \
                   let found: List<Int> = lol.find(function(xs: List<Int>) -> Bool { xs.length() == 2 }).unwrapOr([99])\n    \
                   total = total + got.length() + found.length()\n    \
                   i = i + 1\n  \
                 }\n  \
                 print(total)\n\
               }\n";
    assert_wasm_matches_interp(src, "ref_option_lookups_under_gc_pressure");
}

/// `List.sortBy` on an **empty** list — boundary case for the outer
/// loop's `i = 1; while i < len` guard. With `len == 0` the outer loop
/// never enters, the comparator is never called, and the `phx_list_take`
/// copy of the empty list is returned verbatim. Pins that the
/// `elem_is_ref` ad-hoc-frame push/pop is still balanced (push happens
/// before the loop, pop after) even when the inner body is skipped — a
/// regression that put the pop inside the loop body would leak a frame
/// on every empty-list sortBy.
#[test]
fn list_sortby_on_empty_list_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<Int> = []\n  \
                 let sorted: List<Int> = xs.sortBy(function(a: Int, b: Int) -> Int { a - b })\n  \
                 print(sorted.length())\n\
               }\n";
    assert_wasm_matches_interp(src, "list_sortby_empty_int");
}

/// `List<String>.sortBy` on an **empty** list — same as above but with
/// a ref-typed element (`elem_is_ref = true`), so the ad-hoc shadow
/// frame is *actually pushed* even though the outer loop never executes.
/// This is the path the [`list_sortby_ref_elem_emits_adhoc_shadow_frame`]
/// docstring leans on when it argues placeholder typing is safe ("the
/// outer loop's `i < len` condition is false ... so the inner loop never
/// executes"). Without runtime coverage of the empty-ref path, that
/// argument is unpinned.
#[test]
fn list_sortby_on_empty_string_list_runs_under_wasmtime() {
    let src = "function main() {\n  \
                 let xs: List<String> = []\n  \
                 let sorted: List<String> = xs.sortBy(function(a: String, b: String) -> Int { 0 })\n  \
                 print(sorted.length())\n\
               }\n";
    assert_wasm_matches_interp(src, "list_sortby_empty_string");
}

/// Positive sibling of the historical "frontier" rejection test: all
/// the List / Map / Result / Option methods native supports are now
/// supported by the WASM backend too. The fixture exercises a method
/// from each formerly-gated tier — `List.find` (closure-payload
/// transform with Option result), `Map.get` (Option-returning
/// lookup), `Option.unwrap` (positive payload), `Result.map`
/// (Ok-arm closure transform with Err passthrough) — to pin that
/// the closure-payload + Option-construct + payload-extract paths
/// all compose end-to-end. A regression that re-broke any one of
/// them would fail this test rather than silently surfacing only at
/// an integration-level fixture.
#[test]
fn frontier_method_coverage_complete() {
    let src = "function main() {\n  \
                 let xs: List<Int> = [1, 2, 3]\n  \
                 let m: Map<String, Int> = {\"a\": 10}\n  \
                 let r: Result<Int, String> = Ok(1)\n  \
                 print(xs.find(function(x: Int) -> Bool { x > 1 }).unwrapOr(0))\n  \
                 print(m.get(\"a\").unwrapOr(0))\n  \
                 print(Some(7).unwrap())\n  \
                 print(r.map(function(x: Int) -> Int { x + 100 }).unwrapOr(0))\n\
               }\n";
    assert_wasm_matches_interp(src, "frontier_method_coverage_complete");
}

/// Hand-build an IR module whose `main` constructs an enum value
/// for a variant whose declared field list contains
/// `placeholder_positions`-many `GENERIC_PLACEHOLDER` entries. The
/// total variant arity is `field_types.len()`; positions whose
/// declared type should be concrete are taken from `field_types`,
/// positions named in `placeholder_positions` are overwritten with a
/// placeholder. The constructed alloc uses `value_types` for the
/// *value vids' actual types* (so we can exercise the alloc/get
/// layout-mismatch case where declared and actual types diverge).
///
/// Phoenix's source surface never produces multi-field variants with
/// placeholder fields — stdlib generics only attach one placeholder per
/// variant, and user enums monomorphize their type parameters out before
/// codegen. Hand-building keeps the rejection paths testable without
/// inventing a contrived source-level fixture.
fn build_placeholder_enum_ir(
    enum_name: &str,
    declared_field_types: Vec<IrType>,
    placeholder_positions: &[usize],
    value_types: &[IrType],
) -> IrModule {
    use phoenix_ir::types::GENERIC_PLACEHOLDER;

    let mut declared = declared_field_types;
    let placeholder = IrType::StructRef(GENERIC_PLACEHOLDER.to_string(), Vec::new());
    for &pos in placeholder_positions {
        declared[pos] = placeholder.clone();
    }

    let mut module = IrModule::new();
    module
        .enum_layouts
        .insert(enum_name.to_string(), vec![("V".to_string(), declared)]);

    let mut f = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = f.create_block();
    // Emit a constant for each value position with the requested
    // actual type. We only need the binding's `ir_type` to be right
    // for the alloc-side layout walk; the runtime value can be a
    // trivial constant of the matching type.
    let value_vids: Vec<_> = value_types
        .iter()
        .map(|ty| {
            let op = match ty {
                IrType::I64 => Op::ConstI64(0),
                IrType::F64 => Op::ConstF64(0.0),
                IrType::Bool => Op::ConstBool(false),
                other => panic!(
                    "build_placeholder_enum_ir: unsupported value type {other:?} \
                     (extend the helper if a new actual-type case is needed)"
                ),
            };
            f.emit_value(entry, op, ty.clone(), None)
        })
        .collect();
    f.emit(
        entry,
        Op::EnumAlloc(enum_name.to_string(), 0, value_vids),
        IrType::EnumRef(enum_name.to_string(), Vec::new()),
        None,
    );
    f.set_terminator(entry, Terminator::Return(None));
    module.push_concrete(f);
    module
}

/// Run the compile pipeline against a hand-built IR module and assert
/// the resulting diagnostic mentions `EnumAlloc`, `placeholder`, and
/// the enum name. Used by both the all-placeholder and the mixed
/// placeholder/concrete rejection tests so the assertion wording
/// stays consistent across them.
fn assert_enum_alloc_placeholder_rejection(ir_module: &IrModule, enum_name: &str) {
    match phoenix_cranelift::compile(ir_module, Target::Wasm32Linear) {
        Ok(_) => panic!(
            "wasm32-linear must reject `EnumAlloc` into a multi-field variant \
             with any placeholder-typed declared field ({enum_name})"
        ),
        Err(e) if e.kind == CompileErrorKind::RuntimeWasmNotFound => panic!(
            "unexpected RuntimeWasmNotFound for {enum_name} — the placeholder \
             check should fire during op translation, before the runtime merge. \
             A regression that reordered the phases would surface here."
        ),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("EnumAlloc") && msg.contains("placeholder") && msg.contains(enum_name),
                "expected EnumAlloc/placeholder/{enum_name} in the diagnostic; got: {msg}"
            );
        }
    }
}

/// Pins the up-front rejection added to `Op::EnumAlloc` for
/// multi-field variants whose *declared* field list carries two or
/// more placeholder types. Without it, the IR could construct values
/// that no `Op::EnumGetField` could ever read back (every f_idx
/// would hit at least one placeholder at i != f_idx during the
/// offset-walk guard) — silent unreachable state.
#[test]
fn rejects_enum_alloc_with_multi_placeholder_variant() {
    // `BoxedPair::Pair(__generic, __generic)` — both declared fields
    // are placeholders. Value vids are concrete `Int`.
    let ir_module = build_placeholder_enum_ir(
        "BoxedPair",
        vec![IrType::I64, IrType::I64],
        &[0, 1],
        &[IrType::I64, IrType::I64],
    );
    assert_enum_alloc_placeholder_rejection(&ir_module, "BoxedPair");
}

/// Pins the *tightened* rejection: a multi-field variant with a
/// **single** placeholder field (and other concrete fields) must
/// also be rejected, because the alloc-side layout (computed from the
/// value vids' actual types) and the get-side layout (computed from
/// the declared types with `result_type` substituted at the
/// requested field) can disagree at other positions when the
/// placeholder's actual type differs in size or alignment from any
/// concrete field. The original 2025-Q1 rejection only caught the
/// all-placeholder case; this test fixes that gap.
///
/// Concrete divergence: declared = `[placeholder, I64]`, alloc'd
/// with values `[Bool, I64]`. Alloc lays out field 0 (Bool, align 4)
/// at offset 4, field 1 (I64, align 8) at offset 8. A later
/// `EnumGetField(field=0, result_type=Bool)` walks `[Bool, I64]` and
/// reads from offset 4 — fine. But the IR can also construct that
/// alloc and then **never** read field 0 back via `EnumGetField`
/// — the value just sits in the heap, and the inconsistency
/// surfaces only when a future PR removes the field 0 reject. Better
/// to fail at construction.
#[test]
fn rejects_enum_alloc_with_mixed_placeholder_and_concrete_fields() {
    // `MixedBag::V(__generic, I64)` — first field declared as a
    // placeholder, second declared concrete. Value vids are
    // `[Bool, I64]` (different size/align from any non-placeholder
    // value the get side might substitute).
    let ir_module = build_placeholder_enum_ir(
        "MixedBag",
        vec![IrType::Bool, IrType::I64], // gets overwritten at pos 0
        &[0],
        &[IrType::Bool, IrType::I64],
    );
    assert_enum_alloc_placeholder_rejection(&ir_module, "MixedBag");
}

/// Pins the defense-in-depth check added to `Op::EnumGetField` for a
/// `GENERIC_PLACEHOLDER`-typed `instr.result_type`. Without the check,
/// `is_gc_pointer_type` matches the placeholder sentinel (which is a
/// `StructRef("__generic", [])`), `phx_field_size_bytes` returns 4, and
/// `emit_field_load` emits a 4-byte `i32.load` — truncating an I64/F64
/// payload or returning only the `ptr` half of a `StringRef`. Sema
/// should annotate `instr.result_type` with a concrete type before
/// codegen, but the guard makes a regression fail loud at the get site
/// rather than miscompiling.
#[test]
fn rejects_enum_get_field_with_placeholder_result_type() {
    use phoenix_ir::types::GENERIC_PLACEHOLDER;

    // Build an enum with a single-field placeholder variant — that
    // alloc shape is allowed (only multi-field placeholder variants
    // are rejected at alloc time), so the rejection path we want
    // to exercise here is purely on the get side.
    let placeholder = IrType::StructRef(GENERIC_PLACEHOLDER.to_string(), Vec::new());
    let mut module = IrModule::new();
    module.enum_layouts.insert(
        "OneSlot".to_string(),
        vec![("V".to_string(), vec![placeholder.clone()])],
    );

    let mut f = IrFunction::new(
        FuncId(0),
        "main".to_string(),
        Vec::new(),
        Vec::new(),
        IrType::Void,
        None,
    );
    let entry = f.create_block();

    // Concrete-typed payload value (I64), so the alloc-side
    // placeholder check passes — the value vid carries a concrete IR
    // type even though the declared field type is the placeholder.
    let payload_vid = f.emit_value(entry, Op::ConstI64(0), IrType::I64, None);
    let enum_vid = f.emit_value(
        entry,
        Op::EnumAlloc("OneSlot".to_string(), 0, vec![payload_vid]),
        IrType::EnumRef("OneSlot".to_string(), Vec::new()),
        None,
    );
    // Hand-build the EnumGetField with `result_type = placeholder`
    // (the sema-regression shape this guard catches). A correctly
    // annotated IR would put `IrType::I64` here.
    f.emit_value(
        entry,
        Op::EnumGetField(enum_vid, 0, 0),
        placeholder.clone(),
        None,
    );
    f.set_terminator(entry, Terminator::Return(None));
    module.push_concrete(f);

    match phoenix_cranelift::compile(&module, Target::Wasm32Linear) {
        Ok(_) => panic!(
            "wasm32-linear must reject `Op::EnumGetField` whose `instr.result_type` \
             is the GENERIC_PLACEHOLDER sentinel — the silent-truncation path \
             this guards against would miscompile"
        ),
        Err(e) if e.kind == CompileErrorKind::RuntimeWasmNotFound => {
            eprintln!(
                "warning: skipping rejects_enum_get_field_with_placeholder_result_type \
                 — phoenix_runtime.wasm not built"
            );
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("EnumGetField")
                    && msg.contains("GENERIC_PLACEHOLDER")
                    && msg.contains("result type"),
                "expected EnumGetField/GENERIC_PLACEHOLDER/result type in the \
                 diagnostic; got: {msg}"
            );
        }
    }
}
