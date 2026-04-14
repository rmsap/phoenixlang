//! Calling convention helpers.
//!
//! Manages the expansion of Phoenix types (especially strings) into
//! Cranelift function signatures and value lists.

use crate::types::ir_type_to_cl;
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
        for cl_ty in ir_type_to_cl(ty) {
            sig.params.push(AbiParam::new(cl_ty));
        }
    }
    for cl_ty in ir_type_to_cl(return_type) {
        sig.returns.push(AbiParam::new(cl_ty));
    }
    sig
}
