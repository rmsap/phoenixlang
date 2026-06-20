//! Per-function Phoenix IR → WebAssembly translation.
//!
//! # Op surface (PR 3b + 3c so far)
//!
//! - **Constants:** [`Op::ConstI64`], [`Op::ConstBool`],
//!   [`Op::ConstString`] (data-section pointer + length pair).
//! - **Integer arithmetic:** [`Op::IAdd`] / [`Op::ISub`] / [`Op::IMul`] /
//!   [`Op::IDiv`] / [`Op::IMod`] / [`Op::INeg`]. Wrap-on-overflow for
//!   `+`/`-`/`*`/unary-`-` (matches the `Int` spec); `IDiv`/`IMod` trap
//!   on divide-by-zero and signed overflow, matching native's panic.
//! - **Integer & bool comparisons:** [`Op::IEq`] / [`Op::INe`] /
//!   [`Op::ILt`] / [`Op::IGt`] / [`Op::ILe`] / [`Op::IGe`],
//!   [`Op::BoolEq`] / [`Op::BoolNe`] / [`Op::BoolNot`].
//! - **Direct calls:** [`Op::Call`] to a Phoenix user function (resolved
//!   via [`ModuleBuilder::require_phx_user_func`]). Mutual recursion is
//!   supported — every concrete function is registered before any body
//!   is emitted.
//! - **Built-ins:** [`Op::BuiltinCall`] with name `"print"` routes to
//!   the matching `phx_print_*` runtime export, dispatching on the
//!   Phoenix `IrType` (`Int` / `Bool` / `String`); `"toString"` routes
//!   to the `phx_*_to_str` family via [`emit_sret_string_call`].
//! - **String operations:** [`Op::StringConcat`] lowers to
//!   `phx_str_concat` via the same sret helper.
//! - **Mutable variables (pre-mem2reg):** [`Op::Alloca`] / [`Op::Load`]
//!   / [`Op::Store`]. The slot's binding is the storage; loads/stores
//!   shuffle between the slot's locals and the loaded/stored value's
//!   locals.
//! - **Control flow:** every [`Terminator`] except `Switch` and `None`
//!   (which the IR verifier rejects). Multi-block functions use the
//!   loop+switch dispatcher described in
//!   [`docs/design-decisions.md`](../../../../docs/design-decisions.md)
//!   §Phase 2.4 decision G; single-block functions skip the dispatcher.
//! - **Multi-slot values:** [`IrType::StringRef`] is two WASM slots
//!   (`(i32 ptr, i32 len)`). Function parameters and returns flatten via
//!   [`wasm_valtypes_for`]; SSA bindings carry a `Vec<u32>` of locals;
//!   non-entry block params lower the same way — `emit_block_param_copies`
//!   walks each arg's slot list against the target param's slot list,
//!   so `StringRef` block params work end-to-end alongside scalars.
//!
//! # Shadow-stack root emission
//!
//! Every ref-typed Phoenix binding (entry-block params, non-entry
//! block params, ref-result instructions) is assigned a slot in a
//! per-function shadow-stack frame before any body code is emitted
//! ([`assign_gc_root_slots`] + [`setup_gc_frame`]). Function entry
//! calls `phx_gc_push_frame(n_roots)`; every value-producing op that
//! lands a heap pointer in a binding follows with a
//! `phx_gc_set_root(frame, slot, value)` so a subsequent `phx_gc_alloc`'s
//! mark phase observes a live root for the binding. `Op::Store`,
//! `Jump`, and `Branch` re-use the binding's pre-assigned slot when
//! updating a stored value so the GC always sees the local's current
//! value, not its definition-site value. Every `Return` terminator
//! emits `phx_gc_pop_frame` first; `Unreachable` traps the program
//! and skips the pop (no further WASM execution observes the frame).
//! Data-section literals (`Op::ConstString`) and zero-initialized
//! `Op::Alloca` storage skip the root call entirely — the data-
//! section pointer is never registered with the GC, and the alloca
//! slot is already null after `phx_gc_push_frame`'s init.
//!
//! # Deferred to PR 3c (still unimplemented)
//!
//! List / map / closure allocation, `defer` exit-path emission. Each
//! rejection in this file cites PR 3c so a regression in the
//! deferred-error wording is visible.
//!
//! # SSA → WASM-locals mapping
//!
//! WebAssembly's MVP has no SSA — it has typed locals and an operand
//! stack. Each Phoenix `ValueId` that an instruction defines maps to
//! one or more WASM locals (single-slot for most types; two slots for
//! `StringRef`). Phoenix function parameters bind to the auto-declared
//! WASM parameter locals; the entry block's params alias them. The
//! loop+switch dispatcher (multi-block functions) allocates one i32
//! local for the "next block ID" dispatch value, then a fresh local
//! per non-entry block parameter.
//!
//! # wasm-encoder construction order
//!
//! [`wasm_encoder::Function`] takes its local declarations up front,
//! before any instruction can be pushed. We therefore buffer
//! instructions and the locals list during the IR walk, then finalize
//! into a `Function` at the end. The buffer holds `Instruction<'static>`
//! — every op landed here owns its data, so there's no borrow churn.

use std::collections::HashMap;

use phoenix_ir::block::{BasicBlock, BlockId};
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::IrFunction;
use phoenix_ir::terminator::Terminator;
use phoenix_ir::types::IrType;
use phoenix_runtime::gc::TypeTag;
use wasm_encoder::{BlockType, Function, Instruction, ValType};

use super::builtins::translate_builtin_call;
use super::gc_root::{
    emit_gc_pop_frame, emit_gc_set_root, op_produces_heap_pointer, setup_gc_frame,
};
use super::heap_layout::{
    DYN_VTABLE_ENTRY_SIZE, EnumLayout, LIST_HEADER, StructLayout, align_up, compute_enum_layout,
    compute_struct_layout, compute_variant_field_offsets, field_memarg, i32_memarg,
    is_gc_pointer_type, is_i32_field, phx_field_align_bytes, phx_field_size_bytes,
};
use super::module_builder::ModuleBuilder;
use crate::error::CompileError;

/// One block-param's WASM-local list, in declaration order — a single
/// entry for `Int` / `Bool` / pointer-typed params, two entries
/// (`[ptr_local, len_local]`) for `StringRef`. Aliased here so the
/// nested `Vec<...>` in [`FuncTranslateCtx::block_param_locals`]
/// reads as "list of param slot lists" at call sites.
type ParamSlotLocals = Vec<u32>;

/// One block-param's record: the Phoenix `ValueId` paired with its
/// WASM-local list. The vid is retained so `emit_block_param_copies`
/// can re-root the param on the shadow stack after writing the
/// updated value into its locals (`Jump` / `Branch` reassigns a
/// param's value each time control transfers to the block; the GC
/// must see the latest write, not the value at definition time).
///
/// `Clone` exists solely so [`emit_block_param_copies`] can lift the
/// record list out of a `&FuncTranslateCtx` borrow with `.to_vec()`
/// before it switches to `&mut FuncTranslateCtx` for emission. The
/// records are small (a `ValueId` + a `Vec<u32>` of at most two
/// entries on the current type-flatten surface), so the clone is
/// cheap relative to restructuring the code to interleave reads and
/// writes through the same borrow.
#[derive(Clone)]
struct BlockParamRecord {
    vid: ValueId,
    locals: ParamSlotLocals,
}

/// Flatten a Phoenix [`IrType`] into the WASM `ValType` slots that
/// represent it on the value stack. Most types are single-slot,
/// `StringRef` is two (`i32 ptr`, `i32 len`), and `Void` is zero.
/// GC-heap reference types (`List` / `Map` / `Closure` / user
/// `Struct` / `Enum`) each flatten to a single `i32` GC pointer
/// (see [`is_gc_pointer_type`]).
///
/// Used for both function-signature flattening
/// ([`super::module_builder::ModuleBuilder::declare_phoenix_functions`])
/// and per-SSA-value local allocation (each `Vec<ValType>` entry gets
/// its own WASM local).
///
/// `StructRef` / `EnumRef` flatten so user struct/enum alloc +
/// field-access codegen can hand back single-`i32` GC pointers.
/// `ListRef`, `MapRef`, and `ClosureRef` have no body-translator
/// support for their *alloc* / *method* ops yet, but parameters and
/// returns of these types must still flatten here so user methods
/// declared against them produce valid WASM signatures during
/// declaration — the body translator rejects the unsupported ops with
/// a per-op diagnostic when they're actually invoked.
pub(super) fn wasm_valtypes_for(ty: &IrType) -> Result<Vec<ValType>, CompileError> {
    match ty {
        IrType::I64 => Ok(vec![ValType::I64]),
        IrType::F64 => Ok(vec![ValType::F64]),
        IrType::Bool => Ok(vec![ValType::I32]),
        IrType::Void => Ok(Vec::new()),
        // Two-slot fat pointers: `StringRef` is `(ptr, len)`; `DynRef`
        // is `(data_ptr, vtable_ptr)` per the `dyn Trait` ABI. Both
        // flatten to `[i32, i32]` on wasm32 — the second slot stores
        // a different semantic value (length vs. vtable address) but
        // the slot shape, alignment, and load/store sequence are
        // identical, so the codegen treats them uniformly.
        IrType::StringRef | IrType::DynRef(_) => Ok(vec![ValType::I32, ValType::I32]),
        ty if is_gc_pointer_type(ty) => Ok(vec![ValType::I32]),
        // `JsValue` is a single `i32` JS-side table index on `wasm32-linear`
        // (Phase 2.5, decision D) — a plain opaque host handle, not a
        // Phoenix-heap pointer. It crosses the `extern js` boundary as one i32;
        // the JS glue that owns the handle space lands in PR 6.
        IrType::JsValue => Ok(vec![ValType::I32]),
        _ => Err(unsupported(ty, "wasm32-linear value representation")),
    }
}

/// Declare a custom WASM function import for every *called* `extern js` function.
/// Walks the distinct extern signatures from [`collect_externs`] (which derives
/// each one from its call site and dedups by `(module, name)`), flattens each
/// into a WASM import signature — the per-parameter `IrType`s as flattened
/// params, the return `IrType` as the result — and declares one import per
/// extern.
///
/// Only externs actually *called* get an import: a declared-but-uncalled extern
/// produces no `Op::ExternCall`, so [`collect_externs`] never sees it and no
/// dangling import is declared.
///
/// Coupling the import signature to call-site arg/result IR types (rather than
/// the extern's declared signature in sema's table) is sound because sema
/// coerces every argument to the declared parameter type and rejects
/// non-marshallable types, so every site of a given extern agrees and matches
/// the declaration. The body translator's `Op::ExternCall` arm reads the same
/// per-site bindings when it emits the `call`, so the declared import signature
/// and the emitted operands stay in lockstep by construction. The JS glue
/// generator consumes the very same [`collect_externs`] table, so the imports
/// and their glue thunks can't drift either.
///
/// This is a deliberate **separate pass**, not folded into
/// [`ModuleBuilder::declare_phoenix_functions`]: extern imports **must run
/// before the runtime merge** so they occupy import indices ahead of the
/// runtime's local functions (see [`ModuleBuilder::declare_extern_import`]),
/// whereas `declare_phoenix_functions` runs *after* the merge. The two can't
/// share one walk.
pub(super) fn declare_extern_imports(
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
) -> Result<(), CompileError> {
    for sig in collect_externs(ir_module)? {
        let mut params = Vec::new();
        for (i, ty) in sig.params.iter().enumerate() {
            params.extend(wasm_valtypes_for(ty).map_err(|e| {
                CompileError::new(format!(
                    "wasm32-linear: `extern js` call `{}.{}` parameter {i}: {}",
                    sig.module, sig.name, e.message
                ))
            })?);
        }
        let returns = wasm_valtypes_for(&sig.return_type).map_err(|e| {
            CompileError::new(format!(
                "wasm32-linear: `extern js` call `{}.{}` return type: {}",
                sig.module, sig.name, e.message
            ))
        })?;
        b.declare_extern_import(&sig.module, &sig.name, &params, &returns);
    }
    Ok(())
}

// The IR-level `extern js` analysis (`ExternSig` / `collect_externs` /
// `CallbackSig` / `callback_sigs_in_externs` / `collect_callback_signatures` /
// `callback_sig_is_glue_supported`) is backend-neutral and lives in
// [`crate::extern_abi`], shared with the native binding. Re-exported here so the
// WASM call sites (import declaration, glue generator, trampoline emitter)
// keep referring to `translate::*` unchanged.
pub(super) use crate::extern_abi::{
    CallbackSig, ExternSig, callback_sigs_in_externs, collect_callback_signatures, collect_externs,
};

/// The exported name of the WASM `call_indirect` trampoline for a callback
/// signature: `__phoenix_invoke_closure_<param-codes>_to_<ret-code>` (e.g. every
/// `(Int) -> Void` callback in the module routes through
/// `__phoenix_invoke_closure_i_to_v`). Built from the shared
/// [`crate::extern_abi::callback_sig_codes`] so the trampoline emitter and the
/// glue derive the same name; the native binding formats its own
/// `phx_invoke_closure_*` name from the same codes. Returns `None` for a
/// non-marshallable signature, so callers never name a trampoline they can't also
/// marshal.
pub(super) fn closure_trampoline_name(sig: &CallbackSig) -> Option<String> {
    crate::extern_abi::callback_sig_codes(sig)
        .map(|(params, ret)| format!("__phoenix_invoke_closure_{params}_to_{ret}"))
}

/// Reject a placeholder-typed field at codegen. Used at both the
/// alloc and get sites for enum-variant fields where a `result_type`
/// (or value-vid `ir_type`) carrying the `GENERIC_PLACEHOLDER`
/// sentinel would otherwise silently size as a 4-byte i32 via
/// `is_gc_pointer_type` matching `StructRef("__generic", [])` —
/// truncating I64/F64 payloads or yielding only the `ptr` half of a
/// `StringRef`. `site_label` is interpolated into the diagnostic so
/// the failing op is identifiable; sema should have annotated the
/// type concretely before reaching either call site.
pub(super) fn reject_placeholder_field_type(
    ty: &IrType,
    site_label: &str,
) -> Result<(), CompileError> {
    if ty.is_generic_placeholder() {
        return Err(CompileError::new(format!(
            "wasm32-linear: {site_label} has unresolved IR type \
             (`GENERIC_PLACEHOLDER`); sema/IR should have annotated this \
             with a concrete type before codegen (internal compiler bug)"
        )));
    }
    Ok(())
}

/// The canonical "this IR op has no wasm32-linear lowering yet"
/// diagnostic. Single-sourced here so the up-front validation pass
/// ([`super::validate`], which rejects the deferred-op families
/// *before* the runtime artifact is even located) and the
/// `translate_instruction` catch-all (the authoritative backstop for
/// any op the validation pass doesn't pre-screen) word it identically.
/// A regression in the wording surfaces in `rejects_unsupported_ir_op`.
pub(super) fn unsupported_op_error(op: &Op) -> CompileError {
    CompileError::new(format!(
        "wasm32-linear: IR op `{op:?}` not yet supported \
         (Phase 2.4 PR 3c — see docs/design-decisions.md §Phase 2.4 \
         for the linear-memory port's full op coverage)"
    ))
}

/// Validate an `Op::EnumAlloc` against its declared enum layout: the
/// variant index is in range, the field-value count matches the
/// declared field count, and the variant is layout-stable (no
/// multi-field variant carries a placeholder-typed declared field —
/// alloc/get offset walks can disagree otherwise; see
/// `heap_layout.rs::EnumLayout`). Only fully-concrete variants or
/// single-field placeholder variants (`Option<T>::Some(T)`,
/// `Result<T,_>::Ok(T)`) are layout-stable.
///
/// Single-sourced here so the up-front validation pass
/// ([`super::validate`]) and the `Op::EnumAlloc` translation arm apply
/// the *same* rejection — the validation pass fires it before the
/// runtime merge so the diagnostic doesn't depend on the runtime
/// artifact being present (see `rejects_enum_alloc_with_*`).
pub(super) fn check_enum_alloc_layout_stable(
    declared_layout: &EnumLayout,
    name: &str,
    variant_idx: u32,
    given_field_count: usize,
) -> Result<(), CompileError> {
    let v_idx = variant_idx as usize;
    if v_idx >= declared_layout.variant_field_types.len() {
        return Err(CompileError::new(format!(
            "wasm32-linear: `Op::EnumAlloc({name}, variant={variant_idx})` \
             references variant index out of range (enum has {} variants)",
            declared_layout.variant_field_types.len(),
        )));
    }
    let declared_field_count = declared_layout.variant_field_types[v_idx].len();
    if declared_field_count != given_field_count {
        return Err(CompileError::new(format!(
            "wasm32-linear: `Op::EnumAlloc({name}, variant={variant_idx})` \
             was given {given_field_count} field values but the variant declares \
             {declared_field_count} fields",
        )));
    }
    let declared_placeholder_count = declared_layout.variant_field_types[v_idx]
        .iter()
        .filter(|ty| ty.is_generic_placeholder())
        .count();
    if declared_placeholder_count > 0 && declared_field_count > 1 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `Op::EnumAlloc({name}, variant={variant_idx})` \
             targets a multi-field variant ({declared_field_count} fields) with \
             {declared_placeholder_count} placeholder-typed declared field(s); the \
             alloc-side layout (from value-vid types) and the later \
             `Op::EnumGetField` layout (from declared types) can disagree on \
             other-position offsets when any field is a placeholder. Only \
             single-field placeholder variants (e.g. `Option<T>`) are supported \
             by the current enum layout."
        )));
    }
    Ok(())
}

