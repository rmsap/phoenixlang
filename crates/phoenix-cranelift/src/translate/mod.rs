//! Per-function translation from Phoenix IR to Cranelift IR.
//!
//! This module orchestrates the translation of each Phoenix IR function
//! into Cranelift IR, dispatching individual operations to domain-specific
//! submodules for readability and separation of concerns.
//!
//! Most translation functions take `(&mut FunctionBuilder, &mut CompileContext,
//! &IrModule, &FuncState)` as their first parameters.  A context struct was
//! considered but the two `&mut` borrows (`builder` + `ctx`) make bundling
//! awkward without adding `RefCell`-style indirection.
mod arith;
mod calls;
mod closure_call;
mod control;
mod data;
mod dyn_trait;
mod enum_combinators;
mod enum_helpers;
mod enum_type_inference;
mod gc_roots;
mod helpers;
pub(crate) use helpers::call_runtime;
// `layout` is `pub(crate)` so `abi.rs` can name `TypeLayout` when building
// function signatures. No other crate-level consumers — within `translate`,
// submodules reach it via `super::layout`.
pub(crate) mod layout;
mod list_methods;
mod list_methods_closure;
mod list_methods_complex;
mod map_methods;
mod mutable;
mod option_methods;
mod result_methods;

use std::collections::HashMap;

