//! Shared translation helpers for enum combinator methods.
//!
//! Several methods are structurally identical between Option and Result
//! (e.g., `isSome`/`isOk`, `unwrap`, `unwrapOr`). This module provides
//! the common implementations parameterized by enum and variant names.

use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{self, InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::FuncState;
use super::closure_call::{call_closure, closure_return_type};
use super::enum_helpers::{EnumBranch, load_disc_and_branch};
use super::helpers::emit_panic_with_message;
use super::layout::TypeLayout;

/// Function pointer type for wrapping closure results into an enum variant.
///
/// Called on the positive branch (e.g., `Some` for Option, `Ok` for Result)
/// after the closure transforms the payload.
///
/// Example callers: `build_option_some`, `build_result_ok`.
type WrapFn = fn(
    &mut FunctionBuilder,
    &mut CompileContext,
    &[Value],
    &IrType,
    &IrModule,
) -> Result<Value, CompileError>;

/// Function pointer type for producing the negative-branch enum value
/// (e.g., re-wrapping `None` or propagating the `Err` side).
///
/// Receives the original receiver pointer so it can forward the existing
/// enum value unchanged.
///
/// Example callers: `build_option_none` (ignores receiver),
/// `build_result_err` clone (forwards the Err payload).
type NegativeFn =
    fn(&mut FunctionBuilder, &mut CompileContext, Value, &IrModule) -> Result<Value, CompileError>;

/// Translate `isSome` / `isOk`: returns `true` if the discriminant matches
/// `positive_variant`.
pub(super) fn translate_is_variant(
    builder: &mut FunctionBuilder,
    recv_ptr: Value,
    ir_module: &IrModule,
    enum_name: &str,
    positive_variant: &str,
) -> Result<Vec<Value>, CompileError> {
    let br = load_disc_and_branch(
        builder,
        recv_ptr,
        ir_module,
        enum_name,
        positive_variant,
        &[],
    )?;
    builder.seal_block(br.positive_block);
    builder.switch_to_block(br.positive_block);
    builder.ins().jump(br.merge_block, &[]);
    builder.seal_block(br.negative_block);
    builder.switch_to_block(br.negative_block);
    builder.ins().jump(br.merge_block, &[]);
    builder.seal_block(br.merge_block);
    builder.switch_to_block(br.merge_block);
    Ok(vec![br.is_positive])
}

/// Translate `isNone` / `isErr`: negation of `is_variant`.
pub(super) fn translate_is_not_variant(
    builder: &mut FunctionBuilder,
    recv_ptr: Value,
    ir_module: &IrModule,
    enum_name: &str,
    positive_variant: &str,
) -> Result<Vec<Value>, CompileError> {
    let br = load_disc_and_branch(
        builder,
        recv_ptr,
        ir_module,
        enum_name,
        positive_variant,
        &[],
    )?;
    builder.seal_block(br.positive_block);
    builder.switch_to_block(br.positive_block);
    builder.ins().jump(br.merge_block, &[]);
    builder.seal_block(br.negative_block);
    builder.switch_to_block(br.negative_block);
    builder.ins().jump(br.merge_block, &[]);
    builder.seal_block(br.merge_block);
    builder.switch_to_block(br.merge_block);
    let one = builder.ins().iconst(cl::I8, 1);
    let negated = builder.ins().bxor(br.is_positive, one);
    Ok(vec![negated])
}

/// Translate `unwrap` on `Option` or `Result`: load payload or panic.
#[allow(clippy::too_many_arguments)]
pub(super) fn translate_enum_unwrap(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    recv_ptr: Value,
    ir_module: &IrModule,
    enum_name: &str,
    positive_variant: &str,
    panic_msg: &str,
    payload_ty: &IrType,
) -> Result<Vec<Value>, CompileError> {
    let br = load_disc_and_branch(
        builder,
        recv_ptr,
        ir_module,
        enum_name,
        positive_variant,
        &[],
    )?;

    builder.seal_block(br.negative_block);
    builder.switch_to_block(br.negative_block);
    emit_panic_with_message(builder, ctx, panic_msg)?;

    builder.seal_block(br.positive_block);
    builder.switch_to_block(br.positive_block);
    builder.ins().jump(br.merge_block, &[]);
    builder.seal_block(br.merge_block);
    builder.switch_to_block(br.merge_block);
    Ok(TypeLayout::of(payload_ty).load(builder, recv_ptr, 1))
}

/// Translate `unwrapOr` on `Option` or `Result`: load payload or use default.
#[allow(clippy::too_many_arguments)]
pub(super) fn translate_enum_unwrap_or(
    builder: &mut FunctionBuilder,
    _ctx: &mut CompileContext,
    recv_ptr: Value,
    ir_module: &IrModule,
    enum_name: &str,
    positive_variant: &str,
    default_vals: &[Value],
    payload_ty: &IrType,
) -> Result<Vec<Value>, CompileError> {
    let cl_types = TypeLayout::of(payload_ty).cl_types();
    let br = load_disc_and_branch(
        builder,
        recv_ptr,
        ir_module,
        enum_name,
        positive_variant,
        cl_types,
    )?;

    builder.seal_block(br.positive_block);
    builder.switch_to_block(br.positive_block);
    let payload = TypeLayout::of(payload_ty).load(builder, recv_ptr, 1);
    builder.ins().jump(br.merge_block, &payload);

    builder.seal_block(br.negative_block);
    builder.switch_to_block(br.negative_block);
    builder.ins().jump(br.merge_block, default_vals);

    builder.seal_block(br.merge_block);
    builder.switch_to_block(br.merge_block);
    Ok(builder.block_params(br.merge_block).to_vec())
}

// ── Shared combinator helpers ──────────────────────────────────────
//
// The `map`, `andThen`, `orElse`, and `unwrapOrElse` methods share
// structural patterns between Option and Result.  These helpers reduce
// the per-method boilerplate.

/// Seal a block, switch to it, and return.  Shorthand for the common
/// two-line pattern that appears in every branch arm.
pub(super) fn enter_block(builder: &mut FunctionBuilder, block: ir::Block) {
    builder.seal_block(block);
    builder.switch_to_block(block);
}

/// Seal the merge block, switch to it, and return its block params.
pub(super) fn finish_merge(builder: &mut FunctionBuilder, br: &EnumBranch) -> Vec<Value> {
    builder.seal_block(br.merge_block);
    builder.switch_to_block(br.merge_block);
    builder.block_params(br.merge_block).to_vec()
}

/// Translate `map` on an enum: call closure on positive-variant payload,
/// wrap result in a new variant; negative side produces `negative_val`.
///
/// Used by both `Option.map` and `Result.map`.
#[allow(clippy::too_many_arguments)]
pub(super) fn translate_enum_map(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    recv_ptr: Value,
    enum_name: &str,
    positive_variant: &str,
    payload_ty: &IrType,
    closure_vid: ValueId,
    state: &FuncState,
    wrap_fn: WrapFn,
    negative_fn: NegativeFn,
) -> Result<Vec<Value>, CompileError> {
    let br = load_disc_and_branch(
        builder,
        recv_ptr,
        ir_module,
        enum_name,
        positive_variant,
        &[crate::types::POINTER_TYPE],
    )?;

    enter_block(builder, br.positive_block);
    let payload = TypeLayout::of(payload_ty).load(builder, recv_ptr, 1);
    let result = call_closure(builder, ctx, ir_module, closure_vid, &payload, state)?;
    let result_ty = closure_return_type(state, closure_vid)?;
    let wrapped = wrap_fn(builder, ctx, &result, &result_ty, ir_module)?;
    builder.ins().jump(br.merge_block, &[wrapped]);

    enter_block(builder, br.negative_block);
    let neg_val = negative_fn(builder, ctx, recv_ptr, ir_module)?;
    builder.ins().jump(br.merge_block, &[neg_val]);

    Ok(finish_merge(builder, &br))
}

/// Translate `andThen` on an enum: call closure on positive-variant payload,
/// pass the closure result through (the closure returns the enum type);
/// negative side produces `negative_val`.
#[allow(clippy::too_many_arguments)]
pub(super) fn translate_enum_and_then(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    recv_ptr: Value,
    enum_name: &str,
    positive_variant: &str,
    payload_ty: &IrType,
    closure_vid: ValueId,
    state: &FuncState,
    negative_fn: NegativeFn,
) -> Result<Vec<Value>, CompileError> {
    let br = load_disc_and_branch(
        builder,
        recv_ptr,
        ir_module,
        enum_name,
        positive_variant,
        &[crate::types::POINTER_TYPE],
    )?;

    enter_block(builder, br.positive_block);
    let payload = TypeLayout::of(payload_ty).load(builder, recv_ptr, 1);
    let result = call_closure(builder, ctx, ir_module, closure_vid, &payload, state)?;
    if result.is_empty() {
        return Err(CompileError::new(
            "andThen closure returned no value (expected enum pointer)",
        ));
    }
    builder.ins().jump(br.merge_block, &[result[0]]);

    enter_block(builder, br.negative_block);
    let neg_val = negative_fn(builder, ctx, recv_ptr, ir_module)?;
    builder.ins().jump(br.merge_block, &[neg_val]);

    Ok(finish_merge(builder, &br))
}

/// Translate `unwrapOrElse` on an enum: return payload if positive variant,
/// call closure if negative variant.
#[allow(clippy::too_many_arguments)]
pub(super) fn translate_enum_unwrap_or_else(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    recv_ptr: Value,
    enum_name: &str,
    positive_variant: &str,
    payload_ty: &IrType,
    closure_vid: ValueId,
    closure_args: &[Value],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let cl_types = TypeLayout::of(payload_ty).cl_types();
    let br = load_disc_and_branch(
        builder,
        recv_ptr,
        ir_module,
        enum_name,
        positive_variant,
        cl_types,
    )?;

    enter_block(builder, br.positive_block);
    let payload = TypeLayout::of(payload_ty).load(builder, recv_ptr, 1);
    builder.ins().jump(br.merge_block, &payload);

    enter_block(builder, br.negative_block);
    let result = call_closure(builder, ctx, ir_module, closure_vid, closure_args, state)?;
    builder.ins().jump(br.merge_block, &result);

    Ok(finish_merge(builder, &br))
}

/// Negative-path helper: pass through the receiver pointer unchanged.
/// Used by Result combinators where the non-matching variant is preserved.
pub(super) fn passthrough_recv(
    _builder: &mut FunctionBuilder,
    _ctx: &mut CompileContext,
    recv_ptr: Value,
    _ir_module: &IrModule,
) -> Result<Value, CompileError> {
    Ok(recv_ptr)
}