/// Emit a load of the field of type `ty` at `field_offset` from the
/// allocation whose base pointer is in `base_ptr_local`. Single-slot
/// types push one value onto the operand stack; `StringRef` pushes
/// two (`ptr` then `len`, in declaration order) so the caller's
/// `emit_store_result` can pop them into the matching `[ptr, len]`
/// locals via reverse-order `LocalSet`. `MemArg::align` comes from
/// `phx_field_align_bytes` via `align_log2`. Used by both
/// `Op::StructGetField` and `Op::EnumGetField`.
pub(super) fn emit_field_load(
    ctx: &mut FuncTranslateCtx,
    base_ptr_local: u32,
    field_offset: u32,
    ty: &IrType,
) -> Result<(), CompileError> {
    if matches!(ty, IrType::Void) {
        return Err(CompileError::new(
            "wasm32-linear: `Void` has no field-load representation \
             (internal: sema/IR should reject Void-typed struct/enum fields)",
        ));
    }
    let memarg = field_memarg(field_offset, ty)?;
    match ty {
        IrType::I64 => {
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::I64Load(memarg));
        }
        IrType::F64 => {
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::F64Load(memarg));
        }
        // `Bool` and every GC-pointer reference type collapse to one
        // `i32.load` at the field offset — only the per-type alignment
        // hint (computed by `field_memarg`) can differ. `is_i32_field`
        // keeps this arm and the matching `emit_field_store` arm in
        // lockstep: a new GC-pointer variant added to `is_gc_pointer_type`
        // automatically picks up both paths.
        ty if is_i32_field(ty) => {
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::I32Load(memarg));
        }
        // Two-slot fat pointers: `StringRef = (ptr, len)`, `DynRef =
        // (data_ptr, vtable_ptr)`. Read slot 0 at `field_offset`, slot
        // 1 at `field_offset + 4`. Push them in declaration order so
        // the caller's `emit_store_result` pops via reverse-order
        // `local.set`.
        IrType::StringRef | IrType::DynRef(_) => {
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::I32Load(memarg));
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::I32Load(i32_memarg(field_offset + 4)));
        }
        _ => return Err(unsupported(ty, "field load")),
    }
    Ok(())
}

/// Emit a store of the value held in `value_locals` to the field of
/// type `ty` at `field_offset` from the allocation whose base pointer
/// is in `base_ptr_local`. Single-slot types take one local; multi-
/// slot `StringRef` takes two (`[ptr_local, len_local]`) and emits two
/// `i32.store`s back-to-back at `field_offset` and `field_offset + 4`.
/// Sourcing from locals (rather than the operand stack) keeps the
/// caller from having to thread both slots through any intermediate
/// alloc/store sequence. Used by `Op::StructAlloc`, `Op::StructSetField`,
/// and `Op::EnumAlloc`.
pub(super) fn emit_field_store(
    ctx: &mut FuncTranslateCtx,
    base_ptr_local: u32,
    field_offset: u32,
    ty: &IrType,
    value_locals: &[u32],
) -> Result<(), CompileError> {
    if matches!(ty, IrType::Void) {
        return Err(CompileError::new(
            "wasm32-linear: `Void` has no field-store representation \
             (internal: sema/IR should reject Void-typed struct/enum fields)",
        ));
    }
    let memarg = field_memarg(field_offset, ty)?;
    match ty {
        IrType::I64 => {
            debug_assert_eq!(value_locals.len(), 1, "I64 must be single-slot");
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::LocalGet(value_locals[0]));
            ctx.emit(Instruction::I64Store(memarg));
        }
        IrType::F64 => {
            debug_assert_eq!(value_locals.len(), 1, "F64 must be single-slot");
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::LocalGet(value_locals[0]));
            ctx.emit(Instruction::F64Store(memarg));
        }
        // Single `i32` store covers both `Bool` (0/1 widened to i32)
        // and every GC-pointer reference type. See the matching arm
        // in `emit_field_load` for the alignment-source rationale.
        ty if is_i32_field(ty) => {
            debug_assert_eq!(
                value_locals.len(),
                1,
                "Bool / ref types must be single-slot"
            );
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::LocalGet(value_locals[0]));
            ctx.emit(Instruction::I32Store(memarg));
        }
        // Two-slot fat pointers (see the matching `emit_field_load`
        // arm). `StringRef` is `(ptr, len)`; `DynRef` is `(data_ptr,
        // vtable_ptr)`. Both store slot 0 at `field_offset` and slot
        // 1 at `field_offset + 4`.
        IrType::StringRef | IrType::DynRef(_) => {
            debug_assert_eq!(value_locals.len(), 2, "2-slot type must be 2 slots");
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::LocalGet(value_locals[0]));
            ctx.emit(Instruction::I32Store(memarg));
            ctx.emit(Instruction::LocalGet(base_ptr_local));
            ctx.emit(Instruction::LocalGet(value_locals[1]));
            ctx.emit(Instruction::I32Store(i32_memarg(field_offset + 4)));
        }
        _ => return Err(unsupported(ty, "field store")),
    }
    Ok(())
}

/// Map a Phoenix function's return [`IrType`] to a vector of WASM
/// [`ValType`]s. `Void` returns map to the empty vector; `StringRef`
/// returns to `[I32, I32]` (multi-value return for the fat pointer).
pub(super) fn wasm_return_valtypes(ty: &IrType) -> Result<Vec<ValType>, CompileError> {
    wasm_valtypes_for(ty)
}

/// Flatten a list of Phoenix parameter types into a WASM signature's
/// flattened param-list. Each multi-slot Phoenix type (currently only
/// `StringRef`) expands to multiple WASM `ValType`s in declaration
/// order — `(ptr, len)` for strings — so an `extern "C"` Rust fn with
/// a `PhxFatPtr` param sees `(i32, i32)` in WASM, matching what
/// `phoenix-runtime`'s compiled `phx_str_*` exports declare.
pub(super) fn flatten_param_types(params: &[IrType]) -> Result<Vec<ValType>, CompileError> {
    let mut out = Vec::with_capacity(params.len());
    for ty in params {
        out.extend(wasm_valtypes_for(ty)?);
    }
    Ok(out)
}

/// Translate a `wasmparser::ValType` into the corresponding
/// `wasm_encoder::ValType`. Used by the runtime-merge step
/// (`super::runtime_merge`) when re-encoding type-section entries
/// from the pre-compiled `phoenix_runtime.wasm`. Rejects ref types
/// the runtime shouldn't be producing on wasm32-wasip1 today.
pub(super) fn wasm_valtype_from_parser(ty: wasmparser::ValType) -> Result<ValType, CompileError> {
    match ty {
        wasmparser::ValType::I32 => Ok(ValType::I32),
        wasmparser::ValType::I64 => Ok(ValType::I64),
        wasmparser::ValType::F32 => Ok(ValType::F32),
        wasmparser::ValType::F64 => Ok(ValType::F64),
        wasmparser::ValType::V128 => Ok(ValType::V128),
        wasmparser::ValType::Ref(ref_ty) => {
            // Reference types (funcref, externref, the WASM-GC heap
            // types) are not expected from a wasm32-wasip1 cdylib. If
            // a future Rust toolchain emits them (closures backed by
            // ref-types?), the diagnostic points at this site.
            Err(CompileError::new(format!(
                "wasm32-linear: runtime exposes ref-typed value (`{ref_ty:?}`); \
                 not handled by the embed-and-merge step yet"
            )))
        }
    }
}

/// Translate a concrete Phoenix function body into a complete WASM
/// [`Function`] (locals + body instructions).
///
/// Single-block functions are emitted directly — the function body is
/// the block's instruction stream followed by its terminator. This
/// keeps PR 3a's hello.phx bytecode shape unchanged.
///
/// Multi-block functions are emitted via a loop+switch dispatcher
/// ([decision G](`docs/design-decisions.md`)): each basic block lives
/// inside a labeled WASM `block` nested inside an outer `loop`, and a
/// `br_table` at the top of the loop reads a "next block ID" local
/// and branches to the matching labeled block. Block-param SSA values
/// get fresh locals at the dispatcher's entry; `Jump` / `Branch`
/// terminators copy their args into those locals before re-entering
/// the dispatch.
pub(super) fn translate_function(
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    func: &IrFunction,
) -> Result<Function, CompileError> {
    if func.blocks.is_empty() {
        return Err(CompileError::new(format!(
            "wasm32-linear: function `{}` has no blocks",
            func.name
        )));
    }

    let mut ctx = FuncTranslateCtx::new(func)?;
    // Shadow-stack root setup: assign a unique slot to every ref-typed
    // binding before any body code runs, then emit `phx_gc_push_frame`
    // with the total slot count. Subsequent ref-producing ops emit
    // `phx_gc_set_root` to populate slots; `Op::Store` / `Jump` /
    // `Branch` re-use a binding's pre-assigned slot when updating the
    // stored value. Every Return / Unreachable terminator emits
    // `phx_gc_pop_frame` before exiting.
    //
    // For functions with no ref-typed bindings (most arithmetic-only
    // helpers — fibonacci is the gate example), the map is empty and
    // we skip the push/pop entirely so the bytecode shape stays
    // minimal.
    setup_gc_frame(&mut ctx, b, func)?;
    if func.blocks.len() == 1 {
        translate_block(&mut ctx, b, ir_module, &func.blocks[0], None)?;
    } else {
        translate_multi_block(&mut ctx, b, ir_module, func)?;
    }
    // Every WASM function body must terminate with an `end` opcode
    // (0x0B) — `wasm_encoder::Function` requires it regardless of
    // reachability. Emitting it here as a single fixed point keeps
    // terminator translators from each having to think about
    // function-level closing.
    ctx.emit(Instruction::End);
    Ok(ctx.into_function())
}

/// Emit the loop+switch dispatcher for a multi-block function.
///
/// Structure (for 3 blocks, bb_0..bb_2):
///
/// ```text
/// loop $L                              ;; depth N+1 inside body
///   block $bb_2                        ;; depth N
///     block $bb_1                      ;; depth N-1
///       block $bb_0                    ;; depth 0
///         local.get $dispatch
///         br_table 0 1 2 0             ;; default = bb_0 (unreachable)
///       end                            ;; close $bb_0
///       ;; bb_0 body+terminator (br $L or return)
///     end                              ;; close $bb_1
///     ;; bb_1 body+terminator
///   end                                ;; close $bb_2
///   ;; bb_2 body+terminator
/// end                                  ;; close $L
/// unreachable
/// ```
///
/// `br_table`'s targets are *label indices* relative to the current
/// nesting depth. At the dispatcher's `br_table` site, $bb_i has
/// depth `(n_blocks - 1 - i)`, so the table is filled with that
/// formula and `0` (= $bb_0) as the unreachable default.
///
/// Each block's body terminator emits the appropriate WASM control
/// transfer: `Return` → `return` (function-level exit, ignores
/// nesting); `Jump` / `Branch` → set the dispatch local + `br <depth
/// of $L from here>`. The depth of $L from inside bb_i's body is
/// `(n_blocks - 1 - i)`.
fn translate_multi_block(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    func: &IrFunction,
) -> Result<(), CompileError> {
    let n_blocks = func.blocks.len();
    debug_assert!(n_blocks > 1, "translate_multi_block called with <= 1 block");

    // Allocate the dispatch local *first* so its index is stable
    // before any block-param locals get assigned. Initial value of an
    // i32 local is 0, which matches the entry-block ID (`BlockId(0)`)
    // — no explicit init needed.
    let dispatch_local = ctx.allocate_dispatch_local();

    // Allocate locals for non-entry block params (the entry block's
    // params are the function parameters, already bound in
    // `FuncTranslateCtx::new`). Each non-entry block-param gets its
    // own WASM local; `Jump` / `Branch` terminators copy their args
    // into these locals before dispatching.
    //
    // The dispatcher relies on `func.blocks[i].id == BlockId(i)` —
    // both for the br_table identity-vector (target = block index =
    // BlockId) and for the `Jump`/`Branch` terminators that write the
    // target BlockId into the dispatch local. `IrFunction::create_block`
    // (phoenix-ir/src/module.rs) appends in BlockId order, so this
    // holds by construction today; the assert catches a future IR
    // refactor that reorders or deletes blocks before that change
    // ships an opaque wasmparser-validation error.
    for (block_idx, block) in func.blocks.iter().enumerate().skip(1) {
        let block_id = BlockId(block_idx as u32);
        debug_assert_eq!(
            block.id, block_id,
            "wasm32-linear: block at array index {block_idx} has id {:?}, \
             expected {:?} — the loop+switch dispatcher assumes \
             `func.blocks[i].id == BlockId(i)` (internal compiler bug — \
             IR builder invariant violated)",
            block.id, block_id,
        );
        for (vid, ty) in &block.params {
            // Allocate the multi-slot binding for this block param
            // (single local for `Int`/`Bool`/pointer types, two locals
            // for `StringRef`). The binding registers the param's slot
            // locals so `Jump`/`Branch` terminators copying args into
            // the block can write each slot independently.
            let slot_locals = ctx.allocate_locals_for_ir_type(*vid, ty.clone())?;
            ctx.register_block_param(block_id, *vid, slot_locals);
        }
    }

    // Open the outer loop.
    ctx.emit(Instruction::Loop(BlockType::Empty));

    // Open one labeled block per Phoenix basic block, nested deepest-
    // first (bb_0 innermost). The `n_blocks` `End` markers later close
    // these in reverse — see body emission below.
    for _ in 0..n_blocks {
        ctx.emit(Instruction::Block(BlockType::Empty));
    }

    // Emit the dispatch table at the innermost point. The br_table
    // targets are *label depths* relative to the dispatcher site;
    // $bb_i sits at depth `i` (innermost-first: the last block opened
    // is $bb_0 at depth 0, the first opened is $bb_(N-1) at depth
    // N-1), so the table is the identity vector `[0, 1, ..., N-1]`.
    // The default target (consulted when the index is out of range)
    // is `0` (= $bb_0) — unreachable in a well-formed program but
    // required by the br_table opcode.
    ctx.emit(Instruction::LocalGet(dispatch_local));
    let table_targets: Vec<u32> = (0..n_blocks as u32).collect();
    ctx.emit(Instruction::BrTable(
        std::borrow::Cow::Owned(table_targets),
        0,
    ));

    // Emit each block's body+terminator. Between every pair of
    // bodies, close the corresponding labeled block (so the next br
    // target's label index naturally decreases).
    for (block_idx, block) in func.blocks.iter().enumerate() {
        // Re-assert the BlockId-vs-index invariant at the body-emission
        // site too: a future change that walked `func.blocks` in a
        // different order (or that filtered/deduped blocks between the
        // two loops) would otherwise emit bodies in br_table-disagreeing
        // order. Cheap insurance against a subtle miscompile.
        debug_assert_eq!(
            block.id,
            BlockId(block_idx as u32),
            "wasm32-linear: block at array index {block_idx} has id {:?} \
             at body-emission time — dispatcher index ordering invariant \
             violated (internal compiler bug)",
            block.id,
        );
        // Close the labeled block whose body we're about to emit. For
        // bb_0 this closes the innermost `(block $bb_0)`; for bb_N-1
        // this closes the outermost.
        ctx.emit(Instruction::End);
        // Emit bb_i body. Terminator handling needs to know the depth
        // of $L from this position so a `br $L` translates to the
        // right label index.
        let depth_to_loop = (n_blocks - 1 - block_idx) as u32;
        translate_block(
            ctx,
            b,
            ir_module,
            block,
            Some(DispatcherContext {
                depth_to_loop,
                dispatch_local,
            }),
        )?;
    }

    // Close the outer loop and emit an unreachable sentinel — every
    // block ends with a terminator (`return` or `br $L`), so falling
    // off the loop is impossible. Emitting `unreachable` here keeps
    // wasmparser happy without us having to declare the loop's
    // signature.
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::Unreachable);
    Ok(())
}

