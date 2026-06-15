//! wasm32-gc `dyn Trait` type declaration and op lowering (§Phase 2.4
//! decision K.10).
//!
//! Per K.10, `dyn` dispatches through typed function references
//! (`call_ref`) over a per-trait vtable struct of heterogeneous typed
//! funcrefs — the K.8 closure pattern reapplied:
//!
//! ```text
//! $dynfn_T_i = (func (param (ref null struct))   ;; abstract self
//!                    (param P_i ...) (result R_i))
//! $vtable_T  = (struct (field $m0 (ref $dynfn_T_0)) …)  ;; one per trait
//! $dyn_T     = (struct (field $data (ref null struct))
//!                      (field $vt   (ref null $vtable_T)))
//! ```
//!
//! A `dyn` value's `$data` is the concrete receiver upcast to the
//! abstract `(ref null struct)`; the concrete methods (`Circle.draw`,
//! typed `self: Circle`) are bridged by per-`(trait, concrete, slot)`
//! **trampolines** that `ref.cast` `$data` back to the concrete and
//! `call` the real method, returning its result. Each `(trait,
//! concrete)` pair's vtable is built once into a WASM global (the
//! trampoline `ref.func`s); a
//! `List<dyn Shape>` allocates the vtable once, not per element.
//!
//! `Op::DynAlloc(T, C, val)` → `struct.new $dyn_T(val, global.get
//! $vt_T_C)`. `Op::DynCall(T, slot, recv, args)` → push `recv.$data`,
//! the args, and `recv.$vt.$m{slot}`, then `call_ref $dynfn_T_slot`.
//! `IrType::DynRef(T)` → `(ref null $dyn_T)`, a single-slot ref that
//! slots uniformly into params / returns / fields / list elements.
//!
//! Concrete types are structs (the K.1 struct index is the `ref.cast`
//! target); a non-struct concrete (an enum `impl`) errors until a
//! fixture needs it.

use std::collections::BTreeSet;

use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;
use wasm_encoder::{ConstExpr, Function, HeapType, Instruction, RefType, ValType};

use crate::error::CompileError;

use super::closures::env_valtype;
use super::module_builder::ModuleBuilder;
use super::translate::{FuncCtx, bind_call_result, expect_result, single_slot};

/// `$dyn_T` field index of `$data`.
const DYN_DATA: u32 = 0;
/// `$dyn_T` field index of `$vt`.
const DYN_VT: u32 = 1;

/// The `$dynfn_T_i` return valtypes for a trait method return type.
fn return_valtypes(ret: &IrType, b: &ModuleBuilder) -> Result<Vec<ValType>, CompileError> {
    match ret {
        IrType::Void => Ok(Vec::new()),
        ty => Ok(vec![single_slot(ty, b, "dyn method return")?]),
    }
}

/// The set of traits actually coerced to `dyn` — the distinct trait
/// names across every `(concrete, trait)` vtable key.
fn dyn_traits(ir_module: &IrModule) -> BTreeSet<String> {
    ir_module
        .dyn_vtables
        .keys()
        .map(|(_concrete, trait_name)| trait_name.clone())
        .collect()
}

/// **Reserve** each trait's `$dyn_T` type-section index, early — before
/// structs / lists are declared — so a `dyn` struct field or a
/// `List<dyn T>` element can embed `(ref null $dyn_T)` even though the
/// trait's `$dynfn` / `$vtable_T` (which `$dyn_T` itself points at) are
/// only built later by [`define_types`]. The reserved index is recorded
/// (with placeholder vtable / method indices) so [`dyn_valtype`] — the
/// `IrType::DynRef` → valtype mapping consulted during struct / list
/// declaration — resolves it; the placeholder is overwritten in
/// `define_types`. The whole graph lands in one rec group (§Phase 2.4
/// K.10), which makes the resulting forward references legal.
pub(super) fn reserve_types(builder: &mut ModuleBuilder, ir_module: &IrModule) {
    for trait_name in dyn_traits(ir_module) {
        let dyn_idx = builder.reserve_dyn_struct();
        // Placeholder vtable index / empty method list — overwritten by
        // `define_types`. Only `dyn_idx` is read before then.
        builder.record_dyn_trait(trait_name, dyn_idx, 0, Vec::new());
    }
}

