//! Closure-based List method translations (map, filter, find, any, all, reduce).
//!
//! These single-loop methods are compiled inline as Cranelift loops rather than
//! delegated to the runtime via FFI calls.  Crossing the FFI boundary per closure
//! invocation per element would be prohibitively expensive for large lists.  By
//! inlining the loop, the closure body becomes part of the same Cranelift
//! function, allowing Cranelift to optimize the entire loop as a single unit.
//!
//! Complex nested-loop methods (flatMap, sortBy) are in
//! [`super::list_methods_complex`].

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::closure_call::{call_closure_ptr, closure_return_type};
use super::enum_helpers::{build_option_none, build_option_some};
use super::helpers::{call_runtime, slots_for_type};
use super::layout::{LIST_HEADER, elem_size_bytes};
use super::list_methods::{load_list_element, store_list_element};
use super::{FuncState, get_val, get_val1};

// ── List loop preamble ─────────────────────────────────────────────

/// Shared setup for iterating over list elements.
///
/// Creates the loop header (with counter block param `i`), body, and exit
/// blocks.  The header block checks `i < len` and branches to body or exit.
/// After construction, the builder is positioned at `body_block` (sealed).
///
/// Callers should:
/// 1. Emit the loop body (the builder is positioned at `body_block`).
/// 2. Increment `i` and jump back to `preamble.header_block`.
/// 3. Seal `preamble.header_block` and `preamble.exit_block`.
/// 4. Switch to `preamble.exit_block` to emit the post-loop code.
struct ListLoopPreamble {
    /// Loop counter (`i`): a block parameter of `header_block`.
    i: Value,
    /// The loop header block (has `i` as its first block param).
    header_block: cranelift_codegen::ir::Block,
    /// The exit block (unsealed).
    exit_block: cranelift_codegen::ir::Block,
}

impl ListLoopPreamble {
    /// Build the standard list loop preamble.
    ///
    /// After this returns, the builder is positioned at `body_block` with
    /// `body_block` sealed.  `header_block` and `exit_block` are unsealed.
    fn new(builder: &mut FunctionBuilder, ctx: &mut CompileContext, list_ptr: Value) -> Self {
        let len_vals = call_runtime(builder, ctx, ctx.runtime.list_length, &[list_ptr]);
        let len = len_vals[0];

        let header_block = builder.create_block();
        let body_block = builder.create_block();
        let exit_block = builder.create_block();

        builder.append_block_param(header_block, cl::I64); // i
        let zero = builder.ins().iconst(cl::I64, 0);
        builder.ins().jump(header_block, &[zero]);

        builder.switch_to_block(header_block);
        let i = builder.block_params(header_block)[0];
        let cond = builder.ins().icmp(IntCC::SignedLessThan, i, len);
        builder.ins().brif(cond, body_block, &[], exit_block, &[]);

        builder.seal_block(body_block);
        builder.switch_to_block(body_block);

        Self {
            i,
            header_block,
            exit_block,
        }
    }

    /// Emit the standard loop increment: `i += 1; jump header_block`.
    /// Then seals `header_block`.
    fn increment_and_loop_back(&self, builder: &mut FunctionBuilder) {
        let one = builder.ins().iconst(cl::I64, 1);
        let next_i = builder.ins().iadd(self.i, one);
        builder.ins().jump(self.header_block, &[next_i]);
        builder.seal_block(self.header_block);
    }
}

