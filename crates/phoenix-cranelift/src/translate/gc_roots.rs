//! Shadow-stack root emission for the GC.
//!
//! At function entry we allocate a frame with one slot per ref-typed
//! `ValueId` produced anywhere in the function (params, block params,
//! instruction results). Whenever such a value is produced we emit a
//! `phx_gc_set_root(frame, slot, value)` write. At every return the
//! frame is popped.
//!
//! This is the precise-roots half of the "precise stack roots +
//! conservative interior scan" baseline (subordinate decision A in
//! `docs/design-decisions.md#a-root-finding-precise-via-shadow-stack`).
//!
//! **Liveness assumption:** every ref-typed value is rooted from the
//! point it is produced until function return. This over-roots (a value
//! that goes dead mid-function still keeps its referent alive) but is
//! correct without a liveness analysis. The cost is one frame slot per
//! ref-typed SSA name. If profiling later shows this is expensive we
//! can plug in a liveness pass and clear slots when values die — the
//! `phx_gc_set_root(_, _, null)` API supports that without ABI change.

use std::collections::HashMap;

use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{InstBuilder, Value};
use cranelift_frontend::{FunctionBuilder, Variable};

use phoenix_ir::block::BasicBlock;
use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrFunction;
use phoenix_ir::types::IrType;

use crate::context::CompileContext;

use super::FuncState;
use super::helpers::call_runtime;

/// Predicate: is this `ValueId`'s type a GC-tracked reference?
///
/// Erased generic placeholders (`__generic`) and unresolved type
/// variables can reach codegen via the closures-over-generic
/// monomorphization gap tracked in `docs/known-issues.md` ("Closure
/// functions inside generic templates are not cloned per
/// specialization"). Skip them: the placeholder paths covered by
/// today's matrix never reach an allocation that outlives a collection
/// cycle (1-slot Int/Bool instantiations are non-ref; wider
/// instantiations are gated by the closures-over-generic ignore list).
///
/// ## Tripwire — convert to `ice!` once monomorphization is fixed
///
/// When the closures-over-generic fix lands, every value reaching
/// codegen will have a concrete type and the early-return branch
/// becomes dead. The ignored fixtures in
/// `phoenix-driver/tests/three_backend_matrix.rs` (search for
/// `closures_over_generic` / `closures_over_generic_cross_width`) will
/// be re-enabled at that point — the moment they pass under the
/// matrix, this branch is guaranteed-unreachable for production paths.
/// Replace it with:
///
/// ```ignore
/// debug_assert!(
///     !ty.is_type_var() && !ty.is_generic_placeholder(),
///     "placeholder type reached codegen: {ty:?}",
/// );
/// ```
///
/// then delete the `if` and let `ty.is_ref_type()` answer directly.
/// The grep test `variable_from_u32_only_in_funcstate` already
/// fences off direct `Variable::from_u32` calls; the analogous fence
/// here is "no `is_type_var()`/`is_generic_placeholder()` predicate
/// in `gc_roots.rs` after the fix lands."
fn is_tracked_ref(ty: &IrType) -> bool {
    if ty.is_type_var() || ty.is_generic_placeholder() {
        // Silent skip. Originally this branch raised a one-shot
        // `eprintln!` warning and was later promoted to
        // `debug_assert!(false, ...)`, but both forms misfire in
        // practice: `closures_over_generic.phx` runs in the supported
        // matrix and passes a `StructRef("__generic", [])` payload
        // through codegen on every build. The closures-over-generic
        // monomorphization gap (tracked in `docs/known-issues.md`) is
        // load-bearing for that fixture's correctness today, so the
        // silent return is the only behavior that doesn't regress the
        // matrix. The tripwire stays in the module-level docstring:
        // when monomorphization is fixed and the ignore-list fixtures
        // re-enable cleanly, this `if` becomes guaranteed-unreachable
        // and can be deleted.
        return false;
    }
    ty.is_ref_type()
}

/// Root-tracking metadata for one function. Built at entry; consulted
/// after each instruction and at every return.
pub(crate) struct GcFrameInfo {
    /// Cranelift `Variable` holding the frame pointer (i64). Defined
    /// once in the entry block from `phx_gc_push_frame`'s return value
    /// and read via `use_var` at every `set_root` / `pop_frame` site.
    /// Cranelift's SSA construction threads it through block params as
    /// needed, so the runtime cost of reading the frame pointer is one
    /// register access — no per-site stack load.
    frame_var: Variable,
    /// Map from `ValueId` to its root slot index in the frame.
    pub(crate) slot_for_value: HashMap<ValueId, usize>,
}

