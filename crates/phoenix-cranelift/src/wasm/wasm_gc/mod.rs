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

mod closures;
mod dyn_trait;
mod enums;
mod float_helpers;
mod lists;
mod map_hash_index;
mod maps;
mod module_builder;
mod option_result;
mod ryu_tables;
mod string_helpers;
mod tostring_helpers;
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
    // String types (`$bytes` + `$string`) are declared first: they
    // reference no other type, while struct fields may reference
    // `$string` — a `name: String` field encodes
    // `HeapType::Concrete($string_idx)` inline, so the index must
    // exist before the struct is declared. See §Phase 2.4 K.1 / K.2.
    if helper_needs.string_types {
        builder.declare_string_types();
    }
    // Reserve each `dyn Trait`'s `$dyn_T` type-section index *before*
    // structs and lists, so a `dyn` struct field or a `List<dyn T>`
    // element — declared below — can embed `(ref null $dyn_T)`. The
    // slots are filled later by `declare_phoenix_dyn`; the whole GC type
    // graph lands in one rec group (closed below), which makes the
    // forward reference legal. See §Phase 2.4 decision K.10.
    builder.reserve_phoenix_dyn(ir_module);
    // Reserve every concrete Phoenix struct's nominal WASM-GC index next,
    // so (a) function signatures touching `StructRef` encode the right
    // `HeapType::Concrete(struct_idx)` at intern time (K.1) and (b) the
    // enum / list / map / closure / dyn types declared below can
    // reference a struct (an enum payload, a `List<MyStruct>` element).
    // The struct *bodies* are filled by `define_phoenix_structs` after
    // those types exist, so reference-typed struct fields resolve their
    // targets — a forward reference made legal by the single rec group.
    // See §Phase 2.4 decisions K.1 / K.11.
    builder.reserve_phoenix_structs(ir_module);
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
    // Closures last among the type declarations: capture fields and
    // user param/result types may reference any of the above. (The
    // reverse — `List<Closure>` elements — is deferred until a fixture
    // needs it; see §Phase 2.4 K.8.)
    builder.declare_phoenix_closures(ir_module)?;
    // Maps after lists (a `$map_KV` reuses the K.7 `List<K>`/`List<V>`
    // array+struct types for its `$keys`/`$vals` and for
    // `keys()`/`values()`) and after closures, before signatures
    // touching `MapRef` are interned. See §Phase 2.4 decision K.9.
    builder.declare_phoenix_maps(ir_module)?;
    // dyn-trait types last among the type declarations: method
    // param/return types may reference any of the above. See §Phase 2.4
    // decision K.10.
    builder.declare_phoenix_dyn(ir_module)?;
    // Every type a struct field can reference (other structs via the
    // reserved indices, plus enums / lists / maps / closures / dyn) now
    // exists, so fill the struct bodies reserved above. A field whose
    // target has a *higher* type index than the struct is a forward
    // reference — legal inside the rec group sealed just below. See
    // §Phase 2.4 decision K.11.
    builder.define_phoenix_structs(ir_module)?;
    // All WASM-GC types (and the `$fn_SIG` / `$dynfn` func types they
    // reference) are now declared; seal them into one `(rec …)` group so
    // they may mutually forward-reference — a `dyn` field in a struct, a
    // `List<dyn T>` element, a `dyn` method returning a `List`. Every
    // func type interned after this point (the WASI `fd_write` import,
    // user functions, helpers, trampolines, `_start`) emits standalone,
    // so the import stays type-compatible with the host. See §Phase 2.4
    // decision K.10.
    builder.close_type_rec_group();
    builder.declare_memory();
    // Declare the `extern js` custom imports before any
    // local function: imports occupy the low function indices, so they must be in
    // place before `declare_print_helper` / the user functions append locals.
    // A no-op for a program with no externs.
    builder.declare_extern_imports(ir_module)?;
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
    builder.declare_float_format_helpers(helper_needs)?;
    // `phx_fmod` is a pure function (no imports); it only shares the
    // helpers' immediate-emit-before-deferred-body ordering constraint.
    builder.declare_fmod_helper(helper_needs)?;
    // `toString` constructors — pure construction (no `fd_write`),
    // but the Float arm reuses `phx_ryu_format_f64` from just above
    // and every arm allocates a `$string` (types declared earlier).
    builder.declare_tostring_helpers(helper_needs)?;

    // Declare Phoenix user functions (so call sites can resolve their
    // WASM function indices before any body is emitted), then emit
    // each body. `_start` is declared last so it can call `main` by
    // index after all user functions are in place.
    builder.declare_phoenix_functions(ir_module)?;
    // The deterministic `(concrete, trait)` vtable order, computed once
    // and threaded through the three deferred dyn passes below
    // (declare → vtable globals → bodies) so they provably iterate in
    // lockstep — the function/code sections stay parallel only if all
    // three agree on order. See §Phase 2.4 K.10.
    let dyn_vtable_keys = dyn_trait::ordered_vtable_keys(ir_module);
    // dyn trampolines are deferred-body functions that `call` user
    // functions — declare their signatures right after the user
    // functions (so the function/code sections stay parallel) and emit
    // their bodies right after the user bodies. See §Phase 2.4 K.10.
    builder.declare_dyn_trampolines(ir_module, &dyn_vtable_keys)?;
    // `ref.func` validation requires every closure target and dyn
    // trampoline in an `(elem declare func …)` segment — emitted now
    // that the function indices exist. See §Phase 2.4 K.8 / K.10.
    builder.emit_closure_elem_decls()?;
    builder.emit_dyn_trampoline_elem_decls();
    // Vtable globals are built now (the trampoline indices they
    // `ref.func` exist) so each `DynAlloc`'s `global.get` resolves
    // when its body is emitted below.
    builder.emit_dyn_vtable_globals(ir_module, &dyn_vtable_keys)?;
    builder.declare_start();
    builder.emit_exports();
    builder.emit_phoenix_bodies(ir_module)?;
    // dyn trampoline bodies last among the deferred bodies (after the
    // user bodies), keeping the function/code sections parallel.
    builder.emit_dyn_trampoline_bodies(ir_module, &dyn_vtable_keys)?;
    builder.emit_start_body()?;

    builder.finish()
}