/// `List.map`: apply closure to each element, collect results into a new list.
pub(super) fn translate_list_map(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    list_ptr: Value,
    elem_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let closure_vid = args[1];
    let closure_ptr = get_val1(state, closure_vid)?;

    let out_elem_ty = closure_return_type(state, closure_vid)?;
    let out_es = elem_size_bytes(&out_elem_ty);

    let out_es_val = builder.ins().iconst(cl::I64, out_es);
    let len_preview = call_runtime(builder, ctx, ctx.runtime.list_length, &[list_ptr]);
    let result_list = call_runtime(
        builder,
        ctx,
        ctx.runtime.list_alloc,
        &[out_es_val, len_preview[0]],
    );
    let result_ptr = result_list[0];

    let lp = ListLoopPreamble::new(builder, ctx, list_ptr);
    // Body: load element, call closure, store result.
    let elem_vals = load_list_element(builder, list_ptr, lp.i, elem_ty)?;
    let call_result = call_closure_ptr(
        builder,
        ctx,
        ir_module,
        closure_ptr,
        closure_vid,
        &elem_vals,
        state,
    )?;
    let header_const = builder.ins().iconst(cl::I64, LIST_HEADER as i64);
    let result_base = builder.ins().iadd(result_ptr, header_const);
    let out_es_body = builder.ins().iconst(cl::I64, out_es);
    let out_offset = builder.ins().imul(lp.i, out_es_body);
    store_list_element(builder, result_base, out_offset, &call_result, &out_elem_ty);
    lp.increment_and_loop_back(builder);

    builder.seal_block(lp.exit_block);
    builder.switch_to_block(lp.exit_block);
    Ok(vec![result_ptr])
}

/// `List.filter`: keep elements where closure returns true.
///
/// Allocates a result list with capacity = input length, iterates through
/// elements, and writes matching elements contiguously.  The result list's
/// length field is updated at the end to the actual count.
pub(super) fn translate_list_filter(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    list_ptr: Value,
    elem_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let closure_vid = args[1];
    let closure_ptr = get_val1(state, closure_vid)?;
    let es = elem_size_bytes(elem_ty);

    let len_vals = call_runtime(builder, ctx, ctx.runtime.list_length, &[list_ptr]);
    let len = len_vals[0];

    let es_val = builder.ins().iconst(cl::I64, es);
    let result_list = call_runtime(builder, ctx, ctx.runtime.list_alloc, &[es_val, len]);
    let result_ptr = result_list[0];

    let header_block = builder.create_block();
    let body_block = builder.create_block();
    let store_block = builder.create_block();
    let next_block = builder.create_block();
    let exit_block = builder.create_block();

    // Block params: i, output_count, plus element values to pass through.
    let elem_slots = slots_for_type(elem_ty);
    let elem_cl_types = crate::types::ir_type_to_cl(elem_ty);

    builder.append_block_param(header_block, cl::I64); // i
    builder.append_block_param(header_block, cl::I64); // output_count
    // Declare next_block's param *before* any branch targets it (B1 fix).
    builder.append_block_param(next_block, cl::I64); // updated out_count
    let zero = builder.ins().iconst(cl::I64, 0);
    builder.ins().jump(header_block, &[zero, zero]);

    builder.switch_to_block(header_block);
    let i = builder.block_params(header_block)[0];
    let out_count = builder.block_params(header_block)[1];
    let cond = builder.ins().icmp(IntCC::SignedLessThan, i, len);
    builder.ins().brif(cond, body_block, &[], exit_block, &[]);

    // Body: load elem, call closure.
    builder.seal_block(body_block);
    builder.switch_to_block(body_block);
    let elem_vals = load_list_element(builder, list_ptr, i, elem_ty)?;
    let pred = call_closure_ptr(
        builder,
        ctx,
        ir_module,
        closure_ptr,
        closure_vid,
        &elem_vals,
        state,
    )?;

    // Branch: if predicate is true, go to store_block with element values;
    // otherwise skip to next_block.
    let is_true = builder.ins().icmp_imm(IntCC::NotEqual, pred[0], 0);

    // Set up store_block to receive element values via block params.
    for cl_ty in &elem_cl_types {
        builder.append_block_param(store_block, *cl_ty);
    }
    builder.append_block_param(store_block, cl::I64); // out_count

    let mut store_args: Vec<Value> = elem_vals.clone();
    store_args.push(out_count);
    builder
        .ins()
        .brif(is_true, store_block, &store_args, next_block, &[out_count]);

    // Store block: copy element to result, increment out_count.
    builder.seal_block(store_block);
    builder.switch_to_block(store_block);
    let store_params = builder.block_params(store_block).to_vec();
    let passed_elem_vals = &store_params[..elem_slots];
    let passed_out_count = store_params[elem_slots];

    let header_const = builder.ins().iconst(cl::I64, LIST_HEADER as i64);
    let result_base = builder.ins().iadd(result_ptr, header_const);
    let es_body = builder.ins().iconst(cl::I64, es);
    let store_offset = builder.ins().imul(passed_out_count, es_body);
    store_list_element(
        builder,
        result_base,
        store_offset,
        passed_elem_vals,
        elem_ty,
    );
    let one = builder.ins().iconst(cl::I64, 1);
    let new_out_count = builder.ins().iadd(passed_out_count, one);
    builder.ins().jump(next_block, &[new_out_count]);

    // Next: increment i.
    builder.seal_block(next_block);
    builder.switch_to_block(next_block);
    let final_out_count = builder.block_params(next_block)[0];
    let one2 = builder.ins().iconst(cl::I64, 1);
    let next_i = builder.ins().iadd(i, one2);
    builder.ins().jump(header_block, &[next_i, final_out_count]);
    builder.seal_block(header_block);

    // Exit: set the result list's actual length.
    builder.seal_block(exit_block);
    builder.switch_to_block(exit_block);
    builder
        .ins()
        .store(MemFlags::new(), out_count, result_ptr, 0);
    Ok(vec![result_ptr])
}

