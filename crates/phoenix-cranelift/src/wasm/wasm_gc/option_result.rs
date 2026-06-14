//! wasm32-gc `Option<T>` / `Result<T, E>` builtin-method lowering
//! (§Phase 2.4 decision K.8 follow-up slice).
//!
//! `Option` and `Result` are ordinary Phoenix enums (registered by
//! sema; declared on this target by the K.4 enum pass), so their
//! method builtins lower entirely in terms of machinery that already
//! exists: the K.4 enum representation (`$tag` at slot 0, one final
//! `struct` subtype per variant) and the K.8 closure call
//! (`emit_closure_call`). Nothing new is declared — these are pure
//! op-lowering helpers, dispatched from `translate_builtin_call`.
//!
//! Variant indices are stdlib-fixed (sema registers them in this
//! order): `Some` / `Ok` are variant **0** (the "positive" payload),
//! `None` / `Err` are variant **1**. Every method here branches on
//! the receiver's `$tag` and either reads the positive payload, calls
//! a closure on it, or rebuilds a variant in the *result* enum type.
//!
//! **Why rebuild rather than reuse the receiver on the negative
//! path.** `Result<Int, String>.map(Int -> Bool)` produces
//! `Result<Bool, String>` — a *distinct* WASM type from the receiver
//! under K.4's nominal `(name, type_args)` monomorphization, even
//! though the `Err(String)` payload is structurally identical. So the
//! `Err`/`None` propagation path can't hand back the receiver ref; it
//! extracts the payload (for `Err`) and `struct.new`s a fresh variant
//! in the *output* enum type. (wasm32-linear sidesteps this because
//! its enums are byte-offset layouts with no nominal identity.)
//!
//! Semantics match `phoenix-ir-interp`'s `builtins/option_result.rs`
//! byte-for-byte; `unwrap` on `None`/`Err` **traps** (the established
//! wasm32-gc convention for runtime panics — no message until panic
//! routing lands).

use phoenix_ir::instruction::ValueId;
use phoenix_ir::types::IrType;
use wasm_encoder::{BlockType, HeapType, Instruction, RefType, ValType};

use crate::error::CompileError;

use super::closures::emit_closure_call;
use super::module_builder::ModuleBuilder;
use super::translate::{FuncCtx, enum_parent_idx_of_binding, expect_result, single_slot};

/// Variant index of the positive payload (`Some` / `Ok`).
const POSITIVE: u32 = 0;
/// Variant index of the negative case (`None` / `Err`).
const NEGATIVE: u32 = 1;

/// Resolved enum facts for one builtin call: the receiver's parent
/// type index, its concrete `(name, type_args)`, and the per-variant
/// struct indices — everything the lowerings below need.
struct EnumInfo {
    parent_idx: u32,
    name: String,
    type_args: Vec<IrType>,
    variant_indices: Vec<u32>,
}

/// Gather the receiver's enum facts from its binding type.
fn enum_info(ctx: &FuncCtx, b: &ModuleBuilder, recv: ValueId) -> Result<EnumInfo, CompileError> {
    let parent_idx = enum_parent_idx_of_binding(ctx, recv, "Option/Result builtin receiver")?;
    let (key, variants) = b.enum_by_parent_idx(parent_idx).ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-gc: Option/Result builtin receiver `{recv:?}` is bound to \
             type index {parent_idx}, which is not a recorded enum parent \
             (internal compiler bug)"
        ))
    })?;
    Ok(EnumInfo {
        parent_idx,
        name: key.0.clone(),
        type_args: key.1.clone(),
        variant_indices: variants.to_vec(),
    })
}

/// The result enum's `(name, type_args)` from `instr.result_type` —
/// used by `map` / `andThen`, whose output is a (possibly differently
/// parameterized) enum of the same template.
fn result_enum<'a>(
    instr: &'a phoenix_ir::instruction::Instruction,
    method: &str,
) -> Result<(&'a str, &'a [IrType]), CompileError> {
    match &instr.result_type {
        IrType::EnumRef(name, args) => Ok((name, args)),
        other => Err(CompileError::new(format!(
            "wasm32-gc: `{method}` result type is `{other:?}`, expected \
             `EnumRef` (internal compiler bug)"
        ))),
    }
}

/// Emit `recv.$tag` (read the discriminant through the parent type).
fn emit_tag(ctx: &mut FuncCtx, recv_local: u32, parent_idx: u32) {
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: parent_idx,
        field_index: 0,
    });
}

/// Emit the positive payload extraction: `ref.cast` the receiver to
/// its positive variant and `struct.get` field 1 (slot 0 is `$tag`),
/// leaving the payload on the stack.
fn emit_positive_payload(ctx: &mut FuncCtx, recv_local: u32, info: &EnumInfo) {
    let variant_idx = info.variant_indices[POSITIVE as usize];
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::RefCastNonNull(HeapType::Concrete(variant_idx)));
    ctx.emit(Instruction::StructGet {
        struct_type_index: variant_idx,
        field_index: 1,
    });
}

