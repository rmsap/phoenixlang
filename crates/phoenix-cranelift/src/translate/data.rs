//! Translation of string, struct, and enum operations.

use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use phoenix_ir::instruction::Op;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::helpers::emit_str_cmp;
use super::layout::{SLOT_SIZE, TypeLayout};
use super::{FuncState, get_val, get_val1};

/// Translate a string operation (concat, comparison).
pub(super) fn translate_string(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    op: &Op,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::StringConcat(a, b) => {
            let a_vals = get_val(state, *a)?;
            let b_vals = get_val(state, *b)?;
            let func_ref = ctx
                .module
                .declare_func_in_func(ctx.runtime.str_concat, builder.func);
            let call = builder
                .ins()
                .call(func_ref, &[a_vals[0], a_vals[1], b_vals[0], b_vals[1]]);
            Ok(builder.inst_results(call).to_vec())
        }
        Op::StringEq(a, b) => emit_str_cmp(builder, ctx, state, *a, *b, ctx.runtime.str_eq),
        Op::StringNe(a, b) => emit_str_cmp(builder, ctx, state, *a, *b, ctx.runtime.str_ne),
        Op::StringLt(a, b) => emit_str_cmp(builder, ctx, state, *a, *b, ctx.runtime.str_lt),
        Op::StringGt(a, b) => emit_str_cmp(builder, ctx, state, *a, *b, ctx.runtime.str_gt),
        Op::StringLe(a, b) => emit_str_cmp(builder, ctx, state, *a, *b, ctx.runtime.str_le),
        Op::StringGe(a, b) => emit_str_cmp(builder, ctx, state, *a, *b, ctx.runtime.str_ge),
        _ => unreachable!(),
    }
}

/// Translate a struct operation (alloc, get field, set field).
pub(super) fn translate_struct(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    op: &Op,
    result_type: &IrType,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::StructAlloc(name, fields) => {
            let layout = ir_module
                .struct_layouts
                .get(name.as_str())
                .ok_or_else(|| CompileError::new(format!("unknown struct: {name}")))?;
            let total_slots: usize = layout
                .iter()
                .map(|(_, ty)| TypeLayout::of(ty).slots())
                .sum();
            let size = (total_slots * SLOT_SIZE) as i64;
            let alloc_ref = ctx
                .module
                .declare_func_in_func(ctx.runtime.alloc, builder.func);
            let size_val = builder.ins().iconst(cl::I64, size);
            let call = builder.ins().call(alloc_ref, &[size_val]);
            let ptr = builder.inst_results(call)[0];
            let mut slot = 0usize;
            for (i, fid) in fields.iter().enumerate() {
                let field_vals = get_val(state, *fid)?;
                let field_ty = &layout[i].1;
                let field_layout = TypeLayout::of(field_ty);
                field_layout.store(builder, ptr, slot, &field_vals);
                slot += field_layout.slots();
            }
            Ok(vec![ptr])
        }
        Op::StructGetField(obj, idx) => {
            let ptr = get_val1(state, *obj)?;
            let struct_type = state
                .type_map
                .get(obj)
                .ok_or_else(|| CompileError::new("unknown type for struct field access"))?;
            let struct_name = match struct_type {
                IrType::StructRef(n) => n,
                _ => return Err(CompileError::new("StructGetField on non-struct")),
            };
            let layout = ir_module
                .struct_layouts
                .get(struct_name.as_str())
                .ok_or_else(|| CompileError::new(format!("unknown struct: {struct_name}")))?;
            let slot = layout
                .iter()
                .take(*idx as usize)
                .map(|(_, ty)| TypeLayout::of(ty).slots())
                .sum::<usize>();
            Ok(TypeLayout::of(result_type).load(builder, ptr, slot))
        }
        Op::StructSetField(obj, idx, val) => {
            let ptr = get_val1(state, *obj)?;
            let struct_type = state
                .type_map
                .get(obj)
                .ok_or_else(|| CompileError::new("unknown type for struct field set"))?;
            let struct_name = match struct_type {
                IrType::StructRef(n) => n,
                _ => return Err(CompileError::new("StructSetField on non-struct")),
            };
            let layout = ir_module
                .struct_layouts
                .get(struct_name.as_str())
                .ok_or_else(|| CompileError::new(format!("unknown struct: {struct_name}")))?;
            let slot = layout
                .iter()
                .take(*idx as usize)
                .map(|(_, ty)| TypeLayout::of(ty).slots())
                .sum::<usize>();
            let field_ty = &layout[*idx as usize].1;
            let field_vals = get_val(state, *val)?;
            TypeLayout::of(field_ty).store(builder, ptr, slot, &field_vals);
            Ok(vec![])
        }
        _ => unreachable!(),
    }
}