/// `List.find`: return first element where closure returns true, as Option.
pub(super) fn translate_list_find(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    list_ptr: Value,
    elem_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let closure_vid = args[1];
    let closure_ptr = get_val1(state, closure_vid)?;

    // Use custom exit_block with a POINTER_TYPE param (not the preamble's default).
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, POINTER_TYPE);

    let lp = ListLoopPreamble::new(builder, ctx, list_ptr);
    // Body: load elem, call predicate.
    let elem_vals = load_list_element(builder, list_ptr, lp.i, elem_ty)?;
    let pred = call_closure_ptr(
        builder,
        ctx,
        ir_module,
        closure_ptr,
        closure_vid,
        &elem_vals,
        state,
    )?;
    let is_true = builder.ins().icmp_imm(IntCC::NotEqual, pred[0], 0);

    let continue_block = builder.create_block();
    let match_block = builder.create_block();
    builder
        .ins()
        .brif(is_true, match_block, &[], continue_block, &[]);

    // Match: wrap in Some and exit.
    builder.seal_block(match_block);
    builder.switch_to_block(match_block);
    let some_ptr = build_option_some(builder, ctx, &elem_vals, elem_ty, ir_module)?;
    builder.ins().jump(merge_block, &[some_ptr]);

    // Continue loop.
    builder.seal_block(continue_block);
    builder.switch_to_block(continue_block);
    lp.increment_and_loop_back(builder);

    // Not found (exit_block from preamble): return None.
    builder.seal_block(lp.exit_block);
    builder.switch_to_block(lp.exit_block);
    let none_ptr = build_option_none(builder, ctx, ir_module)?;
    builder.ins().jump(merge_block, &[none_ptr]);

    builder.seal_block(merge_block);
    builder.switch_to_block(merge_block);
    let result = builder.block_params(merge_block)[0];
    Ok(vec![result])
}

/// `List.any`: short-circuit boolean — returns true if any element satisfies the predicate.
pub(super) fn translate_list_any(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    list_ptr: Value,
    elem_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    translate_list_any_all_inner(
        builder, ctx, ir_module, list_ptr, elem_ty, args, state, true,
    )
}

/// `List.all`: short-circuit boolean — returns true if all elements satisfy the predicate.
pub(super) fn translate_list_all(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    list_ptr: Value,
    elem_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    translate_list_any_all_inner(
        builder, ctx, ir_module, list_ptr, elem_ty, args, state, false,
    )
}

