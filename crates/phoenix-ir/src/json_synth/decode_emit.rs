//! The shared emit toolkit for synthesized JSON decoders: the small,
//! shape-agnostic building blocks (`Result` construction and splitting,
//! `JsonError` construction, DOM field access, missing-handle branching,
//! and the decode-a-child-node-or-propagate step) that `decode.rs`'s
//! per-shape decoders (scalar / `Option` / struct / enum — and the
//! coming `List` / `Map` slices) compose. Nothing here inspects a target
//! type's shape; that stays in `decode.rs`.

use std::collections::BTreeMap;

use phoenix_sema::types::Type;

use super::{encode_type_key, enum_variant_index};
use crate::block::BlockId;
use crate::instruction::{FuncId, Op, ValueId};
use crate::lower::LoweringContext;
use crate::terminator::Terminator;
use crate::types::{IrType, JSON_ERROR_ENUM, RESULT_ENUM};

/// `Result<T, JsonError>` as an IR type.
pub(super) fn result_ir(ctx: &LoweringContext<'_>, ty: &Type) -> IrType {
    let t = ctx.lower_type(ty);
    IrType::EnumRef(
        RESULT_ENUM.to_string(),
        vec![t, IrType::EnumRef(JSON_ERROR_ENUM.to_string(), Vec::new())],
    )
}

/// Emit `json.getField(node, key)` for a constant `key`, returning the child
/// node handle.
pub(super) fn emit_get_field(ctx: &mut LoweringContext<'_>, node: ValueId, key: &str) -> ValueId {
    let k = ctx.emit(Op::ConstString(key.to_string()), IrType::StringRef, None);
    ctx.emit(
        Op::BuiltinCall("json.getField".to_string(), vec![node, k]),
        IrType::I64,
        None,
    )
}

/// Branch on `json.isMissing(handle)`, returning `(present_block,
/// missing_block)`; the current block is terminated with the branch.
pub(super) fn emit_missing_split(
    ctx: &mut LoweringContext<'_>,
    handle: ValueId,
) -> (BlockId, BlockId) {
    let missing = ctx.emit(
        Op::BuiltinCall("json.isMissing".to_string(), vec![handle]),
        IrType::Bool,
        None,
    );
    let present = ctx.create_block();
    let absent = ctx.create_block();
    ctx.terminate(Terminator::Branch {
        condition: missing,
        true_block: absent,
        true_args: Vec::new(),
        false_block: present,
        false_args: Vec::new(),
    });
    (present, absent)
}

/// Branch on whether `r: Result<_, JsonError>` is `Err`, returning `(ok_block,
/// prop_block)`; the current block is terminated with the branch.
pub(super) fn emit_result_split(ctx: &mut LoweringContext<'_>, r: ValueId) -> (BlockId, BlockId) {
    let disc = ctx.emit(Op::EnumDiscriminant(r), IrType::I64, None);
    let err_idx = enum_variant_index(ctx, RESULT_ENUM, "Err");
    let err_disc = ctx.emit(Op::ConstI64(err_idx as i64), IrType::I64, None);
    let is_err = ctx.emit(Op::IEq(disc, err_disc), IrType::Bool, None);
    let ok_blk = ctx.create_block();
    let prop_blk = ctx.create_block();
    ctx.terminate(Terminator::Branch {
        condition: is_err,
        true_block: prop_blk,
        true_args: Vec::new(),
        false_block: ok_blk,
        false_args: Vec::new(),
    });
    (ok_blk, prop_blk)
}

/// Extract the `JsonError` from an `Err(je)` result value.
pub(super) fn emit_extract_err(ctx: &mut LoweringContext<'_>, r: ValueId) -> ValueId {
    let err_idx = enum_variant_index(ctx, RESULT_ENUM, "Err");
    ctx.emit(
        Op::EnumGetField(r, err_idx, 0),
        IrType::EnumRef(JSON_ERROR_ENUM.to_string(), Vec::new()),
        None,
    )
}

/// Decode `elem` as `fty`, calling its node decoder. On error, re-wrap the
/// `JsonError` as this result's `Err` and jump to `merge`. On success, leave
/// the context in the continuation block and return the field value.
pub(super) fn emit_decode_field(
    ctx: &mut LoweringContext<'_>,
    elem: ValueId,
    fty: &Type,
    result_ty: &IrType,
    merge: BlockId,
    node_ids: &BTreeMap<String, FuncId>,
) -> ValueId {
    let frt = result_ir(ctx, fty);
    let decoder = node_ids[&encode_type_key(fty)];
    let r = ctx.emit(Op::Call(decoder, Vec::new(), vec![elem]), frt, None);
    let (ok_blk, prop_blk) = emit_result_split(ctx, r);
    ctx.switch_to_block(prop_blk);
    let je = emit_extract_err(ctx, r);
    let err = emit_result_err(ctx, result_ty, je);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![err],
    });
    ctx.switch_to_block(ok_blk);
    let ok_idx = enum_variant_index(ctx, RESULT_ENUM, "Ok");
    let field_ir = ctx.lower_type(fty);
    ctx.emit(Op::EnumGetField(r, ok_idx, 0), field_ir, None)
}

/// `Ok(value)` as a `Result<T, JsonError>` (`result_ty`).
pub(super) fn emit_result_ok(
    ctx: &mut LoweringContext<'_>,
    result_ty: &IrType,
    value: ValueId,
) -> ValueId {
    let ok_idx = enum_variant_index(ctx, RESULT_ENUM, "Ok");
    ctx.emit(
        Op::EnumAlloc(RESULT_ENUM.to_string(), ok_idx, vec![value]),
        result_ty.clone(),
        None,
    )
}

/// `Err(json_error)` as a `Result<T, JsonError>` (`result_ty`).
pub(super) fn emit_result_err(
    ctx: &mut LoweringContext<'_>,
    result_ty: &IrType,
    je: ValueId,
) -> ValueId {
    let err_idx = enum_variant_index(ctx, RESULT_ENUM, "Err");
    ctx.emit(
        Op::EnumAlloc(RESULT_ENUM.to_string(), err_idx, vec![je]),
        result_ty.clone(),
        None,
    )
}

/// A `JsonError::<variant>(message)` value.
pub(super) fn emit_json_error(
    ctx: &mut LoweringContext<'_>,
    variant: &str,
    msg: ValueId,
) -> ValueId {
    let idx = enum_variant_index(ctx, JSON_ERROR_ENUM, variant);
    ctx.emit(
        Op::EnumAlloc(JSON_ERROR_ENUM.to_string(), idx, vec![msg]),
        IrType::EnumRef(JSON_ERROR_ENUM.to_string(), Vec::new()),
        None,
    )
}
