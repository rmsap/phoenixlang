//! wasm32-gc closure type declaration and op lowering (§Phase 2.4
//! decision K.8).
//!
//! Per K.8, closures dispatch through typed function references
//! (`call_ref`) over a per-signature subtype hierarchy that mirrors
//! the IR's env-pointer calling convention exactly:
//!
//! ```text
//! $fn_SIG   = (func (param (ref null struct))   ;; env — abstract
//!                   (param P_wasm ...)
//!                   (result R_wasm))
//! $clo_SIG  = (sub (struct (field $code (ref $fn_SIG))))
//! $site_F   = (sub final $clo_SIG
//!               (struct $code, capture fields ...))
//! ```
//!
//! One `$fn_SIG` + `$clo_SIG` pair per distinct closure *signature*
//! (user params + return — collected like K.4/K.7 instantiations);
//! one final `$site_F` per `ClosureAlloc` *target function*, carrying
//! that closure's capture fields (immutable — there is no
//! capture-store op). Call sites hold `(ref null $clo_SIG)` and never
//! see capture layouts, which structurally preserves the
//! closure-capture-ambiguity fix: closures with one signature but
//! different captures unify through phis as the shared parent type.
//!
//! The env parameter is typed as the **abstract** `(ref null struct)`
//! rather than `(ref null $clo_SIG)` — the precise typing would make
//! `$fn_SIG` and `$clo_SIG` mutually recursive (needing `(rec …)`
//! groups in the type interner); the callee must `ref.cast` its env
//! down to the concrete `$site_F` either way, so the abstract typing
//! costs nothing at runtime. See K.8 for the revisit-trigger.
//!
//! `call_ref` requires wasmtime's `function-references` feature
//! alongside `gc` (`-W function-references=y,gc=y`) — verified on the
//! pinned wasmtime; both test harnesses pass the combined flag.
//!
//! `ref.func $F` additionally requires `$F` to be declared in an
//! `(elem declare func …)` segment; [`ModuleBuilder::emit_closure_elem_decls`]
//! emits one covering every allocation target after the function
//! indices are assigned.

use std::collections::HashSet;

use phoenix_ir::instruction::{FuncId, Op, ValueId};
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;
use wasm_encoder::{AbstractHeapType, HeapType, Instruction, RefType, ValType};

use crate::error::CompileError;

use super::enums::contains_generic_placeholder;
use super::module_builder::ModuleBuilder;
use super::translate::{FuncCtx, expect_result, wasm_valtypes_for};

/// `(param_types, return_type)` — one concrete closure signature.
/// `Option<Int> -> Int` and `String -> Int` closures are distinct
/// keys; closures with the same key share `$fn_SIG` / `$clo_SIG`.
pub(super) type ClosureSigKey = (Vec<IrType>, IrType);

/// The abstract env-parameter type: `(ref null struct)`.
pub(super) fn env_valtype() -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Abstract {
            shared: false,
            ty: AbstractHeapType::Struct,
        },
    })
}

