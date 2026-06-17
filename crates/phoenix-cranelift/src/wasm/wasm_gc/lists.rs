//! wasm32-gc `List<T>` / `ListBuilder<T>` type declaration (§Phase 2.4
//! decision K.7).
//!
//! Per K.7, each distinct concrete element type `T` gets a pair of
//! WASM-GC types (plus a builder struct when the module uses
//! `ListBuilder<T>`):
//!
//! ```text
//! $arr_T     = (array (mut T_wasm))
//! $list_T    = (struct (field $len i64)
//!                      (field $data (ref null $arr_T)))
//! $builder_T = (struct (field $len (mut i64))
//!                      (field $frozen (mut i32))
//!                      (field $data (mut (ref null $arr_T))))
//! ```
//!
//! The array is shared between the builder and the lists it freezes
//! into (zero-copy `freeze()`); the `$data` references are nullable so
//! builder buffers are `array.new_default`-able and so locals/fields
//! follow the K.1/K.2 nullable-ref convention (sema guarantees no null
//! is ever read). This module owns the whole K.7 surface: the
//! *declaration* side — collecting every concrete element type from
//! the IR (mirroring [`super::enums`]'s K.4 instantiation collection)
//! and emitting the type-section entries in inner-list-before-outer
//! order so nested `List<List<T>>` element refs always resolve — and
//! the *op lowering* side (`Op::ListAlloc` and the `List.*` /
//! `ListBuilder.*` builtins, under the "K.7 list lowerings" banner
//! below). `translate.rs` keeps only the dispatch arms. (Unlike
//! [`super::enums`], whose lowerings still live in `translate.rs` —
//! lists moved out first because the K.7 helpers are the largest
//! self-contained block; enums can follow the same pattern.) The
//! query accessors live with the builder state.

use std::collections::HashSet;

use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;
use wasm_encoder::{BlockType, HeapType, Instruction, RefType, ValType};

use crate::error::CompileError;

use super::closures::emit_closure_call;
use super::enums::contains_generic_placeholder;
use super::module_builder::ModuleBuilder;
use super::translate::{FuncCtx, expect_result, single_slot, wasm_valtypes_for};

/// Declare WASM-GC types for every `List<T>` / `ListBuilder<T>`
/// instantiation in the IR module per §Phase 2.4 decision K.7.
///
/// Must run *after* `reserve_phoenix_structs`, `declare_string_types`,
/// and `declare_phoenix_enums` (element types of those kinds encode
/// their already-assigned indices — struct *bodies* are filled later,
/// but the index a `List<MyStruct>` element needs already exists once
/// the struct is reserved) and *before* any function signature
/// touching `IrType::ListRef` / `ListBuilderRef` is interned.
///
/// Element-type restriction for slice 7: anything with an existing
/// WASM-GC mapping — primitives, `StringRef`, `StructRef`, `EnumRef`,
/// and nested `ListRef` (declared inner-first). Maps / closures / dyn
/// Trait elements error through `wasm_valtypes_for`'s per-slice
/// diagnostics.
pub(super) fn declare(
    builder: &mut ModuleBuilder,
    ir_module: &IrModule,
) -> Result<(), CompileError> {
    // Pass 0: collect every concrete element type appearing under a
    // `ListRef` / `ListBuilderRef` anywhere in the IR.
    let mut list_elems: HashSet<IrType> = HashSet::new();
    let mut builder_elems: HashSet<IrType> = HashSet::new();
    collect_list_elems(ir_module, &mut list_elems, &mut builder_elems);
    // Every builder element also needs the list pair — `freeze()`
    // returns `List<T>`. (The freeze site's `ListRef` result type makes
    // the walk above catch this in practice; the union keeps the
    // invariant explicit rather than incidental.)
    for elem in &builder_elems {
        list_elems.insert(elem.clone());
    }

    // Sort by (list-nesting depth, debug string): depth puts inner
    // lists before the outer lists whose element refs need their
    // indices; the debug string makes the order deterministic across
    // runs (HashSet iteration is arbitrary; IrType isn't Ord — same
    // trick as the K.4 enum collection).
    let mut ordered: Vec<IrType> = list_elems.into_iter().collect();
    ordered.sort_by_cached_key(|t| (list_depth(t), format!("{t:?}")));

    for elem in &ordered {
        // The shared IrType → ValType mapping resolves element refs;
        // nested list elements resolve because depth ordering already
        // declared (and recorded) the inner instantiation.
        let slots = wasm_valtypes_for(elem, builder)?;
        if slots.len() != 1 {
            return Err(CompileError::new(format!(
                "wasm32-gc slice 7: list element type `{elem:?}` flattens to \
                 {} WASM slots — only single-slot element types are supported \
                 (internal invariant: every K-mapped type is single-slot)",
                slots.len()
            )));
        }
        let elem_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(slots[0]),
            // `mut` so ListBuilder can push in place and `List.push` /
            // `take` / `drop` can fill fresh arrays; Phoenix-level list
            // immutability is a sema invariant, not a WASM one (K.2's
            // `$bytes` precedent).
            mutable: true,
        };
        let arr_idx = builder.declare_list_array(elem_field);
        let arr_ref = arr_ref(arr_idx);
        // $list_T: both fields immutable — no list op ever struct.sets
        // a frozen list (mirrors the enum `$tag` field's immutability).
        let i64_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(wasm_encoder::ValType::I64),
            mutable: false,
        };
        let data_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(arr_ref),
            mutable: false,
        };
        let list_idx = builder.declare_struct(&[i64_field, data_field]);
        builder.record_list(elem.clone(), arr_idx, list_idx);

        if builder_elems.contains(elem) {
            // $builder_T: everything mutable — push bumps $len, growth
            // swaps $data, freeze sets $frozen.
            let len_field = wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(wasm_encoder::ValType::I64),
                mutable: true,
            };
            let frozen_field = wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(wasm_encoder::ValType::I32),
                mutable: true,
            };
            let data_mut_field = wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(arr_ref),
                mutable: true,
            };
            let builder_idx = builder.declare_struct(&[len_field, frozen_field, data_mut_field]);
            builder.record_list_builder(elem.clone(), builder_idx);
        }
    }
    Ok(())
}

/// Walk every IR type in the module, recursively collecting the
/// element type of every `ListRef` / `ListBuilderRef`. Sources mirror
/// the K.4 enum collection: function param/return types, block params,
/// instruction `result_type`s, struct field types, and enum variant
/// field types. Element types still carrying a generic placeholder
/// (inside a generic enum template's fields) are skipped — they're not
/// concrete instantiations.
fn collect_list_elems(
    ir_module: &IrModule,
    lists: &mut HashSet<IrType>,
    builders: &mut HashSet<IrType>,
) {
    let mut walk_all = |ty: &IrType| walk_type(ty, lists, builders);
    for func in ir_module.concrete_functions() {
        walk_all(&func.return_type);
        for ty in &func.param_types {
            walk_all(ty);
        }
        for block in &func.blocks {
            for (_, ty) in &block.params {
                walk_all(ty);
            }
            for instr in &block.instructions {
                walk_all(&instr.result_type);
            }
        }
    }
    for fields in ir_module.struct_layouts.values() {
        for (_, ty) in fields {
            walk_all(ty);
        }
    }
    for variants in ir_module.enum_layouts.values() {
        for (_, fields) in variants {
            for ty in fields {
                walk_all(ty);
            }
        }
    }
}

