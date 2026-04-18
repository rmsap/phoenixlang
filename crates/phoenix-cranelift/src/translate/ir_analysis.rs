//! IR analysis utilities for Cranelift translation.
//!
//! Functions in this module scan the IR module's function definitions
//! to extract metadata (e.g., closure capture types) without emitting
//! any Cranelift IR. This keeps IR analysis separate from code generation.

use crate::error::CompileError;
use phoenix_ir::instruction::FuncId as PhxFuncId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

/// Find the capture parameter types for a closure function by its `FuncId`.
///
/// The closure function's parameters are `[captures..., user_params...]`.
/// Given the number of user parameters, the capture types are the prefix.
pub(super) fn find_capture_types_by_func_id(
    ir_module: &IrModule,
    func_id: PhxFuncId,
    user_param_count: usize,
) -> Result<Vec<IrType>, CompileError> {
    for func in &ir_module.functions {
        if func.id == func_id {
            if func.param_types.len() >= user_param_count {
                let capture_count = func.param_types.len() - user_param_count;
                return Ok(func.param_types[..capture_count].to_vec());
            }
            return Err(CompileError::new(format!(
                "closure function {:?} has {} params but expected at least {} user params",
                func_id,
                func.param_types.len(),
                user_param_count,
            )));
        }
    }
    Ok(Vec::new())
}

/// Find the capture parameter types for a closure by scanning IR functions
/// for matching user parameter types and return type.
///
/// This is a fallback heuristic used when the closure value comes through
/// a block parameter (phi) and the exact `FuncId` is not known.  Returns
/// an error if multiple closures match with different capture layouts.
///
/// **Known limitation:** This heuristic can produce false matches if two
/// closures share the same user-param types and return type but have
/// different captures.  The ambiguity check (below) catches mismatches,
/// but two closures with identical captures would be silently conflated.
/// A proper fix requires a richer closure representation in the IR that
/// carries capture metadata alongside the function pointer.
pub(super) fn find_closure_capture_types(
    ir_module: &IrModule,
    user_param_types: &[IrType],
    return_type: &IrType,
) -> Result<Vec<IrType>, CompileError> {
    let mut candidates: Vec<Vec<IrType>> = Vec::new();
    for func in &ir_module.functions {
        if !func.name.starts_with("__closure_") {
            continue;
        }
        if func.return_type != *return_type {
            continue;
        }
        if func.param_types.len() < user_param_types.len() {
            continue;
        }
        let capture_count = func.param_types.len() - user_param_types.len();
        let suffix = &func.param_types[capture_count..];
        if suffix == user_param_types {
            candidates.push(func.param_types[..capture_count].to_vec());
        }
    }

    if candidates.is_empty() {
        // No captures found — the closure has no captured variables.
        return Ok(Vec::new());
    }

    // Check that all matching closures agree on capture types.
    let first = &candidates[0];
    if candidates.iter().all(|c| c == first) {
        Ok(candidates.into_iter().next().unwrap())
    } else {
        Err(CompileError::new(
            "ambiguous indirect call: multiple closures with the same user signature \
             but different captures. This pattern requires ClosureAlloc tracking \
             (pass closures directly, not through block parameters).",
        ))
    }
}