/// Declare WASM-GC types for every closure signature and allocation
/// site in the IR module per §Phase 2.4 decision K.8.
///
/// Must run *after* structs / strings / enums / lists (capture fields
/// and user param/result types of those kinds encode their indices)
/// and *before* any function signature touching `IrType::ClosureRef`
/// is interned. (`List<Closure>` elements are therefore not yet
/// representable — the list pass runs first and its element mapping
/// would miss the closure parent; that combination errors with the
/// `require_closure_sig` diagnostic until a fixture needs it.)
pub(super) fn declare(
    builder: &mut ModuleBuilder,
    ir_module: &IrModule,
) -> Result<(), CompileError> {
    // Pass 0: collect signatures (every `ClosureRef` in any walked
    // type position) and allocation targets (every `Op::ClosureAlloc`,
    // whose result type carries the target's signature). Signatures
    // still containing generic placeholders belong to the dead
    // template-copy closures monomorphization leaves behind (see the
    // K.4 known-limitation note) — skipped here, and the functions
    // themselves are skipped by `declare_phoenix_functions` /
    // `emit_phoenix_bodies` via [`ModuleBuilder::is_dead_placeholder_closure`].
    let mut sigs: HashSet<ClosureSigKey> = HashSet::new();
    let mut sites: Vec<(FuncId, ClosureSigKey)> = Vec::new();
    let mut seen_sites: HashSet<FuncId> = HashSet::new();
    for func in ir_module.concrete_functions() {
        walk_type(&func.return_type, &mut sigs);
        for ty in &func.param_types {
            walk_type(ty, &mut sigs);
        }
        for block in &func.blocks {
            for (_, ty) in &block.params {
                walk_type(ty, &mut sigs);
            }
            for instr in &block.instructions {
                walk_type(&instr.result_type, &mut sigs);
                if let Op::ClosureAlloc(target, _) = &instr.op {
                    match &instr.result_type {
                        IrType::ClosureRef {
                            param_types,
                            return_type,
                        } => {
                            let key = (param_types.clone(), (**return_type).clone());
                            // Placeholder check BEFORE the dedup
                            // insert: a target first seen with a
                            // placeholder key must stay eligible for a
                            // later concrete-keyed alloc of the same
                            // target — first-wins dedup would silently
                            // drop the live site.
                            if !key_has_placeholder(&key) && seen_sites.insert(*target) {
                                sites.push((*target, key));
                            }
                        }
                        other => {
                            return Err(CompileError::new(format!(
                                "wasm32-gc: `Op::ClosureAlloc({target:?})` result \
                                     type is `{other:?}`, expected `ClosureRef` \
                                     (internal compiler bug)"
                            )));
                        }
                    }
                }
            }
        }
    }
    // Struct / enum layouts: forward-compat only today —
    // `wasm_field_type_for` rejects closure-typed fields (Int / Float /
    // Bool / String are the supported field types) before this pass
    // ever runs, so nothing collected here is reachable yet. The walk
    // keeps collection exhaustive for the slice that lifts the field
    // restriction.
    for fields in ir_module.struct_layouts.values() {
        for (_, ty) in fields {
            walk_type(ty, &mut sigs);
        }
    }
    for variants in ir_module.enum_layouts.values() {
        for (_, fields) in variants {
            for ty in fields {
                walk_type(ty, &mut sigs);
            }
        }
    }

    // Sort by (closure-nesting depth, debug string): depth puts inner
    // signatures before the outer signatures whose param/result slots
    // need their parent indices — a higher-order signature like
    // `((Int) -> Int) -> Int` references its inner `(Int) -> Int`
    // parent, and the debug-string order alone would declare the outer
    // first (`"([ClosureRef …"` sorts before `"([I64 …"`). The debug
    // string keeps the order deterministic across runs (HashSet
    // iteration is arbitrary; IrType has no Ord) — the K.7 list-depth
    // pattern.
    let mut ordered: Vec<ClosureSigKey> = sigs.into_iter().collect();
    ordered.sort_by_cached_key(|k| (sig_key_depth(k), format!("{k:?}")));
    sites.sort_by_cached_key(|(fid, _)| fid.0);

    // Pass 1: one `$fn_SIG` func type + open `$clo_SIG` parent per
    // signature.
    for key in &ordered {
        let (param_types, return_type) = key;
        let mut params: Vec<ValType> = vec![env_valtype()];
        for ty in param_types {
            params.push(single_slot_for(ty, builder, "closure parameter")?);
        }
        let results: Vec<ValType> = match return_type {
            IrType::Void => Vec::new(),
            ty => vec![single_slot_for(ty, builder, "closure return")?],
        };
        let fn_type_idx = builder.intern_signature(&params, &results);
        // $code: non-nullable — `ref.func` always supplies a value and
        // `struct.new` requires every field, so there is no
        // default-init path. Immutable: a closure's target never
        // changes after allocation.
        let code_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(ValType::Ref(RefType {
                nullable: false,
                heap_type: HeapType::Concrete(fn_type_idx),
            })),
            mutable: false,
        };
        let parent_idx = builder.declare_closure_parent_struct(&[code_field]);
        builder.record_closure_sig(key.clone(), fn_type_idx, parent_idx);
    }

    // Pass 2: one final `$site_F` subtype per allocation target,
    // appending the target's capture fields after `$code`.
    for (target, key) in &sites {
        let (fn_type_idx, parent_idx) = builder.require_closure_sig(key)?;
        let code_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(ValType::Ref(RefType {
                nullable: false,
                heap_type: HeapType::Concrete(fn_type_idx),
            })),
            mutable: false,
        };
        let target_fn = ir_module.get_concrete(*target).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: `Op::ClosureAlloc` targets `{target:?}`, which is \
                 not a concrete function (internal compiler bug — \
                 monomorphization should have specialized every allocated \
                 closure)"
            ))
        })?;
        let mut fields = Vec::with_capacity(target_fn.capture_types.len() + 1);
        fields.push(code_field);
        for cap_ty in &target_fn.capture_types {
            // Captures are immutable by construction (no
            // ClosureStoreCapture op exists — by-value semantics).
            fields.push(wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(single_slot_for(
                    cap_ty,
                    builder,
                    "closure capture",
                )?),
                mutable: false,
            });
        }
        let site_idx = builder.declare_closure_site_struct(&fields, parent_idx);
        builder.record_closure_site(*target, site_idx);
    }
    Ok(())
}