/// **Define** the `$dynfn_T_i` / `$vtable_T` / `$dyn_T` types per trait
/// per §Phase 2.4 decision K.10. Runs after structs / enums / lists /
/// maps / closures (so method param / return types resolve) and fills
/// the `$dyn_T` slots [`reserve_types`] reserved earlier.
pub(super) fn define_types(
    builder: &mut ModuleBuilder,
    ir_module: &IrModule,
) -> Result<(), CompileError> {
    for trait_name in dyn_traits(ir_module) {
        let info = ir_module.traits.get(&trait_name).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: trait `{trait_name}` referenced by a vtable but \
                 missing from `IrModule::traits` (internal compiler bug)"
            ))
        })?;
        // The `$dyn_T` index was reserved by `reserve_types`.
        let (dyn_idx, _placeholder_vt, _placeholder_fns) =
            builder.require_dyn_trait(&trait_name)?;
        // One $dynfn func type per method slot: abstract self, then the
        // user params (excluding self), then the return.
        let mut fn_type_idxs = Vec::with_capacity(info.methods.len());
        for m in &info.methods {
            let mut params = vec![env_valtype()];
            for pt in &m.param_types {
                params.push(single_slot(pt, builder, "dyn method parameter")?);
            }
            let returns = return_valtypes(&m.return_type, builder)?;
            fn_type_idxs.push(builder.intern_dyn_fn_type(&params, &returns));
        }
        // $vtable_T: one non-null typed-funcref field per slot.
        let vtable_fields: Vec<wasm_encoder::FieldType> = fn_type_idxs
            .iter()
            .map(|&fi| wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(fi),
                })),
                mutable: false,
            })
            .collect();
        let vtable_idx = builder.declare_dyn_struct(&vtable_fields);
        // $dyn_T: (data: abstract struct ref, vt: $vtable_T ref) — filled
        // into the reserved slot, not freshly declared.
        let data_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(env_valtype()),
            mutable: false,
        };
        let vt_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(vtable_idx),
            })),
            mutable: false,
        };
        builder.define_dyn_struct(dyn_idx, &[data_field, vt_field]);
        builder.record_dyn_trait(trait_name, dyn_idx, vtable_idx, fn_type_idxs);
    }
    Ok(())
}

/// A `(concrete, trait)` vtable's entry count disagrees with the
/// trait's declared method count, so a slot index has no matching
/// `$dynfn` / method. The IR builds both from the trait declaration in
/// lockstep, so this can only be an internal compiler bug (K.10).
fn vtable_slot_mismatch(
    trait_name: &str,
    concrete: &str,
    slot: usize,
    n_entries: usize,
    n_methods: usize,
) -> CompileError {
    CompileError::new(format!(
        "wasm32-gc: `dyn {trait_name}` vtable for `{concrete}` has {n_entries} \
         entries but the trait declares {n_methods} methods — slot {slot} out \
         of range (internal compiler bug; K.10)"
    ))
}

/// Deterministic `(concrete, trait)` vtable order — sorted clone of the
/// `dyn_vtables` keys, matching the K.4/K.7 determinism convention. This
/// is the *single* source of iteration order for the three deferred
/// passes ([`declare_trampolines`] → [`emit_vtable_globals`] →
/// [`emit_trampoline_bodies`]), which must agree on order so the
/// function and code sections stay positionally parallel. Computed once
/// in `compile_wasm_gc` and threaded through all three rather than
/// recomputed per pass.
pub(super) fn ordered_vtable_keys(ir_module: &IrModule) -> Vec<(String, String)> {
    let mut keys: Vec<(String, String)> = ir_module.dyn_vtables.keys().cloned().collect();
    keys.sort();
    keys
}

/// Reserve a trampoline function (signature = the trait method's
/// `$dynfn`) per `(trait, concrete, slot)`. Deferred-body — bodies
/// emitted by [`emit_trampoline_bodies`] after the user functions.
/// `keys` is the shared [`ordered_vtable_keys`] order.
pub(super) fn declare_trampolines(
    builder: &mut ModuleBuilder,
    ir_module: &IrModule,
    keys: &[(String, String)],
) -> Result<(), CompileError> {
    for (concrete, trait_name) in keys {
        let (_dyn_idx, _vtable_idx, fn_type_idxs) = builder.require_dyn_trait(trait_name)?;
        let fn_type_idxs = fn_type_idxs.to_vec();
        let entries = &ir_module.dyn_vtables[&(concrete.clone(), trait_name.clone())];
        for (slot, _entry) in entries.iter().enumerate() {
            let fn_type_idx = *fn_type_idxs.get(slot).ok_or_else(|| {
                vtable_slot_mismatch(
                    trait_name,
                    concrete,
                    slot,
                    entries.len(),
                    fn_type_idxs.len(),
                )
            })?;
            builder.add_dyn_trampoline(
                trait_name.clone(),
                concrete.clone(),
                slot as u32,
                fn_type_idx,
            );
        }
    }
    Ok(())
}