/// Translate an enum operation (alloc, discriminant, get field).
pub(super) fn translate_enum(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    op: &Op,
    result_type: &IrType,
    state: &FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::EnumAlloc(name, variant_idx, fields) => {
            let layout = ir_module
                .enum_layouts
                .get(name.as_str())
                .ok_or_else(|| CompileError::new(format!("unknown enum: {name}")))?;
            let mut max_payload_slots: usize = layout
                .iter()
                .map(|(_, fs)| fs.iter().map(|t| TypeLayout::of(t).slots()).sum::<usize>())
                .max()
                .unwrap_or(0);
            // Also account for the actual field types of this variant,
            // which may be larger than the generic layout types.
            let actual_slots: usize = fields
                .iter()
                .map(|fid| {
                    // Default to 2 slots when the type is unknown — this is
                    // the max for any current type (StringRef). Under-sizing
                    // would cause a buffer overrun when TypeLayout::store
                    // writes the second slot.
                    state
                        .type_map
                        .get(fid)
                        .map(|t| TypeLayout::of(t).slots())
                        .unwrap_or(2)
                })
                .sum();
            if actual_slots > max_payload_slots {
                max_payload_slots = actual_slots;
            }
            let size = ((1 + max_payload_slots) * SLOT_SIZE) as i64;
            let alloc_ref = ctx
                .module
                .declare_func_in_func(ctx.runtime.alloc, builder.func);
            let size_val = builder.ins().iconst(cl::I64, size);
            let call = builder.ins().call(alloc_ref, &[size_val]);
            let ptr = builder.inst_results(call)[0];
            let disc = builder.ins().iconst(cl::I64, *variant_idx as i64);
            builder.ins().store(MemFlags::new(), disc, ptr, 0);
            let variant_types = &layout[*variant_idx as usize].1;
            let mut slot = 1usize;
            for (i, fid) in fields.iter().enumerate() {
                let field_vals = get_val(state, *fid)?;
                // Prefer the actual value type from the type_map over the
                // (possibly generic) layout type, so fat values like strings
                // are stored with the correct number of slots.
                let field_ty = state.type_map.get(fid).unwrap_or(&variant_types[i]);
                let field_layout = TypeLayout::of(field_ty);
                field_layout.store(builder, ptr, slot, &field_vals);
                slot += field_layout.slots();
            }
            Ok(vec![ptr])
        }
        Op::EnumDiscriminant(v) => {
            let ptr = get_val1(state, *v)?;
            let disc = builder.ins().load(cl::I64, MemFlags::new(), ptr, 0);
            Ok(vec![disc])
        }
        Op::EnumGetField(v, variant_idx, field_idx) => {
            let ptr = get_val1(state, *v)?;
            let enum_type = state
                .type_map
                .get(v)
                .ok_or_else(|| CompileError::new("unknown type for enum field access"))?;
            let enum_name = match enum_type {
                IrType::EnumRef(n) => n,
                _ => return Err(CompileError::new("EnumGetField on non-enum")),
            };
            let layout = ir_module
                .enum_layouts
                .get(enum_name.as_str())
                .ok_or_else(|| CompileError::new(format!("unknown enum: {enum_name}")))?;
            let variant_fields = &layout[*variant_idx as usize].1;
            let slot = 1 + variant_fields
                .iter()
                .take(*field_idx as usize)
                .map(|t| TypeLayout::of(t).slots())
                .sum::<usize>();
            Ok(TypeLayout::of(result_type).load(builder, ptr, slot))
        }
        _ => unreachable!(),
    }
}
