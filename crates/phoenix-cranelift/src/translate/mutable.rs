//! Translation of mutable variable operations (alloca, load, store).
//!
//! Handles fat values (StringRef) correctly by allocating appropriately
//! sized stack slots and using multi-word load/store operations.

use cranelift_codegen::ir::{InstBuilder, StackSlotData, StackSlotKind, Value};
use cranelift_frontend::FunctionBuilder;

use crate::error::CompileError;
use crate::types::POINTER_TYPE;
use phoenix_ir::instruction::{Op, VOID_SENTINEL};

use super::layout::TypeLayout;
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
            let slot_size = TypeLayout::of(ty).size_bytes() as u32;
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
                Ok(TypeLayout::of(&ty).load(builder, addr, 0))
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
                TypeLayout::of(&ty).store(builder, addr, 0, &vals);
                Ok(vec![])
            } else {
                Err(CompileError::new(format!(
                    "Store to unknown alloca {slot_vid}"
                )))
            }
        }
        _ => ice!("translate_mutable dispatched on non-mutable op: {op:?}"),
    }
}