fn walk_type(ty: &IrType, lists: &mut HashSet<IrType>, builders: &mut HashSet<IrType>) {
    match ty {
        IrType::ListRef(inner) => {
            if !contains_generic_placeholder(inner) {
                lists.insert((**inner).clone());
            }
            walk_type(inner, lists, builders);
        }
        IrType::ListBuilderRef(inner) => {
            if !contains_generic_placeholder(inner) {
                builders.insert((**inner).clone());
            }
            walk_type(inner, lists, builders);
        }
        IrType::StructRef(_, args) | IrType::EnumRef(_, args) => {
            for arg in args {
                walk_type(arg, lists, builders);
            }
        }
        IrType::MapRef(k, v) | IrType::MapBuilderRef(k, v) => {
            // A `Map<K,V>` reuses the `List<K>` / `List<V>` array + struct
            // types for its `$keys` / `$vals` and for `keys()` / `values()`
            // (§K.9), so K and V must be declared as list element types
            // even when no `List<K>` / `List<V>` appears directly.
            if !contains_generic_placeholder(k) {
                lists.insert((**k).clone());
            }
            if !contains_generic_placeholder(v) {
                lists.insert((**v).clone());
            }
            walk_type(k, lists, builders);
            walk_type(v, lists, builders);
        }
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            for p in param_types {
                walk_type(p, lists, builders);
            }
            walk_type(return_type, lists, builders);
        }
        _ => {}
    }
}

/// List-nesting depth of a type: how many `ListRef` / `ListBuilderRef`
/// layers it transitively contains. Drives the inner-before-outer
/// declaration order — an element of depth `d` only references list
/// types of depth `< d`, so ascending-depth declaration always
/// resolves.
///
/// `StructRef` / `EnumRef` arms look through *type args only*, not
/// declared field layouts. That is sound today only because list-typed
/// struct/enum fields are rejected by the slice-3 field restriction
/// (K.7 explicitly defers lifting it). Lifting that restriction makes
/// declaration order a dependency sort across structs/enums/lists —
/// this depth heuristic must be replaced then, not patched.
fn list_depth(ty: &IrType) -> usize {
    match ty {
        IrType::ListRef(inner) | IrType::ListBuilderRef(inner) => 1 + list_depth(inner),
        IrType::StructRef(_, args) | IrType::EnumRef(_, args) => {
            args.iter().map(list_depth).max().unwrap_or(0)
        }
        IrType::MapRef(k, v) | IrType::MapBuilderRef(k, v) => list_depth(k).max(list_depth(v)),
        IrType::ClosureRef {
            param_types,
            return_type,
        } => param_types
            .iter()
            .map(list_depth)
            .max()
            .unwrap_or(0)
            .max(list_depth(return_type)),
        _ => 0,
    }
}
// ───────────────────────── K.7 list lowerings ─────────────────────────
//
// `List<T>` is `$list_T = (struct $len:i64, $data:(ref null $arr_T))`
// over `$arr_T = (array (mut T_wasm))`; `ListBuilder<T>` adds
// `$builder_T = (struct (mut $len:i64), (mut $frozen:i32),
// (mut $data))` sharing the same `$arr_T`. Field indices below follow
// those declarations: list `$len`=0 / `$data`=1; builder `$len`=0 /
// `$frozen`=1 / `$data`=2. See §Phase 2.4 decision K.7 and
// [`declare`] above for the declaration side.

/// `$list_T` field index of `$len`.
const LIST_LEN: u32 = 0;
/// `$list_T` field index of `$data`.
const LIST_DATA: u32 = 1;
/// `$builder_T` field index of `$len`.
const BUILDER_LEN: u32 = 0;
/// `$builder_T` field index of `$frozen`.
const BUILDER_FROZEN: u32 = 1;
/// `$builder_T` field index of `$data`.
const BUILDER_DATA: u32 = 2;

/// Which end `translate_list_take_drop` slices from.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum ListSlice {
    Take,
    Drop,
}

/// Unwrap a `ListRef` result type to its element type. The element
/// type is how every list lowering reaches the K.7 type registry.
fn list_elem_of_result<'a>(
    instr: &'a phoenix_ir::instruction::Instruction,
    op_name: &str,
) -> Result<&'a IrType, CompileError> {
    match &instr.result_type {
        IrType::ListRef(elem) => Ok(elem),
        other => Err(CompileError::new(format!(
            "wasm32-gc: `{op_name}` result type is `{other:?}`, expected \
             `ListRef` (internal compiler bug — IR verifier should have \
             caught this)"
        ))),
    }
}

/// Recover the concrete WASM type-section index from a receiver
/// binding's `(ref null $T)` ValType. Used by lowerings that key off
/// the receiver (list/builder struct ops) rather than the result type.
fn concrete_ref_idx(ctx: &FuncCtx, vid: ValueId, what: &str) -> Result<u32, CompileError> {
    match ctx.binding_type_of(vid)? {
        ValType::Ref(RefType {
            heap_type: HeapType::Concrete(idx),
            ..
        }) => Ok(idx),
        other => Err(CompileError::new(format!(
            "wasm32-gc: {what} receiver lowered to `{other:?}`, expected a \
             concrete ref (internal compiler bug — the IR said this value \
             is a list/builder)"
        ))),
    }
}

/// Arity guard shared by the list lowerings.
fn expect_args(name: &str, args: &[ValueId], n: usize) -> Result<(), CompileError> {
    if args.len() == n {
        Ok(())
    } else {
        Err(CompileError::new(format!(
            "wasm32-gc: `BuiltinCall(\"{name}\")` requires {n} args, got {} \
             (internal compiler bug — IR verifier should have caught this)",
            args.len()
        )))
    }
}

/// `Op::ListAlloc(elems)` — a list literal. Pushes `$len`, then every
/// element for `array.new_fixed`, then wraps both in `struct.new`:
/// the field-order stack discipline avoids any scratch local. Literals
/// beyond `array.new_fixed`'s 10 000-operand engine cap get a real
/// diagnostic here rather than an opaque downstream validation error.
pub(super) fn translate_list_alloc(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    elems: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    // WASM engines cap `array.new_fixed` at 10 000 operands (the
    // JS-API / wasmparser limit), so a larger literal can never
    // validate on this target.
    if elems.len() > 10_000 {
        return Err(CompileError::new(format!(
            "wasm32-gc: a list literal with {} elements exceeds the WASM \
             `array.new_fixed` limit of 10000 — build it with \
             `ListBuilder` and `freeze()` instead",
            elems.len()
        )));
    }
    let vid = expect_result(instr, "Op::ListAlloc")?;
    let elem_ty = list_elem_of_result(instr, "Op::ListAlloc")?;
    let (arr_idx, list_idx) = b.require_list_types(elem_ty)?;
    ctx.emit(Instruction::I64Const(elems.len() as i64));
    for e in elems {
        let local = ctx.binding_of(*e)?;
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit(Instruction::ArrayNewFixed {
        array_type_index: arr_idx,
        array_size: elems.len() as u32,
    });
    ctx.emit(Instruction::StructNew(list_idx));
    let wasm_ty = list_ref(list_idx);
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `List.length(list) -> Int` — one `struct.get $len`. The receiver's
/// binding type carries the `$list_T` index directly.
pub(super) fn translate_list_length(
    ctx: &mut FuncCtx,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"List.length\")")?;
    expect_args("List.length", args, 1)?;
    let list_idx = concrete_ref_idx(ctx, args[0], "`List.length`")?;
    let list_local = ctx.binding_of(args[0])?;
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_LEN,
    });
    let local = ctx.allocate_local(vid, ValType::I64);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `List.get(list, index) -> T` — unsigned-compare bounds check (a
/// negative i64 index wraps to a huge unsigned value, so one `i64.ge_u`
/// covers both native panic cases), trap on failure, then
/// `array.get`. Native prints `runtime error: list index … out of
/// bounds` and exits 1; wasm32-gc follows the established trap
/// precedent (see K.7 "Semantics mapping").
pub(super) fn translate_list_get(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"List.get\")")?;
    expect_args("List.get", args, 2)?;
    // The result type IS the element type.
    let (arr_idx, list_idx) = b.require_list_types(&instr.result_type)?;
    let elem_slots = wasm_valtypes_for(&instr.result_type, b)?;
    let list_local = ctx.binding_of(args[0])?;
    let idx_local = ctx.binding_of(args[1])?;
    // if (index as u64) >= (len as u64) { trap }
    ctx.emit(Instruction::LocalGet(idx_local));
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_LEN,
    });
    ctx.emit(Instruction::I64GeU);
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::Unreachable);
    ctx.emit(Instruction::End);
    // list.$data[index]
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    ctx.emit(Instruction::LocalGet(idx_local));
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::ArrayGet(arr_idx));
    let local = ctx.allocate_local(vid, elem_slots[0]);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `List.push(list, elem) -> List<T>` — immutable push: a fresh list
