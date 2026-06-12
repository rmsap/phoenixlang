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

use super::enums::contains_generic_placeholder;
use super::module_builder::ModuleBuilder;
use super::translate::{FuncCtx, expect_result, wasm_valtypes_for};

/// Declare WASM-GC types for every `List<T>` / `ListBuilder<T>`
/// instantiation in the IR module per §Phase 2.4 decision K.7.
///
/// Must run *after* `declare_phoenix_structs`, `declare_string_types`,
/// and `declare_phoenix_enums` (element types of those kinds encode
/// their already-declared indices) and *before* any function signature
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
        let arr_ref = wasm_encoder::ValType::Ref(wasm_encoder::RefType {
            nullable: true,
            heap_type: wasm_encoder::HeapType::Concrete(arr_idx),
        });
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
        let list_idx = builder.declare_list_struct(&[i64_field, data_field]);
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
            let builder_idx =
                builder.declare_list_struct(&[len_field, frozen_field, data_mut_field]);
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
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(list_idx),
    });
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
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(list_idx),
    });
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
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(list_idx),
    });
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
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(builder_idx),
    });
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
        let arr_ref_ty = ValType::Ref(RefType {
            nullable: true,
            heap_type: HeapType::Concrete(arr_idx),
        });
        let grown_local = ctx.scratch_local(arr_ref_ty);
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
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(list_idx),
    });
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}
