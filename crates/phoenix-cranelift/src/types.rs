//! Cranelift type constants used across the crate.
//!
//! Per-`IrType` layout (slot count, Cranelift expansion, load/store) lives
//! on [`crate::translate::layout::TypeLayout`].

use cranelift_codegen::ir::Type as CraneliftType;
use cranelift_codegen::ir::types as cl;

/// The Cranelift pointer type (64-bit on the target platform).
pub const POINTER_TYPE: CraneliftType = cl::I64;
