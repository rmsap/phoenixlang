//! Translation of `Map.*` builtin method calls to Cranelift IR.
//!
//! All map methods delegate to runtime functions in `phoenix-runtime`.
//! `Map.get` returns `Option<V>`, so it uses the enum helpers for
//! wrapping the result.

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

use super::enum_helpers::{build_option_none, build_option_some};
use super::helpers::{call_runtime, load_fat_value, slots_for_type, store_fat_value};
use super::layout::{MAP_HEADER, elem_size_bytes};
use super::list_methods::store_to_temp;
use super::{FuncState, get_val, get_val1};
use phoenix_ir::instruction::Op;

/// Get the key and value types of a map from the receiver's type.
///
/// If the receiver has generic placeholder types (from empty map literals
/// whose concrete types were not propagated by sema), tries to resolve
/// them from the method's key/value argument types.
fn map_key_val_types(
    state: &FuncState,
    recv_id: ValueId,
    args: &[ValueId],
    method: &str,
) -> Result<(IrType, IrType), CompileError> {
    let map_ty = state
        .type_map
        .get(&recv_id)
        .ok_or_else(|| CompileError::new("unknown type for map receiver"))?;
    let (key_ty, val_ty) = match map_ty {
        IrType::MapRef(k, v) => (k.as_ref().clone(), v.as_ref().clone()),
        _ => return Err(CompileError::new("Map method called on non-map type")),
    };
    // Resolve generic placeholders from method argument types.
    let key_ty = if key_ty.is_generic_placeholder() {
        // Methods with a key argument: get(key), set(key, val), contains(key), remove(key)
        if args.len() > 1 {
            state.type_map.get(&args[1]).cloned().unwrap_or(key_ty)
        } else {
            key_ty
        }
    } else {
        key_ty
    };
    let val_ty = if val_ty.is_generic_placeholder() {
        // set(key, val) has the value at args[2]
        if method == "set" && args.len() > 2 {
            state.type_map.get(&args[2]).cloned().unwrap_or(val_ty)
        } else if method == "get" {
            // For get(key), try to resolve the value type from the
            // MapAlloc's result_type which is already in the type_map
            // for the receiver. Without this, Map<K, String>.get()
            // would treat the value as a single pointer instead of a
            // fat (ptr, len) pair.
            state
                .type_map
                .get(&recv_id)
                .and_then(|t| match t {
                    IrType::MapRef(_, v) if !v.is_generic_placeholder() => Some(v.as_ref().clone()),
                    _ => None,
                })
                .unwrap_or(val_ty)
        } else {
            val_ty
        }
    } else {
        val_ty
    };
    Ok((key_ty, val_ty))
}

