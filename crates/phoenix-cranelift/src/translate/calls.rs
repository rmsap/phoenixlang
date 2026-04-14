//! Translation of closure allocation, function calls, and builtin calls.

use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::{FuncId as PhxFuncId, Op, ValueId};
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::helpers::{call_runtime, load_fat_value, slots_for_type, store_fat_value};
use super::{FuncState, get_val, get_val1};

/// Translate a `ClosureAlloc` operation.
///
/// Allocates a closure object on the heap: slot 0 is the function pointer,
/// slots 1..N hold captured values (respecting fat values for strings).
pub(super) fn translate_closure_alloc(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    op: &Op,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let Op::ClosureAlloc(target_fid, captures) = op else {
        unreachable!()
    };

    let capture_slots: usize = captures
        .iter()
        .map(|cap| {
            let ty = state.type_map.get(cap).cloned().ok_or_else(|| {
                CompileError::new(format!("unknown type for closure capture {cap}"))
            })?;
            Ok(slots_for_type(&ty))
        })
        .collect::<Result<Vec<usize>, CompileError>>()?
        .into_iter()
        .sum();
    let num_slots = 1 + capture_slots;
    let size = (num_slots * 8) as i64;
    let alloc_ref = ctx
        .module
        .declare_func_in_func(ctx.runtime.alloc, builder.func);
    let size_val = builder.ins().iconst(cl::I64, size);
    let call = builder.ins().call(alloc_ref, &[size_val]);
    let ptr = builder.inst_results(call)[0];

    // Store function pointer at slot 0.
    let cl_func_id = ctx.func_ids[target_fid];
    let func_ref = ctx.module.declare_func_in_func(cl_func_id, builder.func);
    let func_addr = builder.ins().func_addr(POINTER_TYPE, func_ref);
    builder.ins().store(MemFlags::new(), func_addr, ptr, 0);

    // Store captures starting at slot 1, respecting fat values.
    let mut slot = 1usize;
    for cap in captures.iter() {
        let cap_vals = get_val(state, *cap)?;
        let ty =
            state.type_map.get(cap).cloned().ok_or_else(|| {
                CompileError::new(format!("unknown type for closure capture {cap}"))
            })?;
        store_fat_value(builder, cap_vals, &ty, ptr, slot);
        slot += slots_for_type(&ty);
    }
    Ok(vec![ptr])
}

/// Translate a function call operation (direct, indirect, or builtin).
pub(super) fn translate_call(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    op: &Op,
    result_type: &IrType,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::Call(fid, args) => {
            let cl_func_id = ctx.func_ids[fid];
            let func_ref = ctx.module.declare_func_in_func(cl_func_id, builder.func);
            let mut cl_args = Vec::new();
            for arg in args {
                cl_args.extend(get_val(state, *arg)?);
            }
            let call = builder.ins().call(func_ref, &cl_args);
            Ok(builder.inst_results(call).to_vec())
        }
        Op::CallIndirect(closure, args) => {
            let closure_ptr = get_val1(state, *closure)?;
            let func_ptr = builder
                .ins()
                .load(POINTER_TYPE, MemFlags::new(), closure_ptr, 0);
            let closure_type = state
                .type_map
                .get(closure)
                .ok_or_else(|| CompileError::new("unknown type for indirect call target"))?;
            let (user_param_types, return_type) = match closure_type {
                IrType::ClosureRef {
                    param_types,
                    return_type,
                } => (param_types, return_type),
                _ => return Err(CompileError::new("CallIndirect on non-closure type")),
            };

            // Try to look up the exact closure function via the ClosureAlloc
            // that produced this value, falling back to a module-wide scan.
            let capture_param_types = if let Some(target_fid) = state.closure_func_map.get(closure)
            {
                find_capture_types_by_func_id(ir_module, *target_fid, user_param_types.len())
            } else {
                find_closure_capture_types(ir_module, user_param_types, return_type)?
            };

            // Load captures from the closure object (slots 1..N).
            let mut cl_args = Vec::new();
            let mut slot = 1usize;
            for cap_ty in &capture_param_types {
                let vals = load_fat_value(builder, cap_ty, closure_ptr, slot)?;
                cl_args.extend(vals);
                slot += slots_for_type(cap_ty);
            }

            // Append user args.
            for arg in args {
                cl_args.extend(get_val(state, *arg)?);
            }

            // Build full signature: capture params + user params.
            let mut full_param_types = capture_param_types;
            full_param_types.extend(user_param_types.iter().cloned());
            let sig = crate::abi::build_signature(&full_param_types, return_type, ctx.call_conv);
            let sig_ref = builder.import_signature(sig);

            let call = builder.ins().call_indirect(sig_ref, func_ptr, &cl_args);
            Ok(builder.inst_results(call).to_vec())
        }
        Op::BuiltinCall(name, args) => {
            translate_builtin(builder, ctx, name, args, result_type, state)
        }
        _ => unreachable!(),
    }
}

