//! Translation of `Result.*` builtin method calls to Cranelift IR.
//!
//! Result is compiled as an enum with variants `Ok(T)` and `Err(E)`.
//! The receiver is an enum pointer; discriminant is at offset 0,
//! payload at offset 8.

use cranelift_codegen::ir::{InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::closure_call::{call_closure, closure_return_type};
use super::enum_combinators::{
    enter_block, finish_merge, passthrough_recv, translate_enum_and_then, translate_enum_map,
    translate_enum_unwrap, translate_enum_unwrap_or, translate_is_not_variant,
    translate_is_variant,
};
use super::enum_helpers::{build_result_err, build_result_ok, load_disc_and_branch};
use super::enum_type_inference::{
    try_type_from_closure_arg, try_type_from_layout, try_type_from_result, try_type_from_value_arg,
};
use super::helpers::load_fat_value;
use super::{FuncState, get_val, get_val1};

/// Translate a `Result.*` builtin method call.
///
/// `result_type` is the IR result type of the BuiltinCall instruction,
/// used to recover concrete payload types `T` and `E` for `Result<T, E>`.
pub(super) fn translate_result_method(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    method: &str,
    args: &[ValueId],
    state: &FuncState,
    result_type: &IrType,
) -> Result<Vec<Value>, CompileError> {
    let recv_ptr = get_val1(state, args[0])?;
    let (ok_ty, err_ty) = result_payload_types(state, ir_module, method, args, result_type)?;

    match method {
        "isOk" => translate_is_variant(builder, recv_ptr, ir_module, "Result", "Ok"),
        "isErr" => translate_is_not_variant(builder, recv_ptr, ir_module, "Result", "Ok"),
        "unwrap" => translate_enum_unwrap(
            builder,
            ctx,
            recv_ptr,
            ir_module,
            "Result",
            "Ok",
            "called unwrap() on Err",
            &ok_ty,
        ),
        "unwrapOr" => {
            let default_vals = get_val(state, args[1])?;
            translate_enum_unwrap_or(
                builder,
                ctx,
                recv_ptr,
                ir_module,
                "Result",
                "Ok",
                &default_vals,
                &ok_ty,
            )
        }
        "map" => translate_enum_map(
            builder,
            ctx,
            ir_module,
            recv_ptr,
            "Result",
            "Ok",
            &ok_ty,
            args[1],
            state,
            build_result_ok,
            passthrough_recv,
        ),
        "mapErr" => {
            let closure_vid = args[1];
            let br = load_disc_and_branch(
                builder,
                recv_ptr,
                ir_module,
                "Result",
                "Ok",
                &[POINTER_TYPE],
            )?;

            enter_block(builder, br.positive_block);
            builder.ins().jump(br.merge_block, &[recv_ptr]);

            enter_block(builder, br.negative_block);
            let payload = load_fat_value(builder, &err_ty, recv_ptr, 1)?;
            let result = call_closure(builder, ctx, ir_module, closure_vid, &payload, state)?;
            let mapped_ty = closure_return_type(state, closure_vid)?;
            let new_err = build_result_err(builder, ctx, &result, &mapped_ty, ir_module)?;
            builder.ins().jump(br.merge_block, &[new_err]);

            Ok(finish_merge(builder, &br))
        }
        "andThen" => translate_enum_and_then(
            builder,
            ctx,
            ir_module,
            recv_ptr,
            "Result",
            "Ok",
            &ok_ty,
            args[1],
            state,
            passthrough_recv,
        ),
        "orElse" => {
            let closure_vid = args[1];
            let br = load_disc_and_branch(
                builder,
                recv_ptr,
                ir_module,
                "Result",
                "Ok",
                &[POINTER_TYPE],
            )?;

            enter_block(builder, br.positive_block);
            builder.ins().jump(br.merge_block, &[recv_ptr]);

            enter_block(builder, br.negative_block);
            let payload = load_fat_value(builder, &err_ty, recv_ptr, 1)?;
            let result = call_closure(builder, ctx, ir_module, closure_vid, &payload, state)?;
            builder.ins().jump(br.merge_block, &[result[0]]);

            Ok(finish_merge(builder, &br))
        }
        "unwrapOrElse" => {
            let closure_vid = args[1];
            let cl_types = crate::types::ir_type_to_cl(&ok_ty);
            let br = load_disc_and_branch(builder, recv_ptr, ir_module, "Result", "Ok", &cl_types)?;

            enter_block(builder, br.positive_block);
            let payload = load_fat_value(builder, &ok_ty, recv_ptr, 1)?;
            builder.ins().jump(br.merge_block, &payload);

            enter_block(builder, br.negative_block);
            let err_payload = load_fat_value(builder, &err_ty, recv_ptr, 1)?;
            let result = call_closure(builder, ctx, ir_module, closure_vid, &err_payload, state)?;
            builder.ins().jump(br.merge_block, &result);

            Ok(finish_merge(builder, &br))
        }
        _ => Err(CompileError::new(format!(
            "result method '{method}' not yet supported in compiled mode"
        ))),
    }
}

/// Get the `Ok` and `Err` payload types from a `Result<T, E>`.
///
/// Uses the shared inference helpers from `option_methods` in priority order:
/// 1. The instruction's `result_type` — for methods returning `T` directly.
/// 2. The enum layout — if it has concrete (non-generic) field types.
/// 3. Method argument types — closure param types or default value types.
/// 4. Error for methods that require a type but can't infer it.
fn result_payload_types(
    state: &FuncState,
    ir_module: &IrModule,
    method: &str,
    args: &[ValueId],
    result_type: &IrType,
) -> Result<(IrType, IrType), CompileError> {
    // Strategy 1: result_type (for unwrap-family methods).
    let ok_from_result = match method {
        "unwrap" | "unwrapOr" | "unwrapOrElse" => try_type_from_result(result_type),
        _ => None,
    };

    // Strategy 2: enum layout.
    let layout_ok_ty = try_type_from_layout(ir_module, "Result", "Ok");
    let layout_err_ty = try_type_from_layout(ir_module, "Result", "Err");

    if let (Some(ok), Some(err)) = (&layout_ok_ty, &layout_err_ty) {
        return Ok((ok.clone(), err.clone()));
    }

    // Strategy 3: method arguments.
    let mut ok_ty = ok_from_result.or(layout_ok_ty);
    let mut err_ty = layout_err_ty;

    match method {
        "unwrapOr" if ok_ty.is_none() => {
            ok_ty = try_type_from_value_arg(state, args, 1);
        }
        "map" | "andThen" if ok_ty.is_none() => {
            ok_ty = try_type_from_closure_arg(state, args);
        }
        "mapErr" | "orElse" | "unwrapOrElse" if err_ty.is_none() => {
            err_ty = try_type_from_closure_arg(state, args);
        }
        _ => {}
    }

    // Methods that don't use the payload can safely fall back.
    if matches!(method, "isOk" | "isErr") {
        return Ok((
            ok_ty.unwrap_or(IrType::I64),
            err_ty.unwrap_or(IrType::StringRef),
        ));
    }

    // For methods that use the payload, require at least the relevant type.
    let needs_ok = matches!(
        method,
        "unwrap" | "unwrapOr" | "unwrapOrElse" | "map" | "andThen"
    );
    let needs_err = matches!(method, "mapErr" | "orElse" | "unwrapOrElse");

    if needs_ok && ok_ty.is_none() {
        return Err(CompileError::new(format!(
            "could not infer Result Ok type for method '{method}'. \
             All inference strategies failed — this is a compiler bug."
        )));
    }
    if needs_err && err_ty.is_none() {
        return Err(CompileError::new(format!(
            "could not infer Result Err type for method '{method}'. \
             All inference strategies failed — this is a compiler bug."
        )));
    }

    // Unknown methods — use dummy types so the dispatch table can produce
    // the proper "not yet supported" error.
    Ok((ok_ty.unwrap_or(IrType::I64), err_ty.unwrap_or(IrType::I64)))
}