/// Whether a signature key still carries an unresolved generic
/// placeholder (a dead template-copy closure's signature).
fn key_has_placeholder(key: &ClosureSigKey) -> bool {
    key.0.iter().any(contains_generic_placeholder) || contains_generic_placeholder(&key.1)
}

/// Closure-nesting depth of a signature key: the max [`closure_depth`]
/// over its param and return types. Drives the inner-before-outer
/// declaration order in [`declare`]'s pass 1 — a nested `ClosureRef`
/// inside a key contributes depth `1 +` its own signature's depth, so
/// every signature a key references has strictly smaller depth and
/// ascending-depth declaration always resolves.
fn sig_key_depth(key: &ClosureSigKey) -> usize {
    key.0
        .iter()
        .map(closure_depth)
        .max()
        .unwrap_or(0)
        .max(closure_depth(&key.1))
}

/// How many `ClosureRef` layers `ty` transitively contains. The K.7
/// `list_depth` pattern; like it, the `StructRef` / `EnumRef` arms look
/// through *type args only*, not declared field layouts — sound while
/// closure-typed struct/enum fields stay rejected by the slice-3 field
/// restriction (see the layout-walk note in [`declare`]).
fn closure_depth(ty: &IrType) -> usize {
    match ty {
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            1 + param_types
                .iter()
                .map(closure_depth)
                .max()
                .unwrap_or(0)
                .max(closure_depth(return_type))
        }
        IrType::StructRef(_, args) | IrType::EnumRef(_, args) => {
            args.iter().map(closure_depth).max().unwrap_or(0)
        }
        IrType::ListRef(inner) | IrType::ListBuilderRef(inner) => closure_depth(inner),
        IrType::MapRef(k, v) | IrType::MapBuilderRef(k, v) => {
            closure_depth(k).max(closure_depth(v))
        }
        _ => 0,
    }
}

/// Resolve one closure-related type to its single WASM slot via the
/// shared mapping, with a closure-specific diagnostic label.
fn single_slot_for(ty: &IrType, b: &ModuleBuilder, what: &str) -> Result<ValType, CompileError> {
    let slots = wasm_valtypes_for(ty, b)?;
    if slots.len() == 1 {
        Ok(slots[0])
    } else {
        Err(CompileError::new(format!(
            "wasm32-gc: {what} type `{ty:?}` flattens to {} WASM slots — \
             only single-slot types are supported (internal invariant: \
             every K-mapped type is single-slot)",
            slots.len()
        )))
    }
}

/// Recursively collect every concrete `ClosureRef` signature in `ty`.
fn walk_type(ty: &IrType, sigs: &mut HashSet<ClosureSigKey>) {
    match ty {
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            let key = (param_types.clone(), (**return_type).clone());
            if !key_has_placeholder(&key) {
                sigs.insert(key);
            }
            for p in param_types {
                walk_type(p, sigs);
            }
            walk_type(return_type, sigs);
        }
        IrType::StructRef(_, args) | IrType::EnumRef(_, args) => {
            for arg in args {
                walk_type(arg, sigs);
            }
        }
        IrType::ListRef(inner) | IrType::ListBuilderRef(inner) => walk_type(inner, sigs),
        IrType::MapRef(k, v) | IrType::MapBuilderRef(k, v) => {
            walk_type(k, sigs);
            walk_type(v, sigs);
        }
        _ => {}
    }
}

// ───────────────────────── K.8 closure lowerings ─────────────────────────

/// `$clo_SIG` field index of `$code`.
const CLO_CODE: u32 = 0;

/// `Op::ClosureAlloc(target, captures)` — `ref.func $target` plus the
/// capture values, wrapped in `struct.new $site_target`. The result
/// upcasts implicitly to the signature parent `(ref null $clo_SIG)`,
/// which is what the binding (and every later use) carries.
pub(super) fn translate_closure_alloc(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    target: FuncId,
    captures: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::ClosureAlloc")?;
    let key = sig_key_of(&instr.result_type, "Op::ClosureAlloc result")?;
    let (_, parent_idx) = b.require_closure_sig(&key)?;
    let site_idx = b.require_closure_site(target)?;
    let target_idx = b.require_phx_user_func(target)?;
    ctx.emit(Instruction::RefFunc(target_idx));
    for cap in captures {
        let local = ctx.binding_of(*cap)?;
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit(Instruction::StructNew(site_idx));
    let wasm_ty = ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(parent_idx),
    });
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `Op::CallIndirect(closure, args)` — the env-pointer convention over
/// `call_ref`: the closure value itself is the first argument, the
/// `$code` field supplies the typed function reference. Statically
/// signature-checked; no table, no runtime signature test. Result
/// handling mirrors `Op::Call`.
pub(super) fn translate_call_indirect(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    closure: ValueId,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let arg_locals = args
        .iter()
        .map(|a| ctx.binding_of(*a))
        .collect::<Result<Vec<_>, _>>()?;
    emit_closure_call(ctx, b, closure, &arg_locals)?;
    match (instr.result, &instr.result_type) {
        (Some(_), IrType::Void) => Err(CompileError::new(
            "wasm32-gc: `Op::CallIndirect` has a result binding but a Void \
             return type (internal compiler bug)"
                .to_string(),
        )),
        (Some(vid), ty) => {
            let wasm_ty = single_slot_for(ty, b, "indirect-call result")?;
            let local = ctx.allocate_local(vid, wasm_ty);
            ctx.emit(Instruction::LocalSet(local));
            Ok(())
        }
        (None, IrType::Void) => Ok(()),
        (None, ty) => Err(CompileError::new(format!(
            "wasm32-gc: `Op::CallIndirect` returns `{ty:?}` but has no result \
             binding (internal compiler bug)"
        ))),
    }
}

