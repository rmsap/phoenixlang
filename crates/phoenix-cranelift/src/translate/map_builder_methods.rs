//! Cranelift codegen for `MapBuilder<K, V>`.
//!
//! Mirrors `list_builder_methods.rs`. Three methods:
//! - `MapBuilder.alloc` — key/value sizes derived from the call's
//!   result type `IrType::MapBuilderRef(K, V)`.
//! - `MapBuilder.set` — routes to
//!   `phx_map_builder_set(handle, key_ptr, val_ptr, ks, vs)`.
//! - `MapBuilder.freeze` — routes to `phx_map_builder_freeze(handle)`,
//!   which hands the accumulated `(key, value)` pairs to
//!   `phx_map_from_pairs` for a single O(n) hash build (last-wins
//!   dedup on duplicate keys — see the runtime module docstring for
//!   why the builder doesn't dedup on `set` itself).

use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

use crate::context::CompileContext;
use crate::error::CompileError;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::types::IrType;

use super::helpers::call_runtime;
use super::layout::elem_size_bytes;
use super::list_methods::store_to_temp;
use super::{FuncState, get_val, get_val1};

/// Extract `(K, V)` from a `MapBuilderRef(K, V)` receiver, falling back
/// to argument types when monomorphization erased the placeholders.
/// See `list_builder_methods::builder_elem_type` for the full
/// rationale.
fn builder_kv_types(
    state: &FuncState,
    recv_id: ValueId,
    key_arg: Option<ValueId>,
    val_arg: Option<ValueId>,
) -> Result<(IrType, IrType), CompileError> {
    let ty = state
        .type_map
        .get(&recv_id)
        .ok_or_else(|| CompileError::new("unknown type for MapBuilder receiver"))?;
    let (mut k_ty, mut v_ty) = match ty {
        IrType::MapBuilderRef(k, v) => (k.as_ref().clone(), v.as_ref().clone()),
        _ => {
            return Err(CompileError::new(
                "MapBuilder method called on non-builder type",
            ));
        }
    };
    if k_ty.is_generic_placeholder()
        && let Some(arg) = key_arg
        && let Some(arg_ty) = state.type_map.get(&arg)
    {
        k_ty = arg_ty.clone();
    }
    if v_ty.is_generic_placeholder()
        && let Some(arg) = val_arg
        && let Some(arg_ty) = state.type_map.get(&arg)
    {
        v_ty = arg_ty.clone();
    }
    Ok((k_ty, v_ty))
}

pub(super) fn translate_map_builder_method(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    method: &str,
    args: &[ValueId],
    state: &FuncState,
    result_type: &IrType,
) -> Result<Vec<Value>, CompileError> {
    match method {
        "alloc" => {
            let (k_ty, v_ty) = match result_type {
                IrType::MapBuilderRef(k, v) => (k.as_ref().clone(), v.as_ref().clone()),
                _ => {
                    return Err(CompileError::new(
                        "MapBuilder.alloc: result type is not MapBuilderRef",
                    ));
                }
            };
            let ks = builder.ins().iconst(cl::I64, elem_size_bytes(&k_ty));
            let vs = builder.ins().iconst(cl::I64, elem_size_bytes(&v_ty));
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.map_builder_alloc,
                &[ks, vs],
            ))
        }
        "set" => {
            // args[0] = receiver, args[1] = key, args[2] = value.
            let handle = get_val1(state, args[0])?;
            let (k_ty, v_ty) = builder_kv_types(state, args[0], Some(args[1]), Some(args[2]))?;
            let k_vals = get_val(state, args[1])?;
            let v_vals = get_val(state, args[2])?;
            let k_ptr = store_to_temp(builder, &k_vals, &k_ty);
            let v_ptr = store_to_temp(builder, &v_vals, &v_ty);
            let ks = builder.ins().iconst(cl::I64, elem_size_bytes(&k_ty));
            let vs = builder.ins().iconst(cl::I64, elem_size_bytes(&v_ty));
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.map_builder_set,
                &[handle, k_ptr, v_ptr, ks, vs],
            ))
        }
        "freeze" => {
            let handle = get_val1(state, args[0])?;
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.map_builder_freeze,
                &[handle],
            ))
        }
        _ => Err(CompileError::new(format!(
            "MapBuilder.{method}: not yet supported in compiled mode"
        ))),
    }
}
