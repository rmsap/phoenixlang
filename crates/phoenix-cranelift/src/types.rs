//! Mapping from Phoenix IR types to Cranelift types.

use cranelift_codegen::ir::Type as CraneliftType;
use cranelift_codegen::ir::types as cl;
use phoenix_ir::types::IrType;

use crate::error::CompileError;

/// The Cranelift pointer type (64-bit on the target platform).
pub const POINTER_TYPE: CraneliftType = cl::I64;

/// Convert a Phoenix IR type to a list of Cranelift types.
///
/// Most types map to a single Cranelift type.  Strings are represented as
/// fat pointers `(ptr, len)` and expand to two values.  Void produces an
/// empty list (no return value).
pub fn ir_type_to_cl(ty: &IrType) -> Vec<CraneliftType> {
    match ty {
        IrType::I64 => vec![cl::I64],
        IrType::F64 => vec![cl::F64],
        IrType::Bool => vec![cl::I8],
        IrType::Void => vec![],

        // Strings are fat pointers: (ptr, len).
        IrType::StringRef => vec![POINTER_TYPE, cl::I64],

        // All other reference types are opaque pointers.
        IrType::StructRef(_)
        | IrType::EnumRef(_)
        | IrType::ListRef(_)
        | IrType::MapRef(_, _)
        | IrType::ClosureRef { .. } => vec![POINTER_TYPE],
    }
}

/// Convert a Phoenix IR type to a single Cranelift type.
///
/// Returns an error for `Void` (which has no Cranelift representation) and
/// `StringRef` (which requires two Cranelift values).  Use [`ir_type_to_cl`]
/// for those types.
pub fn ir_type_to_cl_single(ty: &IrType) -> Result<CraneliftType, CompileError> {
    match ty {
        IrType::I64 => Ok(cl::I64),
        IrType::F64 => Ok(cl::F64),
        IrType::Bool => Ok(cl::I8),
        IrType::StructRef(_)
        | IrType::EnumRef(_)
        | IrType::ListRef(_)
        | IrType::MapRef(_, _)
        | IrType::ClosureRef { .. } => Ok(POINTER_TYPE),
        IrType::Void => Err(CompileError::new(
            "cannot convert Void to a single Cranelift type",
        )),
        IrType::StringRef => Err(CompileError::new(
            "StringRef requires two Cranelift values — use ir_type_to_cl",
        )),
    }
}
