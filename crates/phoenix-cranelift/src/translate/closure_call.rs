//! Shared helper for calling closures from generated code.
//!
//! Used by list methods (map, filter, etc.), option methods (map, andThen, etc.),
//! and result methods (map, mapErr, etc.) when they need to call user-provided
//! closure arguments.
//!
//! All calls go through the env-pointer ABI: the closure value (a
//! pointer to a `[fn_ptr, capture_0, ...]` heap object) is passed as
//! the first argument, and the closure function reads its captures
//! from that env pointer via [`phoenix_ir::instruction::Op::ClosureLoadCapture`].
//! Capture types never cross the indirect-call boundary, which is
//! what structurally eliminates the closure-capture-ambiguity bug
//! for closures that flow through phi nodes.

use cranelift_codegen::ir::{InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::types::IrType;

use super::{FuncState, get_val1};

/// Call a closure given its ValueId and user arguments.
///
/// Loads the function pointer from slot 0 of the closure heap object,
/// then issues a `call_indirect` with `(closure_ptr, user_args...)`.
pub(super) fn call_closure(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    closure_vid: ValueId,
    user_args: &[Value],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let closure_ptr = get_val1(state, closure_vid)?;
    call_closure_ptr(builder, ctx, closure_ptr, closure_vid, user_args, state)
}

/// Call a closure given its pointer and the ValueId (for type lookup).
pub(super) fn call_closure_ptr(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
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

    // Env-pointer ABI: prepend the closure pointer as the first arg.
    // The callee will read its captures via Op::ClosureLoadCapture
    // indexed off this env. No capture types cross this boundary —
    // closures with the same user signature unify regardless of their
    // capture layouts.
    let mut cl_args = Vec::with_capacity(user_args.len() + 1);
    cl_args.push(closure_ptr);
    cl_args.extend(user_args);

    // Build the closure function's signature: env-ptr first, then user
    // params. The env-ptr's IR type is the closure's own ClosureRef.
    let mut full_param_types: Vec<IrType> = vec![closure_type.clone()];
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
