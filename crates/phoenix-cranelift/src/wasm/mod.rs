//! WebAssembly backend for the Phoenix compiler.
//!
//! Translates a Phoenix [`IrModule`] into a `.wasm` module via the
//! Bytecode Alliance's [`wasm-encoder`] crate. Cranelift's `wasm32`
//! ISA support is *input*-side only (it consumes WASM for wasmtime),
//! so the native backend's Cranelift IR machinery is not reused — this
//! module is a parallel translator targeting the WebAssembly
//! instruction set directly. See
//! `docs/design-decisions.md` §Phase 2.4 decision A0 for the rationale.
//!
//! # Scope (Phase 2.4 PR 3a)
//!
//! PR 2 shipped the WASM emission scaffolding plus three hand-synthesized
//! print helpers (`phx_print_i64` / `phx_print_str` / `phx_print_bool`)
//! that called WASI `fd_write` directly. PR 3a replaces those helpers
//! with the *real* `phoenix-runtime` crate compiled to a wasm32-wasip1
//! cdylib and merged into the user-program module via embed-and-merge
//! ([decision F](`docs/design-decisions.md`)). The real runtime brings
//! the GC, every `phx_str_*` / `phx_list_*` / `phx_map_*` /
//! `phx_gc_*` symbol, and the shadow-stack helpers — laying the
//! foundation for PR 3b's full IR-op coverage. PR 3a itself keeps the
//! translator's user-side surface unchanged: hello.phx still works,
//! and nothing more.
//!
//! # Memory layout
//!
//! The merged module's linear memory is shared between the runtime
//! and any user-emitted data segments.
//!
//! - `[0, runtime_data_end)` — runtime data segments (Rust's static
//!   initializers: panic messages, format strings, allocator state).
//!   Offsets are baked into the runtime's compiled image.
//! - `[runtime_data_end, data_cursor)` — user-emitted data segments
//!   from `Op::ConstString` (PR 3b's deliverable). PR 3a doesn't
//!   write here.
//! - Shadow-stack region — sized and placed by the runtime's compiled
//!   image (rustc's `wasm32-wasip1` lowering reserves a region between
//!   `__data_end` and `__heap_base` with `__stack_pointer` initialized
//!   to the high address and growing downward). The merge preserves
//!   whatever layout the runtime baked in; user code never touches
//!   this region directly.
//! - `[__heap_base, memory_end)` — heap for the runtime allocator.
//!
//! Minimum memory size is `max(17, runtime_min_pages)` 64-KiB pages.
//! 17 pages (~1 MB) gives the GC enough room for current fixtures;
//! the runtime floor is whatever its compiled image declared.
//!
//! # File layout
//!
//! - [`module_builder`] — `ModuleBuilder`, the per-section assembler
//!   driving the merge / declare / emit pipeline.
//! - [`type_interner`] — `TypeInterner`, the WASM type-section
//!   deduplicator. Runtime function-signature types also flow through
//!   it (cheap dedup is always safe), so the type section stays
//!   minimal even when runtime and user signatures coincide.
//! - [`runtime_discovery`] — `find_runtime_wasm_or_diagnostic`, the
//!   search for the compiled `phoenix_runtime.wasm` artifact.
//! - [`runtime_merge`] — `merge_runtime`, the wasmparser → wasm-encoder
//!   embed-and-merge step using `wasm_encoder::reencode::Reencode`.
//! - [`translate`] — Phoenix IR → WASM function-body translation.
//!
//! # PR 3b heap_base bump
//!
//! Today the runtime's compiled image bakes `__heap_base` at "end of
//! the runtime's data section." PR 3a doesn't emit any user data, so
//! that's fine. When PR 3b starts appending user data segments above
//! the runtime's, the bytes will land in the heap region — the
//! allocator will overwrite them on first allocation. PR 3b therefore
//! needs to *rewrite* the `__heap_base` global initializer (or the
//! global's only writer in the runtime's `_initialize`-equivalent
//! path) to the new post-user-data offset. Surfacing this here so the
//! constraint is visible when PR 3b lands rather than discovered as a
//! corruption bug.

use phoenix_ir::module::IrModule;

use crate::error::CompileError;

mod builtins;
mod gc_root;
mod heap_layout;
mod js_glue;
pub(crate) use js_glue::GENERATED_MARKER as GENERATED_GLUE_MARKER;
mod module_builder;
pub(crate) mod runtime_discovery;
mod runtime_merge;
mod translate;
mod type_interner;
mod validate;
mod wasm_gc;

pub(crate) use wasm_gc::compile_wasm_gc;

use module_builder::ModuleBuilder;