/// of `len + 1` (matching native's O(n) copy semantics). The
/// receiver's `$len` is read once into a scratch local (same style as
/// `take`/`drop`'s scratches) and reused for the size, the copy
/// length, and the append index; it is written before any read, so no
/// explicit zero-init is needed.
pub(super) fn translate_list_push(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"List.push\")")?;
    expect_args("List.push", args, 2)?;
    let elem_ty = list_elem_of_result(instr, "List.push")?;
    let (arr_idx, list_idx) = b.require_list_types(elem_ty)?;
    let list_local = ctx.binding_of(args[0])?;
    let elem_local = ctx.binding_of(args[1])?;
    let wasm_ty = list_ref(list_idx);
    // len = list.$len
    let len_local = ctx.scratch_local(ValType::I64);
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_LEN,
    });
    ctx.emit(Instruction::LocalSet(len_local));
    // result = struct.new(len + 1, array.new_default(len + 1))
    ctx.emit(Instruction::LocalGet(len_local));
    ctx.emit(Instruction::I64Const(1));
    ctx.emit(Instruction::I64Add);
    ctx.emit(Instruction::LocalGet(len_local));
    ctx.emit(Instruction::I64Const(1));
    ctx.emit(Instruction::I64Add);
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::ArrayNewDefault(arr_idx));
    ctx.emit(Instruction::StructNew(list_idx));
    let result_local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(result_local));
    // array.copy result.$data[0..len] <- list.$data[0..len]
    ctx.emit(Instruction::LocalGet(result_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(len_local));
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::ArrayCopy {
        array_type_index_dst: arr_idx,
        array_type_index_src: arr_idx,
    });
    // result.$data[old_len] = elem
    ctx.emit(Instruction::LocalGet(result_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    ctx.emit(Instruction::LocalGet(len_local));
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::LocalGet(elem_local));
    ctx.emit(Instruction::ArraySet(arr_idx));
    Ok(())
}

/// `List.contains(list, elem) -> Bool` — linear scan with per-element
/// equality matching native's `elements_equal` (§K.7 "Semantics
/// mapping"): value compare for Int/Bool, IEEE `f64.eq` for Float
/// (`NaN != NaN`, `-0.0 == 0.0`), `phx_str_eq` byte equality for
/// String, and `ref.eq` identity for struct/enum elements (native
/// compares the stored pointer). Dispatch is on the element's WASM
/// ValType, mirroring `translate_print`'s shape.
pub(super) fn translate_list_contains(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"List.contains\")")?;
    expect_args("List.contains", args, 2)?;
    // Result is Bool — recover the element type from the receiver.
    let list_idx = concrete_ref_idx(ctx, args[0], "`List.contains`")?;
    let elem_ty = b
        .list_elem_by_struct_idx(list_idx)
        .ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `List.contains` receiver's struct type is not a \
                 recorded list instantiation (internal compiler bug)"
                    .to_string(),
            )
        })?
        .clone();
    let (arr_idx, _) = b.require_list_types(&elem_ty)?;
    let elem_slots = wasm_valtypes_for(&elem_ty, b)?;
    // Pick the comparison up front (borrow of `b` ends before emission).
    enum Cmp {
        I64,
        F64,
        I32,
        StrEq(u32),
        RefIdentity,
    }
    let cmp = match elem_slots[0] {
        ValType::I64 => Cmp::I64,
        ValType::F64 => Cmp::F64,
        ValType::I32 => Cmp::I32,
        ValType::Ref(RefType {
            heap_type: HeapType::Concrete(idx),
            ..
        }) if Some(idx) == b.string_type_idx_if_set() => Cmp::StrEq(b.require_str_eq_idx()?),
        ValType::Ref(_) => Cmp::RefIdentity,
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `List.contains` element lowered to `{other:?}`, \
                 which has no equality mapping (internal compiler bug — K.7 \
                 covers every mapped element type)"
            )));
        }
    };
    let list_local = ctx.binding_of(args[0])?;
    let elem_local = ctx.binding_of(args[1])?;
    let result_local = ctx.allocate_local(vid, ValType::I32);
    let i_local = ctx.scratch_local(ValType::I64);
    // Explicit init — scratch/result locals are only zeroed at function
    // entry, and this lowering can re-execute inside a Phoenix loop.
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(result_local));
    ctx.emit(Instruction::I64Const(0));
    ctx.emit(Instruction::LocalSet(i_local));
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    // i >= len → done (not found)
    ctx.emit(Instruction::LocalGet(i_local));
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_LEN,
    });
    ctx.emit(Instruction::I64GeS);
    ctx.emit(Instruction::BrIf(1));
    // list.$data[i] == elem ?
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    ctx.emit(Instruction::LocalGet(i_local));
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::ArrayGet(arr_idx));
    ctx.emit(Instruction::LocalGet(elem_local));
    match cmp {
        Cmp::I64 => ctx.emit(Instruction::I64Eq),
        Cmp::F64 => ctx.emit(Instruction::F64Eq),
        Cmp::I32 => ctx.emit(Instruction::I32Eq),
        Cmp::StrEq(idx) => ctx.emit(Instruction::Call(idx)),
        Cmp::RefIdentity => ctx.emit(Instruction::RefEq),
    }
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::LocalSet(result_local));
    ctx.emit(Instruction::Br(2)); // exit the outer Block
    ctx.emit(Instruction::End);
    // i += 1
    ctx.emit(Instruction::LocalGet(i_local));
    ctx.emit(Instruction::I64Const(1));
    ctx.emit(Instruction::I64Add);
    ctx.emit(Instruction::LocalSet(i_local));
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);
    Ok(())
}