/// Codegen-side metadata recorded for every Phoenix `ValueId` bound
/// during translation: the WASM local slot(s) it occupies, and the
/// original Phoenix [`IrType`].
///
/// Most Phoenix types are single-slot (one WASM local each) — `Int`,
/// `Float`, `Bool`. `StringRef` is two-slot: `locals[0]` holds the
/// `i32` data pointer and `locals[1]` holds the `i32` byte length.
/// PR 3c's `List` / `Map` / `Closure` references will each be
/// single-slot pointers into the GC heap. The slot count for a given
/// type is fixed by [`wasm_valtypes_for`].
///
/// `ir_type` is retained so dispatchers like the print-builtin
/// translator can route on the *Phoenix* type even when several IR
/// types collapse to the same WASM `ValType` (e.g. `Bool` and a
/// raw `i32` heap-pointer both occupy `ValType::I32`).
pub(super) struct ValueBinding {
    pub(super) locals: Vec<u32>,
    pub(super) ir_type: IrType,
}

impl ValueBinding {
    /// Convenience accessor for the single-slot case. Panics on a
    /// multi-slot binding — callers that handle multi-slot values
    /// (`print(str)`, returning `String`, etc.) go through `locals`
    /// directly via `emit_load_all` / `emit_store_result`.
    ///
    /// Asserts in release as well as debug: a silent `locals[0]` read
    /// on a `StringRef` binding would forward the `ptr` slot only and
    /// miscompile (the `len` slot would still occupy a WASM local with
    /// no codegen referencing it, and the operand stack would be off
    /// by one for the next instruction). Catching this at codegen time
    /// is better than a far-removed wasmparser validation error.
    pub(super) fn single_local(&self) -> u32 {
        assert_eq!(
            self.locals.len(),
            1,
            "single_local called on a multi-slot binding for IR type {:?} \
             (internal compiler bug — caller should route through `locals` \
             via emit_load_all / emit_store_result)",
            self.ir_type
        );
        self.locals[0]
    }
}

/// Per-function translation state. Buffers instructions until
/// finalization so `wasm_encoder::Function::new` can be called with
/// the complete locals list.
pub(super) struct FuncTranslateCtx {
    /// Instruction buffer. Replayed in [`Self::into_function`].
    instructions: Vec<Instruction<'static>>,
    /// Locals declared by the body, in declaration order, in the
    /// WASM run-length-encoded form `wasm_encoder::Function::new`
    /// expects: each entry is `(count, ValType)` and consecutive
    /// allocations of the same `ValType` merge into the last entry.
    /// Index `n_params + i` (where `i` is the count of locals declared
    /// before this point) is the WASM local index.
    pending_locals: Vec<(u32, ValType)>,
    /// Phoenix `ValueId` → ([`ValueBinding`]) for both parameter
    /// locals (assigned at function entry) and instruction-result
    /// locals (assigned as ops are visited).
    bindings: HashMap<ValueId, ValueBinding>,
    /// Per-block param records, indexed by [`BlockId`] then by
    /// param-position (declaration order). Each entry pairs the
    /// param's Phoenix `ValueId` with its WASM-local list — the vid
    /// lets `emit_block_param_copies` re-root the param on the shadow
    /// stack after a write, the locals carry the storage. The entry-
    /// block params bind to function-parameter locals and are NOT
    /// listed here (`BlockId(0)` resolves to function params via
    /// `func.param_types`). Non-entry blocks' params get fresh locals
    /// allocated in [`translate_multi_block`].
    block_param_locals: HashMap<BlockId, Vec<BlockParamRecord>>,
    /// Cache of computed struct layouts (keyed by struct name) for the
    /// current function body. `Op::StructAlloc` / `Op::StructGetField`
    /// / `Op::StructSetField` all index into the same per-name layout,
    /// and a single source-level struct can be touched many times in
    /// one function (`p.x = p.y + p.z` is three accesses to `Point`'s
    /// layout). Caching here amortizes the `HashMap` walk + per-field
    /// alignment math across those accesses; the cache is scoped to
    /// one function because layouts are pure functions of the IR
    /// module, never mutated during translation.
    struct_layout_cache: HashMap<String, StructLayout>,
    /// Sibling cache for enum-variant layouts. Same scope and
    /// rationale as [`Self::struct_layout_cache`]. Indexed by enum
    /// name; each entry surfaces all declared variants.
    enum_layout_cache: HashMap<String, EnumLayout>,
    /// Next WASM local index to assign for an instruction-result
    /// value. Initialized past the parameter locals.
    next_local: u32,

    /// Merged-module WASM local holding the shadow-stack frame
    /// pointer for the current function. Allocated at function-entry
    /// time when [`assign_gc_root_slots`] returns a non-empty map;
    /// `None` for ref-free functions (most arithmetic-only helpers
    /// fall in this bucket). Consulted by `emit_gc_set_root` and
    /// `emit_gc_pop_frame`.
    gc_frame_local: Option<u32>,
    /// Phoenix `ValueId` → shadow-stack root-slot index. Populated
    /// once at function-entry time by [`assign_gc_root_slots`]; one
    /// slot per ref-typed binding (entry params, non-entry block
    /// params, ref-result instructions). Op::Store / Jump / Branch
    /// re-use the binding's pre-assigned slot when updating the
    /// stored value, so the GC always sees the local's current
    /// value rather than the stale write at definition site.
    gc_root_slot_for: HashMap<ValueId, u32>,

    /// Snapshot of the enclosing function's `capture_types` (cloned
    /// from [`IrFunction::capture_types`] at construction). Read by
    /// `Op::ClosureLoadCapture` to walk capture widths and compute
    /// the byte offset of capture `capture_idx` within the closure
    /// heap object. Always set; an empty vector means the current
    /// function is not a closure body.
    current_capture_types: Vec<IrType>,
}

/// Dispatcher context shared by [`translate_block`] and the
/// terminator translator when the function uses loop+switch dispatch.
/// `Some` for multi-block functions, `None` for single-block (which
/// can `return` directly without touching the dispatch local).
#[derive(Debug, Clone, Copy)]
struct DispatcherContext {
    /// Label depth from a block's body to the outer `(loop $L)`.
    /// Used by `Jump` / `Branch` terminators to compute the operand
    /// to `br <depth>` that re-enters the dispatch.
    depth_to_loop: u32,
    /// WASM local holding the "next block ID" dispatch value.
    /// `Jump` / `Branch` write here before branching to `$L`.
    dispatch_local: u32,
}

impl FuncTranslateCtx {
    fn new(func: &IrFunction) -> Result<Self, CompileError> {
        let mut bindings: HashMap<ValueId, ValueBinding> = HashMap::new();

        // Bind entry-block params (if any) to their parameter local
        // slots. The IR puts params on `blocks[0].params` as
        // `(ValueId, IrType)` pairs; index in that vec matches the
        // WASM parameter local index.
        //
        // Codegen assumes `entry.params.len() == func.param_types.len()`
        // — the WASM function signature is computed from
        // `param_types` while every reference site indexes into
        // `entry.params`. A mismatch would silently shift local
        // indices and emit invalid WASM. The verifier enforces this
        // upstream; this debug_assert converts a verifier regression
        // into a localized panic instead of an opaque
        // wasmparser-validation failure.
        // WASM auto-declares one local per `ValType` in the function's
        // flat parameter list. Phoenix params expand to 1+ slots each
        // via `wasm_valtypes_for` — a `StringRef` param occupies two
        // consecutive WASM-local indices, etc. Walk the entry-block
        // params side-by-side with the function's `param_types` to
        // bind each Phoenix `ValueId` to the right slot range.
        let mut next_wasm_local: u32 = 0;
        if let Some(entry) = func.blocks.first() {
            debug_assert_eq!(
                entry.params.len(),
                func.param_types.len(),
                "wasm32-linear: entry-block param count ({}) does not match \
                 function param_types arity ({}) in `{}`",
                entry.params.len(),
                func.param_types.len(),
                func.name,
            );
            for (i, (vid, ty)) in entry.params.iter().enumerate() {
                // Cross-check that the entry-block param type matches
                // the function-signature param type. They agree today
                // by construction but a future refactor could silently
                // shift slot indices; the assertion catches that
                // before wasmparser does.
                let entry_slots = wasm_valtypes_for(ty)?;
                let sig_slots = wasm_valtypes_for(&func.param_types[i])?;
                debug_assert_eq!(
                    entry_slots, sig_slots,
                    "wasm32-linear: entry-block param {i} valtypes ({entry_slots:?}) \
                     disagree with function signature ({sig_slots:?}) in `{}`",
                    func.name,
                );
                let n_slots = entry_slots.len() as u32;
                let locals: Vec<u32> = (next_wasm_local..next_wasm_local + n_slots).collect();
                next_wasm_local += n_slots;
                bindings.insert(
                    *vid,
                    ValueBinding {
                        locals,
                        ir_type: ty.clone(),
                    },
                );
            }
        }
        // `next_wasm_local` now equals the flattened WASM parameter
        // count — also the index where instruction-result locals
        // start. The function signature was built from the same
        // flatten, so this matches the WASM ABI exactly.

        Ok(Self {
            instructions: Vec::new(),
            pending_locals: Vec::new(),
            bindings,
            block_param_locals: HashMap::new(),
            struct_layout_cache: HashMap::new(),
            enum_layout_cache: HashMap::new(),
            next_local: next_wasm_local,
            gc_frame_local: None,
            gc_root_slot_for: HashMap::new(),
            current_capture_types: func.capture_types.clone(),
        })
    }

    /// Compute the byte offset of capture `idx` within the closure
    /// heap object, given `target_ty` as the *concrete* type of that
    /// capture. The closure layout is `[fn_table_idx: i32 @ 0,
    /// capture_0, capture_1, ...]`: the fn-table-idx occupies offset
    /// 0..4, then each capture lives at its natural alignment past the
    /// previous one.
    ///
    /// Used by `Op::ClosureLoadCapture`, which passes `instr.result_type`
    /// (the sema-substituted, concrete capture type) as `target_ty`
    /// rather than reading `current_capture_types[idx]`. The reason:
    /// for closures defined inside a generic, the closure's declared
    /// `capture_types` can retain an unsubstituted `TypeVar("T")` (the
    /// inner closure function is shared across single-instantiation
    /// specializations rather than cloned). The instruction's
    /// `result_type` *is* substituted, so it gives the real width.
    /// Preceding captures are still walked from `current_capture_types`
    /// — for the single-capture case (the only generic shape exercised
    /// today; see `closures_over_generic.phx`) that slice is empty, so
    /// no `TypeVar` is evaluated. Any closure inside a generic with a
    /// *preceding* capture still typed `TypeVar` (i.e. a multi-capture
    /// generic, regardless of whether the instantiations are cross-width)
    /// instead surfaces a clean `phx_field_align_bytes` error rather than
    /// miscompiling; that case stays a documented known issue.
    ///
    /// Returns `Err` if `idx` is out of range — the IR verifier should
    /// reject `Op::ClosureLoadCapture` with an out-of-range index
    /// before codegen, so reaching the `Err` path indicates an
    /// internal compiler bug.
    pub(super) fn capture_offset(
        &self,
        idx: usize,
        target_ty: &IrType,
    ) -> Result<u32, CompileError> {
        if idx >= self.current_capture_types.len() {
            return Err(CompileError::new(format!(
                "wasm32-linear: `Op::ClosureLoadCapture` capture index {idx} \
                 out of range (current function has {} captures) — internal \
                 compiler bug",
                self.current_capture_types.len()
            )));
        }
        capture_byte_offset(&self.current_capture_types[..idx], target_ty)
    }

