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
use super::enum_helpers::{
    build_option_none, build_option_some, build_result_err, build_result_ok, load_disc_and_branch,
};
use super::enum_type_inference::{
    payload_inference_error, try_result_payload_types_from_args, try_type_from_closure_arg,
    try_type_from_enum_alloc, try_type_from_layout, try_type_from_result,
    try_type_from_result_args, try_type_from_value_arg,
};
use super::layout::TypeLayout;
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
            let payload = TypeLayout::of(&err_ty).load(builder, recv_ptr, 1);
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
            let payload = TypeLayout::of(&err_ty).load(builder, recv_ptr, 1);
            let result = call_closure(builder, ctx, ir_module, closure_vid, &payload, state)?;
            builder.ins().jump(br.merge_block, &[result[0]]);

            Ok(finish_merge(builder, &br))
        }
        "unwrapOrElse" => {
            let closure_vid = args[1];
            let cl_types = TypeLayout::of(&ok_ty).cl_types();
            let br = load_disc_and_branch(builder, recv_ptr, ir_module, "Result", "Ok", cl_types)?;

            enter_block(builder, br.positive_block);
            let payload = TypeLayout::of(&ok_ty).load(builder, recv_ptr, 1);
            builder.ins().jump(br.merge_block, &payload);

            enter_block(builder, br.negative_block);
            let err_payload = TypeLayout::of(&err_ty).load(builder, recv_ptr, 1);
            let result = call_closure(builder, ctx, ir_module, closure_vid, &err_payload, state)?;
            builder.ins().jump(br.merge_block, &result);

            Ok(finish_merge(builder, &br))
        }
        // Convert `Result<T, E>` to `Option<T>`: `Ok(v) → Some(v)`, `Err(_) → None`.
        "ok" => {
            let br = load_disc_and_branch(
                builder,
                recv_ptr,
                ir_module,
                "Result",
                "Ok",
                &[POINTER_TYPE],
            )?;

            enter_block(builder, br.positive_block);
            let payload = TypeLayout::of(&ok_ty).load(builder, recv_ptr, 1);
            let some_ptr = build_option_some(builder, ctx, &payload, &ok_ty, ir_module)?;
            builder.ins().jump(br.merge_block, &[some_ptr]);

            enter_block(builder, br.negative_block);
            let none_ptr = build_option_none(builder, ctx, ir_module)?;
            builder.ins().jump(br.merge_block, &[none_ptr]);

            Ok(finish_merge(builder, &br))
        }
        // Convert `Result<T, E>` to `Option<E>`: `Ok(_) → None`, `Err(e) → Some(e)`.
        "err" => {
            let br = load_disc_and_branch(
                builder,
                recv_ptr,
                ir_module,
                "Result",
                "Ok",
                &[POINTER_TYPE],
            )?;

            enter_block(builder, br.positive_block);
            let none_ptr = build_option_none(builder, ctx, ir_module)?;
            builder.ins().jump(br.merge_block, &[none_ptr]);

            enter_block(builder, br.negative_block);
            let err_payload = TypeLayout::of(&err_ty).load(builder, recv_ptr, 1);
            let some_ptr = build_option_some(builder, ctx, &err_payload, &err_ty, ir_module)?;
            builder.ins().jump(br.merge_block, &[some_ptr]);

            Ok(finish_merge(builder, &br))
        }
        _ => Err(CompileError::new(format!(
            "result method '{method}' not yet supported in compiled mode"
        ))),
    }
}

