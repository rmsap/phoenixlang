//! Declarations of runtime functions imported from `phoenix-runtime`.
//!
//! Each runtime function is declared as an imported function in the
//! Cranelift module so compiled code can call it.

use cranelift_codegen::ir::types::{F64, I8, I64};
use cranelift_codegen::ir::{AbiParam, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_module::{FuncId, Linkage, Module};

use crate::error::CompileError;

/// All runtime functions that can be imported.
pub struct RuntimeFunctions {
    /// `phx_print_i64(i64)`.
    pub print_i64: FuncId,
    /// `phx_print_f64(f64)`.
    pub print_f64: FuncId,
    /// `phx_print_bool(i8)`.
    pub print_bool: FuncId,
    /// `phx_print_str(ptr, len)`.
    pub print_str: FuncId,
    /// `phx_panic(ptr, len)` — abort with message.
    pub panic: FuncId,
    /// `phx_str_concat(p1, l1, p2, l2) -> (ptr, len)`.
    pub str_concat: FuncId,
    /// `phx_i64_to_str(i64) -> (ptr, len)`.
    pub i64_to_str: FuncId,
    /// `phx_f64_to_str(f64) -> (ptr, len)`.
    pub f64_to_str: FuncId,
    /// `phx_bool_to_str(i8) -> (ptr, len)`.
    pub bool_to_str: FuncId,
    /// `phx_str_eq`.
    pub str_eq: FuncId,
    /// `phx_str_ne`.
    pub str_ne: FuncId,
    /// `phx_str_lt`.
    pub str_lt: FuncId,
    /// `phx_str_gt`.
    pub str_gt: FuncId,
    /// `phx_str_le`.
    pub str_le: FuncId,
    /// `phx_str_ge`.
    pub str_ge: FuncId,
    /// `phx_alloc(size) -> ptr`.
    pub alloc: FuncId,
}

impl RuntimeFunctions {
    /// Declare all runtime functions in the given module.
    pub fn declare(module: &mut impl Module, call_conv: CallConv) -> Result<Self, CompileError> {
        Ok(Self {
            print_i64: declare_func(module, "phx_print_i64", &[I64], &[], call_conv)?,
            print_f64: declare_func(module, "phx_print_f64", &[F64], &[], call_conv)?,
            print_bool: declare_func(module, "phx_print_bool", &[I8], &[], call_conv)?,
            print_str: declare_func(module, "phx_print_str", &[I64, I64], &[], call_conv)?,
            panic: declare_func(module, "phx_panic", &[I64, I64], &[], call_conv)?,
            str_concat: declare_func(
                module,
                "phx_str_concat",
                &[I64, I64, I64, I64],
                &[I64, I64],
                call_conv,
            )?,
            i64_to_str: declare_func(module, "phx_i64_to_str", &[I64], &[I64, I64], call_conv)?,
            f64_to_str: declare_func(module, "phx_f64_to_str", &[F64], &[I64, I64], call_conv)?,
            bool_to_str: declare_func(module, "phx_bool_to_str", &[I8], &[I64, I64], call_conv)?,
            str_eq: declare_str_cmp(module, "phx_str_eq", call_conv)?,
            str_ne: declare_str_cmp(module, "phx_str_ne", call_conv)?,
            str_lt: declare_str_cmp(module, "phx_str_lt", call_conv)?,
            str_gt: declare_str_cmp(module, "phx_str_gt", call_conv)?,
            str_le: declare_str_cmp(module, "phx_str_le", call_conv)?,
            str_ge: declare_str_cmp(module, "phx_str_ge", call_conv)?,
            alloc: declare_func(module, "phx_alloc", &[I64], &[I64], call_conv)?,
        })
    }
}

/// Declare a single runtime function with the given parameter and return types.
fn declare_func(
    module: &mut impl Module,
    name: &str,
    params: &[cranelift_codegen::ir::Type],
    returns: &[cranelift_codegen::ir::Type],
    call_conv: CallConv,
) -> Result<FuncId, CompileError> {
    let mut sig = Signature::new(call_conv);
    for &ty in params {
        sig.params.push(AbiParam::new(ty));
    }
    for &ty in returns {
        sig.returns.push(AbiParam::new(ty));
    }
    Ok(module.declare_function(name, Linkage::Import, &sig)?)
}

/// Declare a string comparison runtime function (`(ptr, len, ptr, len) -> i8`).
fn declare_str_cmp(
    module: &mut impl Module,
    name: &str,
    call_conv: CallConv,
) -> Result<FuncId, CompileError> {
    declare_func(module, name, &[I64, I64, I64, I64], &[I8], call_conv)
}
