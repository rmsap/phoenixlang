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

mod module_builder;
mod translate;

use module_builder::ModuleBuilder;

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
    // Declare every Phoenix struct's nominal WASM-GC type first, so
    // any subsequent function signature whose params/returns include
    // `IrType::StructRef(name, _)` can encode the right
    // `HeapType::Concrete(struct_idx)` at intern time. See §Phase 2.4
    // decision K.1.
    builder.declare_phoenix_structs(ir_module)?;
    builder.declare_memory();
    if needs_print {
        builder.declare_imports();
        builder.declare_print_helper()?;
    }

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
