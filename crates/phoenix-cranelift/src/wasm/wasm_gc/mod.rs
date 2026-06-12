//! WASM GC backend for the Phoenix compiler.
//!
//! Parallel sibling of [`super::wasm`] (the wasm32-linear backend).
//! Emits WebAssembly that uses the **WASM GC proposal** for managed
//! references — Phoenix structs / enums / lists / maps / closures
//! become `(struct ...)` / `(array ...)` typed managed refs rather
//! than tagged offsets into a hand-managed linear-memory heap, and
//! the host VM (wasmtime under `-W gc=y`) handles tracing.
//!
//! See `docs/design-decisions.md` §Phase 2.4:
//! - **A** — dual-backend rationale (WASM GC primary, linear-memory
//!   fallback).
//! - **I** — runtime architecture: codegen emits all allocations
//!   inline; no Rust runtime crate is built or merged on this target.
//! - **J** — PR 5 MVP scope: `hello.phx`, `fibonacci.phx`, plus one
//!   struct fixture passing under `wasmtime -W gc=y`.
//! - **K** — codegen layout: this parallel module tree.
//!
//! # Scope (Phase 2.4 PR 5)
//!
//! This first slice wires up the dispatch and gets `hello.phx`
//! (`let x: Int = 42; print(x)`) compiling end-to-end. The translator
//! covers:
//!
//! - `Op::ConstI64` lowered to `i64.const + local.set`. `hello.phx`'s
//!   `let x: Int = 42` is *immutable*, so it binds the initializer's SSA
//!   value directly — no `Op::Alloca` / `Store` / `Load` is emitted for
//!   it. The translator does cover that trio (a *mutable* `let mut`
//!   lowers `Alloca` + `Store`, and each read emits `Load`), exercised by
//!   the `let mut` integration test rather than by `hello.phx`.
//! - `Op::BuiltinCall("print", Int)` routed through a synthesized
//!   `phx_print_i64` helper that converts the integer to decimal ASCII
//!   in a linear-memory scratch buffer and calls `fd_write` inline. No
//!   imported `phoenix-runtime` symbol — per decision I, wasm32-gc owns
//!   its helper synthesis.
//! - WASI `_start` → user `main` plumbing. No `phx_gc_enable` /
//!   `phx_gc_shutdown` calls (the host GC handles memory; the
//!   shadow-stack suppression from §2.3 decision A applies here).
//!
//! Subsequent slices add arithmetic + recursion + multi-block control
//! flow (fibonacci), `Op::ConstString` + `print(String)` via a
//! synthesized `phx_print_str` helper, then `Op::StructAlloc` /
//! `Op::StructGetField` lowered as `struct.new` / `struct.get` (the
//! first struct fixture) — each with its own design-decisions entry as
//! the type-mapping question gets locked in.
//!
//! # File layout
//!
//! - [`module_builder`] — `ModuleBuilder`, the per-section assembler
//!   for the wasm32-gc pipeline. Distinct from the wasm32-linear
//!   builder because the section needs diverge (no runtime merge, GC
//!   types in the type section, synthesized helpers rather than
//!   imported `phx_*` symbols).
//! - [`translate`] — Phoenix IR → WASM-GC instruction translation.
//!
//! Shared scaffolding lives in the parent `super::` namespace
//! (`super::type_interner` is reused; `super::runtime_discovery` /
//! `super::runtime_merge` are not referenced — there's no runtime to
//! discover or merge on this target).

use phoenix_ir::module::IrModule;

use crate::error::CompileError;

mod enums;
mod float_helpers;
mod lists;
mod module_builder;
mod ryu_tables;
mod string_helpers;
mod translate;

use module_builder::{ModuleBuilder, scan_helper_needs};

