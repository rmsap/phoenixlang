//! Shared helper for calling closures from generated code.
//!
//! Used by list methods (map, filter, etc.), option methods (map, andThen, etc.),
//! and result methods (map, mapErr, etc.) when they need to call user-provided
//! closure arguments.

use cranelift_codegen::ir::{InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::helpers::{load_fat_value, slots_for_type};
use super::ir_analysis::{find_capture_types_by_func_id, find_closure_capture_types};
use super::{FuncState, get_val1};

/// Call a closure given its ValueId and user arguments.
///
/// Resolves the closure's function pointer, loads captured values,
/// builds the full argument list, and emits a `call_indirect`.
pub(super) fn call_closure(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    closure_vid: ValueId,
    user_args: &[Value],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let closure_ptr = get_val1(state, closure_vid)?;
    call_closure_ptr(
        builder,
        ctx,
        ir_module,
        closure_ptr,
        closure_vid,
        user_args,
        state,
    )
}

/// Call a closure given its pointer and the ValueId (for type lookup).
pub(super) fn call_closure_ptr(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    closure_ptr: Value,
    closure_vid: ValueId,
    user_args: &[Value],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let func_ptr = builder
        .ins()
        .load(POINTER_TYPE, MemFlags::new(), closure_ptr, 0);
    let closure_type = state
        .type_map
        .get(&closure_vid)
        .ok_or_else(|| CompileError::new("unknown type for closure in method call"))?;
    let (user_param_types, return_type) = match closure_type {
        IrType::ClosureRef {
            param_types,
            return_type,
        } => (param_types, return_type),
        _ => {
            return Err(CompileError::new("expected ClosureRef for method callback"));
        }
    };

    let capture_param_types = if let Some(target_fid) = state.closure_func_map.get(&closure_vid) {
        find_capture_types_by_func_id(ir_module, *target_fid, user_param_types.len())?
    } else {
        find_closure_capture_types(ir_module, user_param_types, return_type)?
    };

    // Load captures.
    let mut cl_args = Vec::new();
    let mut slot = 1usize;
    for cap_ty in &capture_param_types {
        let vals = load_fat_value(builder, cap_ty, closure_ptr, slot)?;
        cl_args.extend(vals);
        slot += slots_for_type(cap_ty);
    }

    // Append user args.
    cl_args.extend(user_args);

    // Build full signature.
    let mut full_param_types = capture_param_types;
    full_param_types.extend(user_param_types.iter().cloned());
    let sig = crate::abi::build_signature(&full_param_types, return_type, ctx.call_conv);
    let sig_ref = builder.import_signature(sig);

    let call = builder.ins().call_indirect(sig_ref, func_ptr, &cl_args);
    Ok(builder.inst_results(call).to_vec())
}

/// Get the return type of a closure from its IrType.
pub(super) fn closure_return_type(state: &FuncState, vid: ValueId) -> Result<IrType, CompileError> {
    let ty = state
        .type_map
        .get(&vid)
        .ok_or_else(|| CompileError::new("unknown type for closure"))?;
    match ty {
        IrType::ClosureRef { return_type, .. } => Ok(return_type.as_ref().clone()),
        _ => Err(CompileError::new("expected ClosureRef type")),
    }
}

/// Get the element type of the List returned by a closure (for flatMap).
pub(super) fn closure_return_elem_type(
    state: &FuncState,
    vid: ValueId,
) -> Result<IrType, CompileError> {
    let ret_ty = closure_return_type(state, vid)?;
    match ret_ty {
        IrType::ListRef(t) => Ok(t.as_ref().clone()),
        _ => Err(CompileError::new("flatMap closure must return List")),
    }
}
