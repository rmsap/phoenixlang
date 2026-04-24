//! Per-function translation from Phoenix IR to Cranelift IR.
//!
//! This module orchestrates the translation of each Phoenix IR function
//! into Cranelift IR, dispatching individual operations to domain-specific
//! submodules for readability and separation of concerns.
//!
//! Most translation functions take `(&mut FunctionBuilder, &mut CompileContext,
//! &IrModule, &FuncState)` as their first parameters.  A context struct was
//! considered but the two `&mut` borrows (`builder` + `ctx`) make bundling
//! awkward without adding `RefCell`-style indirection.
mod arith;
mod calls;
mod closure_call;
mod control;
mod data;
mod dyn_trait;
mod enum_combinators;
mod enum_helpers;
mod enum_type_inference;
mod helpers;
mod ir_analysis;
// `layout` is `pub(crate)` so `abi.rs` can name `TypeLayout` when building
// function signatures. No other crate-level consumers — within `translate`,
// submodules reach it via `super::layout`.
pub(crate) mod layout;
mod list_methods;
mod list_methods_closure;
mod list_methods_complex;
mod map_methods;
mod mutable;
mod option_methods;
mod result_methods;

use std::collections::HashMap;

use cranelift_codegen::Context;
use cranelift_codegen::ir::{self, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::translate::layout::TypeLayout;
use phoenix_ir::block::BlockId as PhxBlockId;
use phoenix_ir::instruction::{FuncId as PhxFuncId, Op, VOID_SENTINEL, ValueId};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::types::IrType;

/// Per-function translation state shared across all op translators.
///
/// Bundles the mappings from Phoenix value/block IDs to their Cranelift
/// equivalents, plus type and closure metadata needed during translation.
pub(crate) struct FuncState {
    /// Mapping from Phoenix `ValueId` to Cranelift `Value`(s).
    /// Most types map to one value; strings map to two (ptr, len).
    pub value_map: HashMap<ValueId, Vec<Value>>,
    /// Mapping from `Alloca` result `ValueId` to its Cranelift stack slot and type.
    pub alloca_map: HashMap<ValueId, (ir::StackSlot, IrType)>,
    /// The `IrType` of each `ValueId`, for type-dispatched operations (e.g. print).
    pub type_map: HashMap<ValueId, IrType>,
    /// Tracks which `ClosureAlloc` produced each `ValueId`, so `CallIndirect`
    /// can look up the target function directly instead of using a heuristic.
    pub closure_func_map: HashMap<ValueId, PhxFuncId>,
    /// Records the allocated variant and concrete payload field types from
    /// `EnumAlloc` instructions. Used by `option_payload_type` /
    /// `result_payload_types` as a Strategy 4 fallback when Strategy 0 can't
    /// read the payload type directly from `EnumRef` args.
    ///
    /// The variant index is tracked so `Result<T, E>` can distinguish an
    /// `Ok(t)` allocation (payload type = T) from an `Err(e)` allocation
    /// (payload type = E) — both record `field_types[0]`, but the meaning
    /// differs per variant. Option-like enums only allocate the payload-
    /// bearing variant so this is trivially 0 for them.
    pub enum_payload_types: HashMap<ValueId, (u32, Vec<IrType>)>,
    /// Mapping from Phoenix `BlockId` to Cranelift block.
    pub block_map: HashMap<PhxBlockId, ir::Block>,
}

/// Translate all functions in the IR module and define them in the Cranelift module.
pub fn translate_module(
    ctx: &mut CompileContext,
    ir_module: &IrModule,
) -> Result<(), CompileError> {
    let mut cl_ctx = Context::new();
    let mut fb_ctx = FunctionBuilderContext::new();

    // Generic templates are inert post-monomorphization; their bodies
    // contain `IrType::TypeVar` which has no Cranelift lowering. Iterating
    // via `concrete_functions()` filters them out.
    for func in ir_module.concrete_functions() {
        translate_function(ctx, ir_module, func, &mut cl_ctx, &mut fb_ctx)?;
    }

    Ok(())
}

/// Translate a single Phoenix IR function into its Cranelift definition.
fn translate_function(
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    func: &IrFunction,
    cl_ctx: &mut Context,
    fb_ctx: &mut FunctionBuilderContext,
) -> Result<(), CompileError> {
    let cl_func_id = ctx.func_ids[&func.id];

    let sig = crate::abi::build_signature(&func.param_types, &func.return_type, ctx.call_conv);
    cl_ctx.func.signature = sig;

    let mut builder = FunctionBuilder::new(&mut cl_ctx.func, fb_ctx);

    let mut state = FuncState {
        value_map: HashMap::new(),
        alloca_map: HashMap::new(),
        type_map: HashMap::new(),
        closure_func_map: HashMap::new(),
        enum_payload_types: HashMap::new(),
        block_map: HashMap::new(),
    };

    // Create Cranelift blocks for each Phoenix basic block.
    for block in &func.blocks {
        let cl_block = builder.create_block();
        state.block_map.insert(block.id, cl_block);
    }

    // Set up block parameters for all blocks.
    // For the entry block, these are the function parameters.
    for block in &func.blocks {
        let cl_block = state.block_map[&block.id];
        for (vid, ir_ty) in &block.params {
            state.type_map.insert(*vid, ir_ty.clone());
            let mut vals = Vec::new();
            for &cl_ty in TypeLayout::of(ir_ty).cl_types() {
                let val = builder.append_block_param(cl_block, cl_ty);
                vals.push(val);
            }
            state.value_map.insert(*vid, vals);
        }
    }

    // Translate each block.
    for block in &func.blocks {
        let cl_block = state.block_map[&block.id];
        builder.switch_to_block(cl_block);

        // Translate instructions.
        for inst in &block.instructions {
            let result_vid = inst.result;
            let result_type = &inst.result_type;

            let result_vals = translate_op(
                &mut builder,
                ctx,
                ir_module,
                &mut state,
                &inst.op,
                result_type,
            )?;

            if let Some(vid) = result_vid {
                state.type_map.insert(vid, result_type.clone());
                state.value_map.insert(vid, result_vals);
                // Move alloca slot info from the VOID_SENTINEL temp key to the
                // actual result ValueId.  VOID_SENTINEL is used as a temporary
                // key because the Alloca op doesn't know its own result ValueId.
                if matches!(inst.op, Op::Alloca(_))
                    && let Some(slot_info) = state.alloca_map.remove(&VOID_SENTINEL)
                {
                    state.alloca_map.insert(vid, slot_info);
                }
                // Record closure → func mapping for CallIndirect.
                if let Op::ClosureAlloc(target_fid, _) = &inst.op {
                    state.closure_func_map.insert(vid, *target_fid);
                }
                // Record concrete payload types from EnumAlloc for later inference.
                if let Op::EnumAlloc(_name, variant_idx, fields) = &inst.op
                    && !fields.is_empty()
                {
                    // Use the actual type of each field, falling back to I64
                    // only if the type is not yet known (forward references
                    // within a function — rare in practice because IR is
                    // emitted in depth-first order). This is a Strategy 4
                    // backstop: Strategy 0 (reading `EnumRef` args) already
                    // ran and preferred its result via the agreement
                    // `debug_assert` in `enum_type_inference.rs`, so the
                    // I64 here only surfaces if *every* earlier strategy
                    // also failed. If that path ever widens (e.g. new op
                    // shapes defer type resolution), this fallback is the
                    // same shape as the `okOr` payload bug — a silent
                    // corruption of multi-slot payloads — and must be
                    // replaced with a real lookup or an explicit error.
                    // Using `map` instead of `filter_map` preserves the
                    // field count so downstream consumers get the correct
                    // payload arity.
                    let field_types: Vec<IrType> = fields
                        .iter()
                        .map(|fid| state.type_map.get(fid).cloned().unwrap_or(IrType::I64))
                        .collect();
                    state
                        .enum_payload_types
                        .insert(vid, (*variant_idx, field_types));
                }
            }
        }

        // Propagate enum_payload_types through block-parameter forwarding.
        // When a Jump/Branch passes a value that has known payload types to
        // a target block, the block parameter's ValueId should inherit the
        // payload type info so downstream code can infer enum inner types
        // even when the value flows through phi nodes.
        propagate_enum_payload_types(&block.terminator, func, &mut state);

        // Translate terminator.
        control::translate_terminator(&mut builder, &block.terminator, &state, func)?;
    }

    // Seal all blocks (all predecessors are known after full translation).
    for block in state.block_map.values() {
        builder.seal_block(*block);
    }

    builder.finalize();

    // Define the function in the module.
    ctx.module
        .define_function(cl_func_id, cl_ctx)
        .map_err(|e| CompileError::new(format!("failed to define function {}: {e}", func.name)))?;

    cl_ctx.clear();

    Ok(())
}

/// Propagate `enum_payload_types` from jump/branch arguments to the target
/// block's parameter ValueIds.  This ensures that when an `EnumAlloc` value
/// flows through a phi node (e.g., `if/else` producing `Some(x)` vs `None`),
/// the block parameter inherits the payload type info so downstream methods
/// like `option_payload_type` can find it.
fn propagate_enum_payload_types(
    term: &phoenix_ir::terminator::Terminator,
    func: &IrFunction,
    state: &mut FuncState,
) {
    let targets: Vec<(&PhxBlockId, &[ValueId])> = match term {
        phoenix_ir::terminator::Terminator::Jump { target, args } => {
            vec![(target, args)]
        }
        phoenix_ir::terminator::Terminator::Branch {
            true_block,
            true_args,
            false_block,
            false_args,
            ..
        } => {
            vec![(true_block, true_args), (false_block, false_args)]
        }
        _ => return,
    };

    for (target_block, args) in targets {
        // Find the corresponding block in the IR to get its parameter ValueIds.
        let Some(block) = func.blocks.iter().find(|b| b.id == *target_block) else {
            continue;
        };
        for (arg_vid, (param_vid, _param_ty)) in args.iter().zip(block.params.iter()) {
            if let Some(payload) = state.enum_payload_types.get(arg_vid).cloned() {
                state
                    .enum_payload_types
                    .entry(*param_vid)
                    .or_insert(payload);
            }
        }
    }
}

// ── Value helpers ──────────────────────────────────────────────────

/// Get the Cranelift value(s) for a Phoenix `ValueId`.
pub(crate) fn get_val(state: &FuncState, vid: ValueId) -> Result<Vec<Value>, CompileError> {
    state
        .value_map
        .get(&vid)
        .cloned()
        .ok_or_else(|| CompileError::new(format!("undefined value {vid}")))
}

/// Get a single Cranelift value for a Phoenix `ValueId`.
///
/// Returns an error if the value maps to multiple Cranelift values (e.g. strings).
pub(crate) fn get_val1(state: &FuncState, vid: ValueId) -> Result<Value, CompileError> {
    let vals = get_val(state, vid)?;
    if vals.len() != 1 {
        return Err(CompileError::new(format!(
            "expected single value for {vid}, got {}",
            vals.len()
        )));
    }
    Ok(vals[0])
}

// ── Top-level op dispatch ──────────────────────────────────────────

/// Translate a single Phoenix IR operation to Cranelift instructions.
///
/// Dispatches to domain-specific helpers in submodules for readability.
fn translate_op(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    state: &mut FuncState,
    op: &Op,
    result_type: &IrType,
) -> Result<Vec<Value>, CompileError> {
    match op {
        // Constants
        Op::ConstI64(_) | Op::ConstF64(_) | Op::ConstBool(_) | Op::ConstString(_) => {
            arith::translate_const(builder, ctx, op)
        }

        // Integer arithmetic
        Op::IAdd(..) | Op::ISub(..) | Op::IMul(..) | Op::IDiv(..) | Op::IMod(..) | Op::INeg(..) => {
            arith::translate_int_arith(builder, ctx, op, state)
        }

        // Float arithmetic
        Op::FAdd(..) | Op::FSub(..) | Op::FMul(..) | Op::FDiv(..) | Op::FMod(..) | Op::FNeg(..) => {
            arith::translate_float_arith(builder, op, state)
        }

        // Comparisons
        Op::IEq(..)
        | Op::INe(..)
        | Op::ILt(..)
        | Op::IGt(..)
        | Op::ILe(..)
        | Op::IGe(..)
        | Op::FEq(..)
        | Op::FNe(..)
        | Op::FLt(..)
        | Op::FGt(..)
        | Op::FLe(..)
        | Op::FGe(..)
        | Op::BoolEq(..)
        | Op::BoolNe(..)
        | Op::BoolNot(..) => arith::translate_cmp(builder, op, state),

        // String operations
        Op::StringConcat(..)
        | Op::StringEq(..)
        | Op::StringNe(..)
        | Op::StringLt(..)
        | Op::StringGt(..)
        | Op::StringLe(..)
        | Op::StringGe(..) => data::translate_string(builder, ctx, op, state),

        // Struct operations
        Op::StructAlloc(..) | Op::StructGetField(..) | Op::StructSetField(..) => {
            data::translate_struct(builder, ctx, ir_module, op, result_type, state)
        }

        // Enum operations
        Op::EnumAlloc(..) | Op::EnumDiscriminant(..) | Op::EnumGetField(..) => {
            data::translate_enum(builder, ctx, ir_module, op, result_type, state)
        }

        // Collection operations
        Op::ListAlloc(_) => {
            list_methods::translate_list_alloc(builder, ctx, op, result_type, state)
        }
        Op::MapAlloc(_) => map_methods::translate_map_alloc(builder, ctx, op, result_type, state),

        // Closure operations
        Op::ClosureAlloc(..) => calls::translate_closure_alloc(builder, ctx, op, state),

        // Function calls
        Op::Call(..)
        | Op::CallIndirect(..)
        | Op::BuiltinCall(..)
        | Op::UnresolvedTraitMethod(..) => {
            calls::translate_call(builder, ctx, ir_module, op, result_type, state)
        }

        Op::DynAlloc(..) | Op::UnresolvedDynAlloc(..) | Op::DynCall(..) => {
            dyn_trait::translate_dyn_op(builder, ctx, ir_module, op, state)
        }

        // Mutable variables
        Op::Alloca(..) | Op::Load(..) | Op::Store(..) => {
            mutable::translate_mutable(builder, op, state)
        }

        // Miscellaneous
        Op::Copy(v) => get_val(state, *v),
    }
}