/// Compile a Phoenix IR module to a linear-memory WebAssembly module.
///
/// Returns the raw bytes of a `.wasm` module that:
/// - Merges the pre-compiled `phoenix_runtime.wasm` (located via
///   `runtime_discovery::find_runtime_wasm_or_diagnostic`) into the
///   output, contributing every `phx_*` runtime symbol plus the WASI
///   imports it needs.
/// - Translates each concrete Phoenix function into a WASM function.
/// - Exports a WASI-compatible `_start` that wires
///   `phx_gc_enable` → user `main` → `phx_gc_shutdown`.
/// - Exports `memory` so WASI hosts can read the iovec staging area.
///
/// The output is well-formed enough to load under `wasmtime` and pass
/// `wasmparser` validation; the integration test in
/// `crates/phoenix-cranelift/tests/compile_wasm_linear.rs` exercises
/// both.
pub(super) fn compile_wasm_linear(ir_module: &IrModule) -> Result<Vec<u8>, CompileError> {
    // Validate the IR *before* locating the runtime artifact. The
    // structural rejections (no `main`, bad `main` shape, unsupported
    // op, layout-unstable `EnumAlloc`) depend only on the IR, so firing
    // them here makes the diagnostic independent of whether
    // `phoenix_runtime.wasm` is present — otherwise a missing artifact's
    // `RuntimeWasmNotFound` (from the merge below) would preempt them.
    // See `validate` and the `rejects_*` integration tests.
    validate::validate(ir_module)?;

    // Locate the pre-built runtime. The `RuntimeWasmNotFound → CompileError`
    // conversion (impl in `runtime_discovery`) tags the error with
    // `CompileErrorKind::RuntimeWasmNotFound` so integration tests can
    // branch on it without grepping the message text. The diagnostic
    // itself carries the canonical "how do I fix this?" hint
    // (`cargo build -p phoenix-runtime --target wasm32-wasip1`).
    let runtime_path = runtime_discovery::find_runtime_wasm_or_diagnostic()?;
    let runtime_bytes = std::fs::read(&runtime_path).map_err(|e| {
        CompileError::new(format!(
            "wasm32-linear: could not read runtime at {}: {e}",
            runtime_path
        ))
    })?;

    let mut builder = ModuleBuilder::new();

    // Declare custom `extern js` imports BEFORE the merge. Imports
    // and local functions share one index space (imports first), so these must
    // occupy import indices ahead of the runtime's local functions — declaring
    // them after the merge would collide a new import index with an already-
    // assigned runtime-local index. `add_local_function` is purely relative to
    // the live `import_func_count`, so the merge's index bookkeeping shifts up
    // consistently. See `translate::declare_extern_imports`.
    translate::declare_extern_imports(&mut builder, ir_module)?;

    // Merge: every `phx_*` runtime symbol must resolve to a merged-module
    // function index before the user-side translator (which looks up names like
    // `phx_print_i64`) runs.
    let outcome = runtime_merge::merge_runtime(&mut builder, &runtime_bytes)?;
    builder.finalize_merge(
        outcome.phx_funcs,
        outcome.runtime_min_pages,
        outcome.runtime_max_pages,
        outcome.stack_pointer_global,
    );

    // Memory is declared after merge so the page floor can absorb the
    // runtime's required minimum. The user-side data section (PR 3b)
    // will start above the runtime's data, which the merge tracked
    // via `data_cursor`.
    builder.declare_memory();

    builder.declare_phoenix_functions(ir_module)?;
    // After every Phoenix function has a stable WASM function index,
    // pre-scan the IR for `Op::ClosureAlloc(fid, _)` references and
    // declare/populate the closure funcref table that
    // `Op::CallIndirect` reads. Must come *after*
    // `declare_phoenix_functions` (the element segment writes WASM
    // function indices read from `phx_user_funcs`) and *before*
    // `emit_phoenix_bodies` (so body emission can resolve
    // `closure_target_slot(fid)` and `require_closure_table_idx()`).
    builder.register_closure_table(ir_module)?;
    builder.declare_start();
    builder.emit_exports();
    builder.emit_phoenix_bodies(ir_module)?;
    builder.emit_start_body()?;

    Ok(builder.finish())
}

/// Generate the paired JavaScript glue for a wasm32-linear module's `extern js`
/// imports, or `None` if the program declares/calls no externs
/// (in which case no `.js` sidecar is emitted — the bare `.wasm` runs under
/// wasmtime as before). Driven by the same [`translate::collect_externs`] table
/// the import section was built from, so imports and glue can't drift.
pub(super) fn generate_js_glue(ir_module: &IrModule) -> Result<Option<String>, CompileError> {
    let externs = translate::collect_externs(ir_module)?;
    if externs.is_empty() {
        return Ok(None);
    }
    Ok(Some(js_glue::generate(&externs)?))
}