/// Get the `Ok` and `Err` payload types from a `Result<T, E>`.
///
/// Strategies run in the order below; each step maps to a labelled
/// `// Strategy N` block in the body. Unlike Option (single payload),
/// Result has two independent slots, so each strategy fills whichever
/// slots it can and the next strategy covers the rest. Return points
/// occur whenever both slots are known.
///
/// - **Strategy 0 — receiver's `EnumRef("Result", [ok, err])` args.**
///   If both resolve, return; otherwise seed `arg_ok` / `arg_err`.
/// - **Strategy 1 — instruction's `result_type`.** Only meaningful for
///   `unwrap` / `unwrapOr` / `unwrapOrElse`, where the result IS the Ok
///   payload. Seeds `ok_from_result`.
/// - **Strategy 1b — `result_type`'s own args.** For `ok` (returns
///   `Option<T>`) and `err` (returns `Option<E>`), peel the Option's
///   arg to recover the receiver's payload. Covers the case where the
///   receiver's EnumRef carried an unresolved TypeVar because sema
///   typed the RHS (e.g. `Err("boom")`) independently of the `let`
///   annotation, but still typed the method call from the binding.
/// - **Strategy 2 — enum layout.** Stdlib layouts use
///   `GENERIC_PLACEHOLDER`, so this only fires for layouts with
///   concrete field types.
/// - **Strategy 3 — method argument types.** Closure param / value arg
///   types, per-method. `isOk`/`isErr` don't read the payload, so any
///   partial values are padded with dummies and we return.
/// - **Strategy 4 — recorded `EnumAlloc` info.** Filtered by variant
///   (Ok = 0, Err = 1) so an `Ok(_)` allocation cannot seed the Err
///   slot and vice versa.
/// - **Strategy 5 — terminate.** Error for methods that need a slot we
///   can't infer; `I64` dummies for unknown methods so the dispatch
///   table can produce a "not yet supported" error.
fn result_payload_types(
    state: &FuncState,
    ir_module: &IrModule,
    method: &str,
    args: &[ValueId],
    result_type: &IrType,
) -> Result<(IrType, IrType), CompileError> {
    // Strategy 0: read Ok and Err types directly from the receiver's
    // `EnumRef("Result", [ok_ty, err_ty])` generic args.  The preferred
    // path when the IR preserves the args through lowering.
    let (arg_ok, arg_err) = try_result_payload_types_from_args(state, args[0]);
    if let (Some(ok), Some(err)) = (&arg_ok, &arg_err) {
        return Ok((ok.clone(), err.clone()));
    }

    // Strategy 1: result_type (for unwrap-family methods).
    let ok_from_result = match method {
        "unwrap" | "unwrapOr" | "unwrapOrElse" => try_type_from_result(result_type),
        _ => None,
    };

    // Strategy 1b: peel the result_type's args when the method re-wraps
    // the Result's payload into another enum. Works even when the
    // receiver's `EnumRef` carried an unresolved TypeVar (common for
    // `Err(_)`/`Ok(_)` RHSs whose sibling slot wasn't constrained by the
    // initializer), because sema types the method call from the
    // receiver's binding type, not its RHS expression type.
    //
    // - `ok() -> Option<T>` → args[0] is the Ok payload
    // - `err() -> Option<E>` → args[0] is the Err payload
    let arg_ok = arg_ok.or_else(|| match method {
        "ok" => try_type_from_result_args(result_type, 0),
        _ => None,
    });
    let arg_err = arg_err.or_else(|| match method {
        "err" => try_type_from_result_args(result_type, 0),
        _ => None,
    });
    if let (Some(ok), Some(err)) = (&arg_ok, &arg_err) {
        return Ok((ok.clone(), err.clone()));
    }

    // Strategy 2: enum layout.
    let layout_ok_ty = try_type_from_layout(ir_module, "Result", "Ok");
    let layout_err_ty = try_type_from_layout(ir_module, "Result", "Err");

    if let (Some(ok), Some(err)) = (&layout_ok_ty, &layout_err_ty) {
        return Ok((ok.clone(), err.clone()));
    }

    // Strategy 3: method arguments.  Seed with whichever arg-derived or
    // result-derived or layout-derived types we already have.
    let mut ok_ty = arg_ok.or(ok_from_result).or(layout_ok_ty);
    let mut err_ty = arg_err.or(layout_err_ty);

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

    // `isOk`/`isErr` only read the discriminant, never the payload, so the
    // payload types we return are unused. `I64` for both matches
    // option_methods' parallel fallback and avoids picking an arbitrary
    // reference type that a future reader might assume is load-bearing.
    if matches!(method, "isOk" | "isErr") {
        return Ok((ok_ty.unwrap_or(IrType::I64), err_ty.unwrap_or(IrType::I64)));
    }

    // For methods that use the payload, require at least the relevant type.
    let needs_ok = matches!(
        method,
        "unwrap" | "unwrapOr" | "unwrapOrElse" | "map" | "andThen" | "ok"
    );
    let needs_err = matches!(method, "mapErr" | "orElse" | "unwrapOrElse" | "err");

    // Strategy 4: EnumAlloc tracking — when prior strategies fail, inspect
    // recorded `EnumAlloc` payload types on the receiver (or consistent
    // same-enum allocations in the function).  Ok = variant 0, Err = variant
    // 1: without that filter, an `Ok(i)` receiver's payload would seed the
    // Err slot and vice versa.
    if needs_ok
        && ok_ty.is_none()
        && let Some(ty) = try_type_from_enum_alloc(state, args[0], "Result", 0)
    {
        ok_ty = Some(ty);
    }
    if needs_err
        && err_ty.is_none()
        && let Some(ty) = try_type_from_enum_alloc(state, args[0], "Result", 1)
    {
        err_ty = Some(ty);
    }

    if needs_ok && ok_ty.is_none() {
        return Err(payload_inference_error(
            "Result",
            "Ok",
            method,
            "Result<T, E>",
        ));
    }
    if needs_err && err_ty.is_none() {
        return Err(payload_inference_error(
            "Result",
            "Err",
            method,
            "Result<T, E>",
        ));
    }

    // Unknown methods — use dummy types so the dispatch table can produce
    // the proper "not yet supported" error.
    Ok((ok_ty.unwrap_or(IrType::I64), err_ty.unwrap_or(IrType::I64)))
}
