//! Translation of closure allocation, function calls, and builtin call dispatch.
//!
//! Core call mechanics live here (direct calls, indirect calls, closure
//! allocation).  Builtin method calls are dispatched to domain-specific
//! modules: `list_methods`, `map_methods`, `option_methods`, `result_methods`.

use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::helpers::call_runtime;
use super::layout::{SLOT_SIZE, TypeLayout};
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
        ice!("translate_closure_alloc dispatched on non-ClosureAlloc op: {op:?}")
    };

    let capture_slots: usize = captures
        .iter()
        .map(|cap| {
            let ty = state.type_map.get(cap).cloned().ok_or_else(|| {
                CompileError::new(format!("unknown type for closure capture {cap}"))
            })?;
            Ok(TypeLayout::of(&ty).slots())
        })
        .collect::<Result<Vec<usize>, CompileError>>()?
        .into_iter()
        .sum();
    let num_slots = 1 + capture_slots;
    let size = (num_slots * SLOT_SIZE) as i64;
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
        let cap_layout = TypeLayout::of(&ty);
        cap_layout.store(builder, ptr, slot, &cap_vals);
        slot += cap_layout.slots();
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
        Op::Call(fid, type_args, args) => {
            // This is a compiler-invariant violation, not a debug-only
            // condition: if it ever fires we would miscompile a generic
            // call. `assert!` so release builds catch it too.
            assert!(
                type_args.is_empty(),
                "Op::Call reached codegen with non-empty type_args ({type_args:?}) \
                 — monomorphization should have cleared them"
            );
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
            // Collect user arguments as Cranelift values.
            let mut user_args = Vec::new();
            for arg in args {
                user_args.extend(get_val(state, *arg)?);
            }
            // Delegate to the shared closure-calling helper.
            super::closure_call::call_closure(builder, ctx, ir_module, *closure, &user_args, state)
        }
        Op::BuiltinCall(name, args) => {
            translate_builtin(builder, ctx, ir_module, name, args, state, result_type)
        }
        Op::UnresolvedTraitMethod(method, _, _) => Err(CompileError::new(format!(
            "internal error: unresolved trait-bound method call `.{method}` \
             reached Cranelift codegen — monomorphization was expected to \
             rewrite it to a concrete Op::Call"
        ))),
        _ => ice!("translate_call dispatched on non-call op: {op:?}"),
    }
}

/// Translate a builtin call (print, toString, method calls).
///
/// Dispatches to domain-specific modules for collection and monad methods.
fn translate_builtin(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    name: &str,
    args: &[ValueId],
    state: &FuncState,
    result_type: &IrType,
) -> Result<Vec<Value>, CompileError> {
    match name {
        "print" => translate_print(builder, ctx, args, state),
        "toString" => translate_to_string(builder, ctx, args, state),
        // String methods.
        _ if name.starts_with("String.") => translate_string_method(
            builder,
            ctx,
            name.strip_prefix("String.").unwrap(),
            args,
            state,
        ),
        // List methods.
        _ if name.starts_with("List.") => super::list_methods::translate_list_method(
            builder,
            ctx,
            ir_module,
            name.strip_prefix("List.").unwrap(),
            args,
            state,
        ),
        // Map methods.
        _ if name.starts_with("Map.") => super::map_methods::translate_map_method(
            builder,
            ctx,
            ir_module,
            name.strip_prefix("Map.").unwrap(),
            args,
            state,
        ),
        // Option methods.
        _ if name.starts_with("Option.") => super::option_methods::translate_option_method(
            builder,
            ctx,
            ir_module,
            name.strip_prefix("Option.").unwrap(),
            args,
            state,
            result_type,
        ),
        // Result methods.
        _ if name.starts_with("Result.") => super::result_methods::translate_result_method(
            builder,
            ctx,
            ir_module,
            name.strip_prefix("Result.").unwrap(),
            args,
            state,
            result_type,
        ),
        _ => Err(CompileError::new(format!(
            "builtin '{name}' not yet supported in compiled mode"
        ))),
    }
}

/// Translate a `String.*` builtin method call.
///
/// String values are represented as fat `(ptr, len)` pairs in Cranelift.
fn translate_string_method(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    method: &str,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let recv = get_val(state, args[0])?;

    match method {
        "length" | "trim" | "toLowerCase" | "toUpperCase" => {
            let func = match method {
                "length" => ctx.runtime.str_length,
                "trim" => ctx.runtime.str_trim,
                "toLowerCase" => ctx.runtime.str_to_lower,
                "toUpperCase" => ctx.runtime.str_to_upper,
                _ => ice!("translate_string_method outer arm guard mismatch: {method}"),
            };
            Ok(call_runtime(builder, ctx, func, &[recv[0], recv[1]]))
        }
        "contains" | "startsWith" | "endsWith" | "indexOf" => {
            let arg = get_val(state, args[1])?;
            let func = match method {
                "contains" => ctx.runtime.str_contains,
                "startsWith" => ctx.runtime.str_starts_with,
                "endsWith" => ctx.runtime.str_ends_with,
                "indexOf" => ctx.runtime.str_index_of,
                _ => ice!("translate_string_method outer arm guard mismatch: {method}"),
            };
            Ok(call_runtime(
                builder,
                ctx,
                func,
                &[recv[0], recv[1], arg[0], arg[1]],
            ))
        }
        "replace" => {
            let from = get_val(state, args[1])?;
            let to = get_val(state, args[2])?;
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.str_replace,
                &[recv[0], recv[1], from[0], from[1], to[0], to[1]],
            ))
        }
        "substring" => {
            let start = get_val1(state, args[1])?;
            let end = get_val1(state, args[2])?;
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.str_substring,
                &[recv[0], recv[1], start, end],
            ))
        }
        "split" => {
            let sep = get_val(state, args[1])?;
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.str_split,
                &[recv[0], recv[1], sep[0], sep[1]],
            ))
        }
        _ => Err(CompileError::new(format!(
            "string method '{method}' not yet supported in compiled mode"
        ))),
    }
}

/// Translate a `print(value)` builtin call.
fn translate_print(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
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

/// Translate a `toString(value)` builtin call.
fn translate_to_string(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
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
        IrType::StringRef => get_val(state, arg),
        _ => Err(CompileError::new(format!(
            "toString not yet supported for type {arg_type} in compiled mode"
        ))),
    }
}
