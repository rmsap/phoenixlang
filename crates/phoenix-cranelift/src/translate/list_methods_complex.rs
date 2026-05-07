//! Complex closure-based List method translations (flatMap, sortBy).
//!
//! These methods require nested Cranelift loops and are separated from the
//! simpler single-loop methods in `list_methods_closure.rs` for readability.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{Block, InstBuilder, StackSlot, StackSlotData, StackSlotKind, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::types::IrType;

use super::closure_call::{call_closure_ptr, closure_return_elem_type};
use super::helpers::call_runtime;
use super::layout::{LIST_HEADER, TypeLayout, elem_size_bytes};
use super::list_methods::{load_list_element, store_list_element, store_to_temp};
use super::{FuncState, get_val1};

/// Append `n` `i64` block params to `block`.
fn append_i64_params(builder: &mut FunctionBuilder, block: Block, n: usize) {
    for _ in 0..n {
        builder.append_block_param(block, cl::I64);
    }
}

/// Shared codegen state for `translate_list_sortby` helpers — the two
/// buffer-pointer stack slots plus a few constants reused in every
/// merge / drain block. Bundled into one struct so helpers don't
/// take a 10-argument signature.
struct SortCtx<'a> {
    src_slot: StackSlot,
    dst_slot: StackSlot,
    header_off: Value,
    es_val: Value,
    one: Value,
    elem_ty: &'a IrType,
}

/// Emit a drain loop that copies elements from `src[counter..bound]`
/// into `dst[k..]`, used by both the left-run drain (`bound = mid`,
/// `bound_param_idx = 2`) and the right-run drain (`bound = end`,
/// `bound_param_idx = 3`) of the merge body.
///
/// `entry_block` must already be created with 6 `i64` params —
/// `[start, width, mid, end, counter, k]` — and have at least one
/// predecessor edge emitted. `merge_done_block` (params `[start,
/// width]`) is the post-drain target. The src/dst pointers are
/// reloaded inside the body each iteration so the swap at
/// `width_next` is visible.
///
/// Emits the body and back-edge, then seals both blocks.
fn emit_drain_loop(
    builder: &mut FunctionBuilder,
    sort: &SortCtx<'_>,
    entry_block: Block,
    merge_done_block: Block,
    bound_param_idx: usize,
) -> Result<(), CompileError> {
    builder.switch_to_block(entry_block);
    let p = builder.block_params(entry_block).to_vec();
    let (start, width, mid, end, counter, k) = (p[0], p[1], p[2], p[3], p[4], p[5]);
    let bound = p[bound_param_idx];
    let counter_lt_bound = builder.ins().icmp(IntCC::SignedLessThan, counter, bound);

    let body = builder.create_block();
    append_i64_params(builder, body, 6);
    builder.ins().brif(
        counter_lt_bound,
        body,
        &[start, width, mid, end, counter, k],
        merge_done_block,
        &[start, width],
    );

    builder.seal_block(body);
    builder.switch_to_block(body);
    let bp = builder.block_params(body).to_vec();
    let (b_start, b_width, b_mid, b_end, b_counter, b_k) =
        (bp[0], bp[1], bp[2], bp[3], bp[4], bp[5]);
    let src_ptr = builder.ins().stack_load(cl::I64, sort.src_slot, 0);
    let val = load_list_element(builder, src_ptr, b_counter, sort.elem_ty)?;
    let dst_ptr = builder.ins().stack_load(cl::I64, sort.dst_slot, 0);
    let dst_base = builder.ins().iadd(dst_ptr, sort.header_off);
    let offset = builder.ins().imul(b_k, sort.es_val);
    store_list_element(builder, dst_base, offset, &val, sort.elem_ty);
    let next_counter = builder.ins().iadd(b_counter, sort.one);
    let next_k = builder.ins().iadd(b_k, sort.one);
    builder.ins().jump(
        entry_block,
        &[b_start, b_width, b_mid, b_end, next_counter, next_k],
    );
    builder.seal_block(entry_block);
    Ok(())
}

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
    let inner_list_vals =
        call_closure_ptr(builder, ctx, closure_ptr, closure_vid, &elem_vals, state)?;
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