use cranelift_codegen::Context;
use cranelift_codegen::ir::{self, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::Module;

use crate::context::CompileContext;
use crate::error::CompileError;
use crate::translate::layout::TypeLayout;
use phoenix_ir::block::BlockId as PhxBlockId;
use phoenix_ir::instruction::{Op, VOID_SENTINEL, ValueId};
use phoenix_ir::module::{IrFunction, IrModule};
use phoenix_ir::types::IrType;

/// Per-function translation state shared across all op translators.
///
/// Bundles the mappings from Phoenix value/block IDs to their Cranelift
/// equivalents, plus type and closure metadata needed during translation.
pub(crate) struct FuncState {
    /// Mapping from Phoenix `ValueId` to Cranelift `Value`(s).
    /// Most types map to one value; strings map to two (ptr, len).
    pub value_map: HashMap<ValueId, Vec<Value>>,
    /// Mapping from `Alloca` result `ValueId` to its Cranelift stack slot and type.
    pub alloca_map: HashMap<ValueId, (ir::StackSlot, IrType)>,
    /// The `IrType` of each `ValueId`, for type-dispatched operations (e.g. print).
    pub type_map: HashMap<ValueId, IrType>,
    /// Capture types of the function currently being translated, in
    /// capture-slot order. Populated for closure functions only;
    /// empty for regular functions. Indexed by
    /// [`Op::ClosureLoadCapture`]'s `capture_idx` field to recover
    /// the slot offset / type of each capture in the env heap object.
    pub current_capture_types: Vec<IrType>,
    /// Records the allocated variant and concrete payload field types from
    /// `EnumAlloc` instructions. Used by `option_payload_type` /
    /// `result_payload_types` as a Strategy 4 fallback when Strategy 0 can't
    /// read the payload type directly from `EnumRef` args.
    ///
    /// The variant index is tracked so `Result<T, E>` can distinguish an
    /// `Ok(t)` allocation (payload type = T) from an `Err(e)` allocation
    /// (payload type = E) — both record `field_types[0]`, but the meaning
    /// differs per variant. Option-like enums only allocate the payload-
    /// bearing variant so this is trivially 0 for them.
    pub enum_payload_types: HashMap<ValueId, (u32, Vec<IrType>)>,
    /// Mapping from Phoenix `BlockId` to Cranelift block.
    pub block_map: HashMap<PhxBlockId, ir::Block>,
    /// Shadow-stack frame info for this function. `None` if the function
    /// has no ref-typed values (no shadow-stack overhead).
    pub gc_frame: Option<gc_roots::GcFrameInfo>,
    /// Counter for `cranelift_frontend::Variable` indices owned by this
    /// function. Use [`FuncState::next_variable`] to obtain a fresh
    /// `Variable` rather than calling `Variable::from_u32` directly —
    /// otherwise two passes that both start at 0 will silently clash.
    pub next_var_index: u32,
}

impl FuncState {
    /// Allocate a fresh `cranelift_frontend::Variable` index for this
    /// function. The single source of truth for `Variable` issuance —
    /// every Cranelift `Variable` used during translation must come
    /// through here so distinct passes (today: `gc_roots`; tomorrow:
    /// `defer`, exception unwinding, ...) can't overlap their indices.
    pub fn next_variable(&mut self) -> cranelift_frontend::Variable {
        let v = cranelift_frontend::Variable::from_u32(self.next_var_index);
        // Practically unreachable (>2³² Variables in one function would
        // require an absurd IR), but the assert is cheap and protects
        // against a future regression that overflows silently.
        debug_assert_ne!(
            self.next_var_index,
            u32::MAX,
            "FuncState::next_variable: u32 index space exhausted",
        );
        self.next_var_index += 1;
        v
    }
}

/// Translate all functions in the IR module and define them in the Cranelift module.
pub fn translate_module(
    ctx: &mut CompileContext,
    ir_module: &IrModule,
) -> Result<(), CompileError> {
    let mut cl_ctx = Context::new();
    let mut fb_ctx = FunctionBuilderContext::new();

    // Generic templates are inert post-monomorphization; their bodies
    // contain `IrType::TypeVar` which has no Cranelift lowering. Iterating
    // via `concrete_functions()` filters them out.
    for func in ir_module.concrete_functions() {
        translate_function(ctx, ir_module, func, &mut cl_ctx, &mut fb_ctx)?;
    }

    Ok(())
}

/// Translate a single Phoenix IR function into its Cranelift definition.
fn translate_function(
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    func: &IrFunction,
    cl_ctx: &mut Context,
    fb_ctx: &mut FunctionBuilderContext,
) -> Result<(), CompileError> {
    let cl_func_id = ctx.func_ids[&func.id];

    let sig = crate::abi::build_signature(&func.param_types, &func.return_type, ctx.call_conv);
    cl_ctx.func.signature = sig;

    let mut builder = FunctionBuilder::new(&mut cl_ctx.func, fb_ctx);

    let mut state = FuncState {
        value_map: HashMap::new(),
        alloca_map: HashMap::new(),
        type_map: HashMap::new(),
        current_capture_types: func.capture_types.clone(),
        enum_payload_types: HashMap::new(),
        block_map: HashMap::new(),
        gc_frame: None,
        next_var_index: 0,
    };

    // Create Cranelift blocks for each Phoenix basic block.
    for block in &func.blocks {
        let cl_block = builder.create_block();
        state.block_map.insert(block.id, cl_block);
    }

    // Set up block parameters for all blocks.
    // For the entry block, these are the function parameters.
    for block in &func.blocks {
        let cl_block = state.block_map[&block.id];
        for (vid, ir_ty) in &block.params {
            state.type_map.insert(*vid, ir_ty.clone());
            let mut vals = Vec::new();
            for &cl_ty in TypeLayout::of(ir_ty).cl_types() {
                let val = builder.append_block_param(cl_block, cl_ty);
                vals.push(val);
            }
            state.value_map.insert(*vid, vals);
        }
    }

    // Translate each block. The entry block is identified via
    // `IrFunction::entry_block()` (which encapsulates the
    // "`blocks[0]` is the entry block" convention) — comparing block
    // IDs against that means a future change to the convention only
    // needs to update one method.
    let entry_block_id = func.entry_block().id;
    for block in func.blocks.iter() {
        let cl_block = state.block_map[&block.id];
        builder.switch_to_block(cl_block);

        if block.id == entry_block_id {
            // Entry block: plan the shadow-stack frame (one slot per
            // ref-typed ValueId across the whole function), push it,
            // then root every ref-typed function parameter (which lives
            // in entry-block params).
            let slots = gc_roots::plan_frame(func);
            let frame = gc_roots::emit_frame_setup(&mut builder, ctx, slots, &mut state);
            gc_roots::emit_block_param_roots(
                &mut builder,
                ctx,
                frame.as_ref(),
                block,
                &state.value_map,
            );
            state.gc_frame = frame;
        } else {
            // Non-entry block: root any ref-typed block parameters
            // received from predecessors.
            gc_roots::emit_block_param_roots(
                &mut builder,
                ctx,
                state.gc_frame.as_ref(),
                block,
                &state.value_map,
            );
        }

        // Translate instructions.
        for inst in &block.instructions {
            let result_vid = inst.result;
            let result_type = &inst.result_type;

            let result_vals = translate_op(
                &mut builder,
                ctx,
                ir_module,
                &mut state,
                &inst.op,
                result_type,
            )?;

            if let Some(vid) = result_vid {
                state.type_map.insert(vid, result_type.clone());
                // Set GC root if this value is tracked. Take the first
                // Cranelift value (slot 0); for fat-pointer types
                // (StringRef, DynRef) that's the heap pointer.
                if let Some(&first) = result_vals.first() {
                    gc_roots::maybe_set_root(
                        &mut builder,
                        ctx,
                        state.gc_frame.as_ref(),
                        vid,
                        first,
                    );
                }
                state.value_map.insert(vid, result_vals);
                // Move alloca slot info from the VOID_SENTINEL temp key to the
                // actual result ValueId.  VOID_SENTINEL is used as a temporary
                // key because the Alloca op doesn't know its own result ValueId.
                if matches!(inst.op, Op::Alloca(_))
                    && let Some(slot_info) = state.alloca_map.remove(&VOID_SENTINEL)
                {
                    state.alloca_map.insert(vid, slot_info);
                }
                // Record concrete payload types from EnumAlloc for later inference.
                if let Op::EnumAlloc(_name, variant_idx, fields) = &inst.op
                    && !fields.is_empty()
                {
                    // Use the actual type of each field, falling back to I64
                    // only if the type is not yet known (forward references
                    // within a function — rare in practice because IR is
                    // emitted in depth-first order). This is a Strategy 4
                    // backstop: Strategy 0 (reading `EnumRef` args) already
                    // ran and preferred its result via the agreement
                    // `debug_assert` in `enum_type_inference.rs`, so the
                    // I64 here only surfaces if *every* earlier strategy
                    // also failed. If that path ever widens (e.g. new op
                    // shapes defer type resolution), this fallback is the
                    // same shape as the `okOr` payload bug — a silent
                    // corruption of multi-slot payloads — and must be
                    // replaced with a real lookup or an explicit error.
                    // Using `map` instead of `filter_map` preserves the
                    // field count so downstream consumers get the correct
                    // payload arity.
                    let field_types: Vec<IrType> = fields
                        .iter()
                        .map(|fid| state.type_map.get(fid).cloned().unwrap_or(IrType::I64))
                        .collect();
                    state
                        .enum_payload_types
                        .insert(vid, (*variant_idx, field_types));
                }
            }
        }

        // Propagate enum_payload_types through block-parameter forwarding.
        // When a Jump/Branch passes a value that has known payload types to
        // a target block, the block parameter's ValueId should inherit the
        // payload type info so downstream code can infer enum inner types
        // even when the value flows through phi nodes.
        propagate_enum_payload_types(&block.terminator, func, &mut state);

        // Translate terminator.
        control::translate_terminator(&mut builder, ctx, &block.terminator, &state, func)?;
    }

    // Seal all blocks (all predecessors are known after full translation).
    for block in state.block_map.values() {
        builder.seal_block(*block);
    }

    builder.finalize();

    // Define the function in the module.
    ctx.module
        .define_function(cl_func_id, cl_ctx)
        .map_err(|e| CompileError::new(format!("failed to define function {}: {e}", func.name)))?;

    cl_ctx.clear();

    Ok(())
}

/// Propagate `enum_payload_types` from jump/branch arguments to the target
/// block's parameter ValueIds.  This ensures that when an `EnumAlloc` value
/// flows through a phi node (e.g., `if/else` producing `Some(x)` vs `None`),
/// the block parameter inherits the payload type info so downstream methods
/// like `option_payload_type` can find it.
fn propagate_enum_payload_types(
    term: &phoenix_ir::terminator::Terminator,
    func: &IrFunction,
    state: &mut FuncState,
) {
    let targets: Vec<(&PhxBlockId, &[ValueId])> = match term {
        phoenix_ir::terminator::Terminator::Jump { target, args } => {
            vec![(target, args)]
        }
        phoenix_ir::terminator::Terminator::Branch {
            true_block,
            true_args,
            false_block,
            false_args,
            ..
        } => {
            vec![(true_block, true_args), (false_block, false_args)]
        }
        _ => return,
    };

    for (target_block, args) in targets {
        // Find the corresponding block in the IR to get its parameter ValueIds.
        let Some(block) = func.blocks.iter().find(|b| b.id == *target_block) else {
            continue;
        };
        for (arg_vid, (param_vid, _param_ty)) in args.iter().zip(block.params.iter()) {
            if let Some(payload) = state.enum_payload_types.get(arg_vid).cloned() {
                state
                    .enum_payload_types
                    .entry(*param_vid)
                    .or_insert(payload);
            }
        }
    }
}

// ── Value helpers ──────────────────────────────────────────────────

/// Get the Cranelift value(s) for a Phoenix `ValueId`.
pub(crate) fn get_val(state: &FuncState, vid: ValueId) -> Result<Vec<Value>, CompileError> {
    state
        .value_map
        .get(&vid)
        .cloned()
        .ok_or_else(|| CompileError::new(format!("undefined value {vid}")))
}

/// Get a single Cranelift value for a Phoenix `ValueId`.
///
/// Returns an error if the value maps to multiple Cranelift values (e.g. strings).
pub(crate) fn get_val1(state: &FuncState, vid: ValueId) -> Result<Value, CompileError> {
    let vals = get_val(state, vid)?;
    if vals.len() != 1 {
        return Err(CompileError::new(format!(
            "expected single value for {vid}, got {}",
            vals.len()
        )));
    }
    Ok(vals[0])
}

// ── Top-level op dispatch ──────────────────────────────────────────

/// Translate a single Phoenix IR operation to Cranelift instructions.
///
/// Dispatches to domain-specific helpers in submodules for readability.
fn translate_op(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    ir_module: &IrModule,
    state: &mut FuncState,
    op: &Op,
    result_type: &IrType,
) -> Result<Vec<Value>, CompileError> {
    match op {
        // Constants
        Op::ConstI64(_) | Op::ConstF64(_) | Op::ConstBool(_) | Op::ConstString(_) => {
            arith::translate_const(builder, ctx, op)
        }

        // Integer arithmetic
        Op::IAdd(..) | Op::ISub(..) | Op::IMul(..) | Op::IDiv(..) | Op::IMod(..) | Op::INeg(..) => {
            arith::translate_int_arith(builder, ctx, op, state)
        }

        // Float arithmetic
        Op::FAdd(..) | Op::FSub(..) | Op::FMul(..) | Op::FDiv(..) | Op::FMod(..) | Op::FNeg(..) => {
            arith::translate_float_arith(builder, op, state)
        }

        // Comparisons
        Op::IEq(..)
        | Op::INe(..)
        | Op::ILt(..)
        | Op::IGt(..)
        | Op::ILe(..)
        | Op::IGe(..)
        | Op::FEq(..)
        | Op::FNe(..)
        | Op::FLt(..)
        | Op::FGt(..)
        | Op::FLe(..)
        | Op::FGe(..)
        | Op::BoolEq(..)
        | Op::BoolNe(..)
        | Op::BoolNot(..) => arith::translate_cmp(builder, op, state),

        // String operations
        Op::StringConcat(..)
        | Op::StringEq(..)
        | Op::StringNe(..)
        | Op::StringLt(..)
        | Op::StringGt(..)
        | Op::StringLe(..)
        | Op::StringGe(..) => data::translate_string(builder, ctx, op, state),

        // Struct operations
        Op::StructAlloc(..) | Op::StructGetField(..) | Op::StructSetField(..) => {
            data::translate_struct(builder, ctx, ir_module, op, result_type, state)
        }

        // Enum operations
        Op::EnumAlloc(..) | Op::EnumDiscriminant(..) | Op::EnumGetField(..) => {
            data::translate_enum(builder, ctx, ir_module, op, result_type, state)
        }

        // Collection operations
        Op::ListAlloc(_) => {
            list_methods::translate_list_alloc(builder, ctx, op, result_type, state)
        }
        Op::MapAlloc(_) => map_methods::translate_map_alloc(builder, ctx, op, result_type, state),

        // Closure operations
        Op::ClosureAlloc(..) => calls::translate_closure_alloc(builder, ctx, op, state),
        Op::ClosureLoadCapture(env_vid, capture_idx) => calls::translate_closure_load_capture(
            builder,
            *env_vid,
            *capture_idx,
            result_type,
            state,
        ),

        // Function calls
        Op::Call(..)
        | Op::CallIndirect(..)
        | Op::BuiltinCall(..)
        | Op::UnresolvedTraitMethod(..) => {
            calls::translate_call(builder, ctx, ir_module, op, result_type, state)
        }

        Op::DynAlloc(..) | Op::UnresolvedDynAlloc(..) | Op::DynCall(..) => {
            dyn_trait::translate_dyn_op(builder, ctx, ir_module, op, state)
        }

        // Mutable variables
        Op::Alloca(..) | Op::Load(..) | Op::Store(..) => {
            mutable::translate_mutable(builder, op, state)
        }

        // Miscellaneous
        Op::Copy(v) => get_val(state, *v),
    }
}

#[cfg(test)]
mod tests {
    /// Direct construction of a `cranelift_frontend::Variable` is
    /// reserved for [`super::FuncState::next_variable`]. Every other
    /// call site must go through that helper so distinct passes
    /// (today: `gc_roots`; tomorrow: `defer`, exception unwinding, ...)
    /// can't overlap on the same index.
    ///
    /// The needle is built via `concat!` so the literal byte sequence
    /// never appears in this test source — any match in `src/translate`
    /// is therefore a real production hit, no `#[cfg(test)]` stripping
    /// required.
    ///
    /// Before counting, single-line comments (`//`, `///`, `//!`) are
    /// stripped so that a future contributor who mentions the literal
    /// `Variable::from_u32(` in a doc comment or explanatory note
    /// doesn't trip a false positive. Block comments (`/* … */`) are
    /// not stripped — the convention in this crate is line-comment
    /// only. If a block comment ever needs to contain the literal,
    /// extend the filter rather than rewrite the comment to dodge it.
    #[test]
    fn variable_construction_only_in_funcstate() {
        use std::path::Path;

        const NEEDLE: &str = concat!("Variable", "::", "from_u32", "(");

        // Drop everything from the first `//` on each line so the count
        // reflects code-only occurrences. A real `//` inside a string
        // literal would also be dropped, but no string literal in
        // `src/translate` contains the needle today, and the test's
        // failure mode (false positive → unrelated test failure) is
        // strictly less harmful than a false negative (silent rule
        // violation).
        fn strip_line_comments(src: &str) -> String {
            src.lines()
                .map(|line| match line.find("//") {
                    Some(idx) => &line[..idx],
                    None => line,
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        let translate_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/translate");
        let mod_rs = translate_dir.join("mod.rs");

        let mod_src = std::fs::read_to_string(&mod_rs).expect("translate/mod.rs unreadable");
        let mod_code = strip_line_comments(&mod_src);
        let count = mod_code.matches(NEEDLE).count();
        assert_eq!(
            count, 1,
            "exactly one direct Variable construction permitted in \
             translate/mod.rs (inside FuncState::next_variable); saw {count}",
        );

        let entries = std::fs::read_dir(&translate_dir).expect("translate dir unreadable");
        for entry in entries {
            let path = entry.expect("dir entry unreadable").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("mod.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} unreadable: {e}", path.display()));
            let code = strip_line_comments(&src);
            assert!(
                !code.contains(NEEDLE),
                "{}: direct Variable construction is reserved for \
                 FuncState::next_variable — call state.next_variable() \
                 instead. Otherwise two passes will silently clash on \
                 the same Variable index.",
                path.display(),
            );
        }
    }
}