/// Shared implementation for `List.any` and `List.all`.
#[allow(clippy::too_many_arguments)]
fn translate_list_any_all_inner(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    list_ptr: Value,
    elem_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
    is_any: bool,
) -> Result<Vec<Value>, CompileError> {
    let closure_vid = args[1];
    let closure_ptr = get_val1(state, closure_vid)?;

    // Custom merge block that carries the boolean result.
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, cl::I8);

    let lp = ListLoopPreamble::new(builder, ctx, list_ptr);
    // Body: call predicate, short-circuit on match.
    let elem_vals = load_list_element(builder, list_ptr, lp.i, elem_ty)?;
    let pred = call_closure_ptr(
        builder,
        ctx,
        ir_module,
        closure_ptr,
        closure_vid,
        &elem_vals,
        state,
    )?;

    let continue_block = builder.create_block();
    if is_any {
        let is_true = builder.ins().icmp_imm(IntCC::NotEqual, pred[0], 0);
        let true_val = builder.ins().iconst(cl::I8, 1);
        builder
            .ins()
            .brif(is_true, merge_block, &[true_val], continue_block, &[]);
    } else {
        let is_false = builder.ins().icmp_imm(IntCC::Equal, pred[0], 0);
        let false_val = builder.ins().iconst(cl::I8, 0);
        builder
            .ins()
            .brif(is_false, merge_block, &[false_val], continue_block, &[]);
    }

    builder.seal_block(continue_block);
    builder.switch_to_block(continue_block);
    lp.increment_and_loop_back(builder);

    // Default (exhausted list): any→false, all→true.
    builder.seal_block(lp.exit_block);
    builder.switch_to_block(lp.exit_block);
    let default_val = builder.ins().iconst(cl::I8, if is_any { 0 } else { 1 });
    builder.ins().jump(merge_block, &[default_val]);

    builder.seal_block(merge_block);
    builder.switch_to_block(merge_block);
    let result = builder.block_params(merge_block)[0];
    Ok(vec![result])
}

/// `List.reduce`: fold over elements with an accumulator and closure.
pub(super) fn translate_list_reduce(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    list_ptr: Value,
    elem_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    // args: [list, initial_value, closure]
    let init_vals = get_val(state, args[1])?;
    let closure_vid = args[2];
    let closure_ptr = get_val1(state, closure_vid)?;
    let acc_ty = state
        .type_map
        .get(&args[1])
        .ok_or_else(|| CompileError::new("unknown type for reduce initial value"))?
        .clone();

    let len_vals = call_runtime(builder, ctx, ctx.runtime.list_length, &[list_ptr]);
    let len = len_vals[0];

    let header_block = builder.create_block();
    let body_block = builder.create_block();
    let exit_block = builder.create_block();

    builder.append_block_param(header_block, cl::I64); // i
    let acc_slots = slots_for_type(&acc_ty);
    let acc_cl_types = crate::types::ir_type_to_cl(&acc_ty);
    for cl_ty in &acc_cl_types {
        builder.append_block_param(header_block, *cl_ty);
        builder.append_block_param(exit_block, *cl_ty);
    }

    let zero = builder.ins().iconst(cl::I64, 0);
    let mut jump_args = vec![zero];
    jump_args.extend(&init_vals);
    builder.ins().jump(header_block, &jump_args);

    builder.switch_to_block(header_block);
    let i = builder.block_params(header_block)[0];
    let acc_vals: Vec<Value> = builder.block_params(header_block)[1..1 + acc_slots].to_vec();
    let in_range = builder.ins().icmp(IntCC::SignedLessThan, i, len);
    builder
        .ins()
        .brif(in_range, body_block, &[], exit_block, &acc_vals);

    builder.seal_block(body_block);
    builder.switch_to_block(body_block);
    let elem_vals = load_list_element(builder, list_ptr, i, elem_ty)?;
    let mut closure_args = acc_vals;
    closure_args.extend(&elem_vals);
    let new_acc = call_closure_ptr(
        builder,
        ctx,
        ir_module,
        closure_ptr,
        closure_vid,
        &closure_args,
        state,
    )?;
    let one = builder.ins().iconst(cl::I64, 1);
    let next_i = builder.ins().iadd(i, one);
    let mut loop_args = vec![next_i];
    loop_args.extend(&new_acc);
    builder.ins().jump(header_block, &loop_args);
    builder.seal_block(header_block);

    builder.seal_block(exit_block);
    builder.switch_to_block(exit_block);
    let result: Vec<Value> = builder.block_params(exit_block).to_vec();
    Ok(result)
}