/// `List.sortBy`: bottom-up iterative merge sort using a closure
/// comparator. **O(n log n)** worst case, replacing the Phase 2.2
/// O(n²) insertion sort.
///
/// ## Algorithm
///
/// Bottom-up iterative merge sort with two same-size buffers that
/// double-buffer (ping-pong) each width pass:
///
/// 1. Allocate `copy` (the list-take of the input) and `aux` (a fresh
///    list of the same shape). Both are GC-managed; the function pushes
///    a dedicated 2-slot shadow-stack frame on entry and roots `copy`
///    in slot 0 / `aux` in slot 1, so the comparator's allocations
///    (which can trigger threshold-driven collection) can't sweep
///    these intermediate buffers mid-sort. The frame is popped on the
///    single exit edge before returning. The frame pointer lives in a
///    Cranelift `StackSlot` instead of a block param so we don't have
///    to thread it through every block in the merge loop.
/// 2. Two more `StackSlot`s — `src_slot` and `dst_slot` — hold the
///    *current* source and destination buffer pointers. Initially
///    `src = copy`, `dst = aux`. Every width pass merges `src → dst`
///    and ends with a swap of `src_slot` / `dst_slot`, so the next
///    pass reads from what the previous pass just wrote and writes
///    into the now-stale buffer. No copyback step is needed.
/// 3. For width = 1, 2, 4, 8, … while width < len: for each pair of
///    adjacent runs `[start..mid)`, `[mid..end)`, merge them into
///    `dst[start..end]`. A run can be shorter than `width` at the
///    tail (`mid` and `end` are clamped to `len`); a single trailing
///    run with no pair is copied verbatim into `dst` by `drain_i`.
/// 4. Return whatever the final swap left in `src_slot` — that's the
///    most recently merged buffer. Both `copy` and `aux` are valid
///    list pointers, so the caller can root either via the function-
///    level shadow frame.
///
/// **Stability.** `cmp ≤ 0` keeps the left run element when ties
/// occur, so sortBy is stable — same contract as the Phase 2.2
/// insertion sort.
///
/// **Length 0 / 1 fast path.** `len < 2` skips merging entirely.
/// `src_slot` is initialized to `copy` before the trivial-length
/// branch, so `exit_block`'s `stack_load(src_slot)` returns `copy` on
/// that path.
///
/// ## CFG shape
///
/// Block params are listed in the order the implementation actually
/// passes them. The carry tuple in the merge body is
/// `(start, width, mid, end, i, j, k)`.
///
/// ```text
///   entry → push gc frame, set_root copy, src_slot = copy → trivial_check
///   trivial_check
///                ├─ len<2 → exit_block (pop frame, return src_slot)
///                └─ alloc_aux → width_header(width=1)
///   alloc_aux: alloc aux, set_root aux, dst_slot = aux, jump width_header(1)
///   width_header(width)
///                ├─ width≥len → after_all_widths → exit_block
///                └─ start_header(start=0, width)
///   start_header(start, width)
///                ├─ start≥len → width_next(width)
///                └─ merge_setup(start, width)
///   merge_setup(start, width)
///                → merge_loop(start, width, mid, end, i=start, j=mid, k=start)
///   merge_loop(start, width, mid, end, i, j, k)
///                ├─ i≥mid → drain_j(start, width, mid, end, j, k)
///                └─ check_j(start, width, mid, end, i, j, k)
///   check_j(start, width, mid, end, i, j, k)
///                ├─ j≥end → drain_i(start, width, mid, end, i, k)
///                └─ load src[i], src[j] + compare → take_a / take_b
///   take_a(start, width, mid, end, i, j, k, a)
///                → store dst[k]=a → merge_loop(...,i+1,j,k+1)
///   take_b(start, width, mid, end, i, j, k, b)
///                → store dst[k]=b → merge_loop(...,i,j+1,k+1)
///   drain_i(start, width, mid, end, i, k)
///                → store dst[k]=src[i] until i=mid → merge_done
///   drain_j(start, width, mid, end, j, k)
///                → store dst[k]=src[j] until j=end → merge_done
///   merge_done(start, width)
///                → start_header(start+2*width, width)
///   width_next(width)
///                → swap src_slot/dst_slot → width_header(width*2)
///   after_all_widths
///                → exit_block
///   exit_block → pop gc frame, return src_slot
/// ```
pub(super) fn translate_list_sortby(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    list_ptr: Value,
    elem_ty: &IrType,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let closure_vid = args[1];
    let closure_ptr = get_val1(state, closure_vid)?;
    let es = elem_size_bytes(elem_ty);

    let elem_layout = TypeLayout::of(elem_ty);
    let elem_cl_types = elem_layout.cl_types().to_vec();
    let elem_slots = elem_layout.slots();

    let len_vals = call_runtime(builder, ctx, ctx.runtime.list_length, &[list_ptr]);
    let len = len_vals[0];

    // ── Stack slots ────────────────────────────────────────────────
    // Three 8-byte slots, all 8-byte aligned (`align_shift = 3`):
    //   • frame_slot — the GC frame pointer (so any block can reload
    //     it without threading through block params).
    //   • src_slot / dst_slot — the *current* source and destination
    //     buffer pointers. The width loop swaps these at the end of
    //     each pass instead of copying `aux` back into `copy`.
    let frame_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let src_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let dst_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));

    // ── GC root frame ──────────────────────────────────────────────
    // Push a dedicated 2-slot shadow-stack frame so the comparator's
    // allocations can't sweep `copy` and `aux` mid-sort. The function-
    // level frame in `gc_roots.rs` only roots IR `ValueId`s; the two
    // intermediate buffers here exist only as Cranelift SSA values, so
    // they need their own frame.
    let n_roots = builder.ins().iconst(cl::I64, 2);
    let push_vals = call_runtime(builder, ctx, ctx.runtime.gc_push_frame, &[n_roots]);
    let frame_ptr = push_vals[0];
    builder.ins().stack_store(frame_ptr, frame_slot, 0);

    // Copy the input list — take(len) returns a new list with the same
    // contents. Root it into slot 0 immediately, before any subsequent
    // allocation can trigger a collection.
    let copy_vals = call_runtime(builder, ctx, ctx.runtime.list_take, &[list_ptr, len]);
    let copy_ptr = copy_vals[0];
    let slot0 = builder.ins().iconst(cl::I64, 0);
    call_runtime(
        builder,
        ctx,
        ctx.runtime.gc_set_root,
        &[frame_ptr, slot0, copy_ptr],
    );
    // src starts as `copy`. dst is set up after aux is allocated; on
    // the trivial path (`len < 2`) dst is never read.
    builder.ins().stack_store(copy_ptr, src_slot, 0);

    // Constants reused across blocks.
    let zero = builder.ins().iconst(cl::I64, 0);
    let one = builder.ins().iconst(cl::I64, 1);
    let two = builder.ins().iconst(cl::I64, 2);
    let es_val = builder.ins().iconst(cl::I64, es);
    let header_off = builder.ins().iconst(cl::I64, LIST_HEADER as i64);

    // ── Trivial-length fast path ───────────────────────────────────
    let trivial_check = builder.create_block();
    let alloc_aux = builder.create_block();
    let exit_block = builder.create_block();

    builder.ins().jump(trivial_check, &[]);

    builder.seal_block(trivial_check);
    builder.switch_to_block(trivial_check);
    let needs_sort = builder.ins().icmp(IntCC::SignedGreaterThan, len, one);
    builder
        .ins()
        .brif(needs_sort, alloc_aux, &[], exit_block, &[]);

    // ── Allocate the auxiliary buffer ──────────────────────────────
    builder.seal_block(alloc_aux);
    builder.switch_to_block(alloc_aux);
    let aux_alloc = call_runtime(builder, ctx, ctx.runtime.list_alloc, &[es_val, len]);
    let aux_ptr = aux_alloc[0];
    // Root aux into slot 1 before any further allocation. Reload the
    // frame pointer from the stack slot to avoid threading it through
    // block params.
    let frame_for_aux = builder.ins().stack_load(cl::I64, frame_slot, 0);
    let slot1 = builder.ins().iconst(cl::I64, 1);
    call_runtime(
        builder,
        ctx,
        ctx.runtime.gc_set_root,
        &[frame_for_aux, slot1, aux_ptr],
    );
    builder.ins().stack_store(aux_ptr, dst_slot, 0);

    // ── Outer loop: width = 1, 2, 4, … while width < len ───────────
    let width_header = builder.create_block();
    let after_all_widths = builder.create_block();
    builder.append_block_param(width_header, cl::I64); // width
    builder.ins().jump(width_header, &[one]);

    builder.switch_to_block(width_header);
    let width = builder.block_params(width_header)[0];
    let width_in_range = builder.ins().icmp(IntCC::SignedLessThan, width, len);
    let start_header = builder.create_block();
    builder.append_block_param(start_header, cl::I64); // start
    builder.append_block_param(start_header, cl::I64); // width (carried)
    builder.ins().brif(
        width_in_range,
        start_header,
        &[zero, width],
        after_all_widths,
        &[],
    );

    // ── start_header: walk pairs of runs ───────────────────────────
    builder.switch_to_block(start_header);
    let start = builder.block_params(start_header)[0];
    let cur_width = builder.block_params(start_header)[1];
    let start_in_range = builder.ins().icmp(IntCC::SignedLessThan, start, len);
    let width_next = builder.create_block();
    builder.append_block_param(width_next, cl::I64); // width
    let merge_setup = builder.create_block();
    builder.append_block_param(merge_setup, cl::I64); // start
    builder.append_block_param(merge_setup, cl::I64); // width
    builder.ins().brif(
        start_in_range,
        merge_setup,
        &[start, cur_width],
        width_next,
        &[cur_width],
    );

    // ── merge_setup: compute mid, end, kick off merge_loop ─────────
    builder.seal_block(merge_setup);
    builder.switch_to_block(merge_setup);
    let ms_start = builder.block_params(merge_setup)[0];
    let ms_width = builder.block_params(merge_setup)[1];
    // mid = min(start + width, len)
    let raw_mid = builder.ins().iadd(ms_start, ms_width);
    let mid_use_raw = builder.ins().icmp(IntCC::SignedLessThan, raw_mid, len);
    let mid = builder.ins().select(mid_use_raw, raw_mid, len);
    // end = min(start + 2*width, len)
    let two_w = builder.ins().imul(ms_width, two);
    let raw_end = builder.ins().iadd(ms_start, two_w);
    let end_use_raw = builder.ins().icmp(IntCC::SignedLessThan, raw_end, len);
    let end = builder.ins().select(end_use_raw, raw_end, len);

    // merge_loop block params: start, width, mid, end, i, j, k
    let merge_loop = builder.create_block();
    append_i64_params(builder, merge_loop, 7);
    builder.ins().jump(
        merge_loop,
        &[ms_start, ms_width, mid, end, ms_start, mid, ms_start],
    );

    // ── merge_loop: dispatch to drain_j / check_j ──────────────────
    builder.switch_to_block(merge_loop);
    let ml_p = builder.block_params(merge_loop).to_vec();
    let (ml_start, ml_width, ml_mid, ml_end, ml_i, ml_j, ml_k) = (
        ml_p[0], ml_p[1], ml_p[2], ml_p[3], ml_p[4], ml_p[5], ml_p[6],
    );
    let i_lt_mid = builder.ins().icmp(IntCC::SignedLessThan, ml_i, ml_mid);
    let check_j = builder.create_block();
    append_i64_params(builder, check_j, 7);
    let drain_j = builder.create_block();
    append_i64_params(builder, drain_j, 6);
    builder.ins().brif(
        i_lt_mid,
        check_j,
        &[ml_start, ml_width, ml_mid, ml_end, ml_i, ml_j, ml_k],
        drain_j,
        &[ml_start, ml_width, ml_mid, ml_end, ml_j, ml_k],
    );

    // ── check_j: if j >= end, drain_i; else load + compare ─────────
    builder.switch_to_block(check_j);
    let cj_p = builder.block_params(check_j).to_vec();
    let (cj_start, cj_width, cj_mid, cj_end, cj_i, cj_j, cj_k) = (
        cj_p[0], cj_p[1], cj_p[2], cj_p[3], cj_p[4], cj_p[5], cj_p[6],
    );
    let j_lt_end = builder.ins().icmp(IntCC::SignedLessThan, cj_j, cj_end);
    let do_compare = builder.create_block();
    append_i64_params(builder, do_compare, 7);
    let drain_i = builder.create_block();
    append_i64_params(builder, drain_i, 6);
    builder.ins().brif(
        j_lt_end,
        do_compare,
        &[cj_start, cj_width, cj_mid, cj_end, cj_i, cj_j, cj_k],
        drain_i,
        &[cj_start, cj_width, cj_mid, cj_end, cj_i, cj_k],
    );

    // ── do_compare: load src[i], src[j]; call comparator; branch ───
    builder.seal_block(do_compare);
    builder.switch_to_block(do_compare);
    let dc_p = builder.block_params(do_compare).to_vec();
    let (dc_start, dc_width, dc_mid, dc_end, dc_i, dc_j, dc_k) = (
        dc_p[0], dc_p[1], dc_p[2], dc_p[3], dc_p[4], dc_p[5], dc_p[6],
    );
    let src_ptr_dc = builder.ins().stack_load(cl::I64, src_slot, 0);
    let elem_a = load_list_element(builder, src_ptr_dc, dc_i, elem_ty)?;
    let elem_b = load_list_element(builder, src_ptr_dc, dc_j, elem_ty)?;
    let mut cmp_args = elem_a.clone();
    cmp_args.extend(&elem_b);
    let cmp_result = call_closure_ptr(builder, ctx, closure_ptr, closure_vid, &cmp_args, state)?;
    // cmp <= 0 → keep `a` (left element); cmp > 0 → take `b`.
    // Stable: ties prefer the left run.
    let take_b = builder
        .ins()
        .icmp_imm(IntCC::SignedGreaterThan, cmp_result[0], 0);

    // take_a / take_b receive: start, width, mid, end, i, j, k, then elem slots.
    let take_a_block = builder.create_block();
    let take_b_block = builder.create_block();
    append_i64_params(builder, take_a_block, 7);
    for &cl_ty in &elem_cl_types {
        builder.append_block_param(take_a_block, cl_ty);
    }
    append_i64_params(builder, take_b_block, 7);
    for &cl_ty in &elem_cl_types {
        builder.append_block_param(take_b_block, cl_ty);
    }
    let mut take_a_args: Vec<Value> = vec![dc_start, dc_width, dc_mid, dc_end, dc_i, dc_j, dc_k];
    take_a_args.extend(&elem_a);
    let mut take_b_args: Vec<Value> = vec![dc_start, dc_width, dc_mid, dc_end, dc_i, dc_j, dc_k];
    take_b_args.extend(&elem_b);
    builder.ins().brif(
        take_b,
        take_b_block,
        &take_b_args,
        take_a_block,
        &take_a_args,
    );

    // ── take_a: store a into dst[k]; jump merge_loop with i+1, k+1 ──
    builder.seal_block(take_a_block);
    builder.switch_to_block(take_a_block);
    let ta_p = builder.block_params(take_a_block).to_vec();
    let (ta_start, ta_width, ta_mid, ta_end, ta_i, ta_j, ta_k) = (
        ta_p[0], ta_p[1], ta_p[2], ta_p[3], ta_p[4], ta_p[5], ta_p[6],
    );
    let ta_vals: Vec<Value> = ta_p[7..7 + elem_slots].to_vec();
    let dst_ptr_ta = builder.ins().stack_load(cl::I64, dst_slot, 0);
    let dst_base_ta = builder.ins().iadd(dst_ptr_ta, header_off);
    let ta_offset_k = builder.ins().imul(ta_k, es_val);
    store_list_element(builder, dst_base_ta, ta_offset_k, &ta_vals, elem_ty);
    let ta_next_i = builder.ins().iadd(ta_i, one);
    let ta_next_k = builder.ins().iadd(ta_k, one);
    builder.ins().jump(
        merge_loop,
        &[
            ta_start, ta_width, ta_mid, ta_end, ta_next_i, ta_j, ta_next_k,
        ],
    );

    // ── take_b: store b into dst[k]; jump merge_loop with j+1, k+1 ──
    builder.seal_block(take_b_block);
    builder.switch_to_block(take_b_block);
    let tb_p = builder.block_params(take_b_block).to_vec();
    let (tb_start, tb_width, tb_mid, tb_end, tb_i, tb_j, tb_k) = (
        tb_p[0], tb_p[1], tb_p[2], tb_p[3], tb_p[4], tb_p[5], tb_p[6],
    );
    let tb_vals: Vec<Value> = tb_p[7..7 + elem_slots].to_vec();
    let dst_ptr_tb = builder.ins().stack_load(cl::I64, dst_slot, 0);
    let dst_base_tb = builder.ins().iadd(dst_ptr_tb, header_off);
    let tb_offset_k = builder.ins().imul(tb_k, es_val);
    store_list_element(builder, dst_base_tb, tb_offset_k, &tb_vals, elem_ty);
    let tb_next_j = builder.ins().iadd(tb_j, one);
    let tb_next_k = builder.ins().iadd(tb_k, one);
    builder.ins().jump(
        merge_loop,
        &[
            tb_start, tb_width, tb_mid, tb_end, tb_i, tb_next_j, tb_next_k,
        ],
    );

    // The check_j and merge_loop blocks both flow back into themselves
    // via the take_a / take_b → merge_loop edges, so seal them now.
    builder.seal_block(check_j);
    builder.seal_block(merge_loop);

    // ── drain_i / drain_j: copy remaining run elements ─────────────
    let merge_done = builder.create_block();
    builder.append_block_param(merge_done, cl::I64); // start
    builder.append_block_param(merge_done, cl::I64); // width
    let sort = SortCtx {
        src_slot,
        dst_slot,
        header_off,
        es_val,
        one,
        elem_ty,
    };
    // drain_i bound = mid (param idx 2). drain_j bound = end (idx 3).
    emit_drain_loop(builder, &sort, drain_i, merge_done, 2)?;
    emit_drain_loop(builder, &sort, drain_j, merge_done, 3)?;

    // ── merge_done: advance to next pair of runs ───────────────────
    // merge_done has predecessors drain_i (post-loop) and drain_j
    // (post-loop). Both edges are emitted by emit_drain_loop above.
    builder.seal_block(merge_done);
    builder.switch_to_block(merge_done);
    let md_start = builder.block_params(merge_done)[0];
    let md_width = builder.block_params(merge_done)[1];
    let md_two_w = builder.ins().imul(md_width, two);
    let md_next_start = builder.ins().iadd(md_start, md_two_w);
    builder.ins().jump(start_header, &[md_next_start, md_width]);
    builder.seal_block(start_header);

    // ── width_next: swap src/dst slots, double width, loop back ────
    // The just-completed pass merged `src → dst`. Swap so the next
    // pass reads from the freshly-merged buffer and overwrites the
    // now-stale one. Both `copy` and `aux` remain rooted in the GC
    // frame — only the "which is current" pointers in the local
    // stack slots flip.
    builder.seal_block(width_next);
    builder.switch_to_block(width_next);
    let wn_width = builder.block_params(width_next)[0];
    let old_src = builder.ins().stack_load(cl::I64, src_slot, 0);
    let old_dst = builder.ins().stack_load(cl::I64, dst_slot, 0);
    builder.ins().stack_store(old_dst, src_slot, 0);
    builder.ins().stack_store(old_src, dst_slot, 0);
    let wn_next_width = builder.ins().imul(wn_width, two);
    builder.ins().jump(width_header, &[wn_next_width]);
    builder.seal_block(width_header);

    // ── Exit ───────────────────────────────────────────────────────
    builder.seal_block(after_all_widths);
    builder.switch_to_block(after_all_widths);
    builder.ins().jump(exit_block, &[]);

    builder.seal_block(exit_block);
    builder.switch_to_block(exit_block);
    // Load the final result *before* popping our GC frame — after pop,
    // neither `copy` nor `aux` is rooted by us; the caller's
    // `maybe_set_root` will re-root the returned pointer before the
    // next allocation. The result is whatever `src_slot` points to
    // (initially `copy`; after at least one width pass, the buffer
    // that received the most recent merged data, courtesy of the swap
    // in `width_next`).
    let result = builder.ins().stack_load(cl::I64, src_slot, 0);
    let frame_for_pop = builder.ins().stack_load(cl::I64, frame_slot, 0);
    call_runtime(builder, ctx, ctx.runtime.gc_pop_frame, &[frame_for_pop]);
    Ok(vec![result])
}
