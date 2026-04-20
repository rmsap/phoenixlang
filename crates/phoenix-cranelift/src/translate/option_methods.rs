//! Translation of `Option.*` builtin method calls to Cranelift IR.
//!
//! Option is compiled as an enum with variants `Some(T)` and `None`.
//! The receiver is an enum pointer; discriminant is at offset 0,
//! payload at offset 8.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::closure_call::call_closure;
use super::enum_combinators::{
    enter_block, finish_merge, translate_enum_and_then, translate_enum_map, translate_enum_unwrap,
    translate_enum_unwrap_or, translate_enum_unwrap_or_else, translate_is_not_variant,
    translate_is_variant,
};
use super::enum_helpers::{
    build_option_none, build_option_some, build_result_err, build_result_ok, load_disc_and_branch,
};
use super::enum_type_inference::{
    try_type_from_closure_arg, try_type_from_enum_alloc, try_type_from_layout,
    try_type_from_result, try_type_from_value_arg,
};
use super::layout::TypeLayout;
use super::{FuncState, get_val, get_val1};

/// Translate an `Option.*` builtin method call.
///
/// `result_type` is the IR result type of the BuiltinCall instruction,
/// used to recover the concrete payload type `T` for `Option<T>` when
/// it cannot be inferred from method arguments alone (e.g., `unwrap()`).
pub(super) fn translate_option_method(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    method: &str,
    args: &[ValueId],
    state: &FuncState,
    result_type: &IrType,
) -> Result<Vec<Value>, CompileError> {
    let recv_ptr = get_val1(state, args[0])?;

    // Get the payload type T of Option<T>.
    let payload_ty = option_payload_type(state, ir_module, method, args, result_type)?;

    match method {
        "isSome" => translate_is_variant(builder, recv_ptr, ir_module, "Option", "Some"),
        "isNone" => translate_is_not_variant(builder, recv_ptr, ir_module, "Option", "Some"),
        "unwrap" => translate_enum_unwrap(
            builder,
            ctx,
            recv_ptr,
            ir_module,
            "Option",
            "Some",
            "called unwrap() on None",
            &payload_ty,
        ),
        "unwrapOr" => {
            let default_vals = get_val(state, args[1])?;
            translate_enum_unwrap_or(
                builder,
                ctx,
                recv_ptr,
                ir_module,
                "Option",
                "Some",
                &default_vals,
                &payload_ty,
            )
        }
        "map" => translate_enum_map(
            builder,
            ctx,
            ir_module,
            recv_ptr,
            "Option",
            "Some",
            &payload_ty,
            args[1],
            state,
            build_option_some,
            build_none_combinator,
        ),
        "andThen" => translate_enum_and_then(
            builder,
            ctx,
            ir_module,
            recv_ptr,
            "Option",
            "Some",
            &payload_ty,
            args[1],
            state,
            build_none_combinator,
        ),
        "orElse" => {
            let closure_vid = args[1];
            let br = load_disc_and_branch(
                builder,
                recv_ptr,
                ir_module,
                "Option",
                "Some",
                &[POINTER_TYPE],
            )?;

            enter_block(builder, br.positive_block);
            builder.ins().jump(br.merge_block, &[recv_ptr]);

            enter_block(builder, br.negative_block);
            let result = call_closure(builder, ctx, ir_module, closure_vid, &[], state)?;
            builder.ins().jump(br.merge_block, &[result[0]]);

            Ok(finish_merge(builder, &br))
        }
        "filter" => {
            let closure_vid = args[1];
            let br = load_disc_and_branch(
                builder,
                recv_ptr,
                ir_module,
                "Option",
                "Some",
                &[POINTER_TYPE],
            )?;

            enter_block(builder, br.positive_block);
            let payload = TypeLayout::of(&payload_ty).load(builder, recv_ptr, 1);
            let pred = call_closure(builder, ctx, ir_module, closure_vid, &payload, state)?;
            let pred_true = builder.ins().icmp_imm(IntCC::NotEqual, pred[0], 0);

            let keep_block = builder.create_block();
            let discard_block = builder.create_block();
            builder
                .ins()
                .brif(pred_true, keep_block, &[], discard_block, &[]);

            enter_block(builder, keep_block);
            builder.ins().jump(br.merge_block, &[recv_ptr]);

            enter_block(builder, discard_block);
            let none1 = build_option_none(builder, ctx, ir_module)?;
            builder.ins().jump(br.merge_block, &[none1]);

            enter_block(builder, br.negative_block);
            let none2 = build_option_none(builder, ctx, ir_module)?;
            builder.ins().jump(br.merge_block, &[none2]);

            Ok(finish_merge(builder, &br))
        }
        "unwrapOrElse" => translate_enum_unwrap_or_else(
            builder,
            ctx,
            ir_module,
            recv_ptr,
            "Option",
            "Some",
            &payload_ty,
            args[1],
            &[],
            state,
        ),
        "okOr" => {
            translate_option_ok_or(builder, ctx, ir_module, recv_ptr, &payload_ty, args, state)
        }
        _ => Err(CompileError::new(format!(
            "option method '{method}' not yet supported in compiled mode"
        ))),
    }
}