/// Translate a `Map.*` builtin method call.
///
/// `args[0]` is the map receiver and `args[1..]` are the method arguments.
pub(super) fn translate_map_method(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    method: &str,
    args: &[ValueId],
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let map_ptr = get_val1(state, args[0])?;
    let (key_ty, val_ty) = map_key_val_types(state, args[0], args, method)?;

    match method {
        "length" => Ok(call_runtime(
            builder,
            ctx,
            ctx.runtime.map_length,
            &[map_ptr],
        )),
        "get" => {
            // get(key) -> Option<V>: returns Some(v) if found, None otherwise.
            let key_vals = get_val(state, args[1])?;
            let ks = elem_size_bytes(&key_ty);
            let temp_key = store_to_temp(builder, &key_vals, &key_ty);
            let ks_val = builder.ins().iconst(cl::I64, ks);
            let result_ptr_vals = call_runtime(
                builder,
                ctx,
                ctx.runtime.map_get_raw,
                &[map_ptr, temp_key, ks_val],
            );
            let val_ptr = result_ptr_vals[0];

            let null = builder.ins().iconst(cl::I64, 0);
            let is_null = builder.ins().icmp(IntCC::Equal, val_ptr, null);

            let found_block = builder.create_block();
            let not_found_block = builder.create_block();
            let merge_block = builder.create_block();
            builder.append_block_param(merge_block, POINTER_TYPE);

            builder
                .ins()
                .brif(is_null, not_found_block, &[], found_block, &[]);

            // Found: load value, wrap in Some.
            builder.seal_block(found_block);
            builder.switch_to_block(found_block);
            let loaded_val = load_fat_value(builder, &val_ty, val_ptr, 0)?;
            let some_ptr = build_option_some(builder, ctx, &loaded_val, &val_ty, ir_module)?;
            builder.ins().jump(merge_block, &[some_ptr]);

            // Not found: return None.
            builder.seal_block(not_found_block);
            builder.switch_to_block(not_found_block);
            let none_ptr = build_option_none(builder, ctx, ir_module)?;
            builder.ins().jump(merge_block, &[none_ptr]);

            builder.seal_block(merge_block);
            builder.switch_to_block(merge_block);
            Ok(builder.block_params(merge_block).to_vec())
        }
        "contains" => {
            let key_vals = get_val(state, args[1])?;
            let ks = elem_size_bytes(&key_ty);
            let temp_key = store_to_temp(builder, &key_vals, &key_ty);
            let ks_val = builder.ins().iconst(cl::I64, ks);
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.map_contains,
                &[map_ptr, temp_key, ks_val],
            ))
        }
        "set" => {
            let key_vals = get_val(state, args[1])?;
            let val_vals = get_val(state, args[2])?;
            let ks = elem_size_bytes(&key_ty);
            let vs = elem_size_bytes(&val_ty);
            let temp_key = store_to_temp(builder, &key_vals, &key_ty);
            let temp_val = store_to_temp(builder, &val_vals, &val_ty);
            let ks_val = builder.ins().iconst(cl::I64, ks);
            let vs_val = builder.ins().iconst(cl::I64, vs);
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.map_set_raw,
                &[map_ptr, temp_key, temp_val, ks_val, vs_val],
            ))
        }
        "remove" => {
            let key_vals = get_val(state, args[1])?;
            let ks = elem_size_bytes(&key_ty);
            let temp_key = store_to_temp(builder, &key_vals, &key_ty);
            let ks_val = builder.ins().iconst(cl::I64, ks);
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.map_remove_raw,
                &[map_ptr, temp_key, ks_val],
            ))
        }
        "keys" => Ok(call_runtime(builder, ctx, ctx.runtime.map_keys, &[map_ptr])),
        "values" => Ok(call_runtime(
            builder,
            ctx,
            ctx.runtime.map_values,
            &[map_ptr],
        )),
        _ => Err(CompileError::new(format!(
            "map method '{method}' not yet supported in compiled mode"
        ))),
    }
}

// ── MapAlloc ────────────────────────────────────────────────────────

/// Translate a `MapAlloc` operation.
///
/// Calls `phx_map_alloc(key_size, val_size, count)`, then stores each
/// key-value pair into the data region.
pub(super) fn translate_map_alloc(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    op: &Op,
    result_type: &IrType,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    let Op::MapAlloc(entries) = op else {
        unreachable!()
    };

    let (key_ty, val_ty) = match result_type {
        IrType::MapRef(k, v) => (k.as_ref(), v.as_ref()),
        _ => return Err(CompileError::new("MapAlloc result type is not MapRef")),
    };

    let ks = elem_size_bytes(key_ty);
    let vs = elem_size_bytes(val_ty);
    let count = entries.len() as i64;

    let ks_val = builder.ins().iconst(cl::I64, ks);
    let vs_val = builder.ins().iconst(cl::I64, vs);
    let count_val = builder.ins().iconst(cl::I64, count);
    let map_ptr = call_runtime(
        builder,
        ctx,
        ctx.runtime.map_alloc,
        &[ks_val, vs_val, count_val],
    );
    let ptr = map_ptr[0];

    let key_slots = slots_for_type(key_ty);
    let val_slots = slots_for_type(val_ty);

    // Store each key-value pair.
    for (i, (k_vid, v_vid)) in entries.iter().enumerate() {
        let k_vals = get_val(state, *k_vid)?;
        let key_slot = MAP_HEADER as usize / super::layout::SLOT_SIZE + i * (key_slots + val_slots);
        store_fat_value(builder, &k_vals, key_ty, ptr, key_slot);

        let v_vals = get_val(state, *v_vid)?;
        let val_slot = key_slot + key_slots;
        store_fat_value(builder, &v_vals, val_ty, ptr, val_slot);
    }

    Ok(vec![ptr])
}
