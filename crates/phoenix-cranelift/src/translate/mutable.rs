//! Translation of mutable variable operations (alloca, load, store).
//!
//! Handles fat values (StringRef) correctly by allocating appropriately
//! sized stack slots and using multi-word load/store operations.

use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{InstBuilder, MemFlags, StackSlotData, StackSlotKind, Value};
use cranelift_frontend::FunctionBuilder;

use crate::error::CompileError;
use crate::types::{POINTER_TYPE, ir_type_to_cl_single};
use phoenix_ir::instruction::{Op, VOID_SENTINEL};
use phoenix_ir::types::IrType;

use super::helpers::slots_for_type;
use super::{FuncState, get_val, get_val1};

/// Translate a mutable variable operation (Alloca, Load, or Store).
///
/// `Alloca` creates a stack slot sized to the variable's type (16 bytes for
/// `StringRef`, 8 bytes for everything else).  `Load` and `Store` handle
/// fat values by reading/writing both pointer and length for strings.
pub(super) fn translate_mutable(
    builder: &mut FunctionBuilder,
    op: &Op,
    state: &mut FuncState,
) -> Result<Vec<Value>, CompileError> {
    match op {
        Op::Alloca(ty) => {
            let slot_size = (slots_for_type(ty) * super::layout::SLOT_SIZE) as u32;
            let data = StackSlotData::new(StackSlotKind::ExplicitSlot, slot_size, 0);
            let slot = builder.create_sized_stack_slot(data);
            let addr = builder.ins().stack_addr(POINTER_TYPE, slot, 0);
            state.alloca_map.insert(VOID_SENTINEL, (slot, ty.clone()));
            Ok(vec![addr])
        }
        Op::Load(slot_vid) => {
            if let Some((_slot, ty)) = state.alloca_map.get(slot_vid) {
                let ty = ty.clone();
                let addr = get_val1(state, *slot_vid)?;
                match &ty {
                    IrType::StringRef => {
                        let ptr_val = builder.ins().load(POINTER_TYPE, MemFlags::new(), addr, 0);
                        let len_val = builder.ins().load(
                            cl::I64,
                            MemFlags::new(),
                            addr,
                            super::layout::SLOT_SIZE as i32,
                        );
                        Ok(vec![ptr_val, len_val])
                    }
                    _ => {
                        let cl_ty = ir_type_to_cl_single(&ty)?;
                        let val = builder.ins().load(cl_ty, MemFlags::new(), addr, 0);
                        Ok(vec![val])
                    }
                }
            } else {
                Err(CompileError::new(format!(
                    "Load from unknown alloca {slot_vid}"
                )))
            }
        }
        Op::Store(slot_vid, val_vid) => {
            if let Some((_slot, ty)) = state.alloca_map.get(slot_vid) {
                let ty = ty.clone();
                let addr = get_val1(state, *slot_vid)?;
                let vals = get_val(state, *val_vid)?;
                match &ty {
                    IrType::StringRef => {
                        builder.ins().store(MemFlags::new(), vals[0], addr, 0);
                        builder.ins().store(
                            MemFlags::new(),
                            vals[1],
                            addr,
                            super::layout::SLOT_SIZE as i32,
                        );
                    }
                    _ => {
                        builder.ins().store(MemFlags::new(), vals[0], addr, 0);
                    }
                }
                Ok(vec![])
            } else {
                Err(CompileError::new(format!(
                    "Store to unknown alloca {slot_vid}"
                )))
            }
        }
        _ => unreachable!(),
    }
}