/// `List.take(list, n)` / `List.drop(list, n)` — a negative `n` traps
/// (every backend errors on negative slice arguments per the
/// 2026-06-10 unification; native aborts with `take()/drop() argument
/// must be non-negative` — K.7 "Semantics mapping"); `n > len` clamps
/// to `len` ("take/skip at most n"). The kept range is copied into a
/// fresh exact-size list.
pub(super) fn translate_list_take_drop(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    which: ListSlice,
) -> Result<(), CompileError> {
    let name = match which {
        ListSlice::Take => "List.take",
        ListSlice::Drop => "List.drop",
    };
    let vid = expect_result(instr, name)?;
    expect_args(name, args, 2)?;
    let elem_ty = list_elem_of_result(instr, name)?;
    let (arr_idx, list_idx) = b.require_list_types(elem_ty)?;
    let list_local = ctx.binding_of(args[0])?;
    let n_local = ctx.binding_of(args[1])?;
    let wasm_ty = list_ref(list_idx);
    let result_local = ctx.allocate_local(vid, wasm_ty);
    // if n < 0 { trap } — negative slice arguments are runtime errors
    // on every backend (2026-06-10 unification).
    ctx.emit(Instruction::LocalGet(n_local));
    ctx.emit(Instruction::I64Const(0));
    ctx.emit(Instruction::I64LtS);
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::Unreachable);
    ctx.emit(Instruction::End);
    // m = min(n, len) — always written before read, so no explicit
    // zero-init is needed despite being a scratch local.
    let m_local = ctx.scratch_local(ValType::I64);
    ctx.emit(Instruction::LocalGet(n_local));
    ctx.emit(Instruction::LocalSet(m_local));
    // min(m, len): select(m, len, m < len)
    ctx.emit(Instruction::LocalGet(m_local));
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_LEN,
    });
    ctx.emit(Instruction::LocalGet(m_local));
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_LEN,
    });
    ctx.emit(Instruction::I64LtS);
    ctx.emit(Instruction::Select);
    ctx.emit(Instruction::LocalSet(m_local));
    // kept = take ? m : len - m   (a second scratch keeps the copy
    // operands readable)
    let kept_local = ctx.scratch_local(ValType::I64);
    match which {
        ListSlice::Take => {
            ctx.emit(Instruction::LocalGet(m_local));
        }
        ListSlice::Drop => {
            ctx.emit(Instruction::LocalGet(list_local));
            ctx.emit(Instruction::StructGet {
                struct_type_index: list_idx,
                field_index: LIST_LEN,
            });
            ctx.emit(Instruction::LocalGet(m_local));
            ctx.emit(Instruction::I64Sub);
        }
    }
    ctx.emit(Instruction::LocalSet(kept_local));
    // result = struct.new(kept, array.new_default(kept))
    ctx.emit(Instruction::LocalGet(kept_local));
    ctx.emit(Instruction::LocalGet(kept_local));
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::ArrayNewDefault(arr_idx));
    ctx.emit(Instruction::StructNew(list_idx));
    ctx.emit(Instruction::LocalSet(result_local));
    // array.copy result.$data[0..kept] <- list.$data[src_off..]
    ctx.emit(Instruction::LocalGet(result_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    match which {
        ListSlice::Take => ctx.emit(Instruction::I32Const(0)),
        ListSlice::Drop => {
            ctx.emit(Instruction::LocalGet(m_local));
            ctx.emit(Instruction::I32WrapI64);
        }
    }
    ctx.emit(Instruction::LocalGet(kept_local));
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::ArrayCopy {
        array_type_index_dst: arr_idx,
        array_type_index_src: arr_idx,
    });
    Ok(())
}

/// `ListBuilder.alloc() -> ListBuilder<T>` — fresh builder: length 0,
/// unfrozen, capacity-8 buffer (matching native's initial capacity).
pub(super) fn translate_list_builder_alloc(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"ListBuilder.alloc\")")?;
    let elem_ty = match &instr.result_type {
        IrType::ListBuilderRef(elem) => elem.as_ref().clone(),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `ListBuilder.alloc` result type is `{other:?}`, \
                 expected `ListBuilderRef` (internal compiler bug)"
            )));
        }
    };
    let builder_idx = b.require_list_builder_idx(&elem_ty)?;
    let (arr_idx, _) = b.require_list_types(&elem_ty)?;
    ctx.emit(Instruction::I64Const(0)); // $len
    ctx.emit(Instruction::I32Const(0)); // $frozen
    ctx.emit(Instruction::I32Const(8)); // initial capacity (native parity)
    ctx.emit(Instruction::ArrayNewDefault(arr_idx));
    ctx.emit(Instruction::StructNew(builder_idx));
    let wasm_ty = list_ref(builder_idx);
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `ListBuilder.push(builder, elem)` (Void) — in-place push with 2×
/// growth at capacity, native parity. Push on a frozen builder traps
/// (native aborts with `builder was already frozen`).
pub(super) fn translate_list_builder_push(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
) -> Result<(), CompileError> {
    expect_args("ListBuilder.push", args, 2)?;
    let builder_idx = concrete_ref_idx(ctx, args[0], "`ListBuilder.push`")?;
    let elem_ty = b
        .builder_elem_by_struct_idx(builder_idx)
        .ok_or_else(|| {
            CompileError::new(
                "wasm32-gc: `ListBuilder.push` receiver's struct type is not \
                 a recorded builder instantiation (internal compiler bug)"
                    .to_string(),
            )
        })?
        .clone();
    let (arr_idx, _) = b.require_list_types(&elem_ty)?;
    let b_local = ctx.binding_of(args[0])?;
    let elem_local = ctx.binding_of(args[1])?;
    // if builder.$frozen { trap }
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_FROZEN,
    });
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::Unreachable);
    ctx.emit(Instruction::End);
    // grow 2× if $len == capacity (== array.len($data))
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_LEN,
    });
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_DATA,
    });
    ctx.emit(Instruction::ArrayLen);
    ctx.emit(Instruction::I64ExtendI32U);
    ctx.emit(Instruction::I64Eq);
    ctx.emit(Instruction::If(BlockType::Empty));
    {
        let grown_local = ctx.scratch_local(arr_ref(arr_idx));
        // grown = array.new_default(capacity * 2). Capacity starts at
        // 8 and only doubles, so it is never 0 here (native's
        // saturating min-1 guard exists for a 0-capacity case this
        // representation can't produce).
        ctx.emit(Instruction::LocalGet(b_local));
        ctx.emit(Instruction::StructGet {
            struct_type_index: builder_idx,
            field_index: BUILDER_DATA,
        });
        ctx.emit(Instruction::ArrayLen);
        ctx.emit(Instruction::I32Const(1));
        ctx.emit(Instruction::I32Shl);
        ctx.emit(Instruction::ArrayNewDefault(arr_idx));
        ctx.emit(Instruction::LocalSet(grown_local));
        // array.copy grown[0..len] <- builder.$data[0..len]
        ctx.emit(Instruction::LocalGet(grown_local));
        ctx.emit(Instruction::I32Const(0));
        ctx.emit(Instruction::LocalGet(b_local));
        ctx.emit(Instruction::StructGet {
            struct_type_index: builder_idx,
            field_index: BUILDER_DATA,
        });
        ctx.emit(Instruction::I32Const(0));
        ctx.emit(Instruction::LocalGet(b_local));
        ctx.emit(Instruction::StructGet {
            struct_type_index: builder_idx,
            field_index: BUILDER_LEN,
        });
        ctx.emit(Instruction::I32WrapI64);
        ctx.emit(Instruction::ArrayCopy {
            array_type_index_dst: arr_idx,
            array_type_index_src: arr_idx,
        });
        // builder.$data = grown
        ctx.emit(Instruction::LocalGet(b_local));
        ctx.emit(Instruction::LocalGet(grown_local));
        ctx.emit(Instruction::StructSet {
            struct_type_index: builder_idx,
            field_index: BUILDER_DATA,
        });
    }
    ctx.emit(Instruction::End);
    // builder.$data[$len] = elem
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_DATA,
    });
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_LEN,
    });
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::LocalGet(elem_local));
    ctx.emit(Instruction::ArraySet(arr_idx));
    // builder.$len += 1
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_LEN,
    });
    ctx.emit(Instruction::I64Const(1));
    ctx.emit(Instruction::I64Add);
    ctx.emit(Instruction::StructSet {
        struct_type_index: builder_idx,
        field_index: BUILDER_LEN,
    });
    Ok(())
}

/// `ListBuilder.freeze(builder) -> List<T>` — the K.7 zero-copy
/// freeze: set `$frozen`, then `struct.new $list_T($len, $data)`
/// sharing the buffer. O(1), vs. native's O(n) memcpy — behavior is
/// identical (the frozen flag blocks all further builder mutation, so
/// the shared buffer is never written again); only the cost model
/// differs. Double-freeze traps (native aborts).
pub(super) fn translate_list_builder_freeze(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"ListBuilder.freeze\")")?;
    expect_args("ListBuilder.freeze", args, 1)?;
    let elem_ty = list_elem_of_result(instr, "ListBuilder.freeze")?;
    let (_, list_idx) = b.require_list_types(elem_ty)?;
    let builder_idx = concrete_ref_idx(ctx, args[0], "`ListBuilder.freeze`")?;
    let b_local = ctx.binding_of(args[0])?;
    // if builder.$frozen { trap }
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_FROZEN,
    });
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::Unreachable);
    ctx.emit(Instruction::End);
    // builder.$frozen = 1
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::StructSet {
        struct_type_index: builder_idx,
        field_index: BUILDER_FROZEN,
    });
    // struct.new $list_T(builder.$len, builder.$data) — zero-copy.
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_LEN,
    });
    ctx.emit(Instruction::LocalGet(b_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: BUILDER_DATA,
    });
    ctx.emit(Instruction::StructNew(list_idx));
    let wasm_ty = list_ref(list_idx);
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

