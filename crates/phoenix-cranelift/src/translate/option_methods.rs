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
    payload_inference_error, try_type_from_closure_arg, try_type_from_enum_alloc,
    try_type_from_enum_args, try_type_from_layout, try_type_from_result, try_type_from_result_args,
    try_type_from_value_arg,
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
/// Strategies run in the order below; the first one that yields a type
/// returns. Each step maps to a labelled `// Strategy N` block in the body,
/// so this comment and the code stay in sync.
///
/// - **Strategy 0 — receiver's `EnumRef` args.** The primary path when
///   IR lowering preserved `Option<T>`'s type arg at the use site.
/// - **Strategy 1 — instruction's `result_type`.** For `unwrap` /
///   `unwrapOr` / `unwrapOrElse`, whose IR result IS the payload type.
/// - **Strategy 1b — `result_type`'s own args.** For `okOr`, whose
///   return `Result<T, E>` embeds the Option payload as `args[0]`. Fills
///   the gap when sema resolved the call's return type from the
///   receiver's binding type but left the receiver's own EnumRef arg
///   unresolved (e.g. `let o: Option<Int> = None`, where `None`'s own
///   sema type is `Option<T>` with a TypeVar).
/// - **Strategy 2 — enum layout.** Only succeeds if the layout has
///   concrete (non-placeholder) field types; stdlib `Option`/`Result`
///   do not.
/// - **Strategy 3 — method argument types.** Closure parameter types
///   (`map`, `andThen`, `filter`, `unwrapOrElse`) or the default value's
///   type (`unwrapOr`).
/// - **Strategy 3b — dummy for payload-free methods.**
///   `isSome`/`isNone`/`orElse` never read the payload, so an `I64` is
///   safe; return here before reaching Strategy 4.
/// - **Strategy 4 — recorded `EnumAlloc` info.** Scan same-function
///   allocations of the payload-bearing variant for a consistent
///   payload type.
/// - **Strategy 5 — terminate.** Error for methods in
///   `known_payload_methods` that can't make a safe dummy choice; `I64`
///   dummy for unknown methods so the dispatch table can still produce
///   a "not yet supported" error.
fn option_payload_type(
    state: &FuncState,
    ir_module: &IrModule,
    method: &str,
    args: &[ValueId],
    result_type: &IrType,
) -> Result<IrType, CompileError> {
    // Strategy 0: read the payload type directly from the receiver's
    // `EnumRef` generic args.  The preferred path when the IR preserves
    // the args through lowering.
    if let Some(ty) = try_type_from_enum_args(state, args[0], "Option", 0) {
        return Ok(ty);
    }

    // Strategy 1: result_type (for unwrap-family methods).
    if matches!(method, "unwrap" | "unwrapOr" | "unwrapOrElse")
        && let Some(ty) = try_type_from_result(result_type)
    {
        return Ok(ty);
    }

    // Strategy 1b: peel the result_type's args for methods whose return
    // type re-wraps the Option's payload. Sema resolves the call's return
    // type using the receiver's *binding* type (post-annotation), even
    // when the receiver's own `EnumRef` carried an unresolved TypeVar
    // from a RHS expression like `None` or `Err("boom")`.
    //
    // `okOr` returns `Result<T, E>` where args[0] is the Option payload T.
    if method == "okOr"
        && let Some(ty) = try_type_from_result_args(result_type, 0)
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
    // Variant 0 = `Some`, the only payload-bearing variant of `Option`.
    if let Some(ty) = try_type_from_enum_alloc(state, args[0], "Option", 0) {
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
        return Err(payload_inference_error(
            "Option",
            "payload",
            method,
            "Option<T>",
        ));
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