/// Emit a `call_ref` to the closure bound to `closure`, taking the
/// arguments already staged in `arg_locals` (in user-parameter
/// order), and leaving the call's result on the WASM stack. Factored
/// out of `translate_call_indirect` so the Option/Result closure
/// builtins (`map` / `andThen`) can invoke a user closure on an
/// extracted payload without a synthetic `Op::CallIndirect`.
///
/// Stack effect: pushes nothing it doesn't consume except the single
/// result value — so a caller may push an enum discriminant *before*
/// calling this and `struct.new` *after*, with the discriminant
/// surviving underneath (the env-pointer ABI's `call_ref` consumes
/// only env + args + funcref from the top).
pub(super) fn emit_closure_call(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    closure: ValueId,
    arg_locals: &[u32],
) -> Result<(), CompileError> {
    let clo_local = ctx.binding_of(closure)?;
    let parent_idx = match ctx.binding_type_of(closure)? {
        ValType::Ref(RefType {
            heap_type: HeapType::Concrete(idx),
            ..
        }) => idx,
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: closure call receiver lowered to `{other:?}`, \
                 expected a closure parent ref (internal compiler bug)"
            )));
        }
    };
    let fn_type_idx = b.closure_fn_by_parent(parent_idx).ok_or_else(|| {
        CompileError::new(
            "wasm32-gc: closure call receiver's struct type is not a recorded \
             closure signature parent (internal compiler bug)"
                .to_string(),
        )
    })?;
    // env, user args, then the function reference (the env-pointer ABI).
    ctx.emit(Instruction::LocalGet(clo_local));
    for &local in arg_locals {
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit(Instruction::LocalGet(clo_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: parent_idx,
        field_index: CLO_CODE,
    });
    ctx.emit(Instruction::CallRef(fn_type_idx));
    Ok(())
}

/// `Op::ClosureLoadCapture(env, idx)` — `ref.cast` the abstract env
/// down to the current function's own `$site_F` (the op only occurs
/// inside `F`'s body, so the target is statically known via
/// [`FuncCtx::closure_site`]), then `struct.get` field `idx + 1`
/// (field 0 is `$code`). The K.4 enum parent→variant pattern.
pub(super) fn translate_closure_load_capture(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    env: ValueId,
    capture_idx: u32,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::ClosureLoadCapture")?;
    let site_idx = ctx.closure_site().ok_or_else(|| {
        CompileError::new(
            "wasm32-gc: `Op::ClosureLoadCapture` outside a closure function \
             body (internal compiler bug — the op's contract is \
             closure-bodies-only)"
                .to_string(),
        )
    })?;
    let env_local = ctx.binding_of(env)?;
    ctx.emit(Instruction::LocalGet(env_local));
    ctx.emit(Instruction::RefCastNonNull(HeapType::Concrete(site_idx)));
    ctx.emit(Instruction::StructGet {
        struct_type_index: site_idx,
        field_index: capture_idx + 1,
    });
    let wasm_ty = single_slot_for(&instr.result_type, b, "capture")?;
    let local = ctx.allocate_local(vid, wasm_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Extract the signature key from a `ClosureRef` type.
pub(super) fn sig_key_of(ty: &IrType, what: &str) -> Result<ClosureSigKey, CompileError> {
    match ty {
        IrType::ClosureRef {
            param_types,
            return_type,
        } => Ok((param_types.clone(), (**return_type).clone())),
        other => Err(CompileError::new(format!(
            "wasm32-gc: {what} is `{other:?}`, expected `ClosureRef` \
             (internal compiler bug)"
        ))),
    }
}