// ─────────────── K.8 follow-up: List closure methods ───────────────
//
// `map` / `filter` / `reduce` / `flatMap` / `sortBy` — each walks the
// receiver's `$data` array, calling a user closure per element via the
// K.8 env-pointer ABI (`emit_closure_call`), and builds a fresh
// `$list_U`. No GC rooting (the host VM traces); no runtime merge
// (decision I) — unlike wasm32-linear these are synthesized inline.
//
// Array indices are i32 (`array.get` / `array.set` operands); the
// list `$len` is i64, wrapped to i32 for the loop bound. `filter`
// relies on the K.7 invariant that `$list.$len` may be < the backing
// array's capacity (it sizes the array at the input length and reports
// the kept count). See §Phase 2.4 decisions K.7 / K.8.

/// Receiver list facts: the `$list_T` struct index, the input `$arr_T`
/// array index, and the element ValType — everything the
/// closure-method walks need. Returning `list_idx` here saves each
/// caller a second `concrete_ref_idx` lookup.
fn recv_list(
    ctx: &FuncCtx,
    b: &ModuleBuilder,
    recv: ValueId,
    what: &str,
) -> Result<(u32, u32, ValType), CompileError> {
    let list_idx = concrete_ref_idx(ctx, recv, what)?;
    let elem_ir = b
        .list_elem_by_struct_idx(list_idx)
        .ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: {what} receiver's struct type is not a recorded \
                 list instantiation (internal compiler bug)"
            ))
        })?
        .clone();
    let (arr_idx, _) = b.require_list_types(&elem_ir)?;
    let elem_vt = single_slot(&elem_ir, b, "list element")?;
    Ok((list_idx, arr_idx, elem_vt))
}

/// Emit `dst = wrap_i32(list_local.$len)`, leaving the i32 length in
/// `dst_local`. Reads field `$len` off the list struct; the receiver
/// ref is non-null (Phoenix lists are always initialized), so the
/// `struct.get` is safe — including for an empty list, whose `$len`
/// is 0.
fn emit_len_i32(ctx: &mut FuncCtx, list_local: u32, list_idx: u32, dst_local: u32) {
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_LEN,
    });
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::LocalSet(dst_local));
}

/// Emit `dst = list_local.$data[i]` — load element `i` of the
/// receiver's backing array (type `arr_idx`) into scratch `dst`. The
/// shared head of every closure-method loop body.
fn emit_load_elem(
    ctx: &mut FuncCtx,
    list_local: u32,
    list_idx: u32,
    arr_idx: u32,
    i: u32,
    dst: u32,
) {
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::ArrayGet(arr_idx));
    ctx.emit(Instruction::LocalSet(dst));
}

/// `List.map(list, f: T -> U) -> List<U>` — `out[i] = f(data[i])` over
/// a fresh `$arr_U` of the same length.
pub(super) fn translate_list_map(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("List.map", args, 2)?;
    let vid = expect_result(instr, "List.map")?;
    let (list_idx, in_arr, in_elem_vt) = recv_list(ctx, b, args[0], "`List.map`")?;
    let list_local = ctx.binding_of(args[0])?;
    let out_elem = list_elem_of_result(instr, "List.map")?;
    let (out_arr, out_list) = b.require_list_types(out_elem)?;

    let len = ctx.scratch_local(ValType::I32);
    let i = ctx.scratch_local(ValType::I32);
    let in_scratch = ctx.scratch_local(in_elem_vt);
    let out_arr_local = ctx.scratch_local(arr_ref(out_arr));

    emit_len_i32(ctx, list_local, list_idx, len);
    // out_arr = array.new_default $arr_U (len)
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayNewDefault(out_arr));
    ctx.emit(Instruction::LocalSet(out_arr_local));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));

    emit_count_loop(ctx, i, len, |ctx| {
        emit_load_elem(ctx, list_local, list_idx, in_arr, i, in_scratch);
        // out_arr[i] = f(in_scratch)
        ctx.emit(Instruction::LocalGet(out_arr_local));
        ctx.emit(Instruction::LocalGet(i));
        emit_closure_call(ctx, b, args[1], &[in_scratch])?;
        ctx.emit(Instruction::ArraySet(out_arr));
        Ok(())
    })?;

    emit_wrap_list(ctx, len, out_arr_local, out_list, vid);
    Ok(())
}

/// `List.filter(list, p: T -> Bool) -> List<T>` — keep elements where
/// `p` holds. The output array is sized at the input length (K.7
/// allows `$len < capacity`) and the result reports the kept count.
pub(super) fn translate_list_filter(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("List.filter", args, 2)?;
    let vid = expect_result(instr, "List.filter")?;
    let (list_idx, in_arr, in_elem_vt) = recv_list(ctx, b, args[0], "`List.filter`")?;
    let list_local = ctx.binding_of(args[0])?;
    let out_elem = list_elem_of_result(instr, "List.filter")?;
    let (out_arr, out_list) = b.require_list_types(out_elem)?;

    let len = ctx.scratch_local(ValType::I32);
    let i = ctx.scratch_local(ValType::I32);
    let count = ctx.scratch_local(ValType::I32);
    let elem_scratch = ctx.scratch_local(in_elem_vt);
    let out_arr_local = ctx.scratch_local(arr_ref(out_arr));

    emit_len_i32(ctx, list_local, list_idx, len);
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayNewDefault(out_arr));
    ctx.emit(Instruction::LocalSet(out_arr_local));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(count));

    emit_count_loop(ctx, i, len, |ctx| {
        emit_load_elem(ctx, list_local, list_idx, in_arr, i, elem_scratch);
        // if p(elem) { out_arr[count] = elem; count += 1 }
        emit_closure_call(ctx, b, args[1], &[elem_scratch])?;
        ctx.emit(Instruction::If(BlockType::Empty));
        ctx.emit(Instruction::LocalGet(out_arr_local));
        ctx.emit(Instruction::LocalGet(count));
        ctx.emit(Instruction::LocalGet(elem_scratch));
        ctx.emit(Instruction::ArraySet(out_arr));
        ctx.emit(Instruction::LocalGet(count));
        ctx.emit(Instruction::I32Const(1));
        ctx.emit(Instruction::I32Add);
        ctx.emit(Instruction::LocalSet(count));
        ctx.emit(Instruction::End);
        Ok(())
    })?;

    // result = struct.new $list_T(count, out_arr) — count, not len.
    emit_wrap_list(ctx, count, out_arr_local, out_list, vid);
    Ok(())
}

/// `(some_variant_idx, none_variant_idx, Option<T> valtype)` for the
/// `Option<T>` a `List<T>` query returns. Shared by `first` / `last` /
/// `find`. The `Option` variant order is Some = 0 (tag/discriminant 0),
/// None = 1 — the K.4 convention the predicate / `Map.get` lowerings
/// also rely on. Everything is derived from `result_type` (which is
/// `EnumRef("Option", [T])` and already carries the element type `T`),
/// so this needs no second lookup against the receiver's list struct.
fn list_option_info(
    b: &ModuleBuilder,
    result_type: &IrType,
) -> Result<(u32, u32, ValType), CompileError> {
    let (name, opt_args) = match result_type {
        IrType::EnumRef(name, args) => (name.as_str(), args.as_slice()),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: List query result type is `{other:?}`, expected \
                 `EnumRef(\"Option\", [T])` (internal compiler bug)"
            )));
        }
    };
    let some_idx = b.require_enum_variant_idx(name, opt_args, 0)?;
    let none_idx = b.require_enum_variant_idx(name, opt_args, 1)?;
    // The valtype is the `(ref null $opt_parent)` both arms settle into.
    let opt_ref = single_slot(result_type, b, "List query `Option` result")?;
    Ok((some_idx, none_idx, opt_ref))
}

