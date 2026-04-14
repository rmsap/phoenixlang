//! Per-function translation from Phoenix IR to Cranelift IR.
//!
//! This module orchestrates the translation of each Phoenix IR function
//! into Cranelift IR, dispatching individual operations to domain-specific
//! submodules for readability and separation of concerns.

mod arith;
mod calls;
mod control;
mod data;
mod helpers;
mod mutable;

use std::collections::HashMap;

use cranelift_codegen::Context;
use cranelift_codegen::ir::{self, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::ir_type_to_cl;
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

    for func in &ir_module.functions {
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
            let cl_types = ir_type_to_cl(ir_ty);
            let mut vals = Vec::new();
            for cl_ty in &cl_types {
                let val = builder.append_block_param(cl_block, *cl_ty);
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
            }
        }

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

        // Collection operations (stub)
        Op::ListAlloc(_) | Op::MapAlloc(_) => Err(CompileError::new(
            "List/Map types are not yet supported in compiled mode. Use `phoenix run-ir`.",
        )),

        // Closure operations
        Op::ClosureAlloc(..) => calls::translate_closure_alloc(builder, ctx, op, state),

        // Function calls
        Op::Call(..) | Op::CallIndirect(..) | Op::BuiltinCall(..) => {
            calls::translate_call(builder, ctx, ir_module, op, state)
        }

        // Mutable variables
        Op::Alloca(..) | Op::Load(..) | Op::Store(..) => {
            mutable::translate_mutable(builder, op, state)
        }

        // Miscellaneous
        Op::Copy(v) => get_val(state, *v),
    }
}