/// Dispatch an `Option.<m>` / `Result.<m>` builtin. `enum_name` is the
/// template (`"Option"` / `"Result"`), `method` the bare method name.
/// The wasm32-gc surface covers exactly the closure-and-unwrap family
/// the `option_result.phx` fixture exercises (matching the IR
/// interpreter); rarer combinators (`mapErr` / `orElse` / `filter` /
/// `ok` / `okOr` / …) error with a clear per-slice diagnostic until a
/// fixture needs them.
pub(super) fn translate_builtin(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    enum_name: &str,
    method: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match method {
        "map" => translate_map(ctx, b, args, instr),
        "andThen" => translate_and_then(ctx, b, args, instr),
        "unwrap" => translate_unwrap(ctx, b, args, instr),
        "unwrapOr" => translate_unwrap_or(ctx, b, args, instr),
        "isOk" | "isSome" => translate_is_variant(ctx, b, args, instr, POSITIVE),
        "isErr" | "isNone" => translate_is_variant(ctx, b, args, instr, NEGATIVE),
        other => Err(CompileError::new(format!(
            "wasm32-gc: `{enum_name}.{other}` is not yet supported — the \
             current slice covers `map` / `andThen` / `unwrap` / `unwrapOr` \
             / `isOk` / `isErr` / `isSome` / `isNone` (the `option_result.phx` \
             surface); other combinators land when a fixture needs them"
        ))),
    }
}