/// `List.first(list) -> Option<T>` / `List.last(list) -> Option<T>` —
/// the boundary element wrapped in `Option`, or `None` on an empty
/// list. No closure; `last` reads index `len - 1`, `first` index 0.
pub(super) fn translate_list_first_last(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    last: bool,
) -> Result<(), CompileError> {
    let label = if last { "`List.last`" } else { "`List.first`" };
    expect_args(if last { "List.last" } else { "List.first" }, args, 1)?;
    let vid = expect_result(instr, "List.first/last")?;
    let (list_idx, in_arr, elem_vt) = recv_list(ctx, b, args[0], label)?;
    let list_local = ctx.binding_of(args[0])?;
    let (some_idx, none_idx, opt_ref) = list_option_info(b, &instr.result_type)?;

    let len = ctx.scratch_local(ValType::I32);
    let idx = ctx.scratch_local(ValType::I32);
    let elem = ctx.scratch_local(elem_vt);
    emit_len_i32(ctx, list_local, list_idx, len);

    // if len == 0 { None } else { Some(elem[last ? len-1 : 0]) }
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32Eqz);
    ctx.emit(Instruction::If(BlockType::Result(opt_ref)));
    ctx.emit(Instruction::I32Const(1)); // None tag
    ctx.emit(Instruction::StructNew(none_idx));
    ctx.emit(Instruction::Else);
    if last {
        ctx.emit(Instruction::LocalGet(len));
        ctx.emit(Instruction::I32Const(1));
        ctx.emit(Instruction::I32Sub);
    } else {
        ctx.emit(Instruction::I32Const(0));
    }
    ctx.emit(Instruction::LocalSet(idx));
    emit_load_elem(ctx, list_local, list_idx, in_arr, idx, elem);
    ctx.emit(Instruction::I32Const(0)); // Some tag
    ctx.emit(Instruction::LocalGet(elem));
    ctx.emit(Instruction::StructNew(some_idx));
    ctx.emit(Instruction::End);
    let local = ctx.allocate_local(vid, opt_ref);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `List.find(list, p: T -> Bool) -> Option<T>` — the first element
/// satisfying `p`, wrapped in `Some`, or `None`. Short-circuits on the
/// first match (matching native), so a side-effecting predicate runs
/// exactly as many times as on the other backends: the result local
/// seeds to `None`, and the first matching iteration writes `Some` and
/// `br`s out of the loop.
pub(super) fn translate_list_find(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("List.find", args, 2)?;
    let vid = expect_result(instr, "List.find")?;
    let (list_idx, in_arr, elem_vt) = recv_list(ctx, b, args[0], "`List.find`")?;
    let list_local = ctx.binding_of(args[0])?;
    let (some_idx, none_idx, opt_ref) = list_option_info(b, &instr.result_type)?;

    let len = ctx.scratch_local(ValType::I32);
    let i = ctx.scratch_local(ValType::I32);
    let elem = ctx.scratch_local(elem_vt);
    // The result binding holds `None` until (and unless) a match writes
    // `Some` into it; it is the final value at loop exit, so no copy.
    let result = ctx.allocate_local(vid, opt_ref);

    emit_len_i32(ctx, list_local, list_idx, len);
    ctx.emit(Instruction::I32Const(1)); // None tag
    ctx.emit(Instruction::StructNew(none_idx));
    ctx.emit(Instruction::LocalSet(result));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));

    emit_count_loop(ctx, i, len, |ctx| {
        emit_load_elem(ctx, list_local, list_idx, in_arr, i, elem);
        emit_closure_call(ctx, b, args[1], &[elem])?;
        ctx.emit(Instruction::If(BlockType::Empty));
        ctx.emit(Instruction::I32Const(0)); // Some tag
        ctx.emit(Instruction::LocalGet(elem));
        ctx.emit(Instruction::StructNew(some_idx));
        ctx.emit(Instruction::LocalSet(result));
        // Break the loop: if=0, loop=1, block=2 (see `emit_count_loop`).
        ctx.emit(Instruction::Br(2));
        ctx.emit(Instruction::End);
        Ok(())
    })?;
    Ok(())
}

/// `List.any(list, p) -> Bool` / `List.all(list, p) -> Bool` —
/// short-circuiting quantifiers (matching native). `any` seeds `false`
/// and `br`s out on the first element where `p` holds; `all` seeds
/// `true` and `br`s out on the first where `p` fails (`i32.eqz` of the
/// predicate). `Bool` is an `i32`.
pub(super) fn translate_list_any_all(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    want_all: bool,
) -> Result<(), CompileError> {
    let label = if want_all { "`List.all`" } else { "`List.any`" };
    expect_args(if want_all { "List.all" } else { "List.any" }, args, 2)?;
    let vid = expect_result(instr, "List.any/all")?;
    let (list_idx, in_arr, elem_vt) = recv_list(ctx, b, args[0], label)?;
    let list_local = ctx.binding_of(args[0])?;

    let len = ctx.scratch_local(ValType::I32);
    let i = ctx.scratch_local(ValType::I32);
    let elem = ctx.scratch_local(elem_vt);
    let result = ctx.allocate_local(vid, ValType::I32); // Bool

    emit_len_i32(ctx, list_local, list_idx, len);
    // any seeds false, all seeds true.
    ctx.emit(Instruction::I32Const(if want_all { 1 } else { 0 }));
    ctx.emit(Instruction::LocalSet(result));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));

    emit_count_loop(ctx, i, len, |ctx| {
        emit_load_elem(ctx, list_local, list_idx, in_arr, i, elem);
        emit_closure_call(ctx, b, args[1], &[elem])?;
        // any breaks when p holds; all breaks when p fails (¬p).
        if want_all {
            ctx.emit(Instruction::I32Eqz);
        }
        ctx.emit(Instruction::If(BlockType::Empty));
        ctx.emit(Instruction::I32Const(if want_all { 0 } else { 1 }));
        ctx.emit(Instruction::LocalSet(result));
        ctx.emit(Instruction::Br(2));
        ctx.emit(Instruction::End);
        Ok(())
    })?;
    Ok(())
}

/// `List.reduce(list, init, f: (Acc, T) -> Acc) -> Acc` — left fold.
pub(super) fn translate_list_reduce(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("List.reduce", args, 3)?;
    let vid = expect_result(instr, "List.reduce")?;
    let (list_idx, in_arr, in_elem_vt) = recv_list(ctx, b, args[0], "`List.reduce`")?;
    let list_local = ctx.binding_of(args[0])?;
    let acc_vt = single_slot(&instr.result_type, b, "reduce accumulator")?;

    let len = ctx.scratch_local(ValType::I32);
    let i = ctx.scratch_local(ValType::I32);
    let elem_scratch = ctx.scratch_local(in_elem_vt);
    let init_local = ctx.binding_of(args[1])?;
    // The accumulator *is* the result binding: it holds the fold value
    // each iteration and the final value at loop exit, so no trailing
    // copy into a separate result local is needed.
    let acc = ctx.allocate_local(vid, acc_vt);

    emit_len_i32(ctx, list_local, list_idx, len);
    ctx.emit(Instruction::LocalGet(init_local));
    ctx.emit(Instruction::LocalSet(acc));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));

    emit_count_loop(ctx, i, len, |ctx| {
        emit_load_elem(ctx, list_local, list_idx, in_arr, i, elem_scratch);
        // acc = f(acc, elem)
        emit_closure_call(ctx, b, args[2], &[acc, elem_scratch])?;
        ctx.emit(Instruction::LocalSet(acc));
        Ok(())
    })?;
    Ok(())
}