    /// Allocate the `i32` "next block ID" local used by the loop+switch
    /// dispatcher. Returns its WASM local index. Must be called before
    /// any block-param locals so the dispatch local sits at a stable
    /// position relative to function params (one local past the last
    /// function param).
    fn allocate_dispatch_local(&mut self) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(ValType::I32);
        self.next_local += 1;
        idx
    }

    /// Record `(vid, locals)` as one block-param record (in
    /// declaration order) for the given target `block`. For
    /// single-slot types `locals` is one entry; for `StringRef` it's
    /// two (`[ptr_local, len_local]`). Used by [`translate_multi_block`]
    /// when reserving locals for non-entry blocks' params. The vid is
    /// retained so re-rooting on the shadow stack after a write
    /// (`emit_block_param_copies`) doesn't have to re-walk
    /// `block.params` to find it.
    fn register_block_param(&mut self, block: BlockId, vid: ValueId, locals: ParamSlotLocals) {
        self.block_param_locals
            .entry(block)
            .or_default()
            .push(BlockParamRecord { vid, locals });
    }

    /// Look up the per-param WASM-local lists for a block's params, in
    /// declaration order. Empty if the block has no params (or is the
    /// entry block, whose params bind to function-parameter locals).
    fn block_param_locals_of(&self, block: BlockId) -> &[BlockParamRecord] {
        self.block_param_locals
            .get(&block)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Look up the struct layout for `struct_name`, computing and
    /// caching it on first reference within this function body. The
    /// returned [`StructLayout`] is cloned out of the cache so callers
    /// can hand it around without borrowing `self` further (and so
    /// subsequent `ctx.emit(...)` calls aren't blocked by an
    /// outstanding immutable borrow).
    fn cached_struct_layout(
        &mut self,
        ir_module: &phoenix_ir::module::IrModule,
        struct_name: &str,
    ) -> Result<StructLayout, CompileError> {
        if let Some(layout) = self.struct_layout_cache.get(struct_name) {
            return Ok(layout.clone());
        }
        let layout = compute_struct_layout(ir_module, struct_name)?;
        self.struct_layout_cache
            .insert(struct_name.to_string(), layout.clone());
        Ok(layout)
    }

    /// Same caching pattern as [`Self::cached_struct_layout`] for enums.
    fn cached_enum_layout(
        &mut self,
        ir_module: &phoenix_ir::module::IrModule,
        enum_name: &str,
    ) -> Result<EnumLayout, CompileError> {
        if let Some(layout) = self.enum_layout_cache.get(enum_name) {
            return Ok(layout.clone());
        }
        let layout = compute_enum_layout(ir_module, enum_name)?;
        self.enum_layout_cache
            .insert(enum_name.to_string(), layout.clone());
        Ok(layout)
    }

    /// Allocate a fresh single-slot WASM local of `wasm_ty` for the
    /// value `vid` (recording its originating Phoenix [`IrType`] for
    /// later type-based dispatch). Returns the WASM local index.
    /// Run-length-encodes consecutive same-type allocations so
    /// [`Self::into_function`] hands `wasm_encoder` the compressed
    /// locals representation directly.
    ///
    /// Multi-slot bindings (currently only `StringRef`) go through
    /// [`Self::allocate_locals_for_ir_type`] which allocates the
    /// matching number of consecutive slots.
    pub(super) fn allocate_local(&mut self, vid: ValueId, wasm_ty: ValType, ir_ty: IrType) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(wasm_ty);
        self.next_local += 1;
        self.bindings.insert(
            vid,
            ValueBinding {
                locals: vec![idx],
                ir_type: ir_ty,
            },
        );
        idx
    }

    /// Allocate the right number of consecutive WASM locals for the
    /// Phoenix [`IrType`] backing `vid`. Returns the locals' indices
    /// in declaration order (matching [`wasm_valtypes_for`]'s slot
    /// ordering — for `StringRef` that's `[ptr_local, len_local]`).
    pub(super) fn allocate_locals_for_ir_type(
        &mut self,
        vid: ValueId,
        ir_ty: IrType,
    ) -> Result<Vec<u32>, CompileError> {
        let slots = wasm_valtypes_for(&ir_ty)?;
        let locals: Vec<u32> = (0..slots.len() as u32)
            .map(|offset| self.next_local + offset)
            .collect();
        for vt in &slots {
            self.push_local_decl(*vt);
            self.next_local += 1;
        }
        self.bindings.insert(
            vid,
            ValueBinding {
                locals: locals.clone(),
                ir_type: ir_ty,
            },
        );
        Ok(locals)
    }

    /// Allocate the right number of consecutive WASM locals for an
    /// [`IrType`] *without* binding them to any Phoenix `ValueId` —
    /// scratch storage for intermediates that the inline list-method
    /// loops materialize (loaded elements, closure results) but that
    /// have no IR vid of their own. Returns the locals in declaration
    /// order (`[ptr, len]` for `StringRef`), matching
    /// [`Self::allocate_locals_for_ir_type`]'s ordering so the same
    /// `emit_field_load` / `emit_field_store` helpers apply.
    pub(super) fn allocate_locals_for_ir_type_anon(
        &mut self,
        ir_ty: &IrType,
    ) -> Result<Vec<u32>, CompileError> {
        let slots = wasm_valtypes_for(ir_ty)?;
        let locals: Vec<u32> = (0..slots.len() as u32)
            .map(|offset| self.next_local + offset)
            .collect();
        for vt in &slots {
            self.push_local_decl(*vt);
            self.next_local += 1;
        }
        Ok(locals)
    }

    /// Allocate a fresh single-slot WASM local of `wasm_ty` that is
    /// *not* bound to any Phoenix `ValueId` — used as scratch by
    /// codegen sequences that need a temporary (e.g. *sret* calls
    /// holding the result-area pointer between the `i32.sub` and the
    /// `i32.load`s). Returns the WASM local index. Does not appear in
    /// `bindings` so future `binding_of` lookups won't find it; that's
    /// intentional — temps are private to the emission sequence that
    /// created them.
    pub(super) fn allocate_temp_local(&mut self, wasm_ty: ValType) -> u32 {
        let idx = self.next_local;
        self.push_local_decl(wasm_ty);
        self.next_local += 1;
        idx
    }

    /// Append one local declaration of type `wasm_ty` to the RLE
    /// `pending_locals` list, merging into the trailing run when the
    /// types match.
    fn push_local_decl(&mut self, wasm_ty: ValType) {
        match self.pending_locals.last_mut() {
            Some((count, last_ty)) if *last_ty == wasm_ty => *count += 1,
            _ => self.pending_locals.push((1, wasm_ty)),
        }
    }

    /// Push an instruction onto the buffered body.
    pub(super) fn emit(&mut self, instr: Instruction<'static>) {
        self.instructions.push(instr);
    }

    // --- Shadow-stack accessors (consumed by `super::gc_root`) ---------
    //
    // The shadow-stack helpers live in a sibling module so this file
    // stays focused on the IR-op switch. They reach into the per-
    // function state via the small accessor surface below rather than
    // touching the fields directly — keeping the cross-module coupling
    // narrow.

    /// Currently-active shadow-stack frame-pointer local, or `None` for
    /// ref-free functions. Set once by `gc_root::setup_gc_frame` at
    /// function entry.
    pub(super) fn gc_frame_local(&self) -> Option<u32> {
        self.gc_frame_local
    }

    /// Record the frame-pointer local allocated at function entry.
    pub(super) fn set_gc_frame_local(&mut self, local: u32) {
        self.gc_frame_local = Some(local);
    }

    /// Pre-assigned shadow-stack slot for `vid`, if any. Returns `None`
    /// for non-ref bindings, which the per-op blanket set-root call
    /// uses as its "skip" signal.
    pub(super) fn gc_root_slot_of(&self, vid: ValueId) -> Option<u32> {
        self.gc_root_slot_for.get(&vid).copied()
    }

    /// Install the slot map produced by `assign_gc_root_slots` once,
    /// at function entry. Subsequent reads go through
    /// [`Self::gc_root_slot_of`].
    pub(super) fn install_gc_root_slot_map(&mut self, map: HashMap<ValueId, u32>) {
        self.gc_root_slot_for = map;
    }

    /// First (root-value-carrying) WASM local for `vid`'s binding, or
    /// `None` if the binding is missing. For multi-slot `StringRef`
    /// bindings this is the `i32 ptr` slot; slot 1 is the i32 length
    /// and is not a pointer. The shadow-stack helpers use this to
    /// look up the source local for a `phx_gc_set_root` call.
    pub(super) fn binding_root_local(&self, vid: ValueId) -> Option<u32> {
        self.bindings
            .get(&vid)
            .and_then(|binding| binding.locals.first().copied())
    }

    /// Emit a sequence of `local.get` for every WASM local backing
    /// the Phoenix `vid`, in declaration order. For single-slot
    /// values this is just one `local.get`; for `StringRef` it's
    /// `(local.get ptr_local) (local.get len_local)` so the operand
    /// stack ends up `[..., ptr, len]` — matching the call-arg order
    /// `phoenix-runtime`'s `extern "C" fn phx_print_str(ptr, len)`
    /// declares.
    pub(super) fn emit_load_all(&mut self, vid: ValueId) -> Result<(), CompileError> {
        let locals = self.binding_of(vid)?.locals.clone();
        for local in locals {
            self.emit(Instruction::LocalGet(local));
        }
        Ok(())
    }

    /// Allocate the locals for a `vid` of the given Phoenix
    /// [`IrType`] and emit `local.set` instructions to pop the call's
    /// return value(s) off the operand stack into them. The stack
    /// effect is "pop N values, store in declaration order": multi-
    /// value returns push the first result *deepest* (so `local.set`
    /// runs in reverse declaration order, popping from the top each
    /// time).
    ///
    /// For a `StringRef` return, the runtime's compiled function
    /// pushes `[ptr, len]` onto the stack at the call's exit; we
    /// `local.set $len_local` first (popping `len`), then
    /// `local.set $ptr_local` (popping `ptr`).
    pub(super) fn emit_store_result(
        &mut self,
        vid: ValueId,
        ir_type: IrType,
    ) -> Result<(), CompileError> {
        let locals = self.allocate_locals_for_ir_type(vid, ir_type)?;
        // Reverse: top-of-stack pops first, which is the last slot in
        // declaration order.
        for local in locals.iter().rev() {
            self.emit(Instruction::LocalSet(*local));
        }
        Ok(())
    }

    /// Look up the binding for a Phoenix `ValueId`. Errors indicate a
    /// use-before-definition, which is an IR bug — the verifier should
    /// catch this before codegen.
    pub(super) fn binding_of(&self, vid: ValueId) -> Result<&ValueBinding, CompileError> {
        self.bindings.get(&vid).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-linear: ValueId {vid:?} used before definition \
                 (internal compiler bug — IR verifier should have caught this)"
            ))
        })
    }

    /// Alias `vid` to `src`'s existing binding — same locals, same IR
    /// type, no `local.get`/`local.set` copies and no new locals. Used
    /// by source-level identities like `toString(String)`, where the
    /// result must resolve to the slots the argument already owns (and
    /// is already rooted through). The IR's single-assignment invariant
    /// guarantees `vid != src`.
    pub(super) fn alias_binding(&mut self, vid: ValueId, src: ValueId) -> Result<(), CompileError> {
        let src_binding = self.binding_of(src)?;
        let aliased = ValueBinding {
            locals: src_binding.locals.clone(),
            ir_type: src_binding.ir_type.clone(),
        };
        self.bindings.insert(vid, aliased);
        Ok(())
    }

    /// Finalize: produce a `wasm_encoder::Function` with the
    /// accumulated locals and instruction stream. The locals list is
    /// already in the `(count, ValType)` RLE shape `wasm_encoder`
    /// expects — built up incrementally by [`Self::allocate_local`].
    fn into_function(self) -> Function {
        let mut f = Function::new(self.pending_locals);
        for instr in &self.instructions {
            f.instruction(instr);
        }
        f
    }
}

/// Translate a single basic block: every instruction, then the
/// terminator. `dispatcher` is `None` for single-block functions and
/// `Some` for multi-block — the terminator translator needs it to
/// route `Jump` / `Branch` through the dispatcher.
fn translate_block(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    block: &BasicBlock,
    dispatcher: Option<DispatcherContext>,
) -> Result<(), CompileError> {
    for instr in &block.instructions {
        translate_instruction(ctx, b, ir_module, instr)?;
    }
    translate_terminator(ctx, b, &block.terminator, dispatcher)?;
    Ok(())
}

/// Pull the SSA result binding off an instruction. Every value-producing
/// op needs one; absence means the IR verifier let through an op that
/// would leave its result stranded on the WASM operand stack and fail
/// validation. Centralizing the diagnostic keeps the phrasing
/// consistent across the (still-growing) set of value-producing ops.
pub(super) fn expect_result(
    instr: &phoenix_ir::instruction::Instruction,
    op_name: &str,
) -> Result<ValueId, CompileError> {
    instr.result.ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-linear: `{op_name}` without a result binding would leave \
             a value stranded on the operand stack and fail validation \
             (internal compiler bug — IR verifier should have caught this)"
        ))
    })
}

