//! Calling convention helpers.
//!
//! Manages the expansion of Phoenix types (especially strings) into
//! Cranelift function signatures and value lists.

use crate::translate::layout::TypeLayout;
use cranelift_codegen::ir::{AbiParam, Signature};
use cranelift_codegen::isa::CallConv;
use phoenix_ir::types::IrType;

/// Build a Cranelift [`Signature`] from Phoenix parameter types and return type.
pub fn build_signature(
    param_types: &[IrType],
    return_type: &IrType,
    call_conv: CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    for ty in param_types {
        for &cl_ty in TypeLayout::of(ty).cl_types() {
            sig.params.push(AbiParam::new(cl_ty));
        }
    }
    for &cl_ty in TypeLayout::of(return_type).cl_types() {
        sig.returns.push(AbiParam::new(cl_ty));
    }
    sig
}