/// `List.flatMap(list, f: T -> List<U>) -> List<U>` — concatenate the
/// per-element sublists. Each *non-empty* step reallocates the output
/// to exactly `out_len + sub_len` and `array.copy`s the running output
/// plus the new sublist in (O(n²) on total length, fine for the small
/// lists the method sees; avoids a growable-buffer dance and needs only
/// `$arr_U` / `$list_U`, both already declared for `List<U>`). Empty
/// sublists are skipped, so they cost nothing.
pub(super) fn translate_list_flat_map(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("List.flatMap", args, 2)?;
    let vid = expect_result(instr, "List.flatMap")?;
    let (list_idx, in_arr, in_elem_vt) = recv_list(ctx, b, args[0], "`List.flatMap`")?;
    let list_local = ctx.binding_of(args[0])?;
    let out_elem = list_elem_of_result(instr, "List.flatMap")?;
    let (out_arr, out_list) = b.require_list_types(out_elem)?;
    // The closure returns `List<U>`, whose `$list_U` struct is the
    // output list type's index.
    let sub_list_idx = out_list;

    let len = ctx.scratch_local(ValType::I32);
    let i = ctx.scratch_local(ValType::I32);
    let out_len = ctx.scratch_local(ValType::I32);
    let out_arr_local = ctx.scratch_local(arr_ref(out_arr));
    let sub_local = ctx.scratch_local(list_ref(sub_list_idx));
    let sub_len = ctx.scratch_local(ValType::I32);
    let new_arr = ctx.scratch_local(arr_ref(out_arr));
    let elem_in = ctx.scratch_local(in_elem_vt);

    emit_len_i32(ctx, list_local, list_idx, len);
    // out_arr = array.new_default $arr_U (0); out_len = 0
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::ArrayNewDefault(out_arr));
    ctx.emit(Instruction::LocalSet(out_arr_local));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(out_len));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));

    emit_count_loop(ctx, i, len, |ctx| {
        // sub = f(data[i])
        emit_load_elem(ctx, list_local, list_idx, in_arr, i, elem_in);
        emit_closure_call(ctx, b, args[1], &[elem_in])?;
        ctx.emit(Instruction::LocalSet(sub_local));
        // sub_len = wrap(sub.$len)
        ctx.emit(Instruction::LocalGet(sub_local));
        ctx.emit(Instruction::StructGet {
            struct_type_index: sub_list_idx,
            field_index: LIST_LEN,
        });
        ctx.emit(Instruction::I32WrapI64);
        ctx.emit(Instruction::LocalSet(sub_len));
        // An empty sublist contributes nothing: skip the realloc +
        // copy, which would otherwise allocate a fresh `out_len`-sized
        // array and copy every existing element to produce an identical
        // one. The `sparse` test hits this branch three times in a row.
        ctx.emit(Instruction::LocalGet(sub_len));
        ctx.emit(Instruction::If(BlockType::Empty));
        // new = array.new_default(out_len + sub_len)
        ctx.emit(Instruction::LocalGet(out_len));
        ctx.emit(Instruction::LocalGet(sub_len));
        ctx.emit(Instruction::I32Add);
        ctx.emit(Instruction::ArrayNewDefault(out_arr));
        ctx.emit(Instruction::LocalSet(new_arr));
        // array.copy new[0..out_len] <- out_arr[0..out_len]
        ctx.emit(Instruction::LocalGet(new_arr));
        ctx.emit(Instruction::I32Const(0));
        ctx.emit(Instruction::LocalGet(out_arr_local));
        ctx.emit(Instruction::I32Const(0));
        ctx.emit(Instruction::LocalGet(out_len));
        ctx.emit(Instruction::ArrayCopy {
            array_type_index_dst: out_arr,
            array_type_index_src: out_arr,
        });
        // array.copy new[out_len..] <- sub.$data[0..sub_len]
        ctx.emit(Instruction::LocalGet(new_arr));
        ctx.emit(Instruction::LocalGet(out_len));
        ctx.emit(Instruction::LocalGet(sub_local));
        ctx.emit(Instruction::StructGet {
            struct_type_index: sub_list_idx,
            field_index: LIST_DATA,
        });
        ctx.emit(Instruction::I32Const(0));
        ctx.emit(Instruction::LocalGet(sub_len));
        ctx.emit(Instruction::ArrayCopy {
            array_type_index_dst: out_arr,
            array_type_index_src: out_arr,
        });
        // out_arr = new; out_len += sub_len
        ctx.emit(Instruction::LocalGet(new_arr));
        ctx.emit(Instruction::LocalSet(out_arr_local));
        ctx.emit(Instruction::LocalGet(out_len));
        ctx.emit(Instruction::LocalGet(sub_len));
        ctx.emit(Instruction::I32Add);
        ctx.emit(Instruction::LocalSet(out_len));
        ctx.emit(Instruction::End); // if sub_len != 0
        Ok(())
    })?;

    emit_wrap_list(ctx, out_len, out_arr_local, out_list, vid);
    Ok(())
}