/// `map(closure)` — positive: `struct.new positive(0, closure(payload))`;
/// negative: rebuild the negative variant in the output enum.
fn translate_map(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("map", args, 2)?;
    let vid = expect_result(instr, "Option/Result.map")?;
    let recv = args[0];
    let closure = args[1];
    let info = enum_info(ctx, b, recv)?;
    let (out_name, out_args) = result_enum(instr, "map")?;
    let out_name = out_name.to_string();
    let out_args = out_args.to_vec();
    let out_parent = b.require_enum_parent_idx(&out_name, &out_args)?;
    let out_pos = b.require_enum_variant_idx(&out_name, &out_args, POSITIVE)?;
    let out_ref = parent_ref(out_parent);
    // Payload type T = receiver's positive type arg (Option<T> → [T],
    // Result<T, E> → [T, E]). Staged to a scratch local for the call.
    let payload_ty = single_slot(&info.type_args[0], b, "map payload")?;
    let recv_local = ctx.binding_of(recv)?;
    let payload_scratch = ctx.scratch_local(payload_ty);

    emit_tag(ctx, recv_local, info.parent_idx);
    ctx.emit(Instruction::I32Eqz); // tag == 0 → positive
    ctx.emit(Instruction::If(BlockType::Result(out_ref)));
    // positive: struct.new out_positive(0, closure(payload))
    emit_positive_payload(ctx, recv_local, &info);
    ctx.emit(Instruction::LocalSet(payload_scratch));
    ctx.emit(Instruction::I32Const(POSITIVE as i32));
    emit_closure_call(ctx, b, closure, &[payload_scratch])?;
    ctx.emit(Instruction::StructNew(out_pos));
    ctx.emit(Instruction::Else);
    emit_negative_rebuild(ctx, b, recv_local, &info, &out_name, &out_args)?;
    ctx.emit(Instruction::End);

    let local = ctx.allocate_local(vid, out_ref);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `andThen(closure)` — positive: the closure's return *is* the result
/// (it already produces an `Option<U>` / `Result<U, E>`); negative:
/// rebuild the negative variant in the output enum.
fn translate_and_then(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("andThen", args, 2)?;
    let vid = expect_result(instr, "Option/Result.andThen")?;
    let recv = args[0];
    let closure = args[1];
    let info = enum_info(ctx, b, recv)?;
    let (out_name, out_args) = result_enum(instr, "andThen")?;
    let out_name = out_name.to_string();
    let out_args = out_args.to_vec();
    let out_parent = b.require_enum_parent_idx(&out_name, &out_args)?;
    let out_ref = parent_ref(out_parent);
    let payload_ty = single_slot(&info.type_args[0], b, "andThen payload")?;
    let recv_local = ctx.binding_of(recv)?;
    let payload_scratch = ctx.scratch_local(payload_ty);

    emit_tag(ctx, recv_local, info.parent_idx);
    ctx.emit(Instruction::I32Eqz);
    ctx.emit(Instruction::If(BlockType::Result(out_ref)));
    // positive: closure(payload) — its return type is the output enum.
    emit_positive_payload(ctx, recv_local, &info);
    ctx.emit(Instruction::LocalSet(payload_scratch));
    emit_closure_call(ctx, b, closure, &[payload_scratch])?;
    ctx.emit(Instruction::Else);
    emit_negative_rebuild(ctx, b, recv_local, &info, &out_name, &out_args)?;
    ctx.emit(Instruction::End);

    let local = ctx.allocate_local(vid, out_ref);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Rebuild the negative variant in the output enum type. `None` is a
/// payload-free `struct.new(1)`; `Err` carries the receiver's error
/// payload (type `E`, identical in input and output), extracted and
/// re-wrapped.
fn emit_negative_rebuild(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    recv_local: u32,
    info: &EnumInfo,
    out_name: &str,
    out_args: &[IrType],
) -> Result<(), CompileError> {
    let out_neg = b.require_enum_variant_idx(out_name, out_args, NEGATIVE)?;
    // `Result`'s negative variant is `Err(E)` (one field); `Option`'s
    // is the payload-free `None`. This module is dispatched only for
    // those two templates, so the name is an exact discriminator.
    let negative_has_payload = info.name == "Result";
    ctx.emit(Instruction::I32Const(NEGATIVE as i32));
    if negative_has_payload {
        // `Err(e)` — carry the error payload across (E is shared).
        let in_neg = info.variant_indices[NEGATIVE as usize];
        ctx.emit(Instruction::LocalGet(recv_local));
        ctx.emit(Instruction::RefCastNonNull(HeapType::Concrete(in_neg)));
        ctx.emit(Instruction::StructGet {
            struct_type_index: in_neg,
            field_index: 1,
        });
    }
    ctx.emit(Instruction::StructNew(out_neg));
    Ok(())
}

/// `unwrap()` — positive payload, or trap (the wasm32-gc panic
/// convention) on the negative variant.
fn translate_unwrap(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("unwrap", args, 1)?;
    let vid = expect_result(instr, "Option/Result.unwrap")?;
    let info = enum_info(ctx, b, args[0])?;
    let payload_ty = single_slot(&instr.result_type, b, "unwrap result")?;
    let recv_local = ctx.binding_of(args[0])?;

    emit_tag(ctx, recv_local, info.parent_idx);
    ctx.emit(Instruction::I32Eqz);
    ctx.emit(Instruction::If(BlockType::Result(payload_ty)));
    emit_positive_payload(ctx, recv_local, &info);
    ctx.emit(Instruction::Else);
    // `unwrap()` on None/Err — trap. No message (panic routing TBD).
    ctx.emit(Instruction::Unreachable);
    ctx.emit(Instruction::End);

    let local = ctx.allocate_local(vid, payload_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `unwrapOr(default)` — positive payload, or the caller's default.
fn translate_unwrap_or(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("unwrapOr", args, 2)?;
    let vid = expect_result(instr, "Option/Result.unwrapOr")?;
    let info = enum_info(ctx, b, args[0])?;
    let payload_ty = single_slot(&instr.result_type, b, "unwrapOr result")?;
    let recv_local = ctx.binding_of(args[0])?;
    let default_local = ctx.binding_of(args[1])?;

    emit_tag(ctx, recv_local, info.parent_idx);
    ctx.emit(Instruction::I32Eqz);
    ctx.emit(Instruction::If(BlockType::Result(payload_ty)));
    emit_positive_payload(ctx, recv_local, &info);
    ctx.emit(Instruction::Else);
    ctx.emit(Instruction::LocalGet(default_local));
    ctx.emit(Instruction::End);

    let local = ctx.allocate_local(vid, payload_ty);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `isOk` / `isSome` (`which == POSITIVE`) and `isErr` / `isNone`
/// (`which == NEGATIVE`) — a discriminant equality check, result
/// `Bool` (i32 0/1).
fn translate_is_variant(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    which: u32,
) -> Result<(), CompileError> {
    expect_args("is<Variant>", args, 1)?;
    let vid = expect_result(instr, "Option/Result.is<Variant>")?;
    let info = enum_info(ctx, b, args[0])?;
    let recv_local = ctx.binding_of(args[0])?;
    emit_tag(ctx, recv_local, info.parent_idx);
    // `POSITIVE` is tag 0, so `i32.eqz` is the one-instruction "== 0"
    // normalize; the negative case needs the general `tag == which`
    // compare. (Don't "simplify" the negative branch to `eqz` — that
    // only works for tag 0.)
    if which == POSITIVE {
        ctx.emit(Instruction::I32Eqz);
    } else {
        ctx.emit(Instruction::I32Const(which as i32));
        ctx.emit(Instruction::I32Eq);
    }
    let local = ctx.allocate_local(vid, ValType::I32);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `(ref null $parent)` valtype for an enum parent index.
fn parent_ref(parent_idx: u32) -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(parent_idx),
    })
}

/// Arity guard shared by the lowerings.
fn expect_args(method: &str, args: &[ValueId], n: usize) -> Result<(), CompileError> {
    if args.len() == n {
        Ok(())
    } else {
        Err(CompileError::new(format!(
            "wasm32-gc: `{method}` takes {n} arg(s) (receiver + {}) but got {} \
             (IR verifier should have caught this)",
            n - 1,
            args.len()
        )))
    }
}