/// Translate a single IR instruction.
fn translate_instruction(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match &instr.op {
        Op::ConstI64(n) => {
            let vid = expect_result(instr, "Op::ConstI64")?;
            ctx.emit(Instruction::I64Const(*n));
            let local = ctx.allocate_local(vid, ValType::I64, IrType::I64);
            ctx.emit(Instruction::LocalSet(local));
        }
        Op::ConstBool(v) => {
            let vid = expect_result(instr, "Op::ConstBool")?;
            ctx.emit(Instruction::I32Const(if *v { 1 } else { 0 }));
            let local = ctx.allocate_local(vid, ValType::I32, IrType::Bool);
            ctx.emit(Instruction::LocalSet(local));
        }
        Op::ConstF64(n) => {
            let vid = expect_result(instr, "Op::ConstF64")?;
            // `wasm_encoder::Instruction::F64Const` takes an `Ieee64`
            // wrapper rather than a raw `f64` so a bit-pattern can be
            // emitted without round-tripping through any NaN-
            // canonicalizing float op. `Ieee64`'s `From<f64>` impl
            // (wasm-encoder ≥0.244) reinterprets the bytes verbatim —
            // `u64::from_le_bytes(value.to_le_bytes())` — so the exact
            // source bits (sign / exponent / mantissa, including
            // signaling NaNs and the sign bit on `-0.0`) reach the
            // emitter unchanged.
            ctx.emit(Instruction::F64Const(wasm_encoder::Ieee64::from(*n)));
            let local = ctx.allocate_local(vid, ValType::F64, IrType::F64);
            ctx.emit(Instruction::LocalSet(local));
        }
        Op::ConstString(s) => {
            // Decision H (string-literal materialization): place the
            // bytes in a user data segment at low offsets (below the
            // runtime's stack region), then push the segment's
            // `(offset, len)` directly as a 2-slot `StringRef` fat
            // pointer. The runtime's `phx_print_str` / `phx_str_concat`
            // / etc. treat their fat-pointer args as borrowed slices,
            // so a data-section pointer composes uniformly with heap
            // pointers produced by runtime ops — no shadow-stack
            // rooting needed for literals (they live in the data
            // section forever).
            //
            // Bounded stack-collision risk: the runtime's stack grows
            // down from offset 1048576 and for the current fixture
            // set stays comfortably above the user-data region. The
            // codegen-time tripwire is `reserve_user_data`'s upper
            // bound (`USER_DATA_LIMIT = STACK_REGION_BASE -
            // STACK_SAFETY_MARGIN`); a measured stack high-water-
            // mark check is on the table for a Phase 2.5 follow-up
            // if deeper-recursion programs surface a collision.
            let vid = expect_result(instr, "Op::ConstString")?;
            let (offset, len) = b.reserve_user_data(s.as_bytes())?;
            ctx.emit(Instruction::I32Const(offset as i32));
            ctx.emit(Instruction::I32Const(len as i32));
            ctx.emit_store_result(vid, IrType::StringRef)?;
        }
        // Integer arithmetic. Phoenix maps `Int` → `i64`; every op
        // here produces an `i64` result.
        //
        // `IAdd` / `ISub` / `IMul` / `INeg` lower to WASM ops that
        // wrap silently on overflow, matching Phoenix's spec ("`Int`
        // is wrapping on overflow") and Rust's release-mode two's-
        // complement semantics on native.
        //
        // `IDiv` / `IMod` lower to `i64.div_s` / `i64.rem_s`. Per the
        // WASM spec, `div_s` traps on both division-by-zero and
        // signed-overflow (`i64::MIN / -1`); `rem_s` traps only on
        // zero. Native Rust panics on the same two cases. So the
        // trap behavior matches end-to-end, but the wrap rationale
        // above does *not* apply to these two — flag this if a
        // future op-coverage pass moves Div/Mod to a wrapping form.
        Op::IAdd(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "IAdd", Instruction::I64Add)?,
        Op::ISub(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "ISub", Instruction::I64Sub)?,
        Op::IMul(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "IMul", Instruction::I64Mul)?,
        Op::IDiv(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "IDiv", Instruction::I64DivS)?,
        Op::IMod(a, b_) => emit_i64_binop(ctx, instr, *a, *b_, "IMod", Instruction::I64RemS)?,
        Op::INeg(a) => {
            // WASM MVP has no `i64.neg`; the canonical lowering is
            // `0 - a` via i64.sub. Two's-complement wrap on
            // `i64::MIN` matches native (per `docs/design-decisions.md`
            // §Numeric error semantics — `Int` negation wraps).
            let vid = expect_result(instr, "Op::INeg")?;
            let a_local = ctx.binding_of(*a)?.single_local();
            ctx.emit(Instruction::I64Const(0));
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::I64Sub);
            let result_local = ctx.allocate_local(vid, ValType::I64, IrType::I64);
            ctx.emit(Instruction::LocalSet(result_local));
        }
        // Integer comparisons → produce a `Bool` (WASM `i32` 0/1).
        Op::IEq(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "IEq", Instruction::I64Eq)?,
        Op::INe(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "INe", Instruction::I64Ne)?,
        Op::ILt(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "ILt", Instruction::I64LtS)?,
        Op::IGt(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "IGt", Instruction::I64GtS)?,
        Op::ILe(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "ILe", Instruction::I64LeS)?,
        Op::IGe(a, b_) => emit_i64_cmp(ctx, instr, *a, *b_, "IGe", Instruction::I64GeS)?,
        // Bool ops.
        Op::BoolEq(a, b_) => emit_i32_cmp(ctx, instr, *a, *b_, "BoolEq", Instruction::I32Eq)?,
        Op::BoolNe(a, b_) => emit_i32_cmp(ctx, instr, *a, *b_, "BoolNe", Instruction::I32Ne)?,
        Op::BoolNot(a) => {
            let vid = expect_result(instr, "Op::BoolNot")?;
            let a_local = ctx.binding_of(*a)?.single_local();
            ctx.emit(Instruction::LocalGet(a_local));
            ctx.emit(Instruction::I32Eqz); // `eqz` = "is zero" = logical NOT for 0/1 bool
            let result_local = ctx.allocate_local(vid, ValType::I32, IrType::Bool);
            ctx.emit(Instruction::LocalSet(result_local));
        }
        // Direct function call to a Phoenix-user function.
        // `Op::Call(func_id, type_args, args)` — type_args are erased
        // post-monomorphization (the IR carries them for now but every
        // call into a concrete function has none left to resolve).
        Op::Call(func_id, type_args, args) => {
            // Catch a sema/IR regression that lets an unmonomorphized
            // call reach codegen. Without this, the call would silently
            // resolve to the template's `FuncId` and likely miscompile
            // (or hit `require_phx_user_func`'s missing-id error, which
            // is less specific). Debug-only because monomorphization is
            // a hard precondition the IR verifier enforces.
            debug_assert!(
                type_args.is_empty(),
                "wasm32-linear: `Op::Call({func_id:?})` reached codegen with \
                 {} unresolved type args — monomorphization should have erased \
                 them (internal compiler bug)",
                type_args.len(),
            );
            let target_idx = b.require_phx_user_func(*func_id)?;
            // Load each argument's slots onto the operand stack in
            // declaration order. Multi-slot args (`StringRef`) expand
            // to multiple `local.get`s; `emit_load_all` handles the
            // count and ordering.
            for arg in args {
                ctx.emit_load_all(*arg)?;
            }
            ctx.emit(Instruction::Call(target_idx));
            // Bind the result, if any. Two mismatches are both
            // internal-compiler-bug shapes the IR verifier should
            // reject — surface them here rather than letting them
            // silently corrupt the operand stack:
            //   * `result: Some` + `result_type: Void`: would emit an
            //     empty `LocalSet` chain (`emit_store_result` allocates
            //     zero locals, pops zero values — no-op — but the
            //     `vid` binding is then unusable).
            //   * `result: None` + `result_type: !Void`: the call
            //     pushes return slots no one consumes; the next
            //     instruction sees a stack-type mismatch that
            //     wasmparser reports far from the actual cause.
            match (instr.result, &instr.result_type) {
                (Some(_), IrType::Void) => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::Call({func_id:?})` has a result \
                         binding but a Void return type (internal compiler bug)"
                    )));
                }
                (Some(vid), ty) => {
                    ctx.emit_store_result(vid, ty.clone())?;
                }
                (None, IrType::Void) => {}
                (None, ty) => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::Call({func_id:?})` returns `{ty:?}` \
                         but has no result binding — the call's return slots \
                         would be stranded on the operand stack (internal \
                         compiler bug — IR verifier should bind every \
                         non-Void call result)"
                    )));
                }
            }
        }
        Op::BuiltinCall(name, args) => {
            translate_builtin_call(ctx, b, ir_module, name, args, instr)?
        }

        // `extern js` host call. The import was declared up front by
        // `declare_extern_imports` (before the runtime merge); here we just push
        // the marshalled args and `call` the import index. The value-level
        // marshalling on the wasm side (e.g. string staging) is the same
        // load/store the runtime ABI already uses — `StringRef` is its 2-slot
        // `(ptr, len)`, `JsValue` its 1-slot i32 handle, etc. The JS glue that
        // *satisfies* this import lands in PR 6.
        Op::ExternCall(module, name, args) => {
            let import_idx = b.get_extern_import(module, name).ok_or_else(|| {
                CompileError::new(format!(
                    "wasm32-linear: no import declared for `extern js` call \
                     `{module}.{name}` (internal compiler bug — `declare_extern_imports` \
                     must run before the merge)"
                ))
            })?;
            // Classify the result binding *before* emitting anything so the
            // internal-bug guards stay side-effect-free (a malformed pairing
            // returns `Err` without leaving a dangling `call` and unconsumed
            // return values on the operand stack).
            let result_vid = match (instr.result, &instr.result_type) {
                (Some(_), IrType::Void) => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `extern js` call `{module}.{name}` has a result \
                         binding but a Void return type (internal compiler bug)"
                    )));
                }
                // A non-void result is stored even when the source discards it
                // (a bare-statement call): `lower::emit` allocates a result vid
                // whenever the result type is non-`Void`, so this arm fires and
                // `emit_store_result` writes into a dead local. Pinned by
                // `discarded_value_returning_extern_result_validates`.
                (Some(vid), ty) => Some((vid, ty.clone())),
                (None, IrType::Void) => None,
                // Unreachable defensive guard: because `emit` always allocates a
                // result vid for a non-`Void` result type, an `Op::ExternCall`
                // with a non-`Void` `result_type` always carries `Some(result)`.
                (None, ty) => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `extern js` call `{module}.{name}` returns \
                         `{ty:?}` but has no result binding (internal compiler bug)"
                    )));
                }
            };
            for arg in args {
                ctx.emit_load_all(*arg)?;
            }
            ctx.emit(Instruction::Call(import_idx));
            if let Some((vid, ty)) = result_vid {
                ctx.emit_store_result(vid, ty)?;
            }
        }

        // --- Mutable-variable surface --------------------------------
        //
        // `Op::Alloca`/`Op::Load`/`Op::Store` are Phoenix's pre-mem2reg
        // representation of `let mut`. Each Alloca's locals *are* the
        // slot's storage — Load reads them, Store overwrites them.
        // WASM locals are mutable, so the slot's binding accumulates
        // the most-recently-stored value across writes.
        //
        // Reached today only from multi-block while/for loops, which
        // route loop counters and mutated bindings through this
        // surface; single-block functions don't emit Alloca.
        //
        // TODO(ir-verifier): the IR verifier should reject Op::Load
        // on a slot with no preceding Store. A fresh Alloca's locals
        // are zero, which is fine for scalars but a bogus null fat
        // pointer for `StringRef`. The Phoenix IR generator emits an
        // explicit Store for every `let mut s: String = …` so the
        // bogus state is unobservable today, but no codegen-layer
        // guard catches a future regression.
        Op::Alloca(ty) => {
            let vid = expect_result(instr, "Op::Alloca")?;
            ctx.allocate_locals_for_ir_type(vid, ty.clone())?;
        }
        Op::Load(slot_vid) => {
            let vid = expect_result(instr, "Op::Load")?;
            ctx.emit_load_all(*slot_vid)?;
            ctx.emit_store_result(vid, instr.result_type.clone())?;
        }
        Op::Store(slot_vid, value_vid) => {
            // Clone before `emit_load_all` — that call takes `&mut ctx`,
            // which conflicts with holding a `&` borrow of the slot's
            // binding across the call.
            let slot_locals = ctx.binding_of(*slot_vid)?.locals.clone();
            // Load source locals in declaration order (`[..., ptr, len]`
            // for StringRef), then pop into slot locals in *reverse*
            // declaration order — `local.set` pops top-of-stack, so
            // `len_local` must set before `ptr_local`. Single-slot
            // values collapse to one get/set pair.
            ctx.emit_load_all(*value_vid)?;
            for local in slot_locals.iter().rev() {
                ctx.emit(Instruction::LocalSet(*local));
            }
            // Re-root the alloca slot on the shadow stack: the slot's
            // local now holds the freshly-stored value, and the GC
            // must see *that* value (not whatever was there at
            // function entry). `emit_gc_set_root` no-ops when the
            // slot's IR type is a value type — Alloca(Int) doesn't
            // get an entry in `gc_root_slot_for` and never reaches
            // the `Call(phx_gc_set_root)` path.
            emit_gc_set_root(ctx, b, *slot_vid)?;
        }

        // --- String concatenation (sret-returning) ------------------
        //
        // `Op::StringConcat(a, b)` lowers to `phx_str_concat(sret,
        // a_ptr, a_len, b_ptr, b_len) -> ()` — the runtime allocates
        // a new GC string holding the concatenated bytes and writes
        // its `PhxFatPtr` through the sret pointer. Same call shape
        // as [`translate_to_string_builtin`]; both go through
        // [`emit_sret_string_call`] so the SP-manipulation dance is
        // declared once.
        //
        // Shadow-stack rooting of the result happens at the bottom of
        // `translate_instruction` via the blanket `emit_gc_set_root`
        // for ref-typed results — no per-op call needed here.
        Op::StringConcat(lhs, rhs) => {
            let vid = expect_result(instr, "Op::StringConcat")?;
            let runtime_idx = b.require_phx_func("phx_str_concat")?;
            emit_sret_string_call(ctx, b, runtime_idx, &[*lhs, *rhs], vid)?;
        }

        // --- Struct alloc + field access --------------------------------
        //
        // User struct values lower to GC-heap allocations via
        // `phx_gc_alloc(size, TypeTag::Struct)` followed by per-field
        // initialization. Field offsets come from
        // [`compute_struct_layout`] (which walks `IrModule::struct_layouts`);
        // codegen emits direct `iN.load` / `iN.store` at those offsets,
        // matching the field-storage size + alignment from
        // [`phx_field_size_bytes`] / [`phx_field_align_bytes`].
        //
        // The result `vid` is bound to a single i32 (the heap pointer).
        // The result is rooted on the shadow stack via the blanket
        // `emit_gc_set_root` at the bottom of `translate_instruction`.
        Op::StructAlloc(name, field_values) => {
            let vid = expect_result(instr, "Op::StructAlloc")?;
            let layout = ctx.cached_struct_layout(ir_module, name)?;
            if layout.field_types.len() != field_values.len() {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `Op::StructAlloc({name})` was given {} field \
                     values but the struct declares {} fields (internal compiler \
                     bug — IR verifier should have caught this)",
                    field_values.len(),
                    layout.field_types.len(),
                )));
            }
            let gc_alloc_idx = b.require_phx_func("phx_gc_alloc")?;
            // phx_gc_alloc(size: i32, tag: i32) -> i32
            ctx.emit(Instruction::I32Const(layout.total_size as i32));
            ctx.emit(Instruction::I32Const(TypeTag::Struct as i32));
            ctx.emit(Instruction::Call(gc_alloc_idx));
            // Bind the result vid to a single i32 local holding the
            // struct pointer. Use the sema-annotated `instr.result_type`
            // (which carries the concrete monomorphized type-args) so
            // the binding round-trips the IR's annotated type rather
            // than collapsing to `StructRef(name, [])`.
            let result_locals = ctx.allocate_locals_for_ir_type(vid, instr.result_type.clone())?;
            assert_eq!(result_locals.len(), 1, "StructRef is single-slot");
            let struct_ptr_local = result_locals[0];
            ctx.emit(Instruction::LocalSet(struct_ptr_local));
            // Store each field at its computed offset. Field-value
            // locals come from each `field_value` binding (which may
            // be multi-slot for StringRef fields).
            for (field_idx, &field_vid) in field_values.iter().enumerate() {
                let offset = layout.field_offsets[field_idx];
                let field_ty = layout.field_types[field_idx].clone();
                let value_locals = ctx.binding_of(field_vid)?.locals.clone();
                emit_field_store(ctx, struct_ptr_local, offset, &field_ty, &value_locals)?;
            }
        }
        Op::StructGetField(struct_vid, field_idx) => {
            let vid = expect_result(instr, "Op::StructGetField")?;
            let struct_ir_ty = ctx.binding_of(*struct_vid)?.ir_type.clone();
            let struct_name = match &struct_ir_ty {
                IrType::StructRef(name, _) => name.clone(),
                other => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::StructGetField` receiver has IR \
                         type `{other:?}` (expected `StructRef`); IR verifier \
                         should have rejected this"
                    )));
                }
            };
            let layout = ctx.cached_struct_layout(ir_module, &struct_name)?;
            let idx = *field_idx as usize;
            if idx >= layout.field_offsets.len() {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `Op::StructGetField({struct_name}, {field_idx})` \
                     out of range (struct has {} fields)",
                    layout.field_offsets.len(),
                )));
            }
            let offset = layout.field_offsets[idx];
            let field_ty = layout.field_types[idx].clone();
            let struct_ptr_local = ctx.binding_of(*struct_vid)?.single_local();
            emit_field_load(ctx, struct_ptr_local, offset, &field_ty)?;
            ctx.emit_store_result(vid, field_ty)?;
        }
        Op::StructSetField(struct_vid, field_idx, value_vid) => {
            let struct_ir_ty = ctx.binding_of(*struct_vid)?.ir_type.clone();
            let struct_name = match &struct_ir_ty {
                IrType::StructRef(name, _) => name.clone(),
                other => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::StructSetField` receiver has IR \
                         type `{other:?}` (expected `StructRef`)"
                    )));
                }
            };
            let layout = ctx.cached_struct_layout(ir_module, &struct_name)?;
            let idx = *field_idx as usize;
            if idx >= layout.field_offsets.len() {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `Op::StructSetField({struct_name}, {field_idx})` \
                     out of range (struct has {} fields)",
                    layout.field_offsets.len(),
                )));
            }
            let offset = layout.field_offsets[idx];
            let field_ty = layout.field_types[idx].clone();
            let struct_ptr_local = ctx.binding_of(*struct_vid)?.single_local();
            let value_locals = ctx.binding_of(*value_vid)?.locals.clone();
            emit_field_store(ctx, struct_ptr_local, offset, &field_ty, &value_locals)?;
        }

        // --- Enum alloc + variant access --------------------------------
        //
        // `EnumAlloc(name, variant_idx, field_values)`: 4-byte
        // discriminant at offset 0, payload laid out from the value
        // vids' actual types at offsets from `compute_variant_field_offsets`.
        // `EnumDiscriminant(v)` reads the i32 at offset 0; `EnumGetField`
        // reconstructs the per-site layout (see heap_layout.rs::EnumLayout
        // for the per-site / declared-vs-value-types rationale).
        Op::EnumAlloc(name, variant_idx, field_values) => {
            let vid = expect_result(instr, "Op::EnumAlloc")?;
            let declared_layout = ctx.cached_enum_layout(ir_module, name)?;
            // Layout-stability rejections (variant range, field-count
            // match, multi-field placeholder). Single-sourced with the
            // up-front validation pass so the same diagnostic fires
            // whether the runtime artifact is present or not.
            check_enum_alloc_layout_stable(
                &declared_layout,
                name,
                *variant_idx,
                field_values.len(),
            )?;
            // Walk value vids' actual types — placeholder declared
            // fields are tolerated here because the layout follows the
            // values' types. The rejection above caps the shapes to
            // (a) single-field placeholder variants and (b) fully-
            // concrete variants — both layout-stable across alloc/get.
            let value_types: Vec<IrType> = field_values
                .iter()
                .map(|fvid| ctx.binding_of(*fvid).map(|b| b.ir_type.clone()))
                .collect::<Result<_, _>>()?;
            // Defense in depth: a sema regression leaving a placeholder
            // here would silently size as a 4-byte i32 (see
            // `reject_placeholder_field_type`).
            for (i, ty) in value_types.iter().enumerate() {
                reject_placeholder_field_type(
                    ty,
                    &format!(
                        "`Op::EnumAlloc({name}, variant={variant_idx})` value at position {i}"
                    ),
                )?;
            }
            let variant = compute_variant_field_offsets(&value_types)?;
            // Pad to the variant's max field alignment (folded with the
            // 4-byte discriminant alignment) so an array of this enum
            // would keep each element naturally aligned. Mirror of
            // `compute_struct_layout`'s tail-padding policy.
            let total_size = align_up(variant.payload_end, variant.max_align);
            let gc_alloc_idx = b.require_phx_func("phx_gc_alloc")?;
            ctx.emit(Instruction::I32Const(total_size as i32));
            ctx.emit(Instruction::I32Const(TypeTag::Enum as i32));
            ctx.emit(Instruction::Call(gc_alloc_idx));
            // Carry through the sema-annotated type-args via
            // `instr.result_type` so the binding matches the rest of
            // the IR's annotated types.
            let result_locals = ctx.allocate_locals_for_ir_type(vid, instr.result_type.clone())?;
            assert_eq!(result_locals.len(), 1, "EnumRef is single-slot");
            let enum_ptr_local = result_locals[0];
            ctx.emit(Instruction::LocalSet(enum_ptr_local));
            // Store discriminant at offset 0.
            ctx.emit(Instruction::LocalGet(enum_ptr_local));
            ctx.emit(Instruction::I32Const(*variant_idx as i32));
            ctx.emit(Instruction::I32Store(i32_memarg(0)));
            // Store each payload field using the value's actual type
            // (so multi-slot StringRef stores both ptr and len even
            // when the IR's declared field type is the placeholder).
            for (field_idx, &field_vid) in field_values.iter().enumerate() {
                let offset = variant.field_offsets[field_idx];
                let field_ty = value_types[field_idx].clone();
                let value_locals = ctx.binding_of(field_vid)?.locals.clone();
                emit_field_store(ctx, enum_ptr_local, offset, &field_ty, &value_locals)?;
            }
        }
        Op::EnumDiscriminant(enum_vid) => {
            let vid = expect_result(instr, "Op::EnumDiscriminant")?;
            let enum_ptr_local = ctx.binding_of(*enum_vid)?.single_local();
            // Discriminant is an i32 at offset 0; promote to i64 to
            // match the IR's "discriminant comparisons use i64
            // constants" convention (lowering emits `IEq v_discrim,
            // ConstI64(N)` for match-arm dispatch).
            ctx.emit(Instruction::LocalGet(enum_ptr_local));
            ctx.emit(Instruction::I32Load(i32_memarg(0)));
            ctx.emit(Instruction::I64ExtendI32U);
            let result_local = ctx.allocate_local(vid, ValType::I64, IrType::I64);
            ctx.emit(Instruction::LocalSet(result_local));
        }
        Op::EnumGetField(enum_vid, variant_idx, field_idx) => {
            let vid = expect_result(instr, "Op::EnumGetField")?;
            let enum_ir_ty = ctx.binding_of(*enum_vid)?.ir_type.clone();
            let enum_name = match &enum_ir_ty {
                IrType::EnumRef(name, _) => name.clone(),
                other => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::EnumGetField` receiver has IR type \
                         `{other:?}` (expected `EnumRef`)"
                    )));
                }
            };
            // Resolve field_idx's offset by walking the variant's
            // *declared* fields with their concrete types — preferring
            // sema-annotated `instr.result_type` for the requested
            // field over the IR's placeholder. The walk reconstructs
            // the same offsets `Op::EnumAlloc` used per-site
            // (single-field-variant case is trivially offset=4;
            // multi-field placeholder variants are explicitly
            // rejected below to keep alloc/get in lockstep).
            let declared_layout = ctx.cached_enum_layout(ir_module, &enum_name)?;
            let v_idx = *variant_idx as usize;
            let f_idx = *field_idx as usize;
            if v_idx >= declared_layout.variant_field_types.len()
                || f_idx >= declared_layout.variant_field_types[v_idx].len()
            {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `Op::EnumGetField({enum_name}, variant={variant_idx}, \
                     field={field_idx})` out of range"
                )));
            }
            let declared_field_types = &declared_layout.variant_field_types[v_idx];
            // Build the variant's per-site field-type list by
            // substituting `instr.result_type` for the requested field
            // (the only one we have an accurate type for from sema);
            // other fields are taken from declared_field_types.
            // Placeholder fields in OTHER positions are an error —
            // we can't compute correct offsets without knowing their
            // sizes. `Op::EnumAlloc` rejects multi-field variants with
            // any placeholder field, so reaching this guard would mean
            // an IR that constructed a value through a different alloc
            // path; keeping the guard here makes the failure local.
            let mut field_types = declared_field_types.clone();
            field_types[f_idx] = instr.result_type.clone();
            // Mirror of the alloc-side check (see
            // `reject_placeholder_field_type`).
            reject_placeholder_field_type(
                &field_types[f_idx],
                &format!(
                    "`Op::EnumGetField({enum_name}, variant={variant_idx}, field={field_idx})` result type"
                ),
            )?;
            for (i, ty) in field_types.iter().enumerate() {
                if i != f_idx && ty.is_generic_placeholder() {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::EnumGetField({enum_name}, variant={variant_idx}, \
                         field={field_idx})` would need to compute offsets through \
                         placeholder-typed field {i}; multi-field variants with \
                         any placeholder field aren't supported by the current enum \
                         layout"
                    )));
                }
            }
            let variant = compute_variant_field_offsets(&field_types)?;
            let offset = variant.field_offsets[f_idx];
            let field_ty = field_types[f_idx].clone();
            let enum_ptr_local = ctx.binding_of(*enum_vid)?.single_local();
            emit_field_load(ctx, enum_ptr_local, offset, &field_ty)?;
            ctx.emit_store_result(vid, field_ty)?;
        }

        // --- List allocation ----------------------------------------
        //
        // `Op::ListAlloc(elements)` lowers to
        // `phx_list_alloc(elem_size, count)` followed by a store of
        // each initial element at `LIST_HEADER + i * elem_size`.
        // Element stride is *WASM-natural*, not native-matched:
        // `phx_field_size_bytes` gives 4 bytes for `Bool` / GC-pointer
        // refs and 8 bytes for `StringRef` (vs. 8 / 16 on native). The
        // runtime is element-size-agnostic — `phx_list_alloc` stores
        // the chosen elem_size in the list header and every subsequent
        // accessor (`phx_list_get_raw`, `phx_list_length`,
        // `phx_list_take`, ...) reads it back from the header, so a
        // wasm32-sized stride works without runtime changes.
        //
        // The blanket post-instruction `emit_gc_set_root` at the end
        // of this function roots the resulting list pointer.
        Op::ListAlloc(elements) => {
            let vid = expect_result(instr, "Op::ListAlloc")?;
            let elem_ty = match &instr.result_type {
                IrType::ListRef(t) => t.as_ref().clone(),
                other => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::ListAlloc` result type must be `ListRef`, \
                         got `{other:?}` (internal compiler bug — IR verifier should \
                         have caught this)"
                    )));
                }
            };
            let elem_size = phx_field_size_bytes(&elem_ty)?;
            let count = elements.len();
            // `phx_list_alloc` signature: `(elem_size: i64, count: i64) -> *mut u8`.
            // Both args are i64 in the runtime ABI even on wasm32 (the
            // runtime crate uses `i64` for portability — the wasm32
            // lowering of `phx_list_alloc` declares an `(i64, i64) -> i32`
            // function type, matching what `b.require_phx_func` returns
            // from the merge).
            let list_alloc_idx = b.require_phx_func("phx_list_alloc")?;
            ctx.emit(Instruction::I64Const(elem_size as i64));
            ctx.emit(Instruction::I64Const(count as i64));
            ctx.emit(Instruction::Call(list_alloc_idx));
            let list_ptr_local = ctx.allocate_local(vid, ValType::I32, instr.result_type.clone());
            ctx.emit(Instruction::LocalSet(list_ptr_local));
            // Initial-element stores. Each element's value sits in its
            // own already-bound binding (the IR lowering pass evaluated
            // the literal before the alloc); read it via `binding_of`
            // and write through `emit_field_store` at the per-i offset.
            // Skipping if `elem_size == 0` is unnecessary — `Void`-typed
            // elements would already have errored at the
            // `phx_field_size_bytes(&elem_ty)?` call above.
            for (i, elem_vid) in elements.iter().enumerate() {
                let offset = LIST_HEADER + (i as u32) * elem_size;
                let arg_binding = ctx.binding_of(*elem_vid)?;
                let arg_locals = arg_binding.locals.clone();
                let arg_ty = arg_binding.ir_type.clone();
                emit_field_store(ctx, list_ptr_local, offset, &arg_ty, &arg_locals)?;
            }
        }

        // --- Map allocation -----------------------------------------
        //
        // `Op::MapAlloc(pairs)` lowers to a single
        // `phx_map_from_pairs(key_size, val_size, n_pairs, pair_data,
        // key_is_string)` runtime call: codegen reserves an on-stack
        // pair buffer (via
        // [`emit_alloc_stack_frame`]), writes each `(key, val)` pair
        // back-to-back at densely packed offsets (no slot padding —
        // the runtime indexes by `(ks + vs) * i`), then hands the
        // whole buffer over and lets the runtime hash-build the table
        // in one pass. Avoids the O(n) per-insert overhead of driving
        // the literal through `phx_map_set_raw`.
        //
        // Empty literals (`{}`) skip the buffer entirely and pass a
        // null pair_data pointer — the runtime's `n_pairs == 0`
        // carve-out makes the buffer unnecessary in that case (it's
        // never dereferenced).
        //
        // The result is a fresh heap-allocated map; the blanket
        // post-instruction `emit_gc_set_root` roots it.
        Op::MapAlloc(entries) => {
            let vid = expect_result(instr, "Op::MapAlloc")?;
            let (key_ty, val_ty) = match &instr.result_type {
                IrType::MapRef(k, v) => (k.as_ref().clone(), v.as_ref().clone()),
                other => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::MapAlloc` result type must be `MapRef`, \
                         got `{other:?}` (internal compiler bug — IR verifier should \
                         have caught this)"
                    )));
                }
            };
            let ks = phx_field_size_bytes(&key_ty)? as i64;
            let vs = phx_field_size_bytes(&val_ty)? as i64;
            let count = entries.len() as i64;
            // `key_is_string`: recorded in the map header so every later
            // lookup compares string keys by content. Required on wasm32,
            // where a `StringRef` key is 8 bytes and can't be told from an
            // `Int` / `Float` key by size. See the runtime's
            // `phx_map_from_pairs` / `elements_equal`. Shared with the
            // cranelift backend via [`IrType::string_flag`].
            let key_is_string = key_ty.string_flag() as i64;
            let map_from_pairs_idx = b.require_phx_func("phx_map_from_pairs")?;

            if entries.is_empty() {
                // `phx_map_from_pairs(ks, vs, 0, null_i32, key_is_string)`
                // — runtime safety doc explicitly allows null `pair_data`
                // when `n_pairs == 0`.
                ctx.emit(Instruction::I64Const(ks));
                ctx.emit(Instruction::I64Const(vs));
                ctx.emit(Instruction::I64Const(0));
                ctx.emit(Instruction::I32Const(0));
                ctx.emit(Instruction::I64Const(key_is_string));
                ctx.emit(Instruction::Call(map_from_pairs_idx));
                ctx.emit_store_result(vid, instr.result_type.clone())?;
            } else {
                let pair_size = (ks + vs) as u32;
                let buf_size = pair_size * entries.len() as u32;
                let (saved_sp, frame_ptr_local) = emit_alloc_stack_frame(ctx, b, buf_size)?;

                // Write each `(key, val)` pair at offset `i * pair_size`.
                // Key at `pair_off`, value at `pair_off + ks` —
                // mirrors the runtime's `pair_data.add(i * pair_size)` /
                // `pair_data.add(i * pair_size + ks)` access shape.
                for (i, (k_vid, v_vid)) in entries.iter().enumerate() {
                    let pair_off = (i as u32) * pair_size;
                    let k_binding = ctx.binding_of(*k_vid)?;
                    let k_locals = k_binding.locals.clone();
                    let k_ty = k_binding.ir_type.clone();
                    emit_field_store(ctx, frame_ptr_local, pair_off, &k_ty, &k_locals)?;
                    let v_binding = ctx.binding_of(*v_vid)?;
                    let v_locals = v_binding.locals.clone();
                    let v_ty = v_binding.ir_type.clone();
                    emit_field_store(ctx, frame_ptr_local, pair_off + ks as u32, &v_ty, &v_locals)?;
                }

                ctx.emit(Instruction::I64Const(ks));
                ctx.emit(Instruction::I64Const(vs));
                ctx.emit(Instruction::I64Const(count));
                ctx.emit(Instruction::LocalGet(frame_ptr_local));
                ctx.emit(Instruction::I64Const(key_is_string));
                ctx.emit(Instruction::Call(map_from_pairs_idx));
                ctx.emit_store_result(vid, instr.result_type.clone())?;
                emit_restore_stack_frame(ctx, b, saved_sp)?;
            }
        }

        // --- Closure allocation -------------------------------------
        //
        // Closure heap layout: `[fn_table_idx: i32 @ 0, capture_0,
        // capture_1, ...]`. The first i32 is the index of the target
        // function within the merged module's *closure table* (a
        // funcref table declared by
        // [`ModuleBuilder::register_closure_table`]'s pre-scan), not
        // a raw function pointer — wasm32 has no observable function
        // addresses, only table slots. `Op::CallIndirect` reloads
        // that i32 and feeds it to `call_indirect`.
        //
        // Captures lay out at WASM-natural alignment past the fn-idx:
        // `phx_field_align_bytes` + `phx_field_size_bytes` (the same
        // helpers struct/enum fields use). This matches the offsets
        // `Op::ClosureLoadCapture` reconstructs via
        // [`FuncTranslateCtx::capture_offset`].
        //
        // The blanket post-instruction `emit_gc_set_root` at the end
        // of this function roots the resulting closure pointer.
        Op::ClosureAlloc(target_fid, captures) => {
            let vid = expect_result(instr, "Op::ClosureAlloc")?;
            // Layout source: the *alloc-site value types* of the
            // capture vids, not the target closure function's declared
            // `capture_types`. For closures defined inside a generic,
            // the declared `capture_types` can retain an unsubstituted
            // `TypeVar("T")` (the inner closure function is shared
            // across single-instantiation specializations rather than
            // cloned — see `closures_over_generic.phx`). The alloc-site
            // values, by contrast, live in the enclosing *monomorphized*
            // function, so their binding types are always concrete.
            // This mirrors the native backend, which reads capture
            // types from `state.type_map` at the alloc site for exactly
            // this reason. The load side (`Op::ClosureLoadCapture`)
            // recovers the concrete target type from the instruction's
            // `result_type`, so both sides agree on offsets.
            // Collect each capture's concrete value type *and* its
            // locals from the SAME binding lookup (one `binding_of` per
            // capture, not two), so the offset computed below and the
            // store that consumes it are derived from one consistent
            // source.
            let mut capture_value_types: Vec<IrType> = Vec::with_capacity(captures.len());
            let mut capture_locals: Vec<ParamSlotLocals> = Vec::with_capacity(captures.len());
            for cap_vid in captures {
                let bnd = ctx.binding_of(*cap_vid)?;
                capture_value_types.push(bnd.ir_type.clone());
                capture_locals.push(bnd.locals.clone());
            }
            // Layout is taken entirely from the alloc-site bindings above,
            // but the supplied capture *count* must still match the target
            // closure's declared capture arity — a mismatch is an IR
            // builder invariant violation. Only the arity is checked here:
            // the declared element types can retain unsubstituted
            // `TypeVar`s for closures inside generics (precisely why layout
            // comes from the bindings, not from here), but their count is
            // always correct. `get_concrete` returns `None` for an
            // out-of-range or un-specialized template slot; that is its own
            // form of drift, surfaced downstream by
            // `require_closure_target_slot`, so this assert simply skips it.
            debug_assert!(
                ir_module
                    .get_concrete(*target_fid)
                    .is_none_or(|f| f.capture_types.len() == captures.len()),
                "wasm32-linear: `Op::ClosureAlloc({target_fid:?})` arity mismatch — \
                 IR target declares {:?} captures but the alloc site supplies {} \
                 (internal compiler bug — IR builder invariant violated)",
                ir_module
                    .get_concrete(*target_fid)
                    .map(|f| f.capture_types.len()),
                captures.len(),
            );
            // Per-capture byte offsets via the shared `place_capture`
            // step — the same per-capture logic the load side
            // (`Op::ClosureLoadCapture` → `capture_byte_offset`) uses, so
            // the two cannot drift. A single running cursor keeps this
            // O(n) (no per-capture prefix-sum recomputation), and each
            // capture's align/size is fetched exactly once. The running
            // cursor's final value is the end of the last capture, so the
            // total allocation size is just that rounded up to the
            // object's max alignment. With no captures the object is the
            // 4-byte fn-table-idx alone.
            let mut offsets: Vec<u32> = Vec::with_capacity(capture_value_types.len());
            let mut cursor: u32 = 4; // skip the fn-table-idx at offset 0
            let mut max_align: u32 = 4; // fn-table-idx is i32
            for ty in &capture_value_types {
                let (offset, end, align) = place_capture(cursor, ty)?;
                offsets.push(offset);
                cursor = end;
                max_align = max_align.max(align);
            }
            let total_size = align_up(cursor, max_align);
            // Allocate. `phx_gc_alloc(size, type_tag)` returns the i32
            // heap pointer (both args are i32 on wasm32; `usize` in the
            // runtime's Rust signature lowers to i32 here, not i64).
            // `TypeTag::Closure` (= 4) tags the allocation for the
            // GC's mark phase.
            let alloc_idx = b.require_phx_func("phx_gc_alloc")?;
            ctx.emit(Instruction::I32Const(total_size as i32));
            ctx.emit(Instruction::I32Const(TypeTag::Closure as i32));
            ctx.emit(Instruction::Call(alloc_idx));
            let closure_ptr_local =
                ctx.allocate_local(vid, ValType::I32, instr.result_type.clone());
            ctx.emit(Instruction::LocalSet(closure_ptr_local));
            // Store fn-table-idx at offset 0 as i32.
            let fn_table_slot = b.require_closure_target_slot(*target_fid)?;
            ctx.emit(Instruction::LocalGet(closure_ptr_local));
            ctx.emit(Instruction::I32Const(fn_table_slot as i32));
            ctx.emit(Instruction::I32Store(i32_memarg(0)));
            // Store each capture at its computed offset, using the same
            // alloc-site value type and locals the offset was derived
            // from so the offset and store width can't drift apart.
            for i in 0..capture_value_types.len() {
                emit_field_store(
                    ctx,
                    closure_ptr_local,
                    offsets[i],
                    &capture_value_types[i],
                    &capture_locals[i],
                )?;
            }
        }

        // --- Closure capture load -----------------------------------
        //
        // Inside a closure function body, read capture #idx from the
        // env pointer (the closure's first param). `capture_offset`
        // reconstructs the same byte offset the alloc side wrote at:
        // it walks `current_capture_types[..idx]` for the preceding
        // captures and takes `instr.result_type` (the concrete,
        // sema-substituted capture type) as the target. Passing
        // `result_type` rather than reading `current_capture_types[idx]`
        // is what lets a closure defined inside a generic resolve its
        // capture even when the declared `capture_types` retains an
        // unsubstituted `TypeVar` — see `capture_offset`'s docs.
        Op::ClosureLoadCapture(env_vid, capture_idx) => {
            let vid = expect_result(instr, "Op::ClosureLoadCapture")?;
            let env_local = ctx.binding_of(*env_vid)?.single_local();
            let result_ty = instr.result_type.clone();
            let offset = ctx.capture_offset(*capture_idx as usize, &result_ty)?;
            emit_field_load(ctx, env_local, offset, &result_ty)?;
            ctx.emit_store_result(vid, result_ty)?;
        }

        // --- Indirect call through a closure ------------------------
        //
        // `Op::CallIndirect(closure_vid, args)` loads the fn-table-
        // idx from the closure's heap object (i32 at offset 0) and
        // emits `call_indirect (type T) (table closure_table_idx)`
        // with `(closure_ptr, user_args...)` on the operand stack
        // (env-pointer ABI: the closure pointer itself is the env,
        // passed as the first arg so the callee can resolve captures
        // via `Op::ClosureLoadCapture` from it).
        //
        // The type signature `T` is built from the closure's own
        // `IrType::ClosureRef { param_types, return_type }`, with an
        // i32 env prepended. Interning per call site keeps the type
        // section minimal — coincident signatures (e.g. all `(Int) ->
        // Int` closures across the module) collapse to one entry.
        Op::CallIndirect(closure_vid, args) => {
            let closure_binding = ctx.binding_of(*closure_vid)?;
            let closure_ptr_local = closure_binding.single_local();
            let closure_ir_ty = closure_binding.ir_type.clone();
            // Collect each arg's slot locals from its binding, then
            // route through the shared `emit_closure_call_raw`. The
            // args are already bound to vids (in locals), so reading
            // `binding_of(arg).locals` matches what `emit_load_all`
            // would push, in the same order.
            let arg_slot_locals: Vec<Vec<u32>> = args
                .iter()
                .map(|arg| ctx.binding_of(*arg).map(|bnd| bnd.locals.clone()))
                .collect::<Result<Vec<_>, _>>()?;
            emit_closure_call_raw(ctx, b, closure_ptr_local, &closure_ir_ty, &arg_slot_locals)?;
            // Bind result. Same mismatch-rejection shape as
            // `Op::Call` above.
            match (instr.result, &instr.result_type) {
                (Some(_), IrType::Void) => {
                    return Err(CompileError::new(
                        "wasm32-linear: `Op::CallIndirect` has a result binding but \
                         a Void return type (internal compiler bug)"
                            .to_string(),
                    ));
                }
                (Some(vid), ty) => {
                    ctx.emit_store_result(vid, ty.clone())?;
                }
                (None, IrType::Void) => {}
                (None, ty) => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::CallIndirect` returns `{ty:?}` but has \
                         no result binding — the call's return slots would be \
                         stranded on the operand stack (internal compiler bug)"
                    )));
                }
            }
        }

        // --- dyn Trait ABI -----------------------------------------
        //
        // `Op::DynAlloc(trait, concrete, value)` produces a 2-slot
        // `(data_ptr, vtable_ptr)` fat pointer. `data_ptr` is the
        // concrete-type's heap pointer (the value vid's single local);
        // `vtable_ptr` is the user-data byte offset of the rodata
        // vtable for `(concrete, trait)`. The vtable holds an i32
        // function-table-index per trait method; `Op::DynCall` reads
        // those indices and does a `call_indirect` through the
        // shared closure table.
        //
        // `Op::UnresolvedDynAlloc` shouldn't reach codegen — the IR
        // monomorphizer rewrites it to a concrete `Op::DynAlloc`
        // before any specialization is emitted. If it does, surface
        // an internal-compiler-bug diagnostic with the trait name so
        // a future regression in the monomorphizer is easy to triage.
        Op::DynAlloc(trait_name, concrete_type, value) => {
            let vid = expect_result(instr, "Op::DynAlloc")?;
            let value_binding = ctx.binding_of(*value)?;
            // The `dyn` fat-pointer ABI stores the concrete value as a
            // single i32 `data_ptr` (a GC-heap pointer). Reject any
            // concrete that isn't a GC-pointer type up front: a `Bool`
            // would smuggle through `single_local` and an `I64`/`F64`
            // would emit a `local.set i32 ← i64/f64` that only fails at
            // wasmparser validation, far from this site. Surface it as
            // an ICE instead — sema's object-safety / coercion rules are
            // expected to keep primitives out of `dyn` positions.
            if !is_gc_pointer_type(&value_binding.ir_type) {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `Op::DynAlloc` of `{concrete_type}` as \
                     `dyn {trait_name}` has a non-GC-pointer data type \
                     `{:?}` — the dyn fat-pointer ABI requires a single-i32 \
                     heap pointer (internal compiler bug — sema should reject \
                     dyn coercion of non-pointer concretes)",
                    value_binding.ir_type
                )));
            }
            let data_ptr_local = value_binding.single_local();
            let vtable_offset = b.require_dyn_vtable(ir_module, concrete_type, trait_name)?;
            let dyn_ty = IrType::DynRef(trait_name.clone());
            let result_locals = ctx.allocate_locals_for_ir_type(vid, dyn_ty)?;
            debug_assert_eq!(result_locals.len(), 2, "DynRef must be 2 slots");
            // data_ptr_local → result_locals[0]
            ctx.emit(Instruction::LocalGet(data_ptr_local));
            ctx.emit(Instruction::LocalSet(result_locals[0]));
            // vtable byte offset → result_locals[1]
            ctx.emit(Instruction::I32Const(vtable_offset as i32));
            ctx.emit(Instruction::LocalSet(result_locals[1]));
        }
        Op::UnresolvedDynAlloc(trait_name, _) => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `Op::UnresolvedDynAlloc` for `dyn {trait_name}` \
                 reached codegen — monomorphization was expected to rewrite \
                 it to a concrete `Op::DynAlloc` first (internal compiler bug)"
            )));
        }
        Op::DynCall(trait_name, method_idx, receiver, args) => {
            let recv_binding = ctx.binding_of(*receiver)?;
            let recv_locals = recv_binding.locals.clone();
            if recv_locals.len() != 2 {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `Op::DynCall` receiver expected 2 slots \
                     (data_ptr, vtable_ptr), got {} (internal compiler bug)",
                    recv_locals.len()
                )));
            }
            let data_ptr_local = recv_locals[0];
            let vtable_ptr_local = recv_locals[1];
            // Resolve the trait method's signature from the IR module
            // (params + return type, excluding self). Build the
            // wasm32 signature: prepend an i32 receiver slot, then
            // flatten each user param and the return.
            let (method_params, method_return) = ir_module
                .trait_method_signature(trait_name, *method_idx as usize)
                .ok_or_else(|| {
                    CompileError::new(format!(
                        "wasm32-linear: no IR trait metadata for `dyn {trait_name}` \
                         slot {method_idx} — trait is missing or non-object-safe \
                         (internal compiler bug)"
                    ))
                })?;
            let mut full_param_valtypes: Vec<ValType> = vec![ValType::I32];
            for pt in method_params {
                full_param_valtypes.extend(wasm_valtypes_for(pt)?);
            }
            let return_valtypes = wasm_return_valtypes(method_return)?;
            // The `call_indirect` type is built from the trait metadata's
            // `method_return`, but `emit_store_result` below binds the
            // result using `instr.result_type`. The two must agree in
            // slot shape or the indirect call would leave a stack-type
            // mismatch the validator reports far from here. They should
            // always match (IR sets `result_type` from this same trait
            // method), so pin it as a debug tripwire rather than a
            // release-path error.
            debug_assert_eq!(
                wasm_valtypes_for(&instr.result_type).ok(),
                Some(return_valtypes.clone()),
                "DynCall result_type {:?} flattens differently from the \
                 trait method's declared return {:?} — call_indirect \
                 signature and emit_store_result would disagree",
                instr.result_type,
                method_return,
            );
            let type_idx = b.intern_call_indirect_type(&full_param_valtypes, &return_valtypes);
            let table_idx = b.require_closure_table_idx()?;
            // Push data_ptr (the self receiver), then each user arg's
            // slots, then the fn-table-idx loaded from
            // `vtable_ptr + method_idx * 4`. `call_indirect` consumes
            // the fn-table-idx from the top of the stack and matches
            // the rest against the signature.
            ctx.emit(Instruction::LocalGet(data_ptr_local));
            for arg in args {
                ctx.emit_load_all(*arg)?;
            }
            ctx.emit(Instruction::LocalGet(vtable_ptr_local));
            // `i32_memarg`'s `align: 2` hint claims 4-byte alignment, but
            // the vtable base comes from `reserve_user_data`, which makes
            // no alignment guarantee — so this load may be effectively
            // unaligned. That is harmless on WASM: the memarg alignment is
            // only a hint, and linear-memory loads are always valid
            // regardless of the operand address's actual alignment.
            ctx.emit(Instruction::I32Load(i32_memarg(
                *method_idx * DYN_VTABLE_ENTRY_SIZE,
            )));
            ctx.emit(Instruction::CallIndirect {
                type_index: type_idx,
                table_index: table_idx,
            });
            match (instr.result, &instr.result_type) {
                (Some(_), IrType::Void) => {
                    return Err(CompileError::new(
                        "wasm32-linear: `Op::DynCall` has a result binding but a \
                         Void return type (internal compiler bug)"
                            .to_string(),
                    ));
                }
                (Some(vid), ty) => {
                    ctx.emit_store_result(vid, ty.clone())?;
                }
                (None, IrType::Void) => {}
                (None, ty) => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `Op::DynCall` returns `{ty:?}` but has \
                         no result binding (internal compiler bug)"
                    )));
                }
            }
        }

        other => {
            // Authoritative backstop. The up-front validation pass
            // ([`super::validate`]) pre-rejects the known deferred-op
            // families (float arithmetic/comparison, string comparison)
            // before the runtime merge, but any op it doesn't screen
            // still lands here.
            return Err(unsupported_op_error(other));
        }
    }
    // Shadow-stack rooting: if this instruction produced a ref-typed
    // binding holding an actual heap pointer, write that pointer into
    // its pre-assigned slot so a subsequent `phx_gc_alloc` mark cycle
    // can see it as a root. `emit_gc_set_root` no-ops for non-ref
    // result types (the binding won't have an entry in
    // `gc_root_slot_for`) and for `Op::Store` (no `instr.result`),
    // keeping this a safe blanket call for the remaining ops.
    //
    // `op_produces_heap_pointer` skips the call for ops whose result
    // is known not to be a fresh heap pointer (`Op::ConstString` lives
    // in the data section; `Op::Alloca`'s storage is zero-initialized
    // and rooted on its first `Op::Store`). `phx_gc_push_frame`
    // already zeroes every slot, so a redundant `set_root(slot, 0)`
    // there would be pure overhead.
    if let Some(vid) = instr.result
        && instr.result_type.is_ref_type()
        && op_produces_heap_pointer(&instr.op)
    {
        emit_gc_set_root(ctx, b, vid)?;
    }
    Ok(())
}

/// Frame reservation for every sret-returning runtime call: 16 bytes
/// (8 for `PhxFatPtr` + 8 of headroom for struct growth). The 16-byte
/// figure matches wasm-ld's stack alignment on wasm32-wasip1; the
/// `% 4` const-assert pins the weaker invariant the loads in
/// [`emit_sret_string_call`] actually rely on (4-byte alignment, which
/// matches the `align: 2` hint on the PhxFatPtr loads). A future
/// tightening of the constant to a non-multiple-of-4 would break the
/// load-alignment guarantee — the const-assert catches that at
/// codegen time.
const SRET_FRAME_BYTES: i32 = 16;
const _: () = assert!(
    SRET_FRAME_BYTES % 4 == 0,
    "SRET_FRAME_BYTES must keep `__stack_pointer` 4-byte aligned \
     to match the `align: 2` hint on the PhxFatPtr loads in \
     `emit_sret_string_call`"
);

/// Emit an sret call to a runtime function returning `PhxFatPtr`
/// (`phx_i64_to_str`, `phx_str_concat`, etc.). The C-ABI struct-return
/// convention on wasm32-wasip1 takes the result-area pointer as an
/// implicit first parameter; this helper handles the
/// `__stack_pointer`-managed allocation + invocation + result-load +
/// SP-restore dance, binding `result_vid` to the two `i32` locals
/// holding the result's `(ptr, len)` fields.
///
/// `value_args` are pushed onto the operand stack in declaration order
/// *after* the sret pointer; each value arg's full slot count is
/// loaded via [`FuncTranslateCtx::emit_load_all`], so `StringRef` args
/// expand to two i32 slots each.
///
/// Shared by [`translate_to_string_builtin`] (single-arg case) and
/// [`Op::StringConcat`] (two-arg case); future PR 3c+ sret callouts
/// (list/map alloc, closure alloc) route through here as well.
///
/// # Stack-pointer protocol
///
/// We save the original SP into a local, subtract [`SRET_FRAME_BYTES`],
/// invoke the callee, load the result, then restore SP from the saved
/// local. Restoring from a saved copy (rather than `current_SP +
/// SRET_FRAME_BYTES`) is robust against any future ABI quirk where a
/// callee fails to restore SP on its return path: even if SP is wrong
/// on return, we put the caller's frame back exactly.
///
/// Per Decision H, the resulting heap pointer is a GC-tracked value.
/// Rooting on the shadow stack happens at the bottom of
/// `translate_instruction` via the blanket `emit_gc_set_root` for
/// ref-typed results — no per-call wiring is needed here.
///
/// # Load alignment
///
/// `PhxFatPtr` in `phoenix-runtime` is `#[repr(C)]` with `ptr` at
/// offset 0 and `len` at offset 4; compile-time assertions in that
/// crate pin the offsets. The `align: 2` (4-byte) hint matches what
/// `SRET_FRAME_BYTES`'s multiple-of-4 invariant guarantees: SP starts
/// at the runtime image's `__stack_pointer` init value (1 MiB, which
/// is 16-aligned — see decision H in design-decisions.md) and every
/// mutating site here subtracts a multiple of 4, so `sret_ptr` is
/// always 4-byte aligned for i32 reads.
pub(super) fn emit_sret_string_call(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    runtime_fn_idx: u32,
    value_args: &[ValueId],
    result_vid: ValueId,
) -> Result<(), CompileError> {
    let sp_global = b.require_stack_pointer_global()?;

    // Allocate two consecutive i32 locals for the (ptr, len) result.
    let result_locals = ctx.allocate_locals_for_ir_type(result_vid, IrType::StringRef)?;
    debug_assert_eq!(result_locals.len(), 2, "StringRef must be 2 slots");
    let result_ptr_local = result_locals[0];
    let result_len_local = result_locals[1];

    let saved_sp_local = ctx.allocate_temp_local(ValType::I32);
    let sret_ptr_local = ctx.allocate_temp_local(ValType::I32);

    // saved_sp = SP
    ctx.emit(Instruction::GlobalGet(sp_global));
    ctx.emit(Instruction::LocalSet(saved_sp_local));

    // SP = saved_sp - SRET_FRAME_BYTES; sret_ptr = SP
    ctx.emit(Instruction::LocalGet(saved_sp_local));
    ctx.emit(Instruction::I32Const(SRET_FRAME_BYTES));
    ctx.emit(Instruction::I32Sub);
    ctx.emit(Instruction::LocalTee(sret_ptr_local));
    ctx.emit(Instruction::GlobalSet(sp_global));

    // Push sret pointer + each value arg's slots in declaration order.
    ctx.emit(Instruction::LocalGet(sret_ptr_local));
    for arg in value_args {
        ctx.emit_load_all(*arg)?;
    }
    ctx.emit(Instruction::Call(runtime_fn_idx));

    // Load PhxFatPtr { ptr at offset 0, len at offset 4 }.
    ctx.emit(Instruction::LocalGet(sret_ptr_local));
    ctx.emit(Instruction::I32Load(i32_memarg(0)));
    ctx.emit(Instruction::LocalSet(result_ptr_local));
    ctx.emit(Instruction::LocalGet(sret_ptr_local));
    ctx.emit(Instruction::I32Load(i32_memarg(4)));
    ctx.emit(Instruction::LocalSet(result_len_local));

    // Restore SP from the saved copy (robust against any callee SP
    // mismanagement).
    ctx.emit(Instruction::LocalGet(saved_sp_local));
    ctx.emit(Instruction::GlobalSet(sp_global));

    Ok(())
}

/// Reserve `n_bytes` (rounded up to 16-byte alignment) on the WASM
/// shadow stack via `__stack_pointer` global manipulation. Returns
/// `(saved_sp_local, frame_ptr_local)`:
///
/// - `saved_sp_local` (i32) holds the pre-decrement SP — pass it to
///   [`emit_restore_stack_frame`] after the call to restore SP exactly,
///   robust against any callee SP mismanagement.
/// - `frame_ptr_local` (i32) holds the post-decrement SP, which is the
///   base address of the reserved frame. Use `emit_field_store` or
///   raw `i32.store` to fill it before invoking the runtime.
///
/// The 16-byte rounding matches [`SRET_FRAME_BYTES`]'s alignment and
/// the runtime image's `__stack_pointer` init value (1 MiB, 16-aligned
/// per decision H) so the resulting frame pointer is 16-byte aligned
/// regardless of `n_bytes`. Callers pass the *exact* byte count they
/// need; this helper does the padding.
///
/// Used by `Op::MapAlloc` (pair-data buffer) and the
/// `Map.<method>(key, ...)` family (single-key buffer). Future
/// callouts that need scratch space on the shadow stack route through
/// here too so the SP-manipulation dance stays in one place.
pub(super) fn emit_alloc_stack_frame(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    n_bytes: u32,
) -> Result<(u32, u32), CompileError> {
    let sp_global = b.require_stack_pointer_global()?;
    // Pad up to 16-byte alignment. Power-of-two-mask trick: rounds
    // `n_bytes` to the next multiple of 16. The +15 can't overflow
    // because n_bytes comes from `phx_field_size_bytes * small_count`
    // in practice — pathologically huge values would hit
    // `wasm-encoder`'s i32 limit long before reaching here.
    let padded = (n_bytes + 15) & !15;

    let saved_sp_local = ctx.allocate_temp_local(ValType::I32);
    let frame_ptr_local = ctx.allocate_temp_local(ValType::I32);

    // saved_sp = SP
    ctx.emit(Instruction::GlobalGet(sp_global));
    ctx.emit(Instruction::LocalSet(saved_sp_local));

    // SP = saved_sp - padded; frame_ptr = SP
    ctx.emit(Instruction::LocalGet(saved_sp_local));
    ctx.emit(Instruction::I32Const(padded as i32));
    ctx.emit(Instruction::I32Sub);
    ctx.emit(Instruction::LocalTee(frame_ptr_local));
    ctx.emit(Instruction::GlobalSet(sp_global));

    Ok((saved_sp_local, frame_ptr_local))
}

/// Restore SP from the local returned by [`emit_alloc_stack_frame`].
/// Pop the operand-stack result of any preceding runtime call first
/// — this helper only modifies the SP global, not the operand stack,
/// so a still-pending result on top of the operand stack would be left
/// orphaned across the restore.
pub(super) fn emit_restore_stack_frame(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    saved_sp_local: u32,
) -> Result<(), CompileError> {
    let sp_global = b.require_stack_pointer_global()?;
    ctx.emit(Instruction::LocalGet(saved_sp_local));
    ctx.emit(Instruction::GlobalSet(sp_global));
    Ok(())
}

/// One step of the closure heap-layout walk: given `cursor` (the byte
/// position just past the previous capture — starts at 4 to clear the
/// fn-table-idx at offset 0), align `ty` to its natural alignment and
/// return `(offset, end, align)` where `offset` is where this capture
/// starts, `end` is the cursor just past it (`offset + size`), and
/// `align` is the capture's alignment (so callers tracking the object's
/// max alignment don't re-fetch it).
///
/// The single source of the per-capture layout step, shared by the
/// alloc-side walk (`Op::ClosureAlloc`) and the load-side
/// reconstruction ([`capture_byte_offset`]) so the two cannot drift.
fn place_capture(cursor: u32, ty: &IrType) -> Result<(u32, u32, u32), CompileError> {
    let align = phx_field_align_bytes(ty)?;
    let offset = align_up(cursor, align);
    Ok((offset, offset + phx_field_size_bytes(ty)?, align))
}

/// Emit a `call_indirect` through a closure value, leaving the call's
/// result slots on the operand stack for the caller to consume.
///
/// Pushes the env pointer (the closure object itself — env-pointer
/// ABI), then each arg's slot locals in declaration order, then loads
/// the fn-table-idx from `closure[0]` and emits the indirect call
/// through the closure table. The signature is interned per call site
/// from the closure's `ClosureRef { param_types, return_type }` with
/// an i32 env prepended.
///
/// `arg_slot_locals` is one entry per user arg, each the list of WASM
/// locals holding that arg's slots (single-slot scalars: one entry;
/// `StringRef`: `[ptr, len]`). Shared by `Op::CallIndirect` (args
/// from vids) and the inline list-method loops (args from temp
/// locals).
pub(super) fn emit_closure_call_raw(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    closure_ptr_local: u32,
    closure_ir_ty: &IrType,
    arg_slot_locals: &[Vec<u32>],
) -> Result<(), CompileError> {
    let (user_param_types, return_type): (&[IrType], &IrType) = match closure_ir_ty {
        IrType::ClosureRef {
            param_types,
            return_type,
        } => (param_types, return_type.as_ref()),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: closure call receiver has IR type `{other:?}`, \
                 expected `ClosureRef` (internal compiler bug — IR verifier \
                 should have caught this)"
            )));
        }
    };
    let mut full_param_valtypes: Vec<ValType> = Vec::new();
    full_param_valtypes.extend(wasm_valtypes_for(closure_ir_ty)?);
    for pt in user_param_types {
        full_param_valtypes.extend(wasm_valtypes_for(pt)?);
    }
    let return_valtypes = wasm_return_valtypes(return_type)?;
    let type_idx = b.intern_call_indirect_type(&full_param_valtypes, &return_valtypes);
    let table_idx = b.require_closure_table_idx()?;
    // env, then user args, then fn-table-idx (top of stack →
    // consumed by call_indirect as the table index).
    ctx.emit(Instruction::LocalGet(closure_ptr_local));
    for slots in arg_slot_locals {
        for &local in slots {
            ctx.emit(Instruction::LocalGet(local));
        }
    }
    ctx.emit(Instruction::LocalGet(closure_ptr_local));
    ctx.emit(Instruction::I32Load(i32_memarg(0)));
    ctx.emit(Instruction::CallIndirect {
        type_index: type_idx,
        table_index: table_idx,
    });
    Ok(())
}

/// Compute the address of list element `index_local` into a fresh i32
/// local and return it: `base_ptr + LIST_HEADER + index * elem_size`.
/// `index_local` is an i64 counter (Phoenix list indices are `Int`);
/// the `index * elem_size` product is computed in i64 then wrapped to
/// i32 for the address add. Used by the inline list-method loops to
/// load / store elements at a runtime-computed offset (the existing
/// `emit_field_load` / `emit_field_store` take a *constant* offset, so
/// the per-iteration index has to be folded into the base pointer
/// here, then those helpers run with offset 0).
pub(super) fn emit_list_elem_addr(
    ctx: &mut FuncTranslateCtx,
    base_ptr_local: u32,
    index_local: u32,
    elem_size: u32,
) -> u32 {
    let addr_local = ctx.allocate_temp_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(base_ptr_local));
    ctx.emit(Instruction::I32Const(LIST_HEADER as i32));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalGet(index_local));
    ctx.emit(Instruction::I64Const(elem_size as i64));
    ctx.emit(Instruction::I64Mul);
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(addr_local));
    addr_local
}

/// Compute the byte offset of a closure capture given the ordered
/// types of the captures that *precede* it plus the concrete type of
/// the target capture. Walks each preceding capture through
/// [`place_capture`], then places the target the same way and returns
/// its offset.
///
/// Used by the load side ([`FuncTranslateCtx::capture_offset`], which
/// passes `current_capture_types[..idx]` for the preceding walk and the
/// instruction's `result_type` as the target). The alloc side
/// (`Op::ClosureAlloc`) walks the captures itself in O(n) but through
/// the same [`place_capture`] step, so the offsets are byte-for-byte
/// consistent without recomputing the prefix sum per capture.
fn capture_byte_offset(preceding: &[IrType], target_ty: &IrType) -> Result<u32, CompileError> {
    let mut cursor: u32 = 4; // skip the fn-table-idx at offset 0
    for cap_ty in preceding {
        let (_, end, _) = place_capture(cursor, cap_ty)?;
        cursor = end;
    }
    let (offset, _, _) = place_capture(cursor, target_ty)?;
    Ok(offset)
}

/// Emit an i64 binary arith op. Loads both operands, applies the WASM
/// instruction, stores the result into a fresh i64 local. Shared by
/// IAdd / ISub / IMul / IDiv / IMod.
fn emit_i64_binop(
    ctx: &mut FuncTranslateCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op_name: &str,
    wasm_op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, &format!("Op::{op_name}"))?;
    let a_local = ctx.binding_of(a)?.single_local();
    let b_local = ctx.binding_of(b)?.single_local();
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(wasm_op);
    let result_local = ctx.allocate_local(vid, ValType::I64, IrType::I64);
    ctx.emit(Instruction::LocalSet(result_local));
    Ok(())
}

