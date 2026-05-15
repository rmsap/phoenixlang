//! Cranelift codegen for `ListBuilder<T>`.
//!
//! Three methods dispatch here:
//! - `ListBuilder.alloc` — emitted by IR lowering for the
//!   `List.builder()` static-method shape; element size is derived
//!   from the call's result `IrType::ListBuilderRef(T)`.
//! - `ListBuilder.push` — emitted for `b.push(v)`. Routes to the
//!   runtime's `phx_list_builder_push(handle, elem_ptr, elem_size)`.
//!   The value is spilled to a stack slot so the runtime can read it
//!   by pointer (matches the `List.push` pattern in
//!   [`super::list_methods::translate_list_method`]).
//! - `ListBuilder.freeze` — emitted for `b.freeze()`. Routes to
//!   `phx_list_builder_freeze(handle) -> list_ptr`. The runtime
//!   memcpys the used portion of the buffer into a fresh `List<T>`.
//!
//! Use-after-freeze is enforced **at the runtime**: every method
//! checks the handle's `frozen` flag and aborts via `runtime_abort`
//! if set. Static enforcement is decision G's deferred linearity
//! story.

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

/// Extract `T` from a `ListBuilderRef(T)` IR type.
///
/// Falls back to `fallback`'s recorded type when the receiver's element
/// type is the `GENERIC_PLACEHOLDER` sentinel left behind by
/// monomorphization for a builder constructed where sema couldn't pin
/// `T` (e.g. the constructor expression `List.builder()` carries
/// `TypeVar(T)` regardless of the let-binding annotation). Same pattern
/// as `map_methods::map_key_val_types` for empty-map-literal receivers.
fn builder_elem_type(
    state: &FuncState,
    recv_id: ValueId,
    fallback: Option<ValueId>,
) -> Result<IrType, CompileError> {
    let ty = state
        .type_map
        .get(&recv_id)
        .ok_or_else(|| CompileError::new("unknown type for ListBuilder receiver"))?;
    let elem_ty = match ty {
        IrType::ListBuilderRef(t) => t.as_ref().clone(),
        _ => {
            return Err(CompileError::new(
                "ListBuilder method called on non-builder type",
            ));
        }
    };
    if elem_ty.is_generic_placeholder()
        && let Some(arg) = fallback
        && let Some(arg_ty) = state.type_map.get(&arg)
    {
        return Ok(arg_ty.clone());
    }
    Ok(elem_ty)
}

pub(super) fn translate_list_builder_method(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    method: &str,
    args: &[ValueId],
    state: &FuncState,
    result_type: &IrType,
) -> Result<Vec<Value>, CompileError> {
    match method {
        "alloc" => {
            // `args` is empty for the static-method form. Element
            // size comes from the result type — sema annotated the
            // `let b: ListBuilder<T>` so `result_type` is
            // `ListBuilderRef(T)`.
            let elem_ty = match result_type {
                IrType::ListBuilderRef(t) => t.as_ref().clone(),
                _ => {
                    return Err(CompileError::new(
                        "ListBuilder.alloc: result type is not ListBuilderRef",
                    ));
                }
            };
            let es = elem_size_bytes(&elem_ty);
            let es_val = builder.ins().iconst(cl::I64, es);
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.list_builder_alloc,
                &[es_val],
            ))
        }
        "push" => {
            // args[0] = receiver (handle), args[1] = element.
            let handle = get_val1(state, args[0])?;
            let elem_ty = builder_elem_type(state, args[0], Some(args[1]))?;
            let elem_vals = get_val(state, args[1])?;
            let es = elem_size_bytes(&elem_ty);
            let temp_ptr = store_to_temp(builder, &elem_vals, &elem_ty);
            let es_val = builder.ins().iconst(cl::I64, es);
            // No return value — push is in-place. `call_runtime`
            // returns `vec![]` for void-returning runtime calls.
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.list_builder_push,
                &[handle, temp_ptr, es_val],
            ))
        }
        "freeze" => {
            let handle = get_val1(state, args[0])?;
            Ok(call_runtime(
                builder,
                ctx,
                ctx.runtime.list_builder_freeze,
                &[handle],
            ))
        }
        _ => Err(CompileError::new(format!(
            "ListBuilder.{method}: not yet supported in compiled mode"
        ))),
    }
}
