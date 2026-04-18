//! Shared helpers for constructing Option and Result enum values.
//!
//! Option and Result are compiled as standard enums with heap-allocated
//! tagged unions.  The layout is:
//!
//! ```text
//! offset 0: i64 discriminant
//! offset 8+: payload fields (one 8-byte slot per value, two for StringRef)
//! ```
//!
//! These helpers look up variant indices from the IR module's enum layouts
//! and emit the Cranelift IR for allocating and populating enum values.
//!
//! Payload type inference helpers (used by `option_methods` and
//! `result_methods`) are in [`super::enum_type_inference`].

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types as cl;
use cranelift_codegen::ir::{self, InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use super::helpers::{slots_for_type, store_fat_value};

/// Look up the variant index for a named variant within an enum layout.
pub(super) fn enum_variant_index(
    ir_module: &IrModule,
    enum_name: &str,
    variant_name: &str,
) -> Result<u32, CompileError> {
    let layout = ir_module
        .enum_layouts
        .get(enum_name)
        .ok_or_else(|| CompileError::new(format!("{enum_name} enum not found in module")))?;
    for (i, (name, _)) in layout.iter().enumerate() {
        if name == variant_name {
            return Ok(i as u32);
        }
    }
    Err(CompileError::new(format!(
        "{variant_name} variant not found in {enum_name}"
    )))
}

/// Compute the max payload slots across all variants of an enum.
///
/// Returns an error if the enum is not found in the module's enum layouts,
/// rather than silently returning 0 (which would under-allocate the enum).
pub(super) fn enum_max_payload_slots(
    ir_module: &IrModule,
    name: &str,
) -> Result<usize, CompileError> {
    let variants = ir_module
        .enum_layouts
        .get(name)
        .ok_or_else(|| CompileError::new(format!("{name} enum not found in module layouts")))?;
    Ok(variants
        .iter()
        .map(|(_, fs)| fs.iter().map(slots_for_type).sum::<usize>())
        .max()
        .unwrap_or(0))
}

/// Build an enum variant allocation in Cranelift IR.
///
/// Allocates a heap object with space for the discriminant plus the max
/// payload across all variants, stores the discriminant, and stores the
/// payload fields starting at slot 1.
pub(super) fn build_enum_variant(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    enum_name: &str,
    variant_name: &str,
    payload: &[Value],
    payload_ty: Option<&IrType>,
) -> Result<Value, CompileError> {
    let disc_idx = enum_variant_index(ir_module, enum_name, variant_name)?;
    let max_slots = enum_max_payload_slots(ir_module, enum_name)?;
    // Ensure we allocate enough for the actual payload (may exceed generic layout).
    let actual_slots = payload_ty.map(slots_for_type).unwrap_or(0);
    let slots = max_slots.max(actual_slots);
    let size = ((1 + slots) * super::layout::SLOT_SIZE) as i64;
    let alloc_ref = ctx
        .module
        .declare_func_in_func(ctx.runtime.alloc, builder.func);
    let size_val = builder.ins().iconst(cl::I64, size);
    let call = builder.ins().call(alloc_ref, &[size_val]);
    let ptr = builder.inst_results(call)[0];
    let disc = builder.ins().iconst(cl::I64, disc_idx as i64);
    builder.ins().store(MemFlags::new(), disc, ptr, 0);
    if let Some(ty) = payload_ty {
        store_fat_value(builder, payload, ty, ptr, 1);
    }
    Ok(ptr)
}

/// Build a `Some(val)` enum allocation.
pub(super) fn build_option_some(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    val: &[Value],
    val_ty: &IrType,
    ir_module: &IrModule,
) -> Result<Value, CompileError> {
    build_enum_variant(builder, ctx, ir_module, "Option", "Some", val, Some(val_ty))
}

/// Build a `None` enum allocation.
pub(super) fn build_option_none(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
) -> Result<Value, CompileError> {
    build_enum_variant(builder, ctx, ir_module, "Option", "None", &[], None)
}

/// Build an `Ok(val)` enum allocation.
pub(super) fn build_result_ok(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    val: &[Value],
    val_ty: &IrType,
    ir_module: &IrModule,
) -> Result<Value, CompileError> {
    build_enum_variant(builder, ctx, ir_module, "Result", "Ok", val, Some(val_ty))
}

/// Build an `Err(val)` enum allocation.
pub(super) fn build_result_err(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    val: &[Value],
    val_ty: &IrType,
    ir_module: &IrModule,
) -> Result<Value, CompileError> {
    build_enum_variant(builder, ctx, ir_module, "Result", "Err", val, Some(val_ty))
}

/// Return value from [`load_disc_and_branch`]: the blocks and comparison
/// flag produced by the common enum-method preamble.
pub(super) struct EnumBranch {
    /// `true` if the discriminant matches `positive_variant`.
    pub is_positive: Value,
    /// Block to switch to when the variant is the positive one.
    pub positive_block: ir::Block,
    /// Block to switch to when the variant is the negative one.
    pub negative_block: ir::Block,
    /// Merge block with the given `merge_param_types` appended.
    pub merge_block: ir::Block,
}

/// Common preamble for Option/Result methods: load discriminant, compare
/// against a named variant, create positive/negative/merge blocks, and
/// emit a conditional branch.
///
/// After calling this, the builder is positioned *before* the branch
/// (all three blocks are unsealed).  The caller should:
///
/// 1. `builder.seal_block(branch.positive_block); builder.switch_to_block(..)`
/// 2. Generate the positive-path code, `jump(merge_block, &results)`.
/// 3. Same for `negative_block`.
/// 4. `seal_block(merge_block); switch_to_block(merge_block)`.
pub(super) fn load_disc_and_branch(
    builder: &mut FunctionBuilder,
    recv_ptr: Value,
    ir_module: &IrModule,
    enum_name: &str,
    positive_variant: &str,
    merge_param_types: &[ir::types::Type],
) -> Result<EnumBranch, CompileError> {
    let pos_idx = enum_variant_index(ir_module, enum_name, positive_variant)?;
    let disc = builder.ins().load(cl::I64, MemFlags::new(), recv_ptr, 0);
    let pos_disc = builder.ins().iconst(cl::I64, pos_idx as i64);
    let is_positive = builder.ins().icmp(IntCC::Equal, disc, pos_disc);

    let positive_block = builder.create_block();
    let negative_block = builder.create_block();
    let merge_block = builder.create_block();
    for ty in merge_param_types {
        builder.append_block_param(merge_block, *ty);
    }
    builder
        .ins()
        .brif(is_positive, positive_block, &[], negative_block, &[]);

    Ok(EnumBranch {
        is_positive,
        positive_block,
        negative_block,
        merge_block,
    })
}