/// Emit an i64 comparison. Same shape as [`emit_i64_binop`] but the
/// result is `Bool` (WASM `i32`).
fn emit_i64_cmp(
    ctx: &mut FuncTranslateCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op_name: &str,
    wasm_op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, &format!("Op::{op_name}"))?;
    let a_local = ctx.binding_of(a)?.single_local();
    let b_local = ctx.binding_of(b)?.single_local();
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(wasm_op);
    let result_local = ctx.allocate_local(vid, ValType::I32, IrType::Bool);
    ctx.emit(Instruction::LocalSet(result_local));
    Ok(())
}

/// Emit an i32 (bool) comparison.
fn emit_i32_cmp(
    ctx: &mut FuncTranslateCtx,
    instr: &phoenix_ir::instruction::Instruction,
    a: ValueId,
    b: ValueId,
    op_name: &str,
    wasm_op: Instruction<'static>,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, &format!("Op::{op_name}"))?;
    let a_local = ctx.binding_of(a)?.single_local();
    let b_local = ctx.binding_of(b)?.single_local();
    ctx.emit(Instruction::LocalGet(a_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(wasm_op);
    let result_local = ctx.allocate_local(vid, ValType::I32, IrType::Bool);
    ctx.emit(Instruction::LocalSet(result_local));
    Ok(())
}

/// Translate a basic-block terminator.
///
/// - `Return(None)` / `Return(Some(v))`: emit a WASM `return` (always
///   exits the function regardless of nesting).
/// - `Jump { target, args }` / `Branch { ... }`: copy args to the
///   target block's param locals, set the dispatch local to the target
///   block's ID, then `br <depth_to_loop>` to re-enter the dispatcher.
/// - `Unreachable`: emit WASM `unreachable` (traps at runtime).
///
/// In single-block functions, `dispatcher` is `None` and only `Return`
/// / `Unreachable` are reachable; the others would mean a sema/IR bug.
fn translate_terminator(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    term: &Terminator,
    dispatcher: Option<DispatcherContext>,
) -> Result<(), CompileError> {
    match term {
        Terminator::Return(None) => {
            // Bare `return` — no operand. WASM `return` exits the
            // function and ignores any nesting; matches Phoenix's
            // "Return ignores enclosing block scopes" semantics.
            // Pop the shadow-stack frame first so the runtime's
            // frame counter stays in lockstep with the actual call
            // depth on every exit path.
            emit_gc_pop_frame(ctx, b)?;
            ctx.emit(Instruction::Return);
            // After `return`, WASM is in unreachable code; we don't
            // need an explicit `unreachable` because every code path
            // ends in a terminator anyway.
            Ok(())
        }
        Terminator::Return(Some(v)) => {
            // Multi-slot returns (`StringRef`) push their slots in
            // declaration order, then `return` exits with all
            // operand-stack values matching the function's return
            // type (WASM multi-value return). Pop the shadow-stack
            // frame *before* pushing the return value: the popped
            // frame is now invisible to GC, but the return value
            // lives on the operand stack (not the shadow stack), so
            // it doesn't matter that the frame is gone — and an
            // intervening `phx_gc_alloc` between the load and the
            // return is impossible (no IR ops can be emitted past a
            // terminator).
            emit_gc_pop_frame(ctx, b)?;
            ctx.emit_load_all(*v)?;
            ctx.emit(Instruction::Return);
            Ok(())
        }
        Terminator::Jump { target, args } => {
            let dispatcher = require_dispatcher(dispatcher)?;
            emit_block_param_copies(ctx, b, *target, args)?;
            ctx.emit(Instruction::I32Const(target.0 as i32));
            ctx.emit(Instruction::LocalSet(dispatcher.dispatch_local));
            ctx.emit(Instruction::Br(dispatcher.depth_to_loop));
            Ok(())
        }
        Terminator::Branch {
            condition,
            true_block,
            true_args,
            false_block,
            false_args,
        } => {
            let dispatcher = require_dispatcher(dispatcher)?;
            let cond_local = ctx.binding_of(*condition)?.single_local();
            ctx.emit(Instruction::LocalGet(cond_local));
            ctx.emit(Instruction::If(BlockType::Empty));
            // Then-branch: jump to `true_block`.
            emit_block_param_copies(ctx, b, *true_block, true_args)?;
            ctx.emit(Instruction::I32Const(true_block.0 as i32));
            ctx.emit(Instruction::LocalSet(dispatcher.dispatch_local));
            ctx.emit(Instruction::Else);
            // Else-branch: jump to `false_block`.
            emit_block_param_copies(ctx, b, *false_block, false_args)?;
            ctx.emit(Instruction::I32Const(false_block.0 as i32));
            ctx.emit(Instruction::LocalSet(dispatcher.dispatch_local));
            ctx.emit(Instruction::End);
            // `If`/`Else`/`End` is `+1` to the WASM nesting depth
            // within the block being emitted; both arms set
            // `dispatch_local` then fall through, and we follow up
            // with the `br` that re-enters the loop. The br depth is
            // measured from *here* (post-`End`), not from inside the
            // If — and since the If/Else/End block has been closed,
            // the depth is the same as the dispatcher's
            // `depth_to_loop`.
            ctx.emit(Instruction::Br(dispatcher.depth_to_loop));
            Ok(())
        }
        Terminator::Unreachable => {
            // `Unreachable` traps the WASM execution; the runtime
            // never returns from a trap, so the frame counter never
            // reads again. Skipping `phx_gc_pop_frame` here is
            // intentional — it'd be unreachable code anyway, and a
            // trap is a hard host-level abort.
            ctx.emit(Instruction::Unreachable);
            Ok(())
        }
        Terminator::Switch { .. } => Err(CompileError::new(
            "wasm32-linear: `Switch` terminator not yet emitted by the IR \
             lowering pass; if it becomes reachable, extend the wasm32-linear \
             terminator translator alongside the IR change",
        )),
        Terminator::None => Err(CompileError::new(
            "wasm32-linear: encountered `Terminator::None` (a placeholder for \
             blocks under construction). The IR verifier should reject any \
             function reaching codegen with such a terminator.",
        )),
    }
}

/// Require a dispatcher context — used by terminators that branch
/// back into the dispatch. The error path indicates a single-block
/// function carrying a `Jump` / `Branch` terminator, which is a
/// sema / IR bug.
fn require_dispatcher(
    dispatcher: Option<DispatcherContext>,
) -> Result<DispatcherContext, CompileError> {
    dispatcher.ok_or_else(|| {
        CompileError::new(
            "wasm32-linear: single-block function carries a `Jump` / `Branch` \
             terminator (internal compiler bug — sema / IR should reject a \
             single-block function with non-`Return` control flow)",
        )
    })
}

/// Copy each Jump/Branch arg into the corresponding block-param local.
/// The IR verifier guarantees `args.len() == target.params.len()`; a
/// mismatch indicates an IR-verifier regression.
///
/// Implemented as a parallel copy: push all source values onto the
/// operand stack first, then pop them into the destinations in
/// reverse order. Why: a back-edge that passes a block's own params
/// in shuffled order (e.g. `jump header(b, a)` where `a, b` are
/// `header`'s params) overlaps the source and destination local
/// sets. A naive `get src; set dest;` per-pair would clobber the
/// remaining sources before they're read — the classic parallel-copy
/// problem. Doing all reads before any writes makes the copy atomic
/// w.r.t. the local-set state and costs only the stack slots that
/// already exist.
///
/// **PR 3b testing status:** the shuffled / overlapping-args path
/// above is *not exercised by any source-level fixture in PR 3b*.
/// Phoenix lowers mutable loop state through `Op::Alloca` rather than
/// block-param threading, so a `Jump { args: [b, a] }` shape can't be
/// expressed in source today. `if_as_expression_runs_under_wasmtime`
/// covers the non-overlapping single-arg case. The overlapping case
/// is correctness-by-construction here until PR 3c lifts loop state
/// onto block params; at that point an end-to-end fixture with a
/// `while`-shaped loop carrying ≥2 block params in shuffled order
/// will pin this path against regression.
fn emit_block_param_copies(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    target: BlockId,
    args: &[ValueId],
) -> Result<(), CompileError> {
    let param_records: Vec<BlockParamRecord> = ctx.block_param_locals_of(target).to_vec();
    if param_records.len() != args.len() {
        return Err(CompileError::new(format!(
            "wasm32-linear: jump to {target:?} has {} args but the target \
             has {} params (internal compiler bug — IR verifier should have \
             caught this)",
            args.len(),
            param_records.len(),
        )));
    }
    // Push every slot of every arg in declaration order so the
    // operand stack ends up `[arg0_slot0, ..., arg0_slotN, arg1_slot0,
    // ...]`. Each arg's slot count matches the corresponding target
    // param's slot count (the IR verifier rejects type mismatches
    // upstream); a multi-slot arg expands to multiple `local.get`s.
    for (arg, record) in args.iter().zip(param_records.iter()) {
        let arg_locals = ctx.binding_of(*arg)?.locals.clone();
        if arg_locals.len() != record.locals.len() {
            return Err(CompileError::new(format!(
                "wasm32-linear: jump to {target:?} arg/param slot-count mismatch \
                 ({} vs {}) — internal compiler bug",
                arg_locals.len(),
                record.locals.len(),
            )));
        }
        for src_local in &arg_locals {
            ctx.emit(Instruction::LocalGet(*src_local));
        }
    }
    // Pop into the target params' slot locals. WASM pops top-of-
    // stack first, so iterate in *reverse* — both across params (last
    // param popped first) and within each param's slots (last slot
    // popped first).
    for record in param_records.iter().rev() {
        for dest_local in record.locals.iter().rev() {
            ctx.emit(Instruction::LocalSet(*dest_local));
        }
    }
    // Re-root any ref-typed block params on the shadow stack. The
    // pre-assigned slot in `gc_root_slot_for` is the param's "home"
    // slot; the value just written into the param's local is what the
    // GC now needs to track. `emit_gc_set_root` filters non-ref params
    // (no entry in `gc_root_slot_for`) and the ref-free-function case
    // (`gc_frame_local == None`), so no spurious `phx_gc_set_root`
    // calls land in the emitted bytecode for either case.
    for record in &param_records {
        emit_gc_set_root(ctx, b, record.vid)?;
    }
    Ok(())
}

pub(super) fn unsupported(ty: &IrType, where_: &str) -> CompileError {
    CompileError::new(format!(
        "wasm32-linear: IR type `{ty:?}` not yet supported in {where_} \
         (Phase 2.4 PR 3 — see docs/design-decisions.md §Phase 2.4)"
    ))
}
