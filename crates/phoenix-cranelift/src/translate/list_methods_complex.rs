//! Complex closure-based List method translations (flatMap, sortBy).
//!
//! These methods require nested Cranelift loops and are separated from the
//! simpler single-loop methods in `list_methods_closure.rs` for readability.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::closure_call::{call_closure_ptr, closure_return_elem_type};
use super::helpers::{call_runtime, slots_for_type};
use super::layout::{LIST_HEADER, elem_size_bytes};
use super::list_methods::{load_list_element, store_list_element, store_to_temp};
use super::{FuncState, get_val1};

/// `List.flatMap`: apply closure to each element (must return List), flatten results.
///
/// ## CFG shape
///
/// ```text
///           ┌──────────────┐
///           │ entry         │  alloc empty result list
///           └──────┬───────┘
///                  ▼
///        ┌─► header_block ──► exit_block ──► return acc_list
///        │    (i, acc)   i<len?
///        │        │ yes
///        │        ▼
///        │    body_block      call closure → inner_list
///        │        │
///        │        ▼
///        │  ┌─► inner_header ──► inner_exit ─┐
///        │  │    (j, acc)   j<inner_len?      │
///        │  │        │ yes                     │
///        │  │        ▼                         │
///        │  │    inner_body                    │
///        │  │    push elem, j++                │
///        │  └────┘                             │
///        │                     i++, acc ◄──────┘
///        └─────────────────────┘
/// ```
pub(super) fn translate_list_flatmap(
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

    let out_elem_ty = closure_return_elem_type(state, closure_vid)?;
    let out_es = elem_size_bytes(&out_elem_ty);

    let len_vals = call_runtime(builder, ctx, ctx.runtime.list_length, &[list_ptr]);
    let len = len_vals[0];

    // Start with an empty result list.
    let out_es_val = builder.ins().iconst(cl::I64, out_es);
    let zero = builder.ins().iconst(cl::I64, 0);
    let init_result = call_runtime(builder, ctx, ctx.runtime.list_alloc, &[out_es_val, zero]);

    let header_block = builder.create_block();
    let body_block = builder.create_block();
    let exit_block = builder.create_block();

    builder.append_block_param(header_block, cl::I64); // i
    builder.append_block_param(header_block, POINTER_TYPE); // accumulated result list

    builder.ins().jump(header_block, &[zero, init_result[0]]);

    builder.switch_to_block(header_block);
    let i = builder.block_params(header_block)[0];
    let acc_list = builder.block_params(header_block)[1];
    let in_range = builder.ins().icmp(IntCC::SignedLessThan, i, len);
    builder
        .ins()
        .brif(in_range, body_block, &[], exit_block, &[]);

    // Body: call closure, get inner list, push each element.
    builder.seal_block(body_block);
    builder.switch_to_block(body_block);
    let elem_vals = load_list_element(builder, list_ptr, i, elem_ty)?;
    let inner_list_vals = call_closure_ptr(
        builder,
        ctx,
        ir_module,
        closure_ptr,
        closure_vid,
        &elem_vals,
        state,
    )?;
    let inner_list = inner_list_vals[0];

    // Inner loop: for j in 0..inner_len, push to acc.
    let inner_len_vals = call_runtime(builder, ctx, ctx.runtime.list_length, &[inner_list]);
    let inner_len = inner_len_vals[0];
    let inner_header = builder.create_block();
    let inner_body = builder.create_block();
    let inner_exit = builder.create_block();

    builder.append_block_param(inner_header, cl::I64); // j
    builder.append_block_param(inner_header, POINTER_TYPE); // acc
    let zero2 = builder.ins().iconst(cl::I64, 0);
    builder.ins().jump(inner_header, &[zero2, acc_list]);

    builder.switch_to_block(inner_header);
    let j = builder.block_params(inner_header)[0];
    let inner_acc = builder.block_params(inner_header)[1];
    let inner_cond = builder.ins().icmp(IntCC::SignedLessThan, j, inner_len);
    builder
        .ins()
        .brif(inner_cond, inner_body, &[], inner_exit, &[]);

    builder.seal_block(inner_body);
    builder.switch_to_block(inner_body);
    let inner_elem = load_list_element(builder, inner_list, j, &out_elem_ty)?;
    let temp_ptr = store_to_temp(builder, &inner_elem, &out_elem_ty);
    let inner_es_val = builder.ins().iconst(cl::I64, out_es);
    let new_acc_vals = call_runtime(
        builder,
        ctx,
        ctx.runtime.list_push_raw,
        &[inner_acc, temp_ptr, inner_es_val],
    );
    let one = builder.ins().iconst(cl::I64, 1);
    let next_j = builder.ins().iadd(j, one);
    builder.ins().jump(inner_header, &[next_j, new_acc_vals[0]]);
    builder.seal_block(inner_header);

    builder.seal_block(inner_exit);
    builder.switch_to_block(inner_exit);
    let one2 = builder.ins().iconst(cl::I64, 1);
    let next_i = builder.ins().iadd(i, one2);
    builder.ins().jump(header_block, &[next_i, inner_acc]);
    builder.seal_block(header_block);

    builder.seal_block(exit_block);
    builder.switch_to_block(exit_block);
    // `acc_list` here is the block parameter of `header_block` — in SSA
    // semantics, it receives the updated accumulator from the back-edge
    // jump at the end of the outer loop body (line above: `jump(header_block,
    // &[next_i, inner_acc])`).  When the outer loop exits, `acc_list`
    // holds the final accumulated value, not a stale initial value.
    Ok(vec![acc_list])
}