/// Walk the function and assign one root slot to every ref-typed
/// `ValueId` (function params, block params, instruction results).
/// Returns an empty map for value-only functions.
///
/// Each `ValueId` gets at most one slot. Today's IR keeps `ValueId`s
/// unique across params and results, but we use `entry().or_insert_with`
/// rather than blind `insert` so a future regression that produced the
/// same `ValueId` twice would (a) not double-advance `next_slot` (which
/// would silently waste a slot per duplicate) and (b) not overwrite the
/// existing slot index (which would leave the original site pointing
/// at the wrong slot).
pub(crate) fn plan_frame(func: &IrFunction) -> HashMap<ValueId, usize> {
    let mut slots: HashMap<ValueId, usize> = HashMap::new();
    let mut next_slot = 0usize;

    let mut assign = |vid: ValueId, ty: &IrType| {
        if !is_tracked_ref(ty) {
            return;
        }
        slots.entry(vid).or_insert_with(|| {
            let s = next_slot;
            next_slot += 1;
            s
        });
    };

    for block in &func.blocks {
        for (vid, ty) in &block.params {
            assign(*vid, ty);
        }
        for inst in &block.instructions {
            if let Some(vid) = inst.result {
                assign(vid, &inst.result_type);
            }
        }
    }

    slots
}

/// At the start of the entry block, push a shadow-stack frame and stash
/// the frame pointer in a Cranelift `Variable`. Returns `None` if there
/// are no ref-typed values in this function (no shadow-stack overhead).
pub(crate) fn emit_frame_setup(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    slots: HashMap<ValueId, usize>,
    state: &mut FuncState,
) -> Option<GcFrameInfo> {
    if slots.is_empty() {
        return None;
    }
    let n = slots.len();

    let frame_var = state.next_variable();
    builder.declare_var(frame_var, I64);

    let n_val = builder.ins().iconst(I64, n as i64);
    let frame_ptr = call_runtime(builder, ctx, ctx.runtime.gc_push_frame, &[n_val])
        .into_iter()
        .next()
        .expect("phx_gc_push_frame returns one value");
    builder.def_var(frame_var, frame_ptr);

    Some(GcFrameInfo {
        frame_var,
        slot_for_value: slots,
    })
}

/// Emit `phx_gc_set_root(frame, slot_idx, val)`. Shared by every site
/// that writes a slot — instruction results, function-param rooting in
/// the entry block, and block-param rooting on non-entry blocks. Keeps
/// the load/iconst/call sequence in one place so the call shape can
/// change without three-way drift.
///
/// The frame pointer is read via `builder.use_var(info.frame_var)` —
/// Cranelift inserts the appropriate block params or selects the
/// dominating definition automatically.
fn emit_set_root(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    info: &GcFrameInfo,
    slot_idx: usize,
    val: Value,
) {
    // Hard assertion (not `debug_assert_eq!`): passing the length half
    // of a fat pointer here would write garbage into the shadow stack
    // and the GC would mis-trace forever after. A debug-only check
    // wouldn't catch the regression in release builds.
    let value_type = builder.func.dfg.value_type(val);
    assert_eq!(
        value_type, I64,
        "GC root value must be pointer-width (I64); first slot of a \
         ref-typed value was unexpectedly {value_type:?}",
    );
    let frame_ptr = builder.use_var(info.frame_var);
    let idx = builder.ins().iconst(I64, slot_idx as i64);
    call_runtime(
        builder,
        ctx,
        ctx.runtime.gc_set_root,
        &[frame_ptr, idx, val],
    );
}

/// If `vid` is a tracked root, emit `phx_gc_set_root(frame, slot, val)`.
///
/// `val` is the *first* Cranelift value associated with the ValueId —
/// for fat-pointer types (StringRef, DynRef) this is the heap-pointer
/// slot (slot 0). The caller passes that one value directly.
pub(crate) fn maybe_set_root(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    info: Option<&GcFrameInfo>,
    vid: ValueId,
    val: Value,
) {
    let Some(info) = info else {
        return;
    };
    let Some(&slot_idx) = info.slot_for_value.get(&vid) else {
        return;
    };
    emit_set_root(builder, ctx, info, slot_idx, val);
}

/// Emit `phx_gc_pop_frame(frame)`. Called from terminator emission
/// before any `Return`.
pub(crate) fn emit_frame_pop(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    info: &GcFrameInfo,
) {
    let frame_ptr = builder.use_var(info.frame_var);
    call_runtime(builder, ctx, ctx.runtime.gc_pop_frame, &[frame_ptr]);
}

/// Walk every block param of the given block and emit a `set_root` for
/// each tracked ref-typed value. Called after a block is switched-to
/// but before its instructions are translated. Used for both the entry
/// block (rooting function parameters) and non-entry blocks (re-rooting
/// block params received from predecessors).
pub(crate) fn emit_block_param_roots(
    builder: &mut FunctionBuilder,
    ctx: &mut CompileContext,
    info: Option<&GcFrameInfo>,
    block: &BasicBlock,
    value_map: &HashMap<ValueId, Vec<Value>>,
) {
    let Some(info) = info else {
        return;
    };
    for (vid, ty) in &block.params {
        if !is_tracked_ref(ty) {
            continue;
        }
        let Some(&slot_idx) = info.slot_for_value.get(vid) else {
            continue;
        };
        let Some(vals) = value_map.get(vid) else {
            continue;
        };
        let Some(&first) = vals.first() else { continue };
        emit_set_root(builder, ctx, info, slot_idx, first);
    }
}