/// Compile a Phoenix IR module to a WASM-GC WebAssembly module.
///
/// Returns the raw bytes of a `.wasm` module that:
/// - Declares a single linear memory (small — used only for WASI
///   iovec staging and the print helper's digit scratch buffer during
///   the MVP phase).
/// - Imports WASI's `fd_write` for stdout **when the program calls
///   `print`** (a non-printing module imports nothing). No `proc_exit`
///   import — `_start` returns normally; panic routing through
///   `proc_exit` lands in a later slice.
/// - Synthesizes a `phx_print_i64(n: i64)` helper **when the program
///   calls `print`** — it converts the integer to decimal ASCII in a
///   linear-memory scratch buffer, stages an iovec at a fixed offset,
///   and calls `fd_write(1, iovec_ptr, 1, nwritten_ptr)`. A program that
///   never prints emits no helper.
/// - Translates each concrete Phoenix function into a WASM function.
/// - Exports a WASI-compatible `_start` that calls `main`.
/// - Exports `memory` so WASI hosts can read the iovec staging area.
///
/// The output is intended to load under `wasmtime -W gc=y` and pass
/// `wasmparser`'s GC-aware validation; the integration test in
/// `crates/phoenix-cranelift/tests/compile_wasm_gc.rs` exercises both.
pub(crate) fn compile_wasm_gc(ir_module: &IrModule) -> Result<Vec<u8>, CompileError> {
    let mut builder = ModuleBuilder::new();

    // The WASI `fd_write` import and the synthesized `phx_print_i64`
    // helper exist only to service `print(Int)`. A program that never
    // prints carries neither — no dead WASI import (which would impose a
    // host capability the module never exercises) and no uncallable
    // helper body. The small linear memory stays unconditional: it is
    // always exported as `memory`, and the upcoming String slice stages
    // literals there regardless of whether the program prints.
    let needs_print = translate::module_calls_print(ir_module);
    let helper_needs = scan_helper_needs(ir_module);
    // Declare every Phoenix struct's nominal WASM-GC type first, so
    // any subsequent function signature whose params/returns include
    // `IrType::StructRef(name, _)` can encode the right
    // `HeapType::Concrete(struct_idx)` at intern time. See §Phase 2.4
    // decision K.1.
    //
    // String types (`$bytes` + `$string`) come right after — same
    // ordering constraint, with the user-program structs preceding so
    // their declarations don't need to forward-reference `$string` (no
    // struct in slice 1 has a String field; the cross-cutting case is
    // a follow-up slice). See §Phase 2.4 decision K.2.
    builder.declare_phoenix_structs(ir_module)?;
    if helper_needs.string_types {
        builder.declare_string_types();
    }
    // Declare every Phoenix enum's parent + per-variant subtypes after
    // structs and string types (so enum variant fields of those types
    // can encode their indices) and before function signatures or the
    // memory declaration. See §Phase 2.4 decision K.4.
    builder.declare_phoenix_enums(ir_module)?;
    // Lists come after structs / strings / enums (element types of
    // those kinds encode their indices) and before any signature
    // touching `ListRef` / `ListBuilderRef` is interned. Nested
    // `List<List<T>>` instantiations are declared inner-first inside
    // this pass. See §Phase 2.4 decision K.7.
    builder.declare_phoenix_lists(ir_module)?;
    builder.declare_memory();
    if needs_print {
        builder.declare_imports();
        builder.declare_print_helper()?;
    }
    // String helpers depend on the imports (only `phx_print_str` needs
    // `fd_write`, but `declare_string_helpers` checks per-flag). The
    // `fd_write` lookup inside `declare_string_helpers` is safe to rely
    // on because `print_str` can only be set by a `print(...)` call
    // site, which `module_calls_print` also detects — so
    // `helper_needs.print_str` implies `needs_print`, which means
    // `declare_imports` ran just above and `fd_write_idx` is populated.
    // The lookup still errors (rather than panics) if that invariant is
    // ever broken by a change to `module_calls_print`.
    builder.declare_string_helpers(helper_needs)?;
    // `print(Bool)` lowers inline — no helper to synthesize — but the
    // two `"true\n"` / `"false\n"` active data segments still have to
    // be declared (and counted toward the `DataCount` section) before
    // the function-body translation can stage iovecs at their fixed
    // offsets. See §Phase 2.4 decision K.3.
    if helper_needs.print_bool {
        builder.declare_bool_data();
    }
    // `phx_print_f64` synthesis needs the `fd_write` import, which is
    // declared above this point.
    builder.declare_print_f64_helper(helper_needs)?;
    // `phx_fmod` is a pure function (no imports); it only shares the
    // helpers' immediate-emit-before-deferred-body ordering constraint.
    builder.declare_fmod_helper(helper_needs)?;

    // Declare Phoenix user functions (so call sites can resolve their
    // WASM function indices before any body is emitted), then emit
    // each body. `_start` is declared last so it can call `main` by
    // index after all user functions are in place.
    builder.declare_phoenix_functions(ir_module)?;
    builder.declare_start();
    builder.emit_exports();
    builder.emit_phoenix_bodies(ir_module)?;
    builder.emit_start_body()?;

    builder.finish()
}