/// `List.sortBy(list, cmp: (T, T) -> Int) -> List<T>` — stable
/// bottom-up iterative merge sort (**O(n log n)**, matching native and
/// the interpreters; `cmp <= 0` keeps ties in input order). Two buffers
/// `src`/`dst` ping-pong each width pass (no per-pass copyback); after
/// the loop the sorted data is in whichever `src` holds — odd pass
/// counts leave it in `aux` rather than `copy`. No GC rooting: the host
/// VM traces the array-ref locals (`src`, `dst`) automatically.
pub(super) fn translate_list_sort_by(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("List.sortBy", args, 2)?;
    let vid = expect_result(instr, "List.sortBy")?;
    let (list_idx, in_arr, elem_vt) = recv_list(ctx, b, args[0], "`List.sortBy`")?;
    let list_local = ctx.binding_of(args[0])?;
    let out_elem = list_elem_of_result(instr, "List.sortBy")?;
    let (out_arr, out_list) = b.require_list_types(out_elem)?;

    let len = ctx.scratch_local(ValType::I32);
    let width = ctx.scratch_local(ValType::I32);
    let i = ctx.scratch_local(ValType::I32); // run-pair start
    let lo = ctx.scratch_local(ValType::I32);
    let mid = ctx.scratch_local(ValType::I32);
    let hi = ctx.scratch_local(ValType::I32);
    let a = ctx.scratch_local(ValType::I32); // left cursor
    let bb = ctx.scratch_local(ValType::I32); // right cursor
    let k = ctx.scratch_local(ValType::I32); // merge write cursor
    let a_elem = ctx.scratch_local(elem_vt);
    let b_elem = ctx.scratch_local(elem_vt);
    let cmp_res = ctx.scratch_local(ValType::I64);
    // `src`/`dst` are array-ref locals swapped each width pass; the host
    // VM auto-roots them across the comparator call.
    let src = ctx.scratch_local(arr_ref(out_arr));
    let dst = ctx.scratch_local(arr_ref(out_arr));
    let tmp = ctx.scratch_local(arr_ref(out_arr)); // swap scratch

    emit_len_i32(ctx, list_local, list_idx, len);
    // copy (src) = array.new_default(len); array.copy src[0..len] <- data[0..len]
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayNewDefault(out_arr));
    ctx.emit(Instruction::LocalSet(src));
    ctx.emit(Instruction::LocalGet(src));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(list_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: list_idx,
        field_index: LIST_DATA,
    });
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayCopy {
        array_type_index_dst: out_arr,
        array_type_index_src: in_arr,
    });

    // Trivial path: len <= 1 → wrap src as-is (no aux, no passes).
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32GtS);
    ctx.emit(Instruction::If(BlockType::Empty));

    // aux (dst) = array.new_default(len).
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayNewDefault(out_arr));
    ctx.emit(Instruction::LocalSet(dst));

    // width = 1
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::LocalSet(width));
    // outer: while width < len
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(width));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1)); // break outer
    // i = 0
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));
    // middle: while i < len
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1)); // break middle
    // lo = i
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalSet(lo));
    // mid = min(i + width, len)
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(width));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(mid));
    ctx.emit(Instruction::LocalGet(mid));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::LocalGet(mid));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32LtS);
    ctx.emit(Instruction::Select);
    ctx.emit(Instruction::LocalSet(mid));
    // hi = min(i + 2*width, len)
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(width));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Shl);
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(hi));
    ctx.emit(Instruction::LocalGet(hi));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::LocalGet(hi));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32LtS);
    ctx.emit(Instruction::Select);
    ctx.emit(Instruction::LocalSet(hi));
    // a = lo; bb = mid; k = lo
    ctx.emit(Instruction::LocalGet(lo));
    ctx.emit(Instruction::LocalSet(a));
    ctx.emit(Instruction::LocalGet(mid));
    ctx.emit(Instruction::LocalSet(bb));
    ctx.emit(Instruction::LocalGet(lo));
    ctx.emit(Instruction::LocalSet(k));
    // merge: while k < hi
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(k));
    ctx.emit(Instruction::LocalGet(hi));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1)); // break merge
    // Load each side's current element into its local *at most once* per
    // iteration, gated on the run still having elements, then reuse that
    // load for both the comparison and the store. `a < mid` ⇒ `src[a]`
    // valid; `bb < hi` ⇒ `src[bb]` valid. The winning side is always one
    // that was loaded (take_left ⇒ a<mid; !take_left ⇒ bb<hi), so the
    // store reuses its local with no reload. (The host VM auto-roots the
    // ref-typed `a_elem`/`b_elem` across the comparator call.)
    ctx.emit(Instruction::LocalGet(a));
    ctx.emit(Instruction::LocalGet(mid));
    ctx.emit(Instruction::I32LtS);
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(src));
    ctx.emit(Instruction::LocalGet(a));
    ctx.emit(Instruction::ArrayGet(out_arr));
    ctx.emit(Instruction::LocalSet(a_elem));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::LocalGet(bb));
    ctx.emit(Instruction::LocalGet(hi));
    ctx.emit(Instruction::I32LtS);
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(src));
    ctx.emit(Instruction::LocalGet(bb));
    ctx.emit(Instruction::ArrayGet(out_arr));
    ctx.emit(Instruction::LocalSet(b_elem));
    ctx.emit(Instruction::End);
    // take_left = (a < mid) && (bb >= hi || cmp(a_elem, b_elem) <= 0)
    // Compute take_left into an i32 via control flow: default false, set
    // true when the left run wins.
    // First: a < mid ?
    ctx.emit(Instruction::LocalGet(a));
    ctx.emit(Instruction::LocalGet(mid));
    ctx.emit(Instruction::I32LtS);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // left has elements — does it win? bb >= hi → yes outright.
    ctx.emit(Instruction::LocalGet(bb));
    ctx.emit(Instruction::LocalGet(hi));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    ctx.emit(Instruction::I32Const(1)); // right exhausted → take left
    ctx.emit(Instruction::Else);
    // both runs live: cmp(a_elem, b_elem) <= 0 → take left (stable).
    emit_closure_call(ctx, b, args[1], &[a_elem, b_elem])?;
    ctx.emit(Instruction::LocalSet(cmp_res));
    ctx.emit(Instruction::LocalGet(cmp_res));
    ctx.emit(Instruction::I64Const(0));
    ctx.emit(Instruction::I64LeS);
    ctx.emit(Instruction::End); // if (bb >= hi)
    ctx.emit(Instruction::Else);
    ctx.emit(Instruction::I32Const(0)); // left exhausted → take right
    ctx.emit(Instruction::End); // if (a < mid)
    // if take_left { dst[k] = a_elem; a += 1 } else { dst[k] = b_elem; bb += 1 }
    // The winning side's element is already in its local (loaded above),
    // so the store reuses it with no reload.
    ctx.emit(Instruction::If(BlockType::Empty));
    // dst[k] = a_elem
    ctx.emit(Instruction::LocalGet(dst));
    ctx.emit(Instruction::LocalGet(k));
    ctx.emit(Instruction::LocalGet(a_elem));
    ctx.emit(Instruction::ArraySet(out_arr));
    // a += 1
    ctx.emit(Instruction::LocalGet(a));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(a));
    ctx.emit(Instruction::Else);
    // dst[k] = b_elem
    ctx.emit(Instruction::LocalGet(dst));
    ctx.emit(Instruction::LocalGet(k));
    ctx.emit(Instruction::LocalGet(b_elem));
    ctx.emit(Instruction::ArraySet(out_arr));
    // bb += 1
    ctx.emit(Instruction::LocalGet(bb));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(bb));
    ctx.emit(Instruction::End); // if take_left
    // k += 1
    ctx.emit(Instruction::LocalGet(k));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(k));
    ctx.emit(Instruction::Br(0)); // continue merge
    ctx.emit(Instruction::End); // loop merge
    ctx.emit(Instruction::End); // block merge
    // i += 2*width
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(width));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Shl);
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(i));
    ctx.emit(Instruction::Br(0)); // continue middle
    ctx.emit(Instruction::End); // loop middle
    ctx.emit(Instruction::End); // block middle
    // swap src and dst
    ctx.emit(Instruction::LocalGet(src));
    ctx.emit(Instruction::LocalSet(tmp));
    ctx.emit(Instruction::LocalGet(dst));
    ctx.emit(Instruction::LocalSet(src));
    ctx.emit(Instruction::LocalGet(tmp));
    ctx.emit(Instruction::LocalSet(dst));
    // width *= 2
    ctx.emit(Instruction::LocalGet(width));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Shl);
    ctx.emit(Instruction::LocalSet(width));
    ctx.emit(Instruction::Br(0)); // continue outer
    ctx.emit(Instruction::End); // loop outer
    ctx.emit(Instruction::End); // block outer

    ctx.emit(Instruction::End); // if len > 1

    // Sorted data is in `src` (the swap leaves the final merged buffer there).
    emit_wrap_list(ctx, len, src, out_list, vid);
    Ok(())
}

/// `for i in 0..bound { body }` over an i32 counter `i` already
/// initialized to 0, with `bound` an i32 local. The body must leave
/// the stack balanced.
///
/// The loop is `block(loop(...))` (two nesting levels), so body code may
/// short-circuit by emitting `Br(2)` to exit the whole loop — `if` =
/// depth 0, `loop` = 1, the outer `block` = 2 (see `find` / `any` /
/// `all`). Anything that wraps this in another block must adjust that
/// depth.
fn emit_count_loop(
    ctx: &mut FuncCtx,
    i: u32,
    bound: u32,
    body: impl FnOnce(&mut FuncCtx) -> Result<(), CompileError>,
) -> Result<(), CompileError> {
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(bound));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1)); // break
    body(ctx)?;
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(i));
    ctx.emit(Instruction::Br(0)); // continue
    ctx.emit(Instruction::End); // loop
    ctx.emit(Instruction::End); // block
    Ok(())
}

/// `result = struct.new $list(extend_i64_u(len_i32), arr)` bound to
/// `vid` (the standard list-producing tail).
fn emit_wrap_list(ctx: &mut FuncCtx, len_i32: u32, arr_local: u32, list_idx: u32, vid: ValueId) {
    ctx.emit(Instruction::LocalGet(len_i32));
    ctx.emit(Instruction::I64ExtendI32U);
    ctx.emit(Instruction::LocalGet(arr_local));
    ctx.emit(Instruction::StructNew(list_idx));
    let wasm_ty = list_ref(list_idx);
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
}

/// `(ref null $arr)` ValType for an array type index.
fn arr_ref(arr_idx: u32) -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(arr_idx),
    })
}

/// `(ref null $struct)` ValType for a list-family struct index —
/// `$list_T` or the structurally-identical `$builder_T`.
fn list_ref(list_idx: u32) -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(list_idx),
    })
}
