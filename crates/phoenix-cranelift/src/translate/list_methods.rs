//! Translation of `List.*` builtin method calls to Cranelift IR.
//!
//! Simple methods (length, get, push, contains, take, drop) delegate to
//! runtime functions in `phoenix-runtime`.  Closure-based methods (map,
//! filter, find, any, all, reduce, flatMap, sortBy) are compiled inline
//! as Cranelift loops that call the user's closure on each element.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{self, InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::enum_helpers::{build_option_none, build_option_some};
use super::helpers::call_runtime;
use super::layout::{LIST_HEADER, SLOT_SIZE, TypeLayout, elem_size_bytes};
use super::{FuncState, get_val, get_val1};
use phoenix_ir::instruction::Op;

/// Get the element type of a list from the receiver's type in the state.
/// Extract the element type `T` from a `List<T>` receiver's IR type.
fn list_elem_type(state: &FuncState, recv_id: ValueId) -> Result<IrType, CompileError> {
    let list_ty = state
        .type_map
        .get(&recv_id)
        .ok_or_else(|| CompileError::new("unknown type for list receiver"))?;
    match list_ty {
        IrType::ListRef(t) => Ok(t.as_ref().clone()),
        _ => Err(CompileError::new("List method called on non-list type")),
    }
}

/// Store a value to a temporary stack location and return the pointer.
///
/// Used when we need to pass a value by pointer to a runtime function
/// (e.g., `phx_list_push_raw` which takes an element pointer).
///
/// Each call creates a new `ExplicitSlot`.  In hot loops (e.g., the inner
/// loop of `flatMap`), this may produce many stack slots.  Cranelift
/// collapses stack slots with non-overlapping live ranges during regalloc,
/// so the actual stack usage should not grow unboundedly.
pub(super) fn store_to_temp(builder: &mut FunctionBuilder, vals: &[Value], ty: &IrType) -> Value {
    let layout = TypeLayout::of(ty);
    let ss = builder.create_sized_stack_slot(ir::StackSlotData::new(
        ir::StackSlotKind::ExplicitSlot,
        layout.size_bytes() as u32,
        0,
    ));
    let addr = builder.ins().stack_addr(POINTER_TYPE, ss, 0);
    layout.store(builder, addr, 0, vals);
    addr
}

/// Load a value from the list data region at a dynamic index.
///
/// Computes `data_ptr = list_ptr + LIST_HEADER + index * elem_size`,
/// then loads the value from that address.
pub(super) fn load_list_element(
    builder: &mut FunctionBuilder,
    list_ptr: Value,
    index: Value,
    elem_ty: &IrType,
) -> Result<Vec<Value>, CompileError> {
    let es = elem_size_bytes(elem_ty);
    let header = builder.ins().iconst(cl::I64, LIST_HEADER as i64);
    let base = builder.ins().iadd(list_ptr, header);
    let es_val = builder.ins().iconst(cl::I64, es);
    let offset = builder.ins().imul(index, es_val);
    let elem_addr = builder.ins().iadd(base, offset);
    Ok(TypeLayout::of(elem_ty).load(builder, elem_addr, 0))
}

/// Store a value into the list data region at a dynamic byte offset from base.
pub(super) fn store_list_element(
    builder: &mut FunctionBuilder,
    base: Value,
    byte_offset: Value,
    vals: &[Value],
    elem_ty: &IrType,
) {
    let addr = builder.ins().iadd(base, byte_offset);
    TypeLayout::of(elem_ty).store(builder, addr, 0, vals);
}