/// Translate a builtin call (print, toString, method calls).
fn translate_builtin(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    name: &str,
    args: &[ValueId],
    result_type: &IrType,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match name {
        "print" => {
            let arg = args[0];
            let arg_type = state
                .type_map
                .get(&arg)
                .ok_or_else(|| CompileError::new("unknown type for print argument"))?;
            match arg_type {
                IrType::I64 => {
                    call_runtime(
                        builder,
                        ctx,
                        ctx.runtime.print_i64,
                        &[get_val1(state, arg)?],
                    );
                }
                IrType::F64 => {
                    call_runtime(
                        builder,
                        ctx,
                        ctx.runtime.print_f64,
                        &[get_val1(state, arg)?],
                    );
                }
                IrType::Bool => {
                    call_runtime(
                        builder,
                        ctx,
                        ctx.runtime.print_bool,
                        &[get_val1(state, arg)?],
                    );
                }
                IrType::StringRef => {
                    let vals = get_val(state, arg)?;
                    call_runtime(builder, ctx, ctx.runtime.print_str, &[vals[0], vals[1]]);
                }
                _ => {
                    return Err(CompileError::new(format!(
                        "print not yet supported for type {arg_type} in compiled mode"
                    )));
                }
            }
            Ok(vec![])
        }
        "toString" => {
            let arg = args[0];
            let arg_type = state
                .type_map
                .get(&arg)
                .ok_or_else(|| CompileError::new("unknown type for toString argument"))?;
            match arg_type {
                IrType::I64 => Ok(call_runtime(
                    builder,
                    ctx,
                    ctx.runtime.i64_to_str,
                    &[get_val1(state, arg)?],
                )),
                IrType::F64 => Ok(call_runtime(
                    builder,
                    ctx,
                    ctx.runtime.f64_to_str,
                    &[get_val1(state, arg)?],
                )),
                IrType::Bool => Ok(call_runtime(
                    builder,
                    ctx,
                    ctx.runtime.bool_to_str,
                    &[get_val1(state, arg)?],
                )),
                IrType::StringRef => {
                    // toString on a string is identity.
                    get_val(state, arg)
                }
                _ => Err(CompileError::new(format!(
                    "toString not yet supported for type {arg_type} in compiled mode"
                ))),
            }
        }
        _ => {
            // Method calls (e.g. "String.length") — stub for now.
            let _ = result_type;
            Err(CompileError::new(format!(
                "builtin '{name}' not yet supported in compiled mode"
            )))
        }
    }
}

/// Find the capture parameter types for a closure function by its `FuncId`.
///
/// The closure function's parameters are `[captures..., user_params...]`.
/// Given the number of user parameters, the capture types are the prefix.
fn find_capture_types_by_func_id(
    ir_module: &IrModule,
    func_id: PhxFuncId,
    user_param_count: usize,
) -> Vec<IrType> {
    for func in &ir_module.functions {
        if func.id == func_id {
            if func.param_types.len() >= user_param_count {
                let capture_count = func.param_types.len() - user_param_count;
                return func.param_types[..capture_count].to_vec();
            }
            break;
        }
    }
    Vec::new()
}

/// Find the capture parameter types for a closure by scanning IR functions
/// for matching user parameter types and return type.
///
/// This is a fallback heuristic used when the closure value comes through
/// a block parameter (phi) and the exact `FuncId` is not known.  Returns
/// an error if multiple closures match with different capture layouts.
fn find_closure_capture_types(
    ir_module: &IrModule,
    user_param_types: &[IrType],
    return_type: &IrType,
) -> Result<Vec<IrType>, CompileError> {
    let mut candidates: Vec<Vec<IrType>> = Vec::new();
    for func in &ir_module.functions {
        if !func.name.starts_with("__closure_") {
            continue;
        }
        if func.return_type != *return_type {
            continue;
        }
        if func.param_types.len() < user_param_types.len() {
            continue;
        }
        let capture_count = func.param_types.len() - user_param_types.len();
        let suffix = &func.param_types[capture_count..];
        if suffix == user_param_types {
            candidates.push(func.param_types[..capture_count].to_vec());
        }
    }

    if candidates.is_empty() {
        // No captures found — the closure has no captured variables.
        return Ok(Vec::new());
    }

    // Check that all matching closures agree on capture types.
    let first = &candidates[0];
    if candidates.iter().all(|c| c == first) {
        Ok(candidates.into_iter().next().unwrap())
    } else {
        Err(CompileError::new(
            "ambiguous indirect call: multiple closures with the same user signature \
             but different captures. This pattern requires ClosureAlloc tracking \
             (pass closures directly, not through block parameters).",
        ))
    }
}