/// `List.sortBy`: insertion sort using a closure comparator.
///
/// Uses O(n^2) insertion sort.  This is acceptable for small lists but
/// should be replaced with a more efficient algorithm (e.g., merge sort)
/// for production use.
///
/// ## CFG shape
///
/// ```text
///              ┌──────────────┐
///              │ entry         │  copy list via take(len)
///              └──────┬───────┘
///                     ▼
///           ┌─► outer_header ──► outer_exit ──► return copy
///           │    (i)        i<len?
///           │        │ yes
///           │        ▼
///           │    outer_body
///           │        │
///           │        ▼
///           │  ┌─► inner_header ──► inner_exit ─┐
///           │  │    (j)        j>0?              │
///           │  │        │ yes                     │
///           │  │        ▼                         │
///           │  │    inner_body                    │
///           │  │    compare [j-1] vs [j]         │
///           │  │        │                         │
///           │  │   ┌────┴────┐                    │
///           │  │   ▼         ▼                    │
///           │  │ swap    no_swap ──► inner_exit   │
///           │  │ j--        │                     │
///           │  └─┘          └─────────────────────┘
///           │                       i++
///           └───────────────────────┘
/// ```
pub(super) fn translate_list_sortby(
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

    // Copy the list: take(len) gives us a copy.
    let copy_vals = call_runtime(builder, ctx, ctx.runtime.list_take, &[list_ptr, len]);
    let copy_ptr = copy_vals[0];

    // Outer loop: for i in 1..len
    let outer_header = builder.create_block();
    let outer_body = builder.create_block();
    let outer_exit = builder.create_block();

    builder.append_block_param(outer_header, cl::I64); // i
    let one = builder.ins().iconst(cl::I64, 1);
    builder.ins().jump(outer_header, &[one]);

    builder.switch_to_block(outer_header);
    let i = builder.block_params(outer_header)[0];
    let outer_cond = builder.ins().icmp(IntCC::SignedLessThan, i, len);
    builder
        .ins()
        .brif(outer_cond, outer_body, &[], outer_exit, &[]);

    // Outer body: inner loop j = i down to 1
    builder.seal_block(outer_body);
    builder.switch_to_block(outer_body);
    let inner_header = builder.create_block();
    let inner_body = builder.create_block();
    let inner_exit = builder.create_block();

    builder.append_block_param(inner_header, cl::I64); // j
    builder.ins().jump(inner_header, &[i]);

    builder.switch_to_block(inner_header);
    let j = builder.block_params(inner_header)[0];
    let zero = builder.ins().iconst(cl::I64, 0);
    let j_gt_0 = builder.ins().icmp(IntCC::SignedGreaterThan, j, zero);
    builder.ins().brif(j_gt_0, inner_body, &[], inner_exit, &[]);

    // Inner body: compare copy[j-1] and copy[j], swap if needed.
    builder.seal_block(inner_body);
    builder.switch_to_block(inner_body);
    let one_inner = builder.ins().iconst(cl::I64, 1);
    let j_minus_1 = builder.ins().isub(j, one_inner);
    let elem_a = load_list_element(builder, copy_ptr, j_minus_1, elem_ty)?;
    let elem_b = load_list_element(builder, copy_ptr, j, elem_ty)?;
    let mut cmp_args = elem_a.clone();
    cmp_args.extend(&elem_b);
    let cmp_result = call_closure_ptr(
        builder,
        ctx,
        ir_module,
        closure_ptr,
        closure_vid,
        &cmp_args,
        state,
    )?;
    // If cmp_result > 0, swap.
    let should_swap = builder
        .ins()
        .icmp_imm(IntCC::SignedGreaterThan, cmp_result[0], 0);
    let swap_block = builder.create_block();
    let no_swap_block = builder.create_block();
    let elem_cl_types = crate::types::ir_type_to_cl(elem_ty);
    let elem_slots = slots_for_type(elem_ty);
    for cl_ty in &elem_cl_types {
        builder.append_block_param(swap_block, *cl_ty); // elem_a
    }
    for cl_ty in &elem_cl_types {
        builder.append_block_param(swap_block, *cl_ty); // elem_b
    }
    let mut swap_args: Vec<Value> = elem_a.clone();
    swap_args.extend(&elem_b);
    builder
        .ins()
        .brif(should_swap, swap_block, &swap_args, no_swap_block, &[]);

    // Swap: write elem_b to position j-1 and elem_a to position j.
    builder.seal_block(swap_block);
    builder.switch_to_block(swap_block);
    let swap_params = builder.block_params(swap_block).to_vec();
    let passed_a = &swap_params[..elem_slots];
    let passed_b = &swap_params[elem_slots..elem_slots * 2];
    let header_const = builder.ins().iconst(cl::I64, LIST_HEADER as i64);
    let data_base = builder.ins().iadd(copy_ptr, header_const);
    let es_val = builder.ins().iconst(cl::I64, es);
    let offset_jm1 = builder.ins().imul(j_minus_1, es_val);
    let offset_j = builder.ins().imul(j, es_val);
    store_list_element(builder, data_base, offset_jm1, passed_b, elem_ty);
    store_list_element(builder, data_base, offset_j, passed_a, elem_ty);
    let next_j = builder.ins().isub(j, one_inner);
    builder.ins().jump(inner_header, &[next_j]);
    builder.seal_block(inner_header);

    // No swap: stop inner loop.
    builder.seal_block(no_swap_block);
    builder.switch_to_block(no_swap_block);
    builder.ins().jump(inner_exit, &[]);

    builder.seal_block(inner_exit);
    builder.switch_to_block(inner_exit);
    let one_outer = builder.ins().iconst(cl::I64, 1);
    let next_i = builder.ins().iadd(i, one_outer);
    builder.ins().jump(outer_header, &[next_i]);
    builder.seal_block(outer_header);

    builder.seal_block(outer_exit);
    builder.switch_to_block(outer_exit);
    Ok(vec![copy_ptr])
}