/// Translate a `List.*` builtin method call.
///
/// `args[0]` is the list receiver and `args[1..]` are the method arguments.
pub(super) fn translate_list_method(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    method: &str,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let list_ptr = get_val1(state, args[0])?;
    let elem_ty = list_elem_type(state, args[0])?;

    match method {
        "length" => Ok(call_runtime(
            builder,
            ctx,
            ctx.runtime.list_length,
            &[list_ptr],
        )),
        "get" => {
            let index = get_val1(state, args[1])?;
            let elem_ptr = call_runtime(builder, ctx, ctx.runtime.list_get_raw, &[list_ptr, index]);
            Ok(TypeLayout::of(&elem_ty).load(builder, elem_ptr[0], 0))
        }
        "push" => {
            let elem_vals = get_val(state, args[1])?;
            let es = elem_size_bytes(&elem_ty);
            let temp_ptr = store_to_temp(builder, &elem_vals, &elem_ty);
            let es_val = builder.ins().iconst(cl::I64, es);
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.list_push_raw,
                &[list_ptr, temp_ptr, es_val],
            ))
        }
        "contains" => {
            let elem_vals = get_val(state, args[1])?;
            let es = elem_size_bytes(&elem_ty);
            let temp_ptr = store_to_temp(builder, &elem_vals, &elem_ty);
            let es_val = builder.ins().iconst(cl::I64, es);
            let is_float = builder
                .ins()
                .iconst(ir::types::I8, if elem_ty == IrType::F64 { 1 } else { 0 });
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.list_contains,
                &[list_ptr, temp_ptr, es_val, is_float],
            ))
        }
        "first" | "last" => {
            translate_list_first_last(builder, ctx, ir_module, list_ptr, &elem_ty, method)
        }
        "take" => {
            let n = get_val1(state, args[1])?;
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.list_take,
                &[list_ptr, n],
            ))
        }
        "drop" => {
            let n = get_val1(state, args[1])?;
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.list_drop,
                &[list_ptr, n],
            ))
        }
        "map" => super::list_methods_closure::translate_list_map(
            builder, ctx, list_ptr, &elem_ty, args, state,
        ),
        "filter" => super::list_methods_closure::translate_list_filter(
            builder, ctx, list_ptr, &elem_ty, args, state,
        ),
        "find" => super::list_methods_closure::translate_list_find(
            builder, ctx, ir_module, list_ptr, &elem_ty, args, state,
        ),
        "any" => super::list_methods_closure::translate_list_any(
            builder, ctx, list_ptr, &elem_ty, args, state,
        ),
        "all" => super::list_methods_closure::translate_list_all(
            builder, ctx, list_ptr, &elem_ty, args, state,
        ),
        "reduce" => super::list_methods_closure::translate_list_reduce(
            builder, ctx, list_ptr, &elem_ty, args, state,
        ),
        "flatMap" => super::list_methods_complex::translate_list_flatmap(
            builder, ctx, list_ptr, &elem_ty, args, state,
        ),
        "sortBy" => super::list_methods_complex::translate_list_sortby(
            builder, ctx, list_ptr, &elem_ty, args, state,
        ),
        _ => Err(CompileError::new(format!(
            "list method '{method}' not yet supported in compiled mode"
        ))),
    }
}

// ── first/last ───────────────────────────────────────────────────────

/// `List.first` / `List.last`: return `Option<T>`.
fn translate_list_first_last(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    list_ptr: Value,
    elem_ty: &IrType,
    method: &str,
) -> Result<Vec<Value>, CompileError> {
    let len_vals = call_runtime(builder, ctx, ctx.runtime.list_length, &[list_ptr]);
    let len = len_vals[0];
    let zero = builder.ins().iconst(cl::I64, 0);
    let is_empty = builder.ins().icmp(IntCC::Equal, len, zero);

    let some_block = builder.create_block();
    let none_block = builder.create_block();
    let merge_block = builder.create_block();
    builder.append_block_param(merge_block, POINTER_TYPE);

    builder
        .ins()
        .brif(is_empty, none_block, &[], some_block, &[]);

    builder.seal_block(some_block);
    builder.switch_to_block(some_block);
    let idx = if method == "first" {
        builder.ins().iconst(cl::I64, 0)
    } else {
        let one = builder.ins().iconst(cl::I64, 1);
        builder.ins().isub(len, one)
    };
    let elem_vals = load_list_element(builder, list_ptr, idx, elem_ty)?;
    let some_ptr = build_option_some(builder, ctx, &elem_vals, elem_ty, ir_module)?;
    builder.ins().jump(merge_block, &[some_ptr]);

    builder.seal_block(none_block);
    builder.switch_to_block(none_block);
    let none_ptr = build_option_none(builder, ctx, ir_module)?;
    builder.ins().jump(merge_block, &[none_ptr]);

    builder.seal_block(merge_block);
    builder.switch_to_block(merge_block);
    let result = builder.block_params(merge_block)[0];
    Ok(vec![result])
}

// ── ListAlloc ───────────────────────────────────────────────────────

/// Translate a `ListAlloc` operation.
///
/// Calls `phx_list_alloc(elem_size, count)`, then stores each element
/// into the data region at the correct offset.
pub(super) fn translate_list_alloc(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    op: &Op,
    result_type: &IrType,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let Op::ListAlloc(elements) = op else {
        ice!("translate_list_alloc dispatched on non-ListAlloc op: {op:?}")
    };

    let elem_ty = match result_type {
        IrType::ListRef(t) => t.as_ref(),
        _ => return Err(CompileError::new("ListAlloc result type is not ListRef")),
    };

    let es = elem_size_bytes(elem_ty);
    let count = elements.len() as i64;

    let es_val = builder.ins().iconst(cl::I64, es);
    let count_val = builder.ins().iconst(cl::I64, count);
    let list_ptr = call_runtime(builder, ctx, ctx.runtime.list_alloc, &[es_val, count_val]);
    let ptr = list_ptr[0];

    // Store each element into the data region.
    let elem_layout = TypeLayout::of(elem_ty);
    for (i, vid) in elements.iter().enumerate() {
        let vals = get_val(state, *vid)?;
        let slot = LIST_HEADER as usize / SLOT_SIZE + i * elem_layout.slots();
        elem_layout.store(builder, ptr, slot, &vals);
    }

    Ok(vec![ptr])
}