/// Translate `Option.okOr`: convert `Option<T>` to `Result<T, E>`.
fn translate_option_ok_or(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    recv_ptr: Value,
    payload_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let err_vals = get_val(state, args[1])?;
    let err_ty = state
        .type_map
        .get(&args[1])
        .ok_or_else(|| CompileError::new("unknown type for okOr error"))?
        .clone();
    let br = load_disc_and_branch(
        builder,
        recv_ptr,
        ir_module,
        "Option",
        "Some",
        &[POINTER_TYPE],
    )?;

    enter_block(builder, br.positive_block);
    let payload = TypeLayout::of(payload_ty).load(builder, recv_ptr, 1);
    let ok_ptr = build_result_ok(builder, ctx, &payload, payload_ty, ir_module)?;
    builder.ins().jump(br.merge_block, &[ok_ptr]);

    enter_block(builder, br.negative_block);
    let err_ptr = build_result_err(builder, ctx, &err_vals, &err_ty, ir_module)?;
    builder.ins().jump(br.merge_block, &[err_ptr]);

    Ok(finish_merge(builder, &br))
}

// ── Payload type inference ──────────────────────────────────────────
//
// The `try_type_from_*` inference helpers live in `enum_type_inference.rs`
// so that both `option_methods` and `result_methods` can share them.

/// Get the payload type `T` from an `Option<T>` by examining available context.
///
/// Uses multiple strategies in priority order:
/// 1. The instruction's `result_type` — for methods that return `T` directly.
/// 2. The enum layout — if it has concrete (non-generic) field types.
/// 3. Method argument types — closure parameter types or default value types.
/// 4. Recorded `EnumAlloc` info — for `okOr` where other strategies fail.
/// 5. Safe fallback for methods that don't use the payload.
fn option_payload_type(
    state: &FuncState,
    ir_module: &IrModule,
    method: &str,
    args: &[ValueId],
    result_type: &IrType,
) -> Result<IrType, CompileError> {
    // Strategy 1: result_type (for unwrap-family methods).
    if matches!(method, "unwrap" | "unwrapOr" | "unwrapOrElse")
        && let Some(ty) = try_type_from_result(result_type)
    {
        return Ok(ty);
    }

    // Strategy 2: enum layout.
    if let Some(ty) = try_type_from_layout(ir_module, "Option", "Some") {
        return Ok(ty);
    }

    // Strategy 3: method arguments.
    match method {
        "unwrapOr" => {
            if let Some(ty) = try_type_from_value_arg(state, args, 1) {
                return Ok(ty);
            }
        }
        "map" | "andThen" | "filter" | "unwrapOrElse" => {
            if let Some(ty) = try_type_from_closure_arg(state, args) {
                return Ok(ty);
            }
        }
        _ => {}
    }

    // Methods that don't use the payload type can safely fall back.
    if matches!(method, "isSome" | "isNone" | "orElse") {
        return Ok(IrType::I64);
    }

    // Strategy 4: EnumAlloc tracking (for okOr and other difficult cases).
    if let Some(ty) = try_type_from_enum_alloc(state, args[0], "Option") {
        return Ok(ty);
    }

    // For known methods that need the payload type, error rather than
    // silently miscompiling with a wrong dummy type.
    let known_payload_methods = [
        "unwrap",
        "unwrapOr",
        "map",
        "andThen",
        "filter",
        "unwrapOrElse",
        "okOr",
    ];
    if known_payload_methods.contains(&method) {
        return Err(CompileError::new(format!(
            "could not infer Option payload type for method '{method}'. \
             All inference strategies failed — the Option value may come from \
             a function parameter or cross-function return where generic type \
             arguments are not yet propagated. Use pattern matching as a \
             workaround. This is a known compiler limitation.",
        )));
    }

    // Unknown method — use a dummy type so the method dispatch table
    // can produce the proper "not yet supported" error.
    Ok(IrType::I64)
}

/// Wrapper for `build_option_none` matching the combinator function signature.
fn build_none_combinator(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    _recv_ptr: Value,
    ir_module: &IrModule,
) -> Result<Value, CompileError> {
    build_option_none(builder, ctx, ir_module)
}
