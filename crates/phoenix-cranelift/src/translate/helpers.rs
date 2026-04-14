//! Shared helper functions for Cranelift IR translation.
//!
//! Provides fat-value memory operations, division-by-zero checks,
//! runtime function calls, and string comparison emission.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{self, InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::types::{POINTER_TYPE, ir_type_to_cl_single};
use phoenix_ir::instruction::ValueId;
use phoenix_ir::types::IrType;

use super::FuncState;

/// Number of 8-byte slots a value of the given IR type occupies.
///
/// `StringRef` is a fat pointer (ptr + len) and takes 2 slots.
/// All other types occupy 1 slot.
pub(crate) fn slots_for_type(ty: &IrType) -> usize {
    match ty {
        IrType::StringRef => 2,
        _ => 1,
    }
}

/// Store a (possibly fat) value into heap memory at the given slot offset.
///
/// For `StringRef`, stores both the pointer and length at consecutive slots.
/// For all other types, stores a single value.
pub(crate) fn store_fat_value(
    builder: &mut FunctionBuilder,
    vals: Vec<Value>,
    ty: &IrType,
    base_ptr: Value,
    slot: usize,
) {
    match ty {
        IrType::StringRef => {
            builder
                .ins()
                .store(MemFlags::new(), vals[0], base_ptr, (slot * 8) as i32);
            builder
                .ins()
                .store(MemFlags::new(), vals[1], base_ptr, ((slot + 1) * 8) as i32);
        }
        _ => {
            builder
                .ins()
                .store(MemFlags::new(), vals[0], base_ptr, (slot * 8) as i32);
        }
    }
}

/// Load a (possibly fat) value from heap memory at the given slot offset.
///
/// For `StringRef`, loads both the pointer and length from consecutive slots.
/// For all other types, loads a single value.
pub(crate) fn load_fat_value(
    builder: &mut FunctionBuilder,
    ty: &IrType,
    base_ptr: Value,
    slot: usize,
) -> Result<Vec<Value>, CompileError> {
    match ty {
        IrType::StringRef => {
            let ptr_val =
                builder
                    .ins()
                    .load(POINTER_TYPE, MemFlags::new(), base_ptr, (slot * 8) as i32);
            let len_val =
                builder
                    .ins()
                    .load(cl::I64, MemFlags::new(), base_ptr, ((slot + 1) * 8) as i32);
            Ok(vec![ptr_val, len_val])
        }
        _ => {
            let cl_ty = ir_type_to_cl_single(ty)?;
            let val = builder
                .ins()
                .load(cl_ty, MemFlags::new(), base_ptr, (slot * 8) as i32);
            Ok(vec![val])
        }
    }
}

/// Emit a call to a runtime function, returning the Cranelift results.
pub(crate) fn call_runtime(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    func_id: cranelift_module::FuncId,
    args: &[Value],
) -> Vec<Value> {
    let func_ref = ctx.module.declare_func_in_func(func_id, builder.func);
    let call = builder.ins().call(func_ref, args);
    builder.inst_results(call).to_vec()
}

/// Emit a string comparison call to a runtime function.
pub(crate) fn emit_str_cmp(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    state: &FuncState,
    a: ValueId,
    b: ValueId,
    runtime_func: cranelift_module::FuncId,
) -> Result<Vec<Value>, CompileError> {
    let a_vals = super::get_val(state, a)?;
    let b_vals = super::get_val(state, b)?;
    let func_ref = ctx.module.declare_func_in_func(runtime_func, builder.func);
    let call = builder
        .ins()
        .call(func_ref, &[a_vals[0], a_vals[1], b_vals[0], b_vals[1]]);
    Ok(builder.inst_results(call).to_vec())
}

/// Emit a runtime panic with a string message.
///
/// Declares the message as anonymous data, calls `phx_panic`, then traps.
pub(crate) fn emit_panic_with_message(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    msg: &str,
) -> Result<(), CompileError> {
    let msg_data_id = ctx
        .module
        .declare_anonymous_data(false, false)
        .map_err(CompileError::from_display)?;
    let mut data_desc = cranelift_module::DataDescription::new();
    data_desc.define(msg.as_bytes().to_vec().into_boxed_slice());
    ctx.module
        .define_data(msg_data_id, &data_desc)
        .map_err(CompileError::from_display)?;
    let gv = ctx.module.declare_data_in_func(msg_data_id, builder.func);
    let msg_ptr = builder.ins().global_value(POINTER_TYPE, gv);
    let msg_len = builder.ins().iconst(cl::I64, msg.len() as i64);
    let panic_ref = ctx
        .module
        .declare_func_in_func(ctx.runtime.panic, builder.func);
    builder.ins().call(panic_ref, &[msg_ptr, msg_len]);
    builder.ins().trap(ir::TrapCode::unwrap_user(2));
    Ok(())
}

/// Emit a division-by-zero check: if `divisor` is zero, call `phx_panic`.
pub(crate) fn emit_div_zero_check(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    divisor: Value,
) -> Result<(), CompileError> {
    let zero = builder.ins().iconst(cl::I64, 0);
    let is_zero = builder.ins().icmp(IntCC::Equal, divisor, zero);

    let panic_block = builder.create_block();
    let ok_block = builder.create_block();

    builder.ins().brif(is_zero, panic_block, &[], ok_block, &[]);

    builder.seal_block(panic_block);
    builder.switch_to_block(panic_block);
    emit_panic_with_message(builder, ctx, "division by zero")?;

    builder.seal_block(ok_block);
    builder.switch_to_block(ok_block);

    Ok(())
}