/// Emit each trampoline body: `ref.cast` the abstract `self` to the
/// concrete struct, `call` the real method, and return its result.
pub(super) fn emit_trampoline_bodies(
    builder: &mut ModuleBuilder,
    ir_module: &IrModule,
    keys: &[(String, String)],
) -> Result<(), CompileError> {
    for (concrete, trait_name) in keys {
        let concrete_struct = builder.require_phx_struct(concrete).map_err(|_| {
            CompileError::new(format!(
                "wasm32-gc: `dyn {trait_name}` concrete `{concrete}` is not a \
                 declared struct — dyn over non-struct concretes (enum impls) \
                 is not yet supported (the `ref.cast` needs a struct target); \
                 lands when a fixture needs it (K.10)"
            ))
        })?;
        let info = ir_module.traits.get(trait_name).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: trait `{trait_name}` referenced by a vtable but \
                 missing from `IrModule::traits` (internal compiler bug; K.10)"
            ))
        })?;
        let entries = &ir_module.dyn_vtables[&(concrete.clone(), trait_name.clone())];
        for (slot, (_method_name, fid)) in entries.iter().enumerate() {
            let user_idx = builder.require_phx_user_func(*fid)?;
            let method = info.methods.get(slot).ok_or_else(|| {
                vtable_slot_mismatch(
                    trait_name,
                    concrete,
                    slot,
                    entries.len(),
                    info.methods.len(),
                )
            })?;
            let n_params = method.param_types.len();
            let mut f = Function::new([]); // no extra locals beyond params
            // self (local 0) → ref.cast → (ref $concrete)
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                concrete_struct,
            )));
            // user params (locals 1..=n)
            for i in 0..n_params {
                f.instruction(&Instruction::LocalGet((i + 1) as u32));
            }
            f.instruction(&Instruction::Call(user_idx));
            f.instruction(&Instruction::End);
            builder.emit_dyn_trampoline_body(&f);
        }
    }
    Ok(())
}

/// Build each `(trait, concrete)` vtable global: `struct.new $vtable_T`
/// of the trampoline `ref.func`s, as a constant init expression.
pub(super) fn emit_vtable_globals(
    builder: &mut ModuleBuilder,
    ir_module: &IrModule,
    keys: &[(String, String)],
) -> Result<(), CompileError> {
    for (concrete, trait_name) in keys {
        let (_dyn_idx, vtable_idx, _fn) = builder.require_dyn_trait(trait_name)?;
        let entries = &ir_module.dyn_vtables[&(concrete.clone(), trait_name.clone())];
        let mut insns: Vec<Instruction> = Vec::with_capacity(entries.len() + 1);
        for slot in 0..entries.len() {
            let tramp = builder.require_dyn_trampoline(trait_name, concrete, slot as u32)?;
            insns.push(Instruction::RefFunc(tramp));
        }
        insns.push(Instruction::StructNew(vtable_idx));
        let init = ConstExpr::extended(insns);
        builder.add_dyn_vtable_global(trait_name.clone(), concrete.clone(), vtable_idx, &init);
    }
    Ok(())
}

// ───────────────────────── K.10 lowerings ─────────────────────────

/// `Op::DynAlloc(trait, concrete, value)` — `struct.new $dyn_T(value,
/// global.get $vt_T_C)`. The concrete `value` upcasts implicitly to
/// the abstract `(ref null struct)` `$data` field.
pub(super) fn translate_dyn_alloc(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    trait_name: &str,
    concrete: &str,
    value: ValueId,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::DynAlloc")?;
    let (dyn_idx, _vtable_idx, _fn) = b.require_dyn_trait(trait_name)?;
    let vt_global = b.require_dyn_vtable_global(trait_name, concrete)?;
    let value_local = ctx.binding_of(value)?;
    ctx.emit(Instruction::LocalGet(value_local));
    ctx.emit(Instruction::GlobalGet(vt_global));
    ctx.emit(Instruction::StructNew(dyn_idx));
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(dyn_idx),
    });
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `Op::DynCall(trait, slot, receiver, args)` — push `recv.$data`
/// (self), the args, and `recv.$vt.$m{slot}` (the typed funcref), then
/// `call_ref $dynfn_T_slot`. Statically signature-checked; no table.
pub(super) fn translate_dyn_call(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    trait_name: &str,
    slot: u32,
    receiver: ValueId,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let (dyn_idx, vtable_idx, fn_type_idxs) = b.require_dyn_trait(trait_name)?;
    let fn_type_idx = *fn_type_idxs.get(slot as usize).ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-gc: `Op::DynCall(dyn {trait_name})` slot {slot} is out of \
             range — the trait has {} methods (IR verifier should have caught \
             this)",
            fn_type_idxs.len()
        ))
    })?;
    let recv_local = ctx.binding_of(receiver)?;
    // self = recv.$data
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: dyn_idx,
        field_index: DYN_DATA,
    });
    // user args
    for arg in args {
        let local = ctx.binding_of(*arg)?;
        ctx.emit(Instruction::LocalGet(local));
    }
    // funcref = recv.$vt.$m{slot}
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: dyn_idx,
        field_index: DYN_VT,
    });
    ctx.emit(Instruction::StructGet {
        struct_type_index: vtable_idx,
        field_index: slot,
    });
    ctx.emit(Instruction::CallRef(fn_type_idx));
    // Result handling mirrors `Op::Call` — shared helper keeps the two
    // call paths' return-binding logic in lockstep.
    bind_call_result(ctx, b, instr, "Op::DynCall")
}

/// `IrType::DynRef(trait)` → `(ref null $dyn_T)` (single slot).
pub(super) fn dyn_valtype(b: &ModuleBuilder, trait_name: &str) -> Result<ValType, CompileError> {
    let (dyn_idx, _v, _f) = b.require_dyn_trait(trait_name)?;
    Ok(ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(dyn_idx),
    }))
}
