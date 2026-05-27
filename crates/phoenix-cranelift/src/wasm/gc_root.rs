//! Shadow-stack root emission for the wasm32-linear backend.
//!
//! Each per-function pass:
//! 1. [`assign_gc_root_slots`] walks the IR and assigns a unique
//!    shadow-stack slot to every ref-typed binding (entry params,
//!    non-entry block params, ref-result instructions).
//! 2. [`setup_gc_frame`] emits `phx_gc_push_frame(n_roots)` at
//!    function entry, allocates the frame-pointer local, and seeds
//!    entry-block params' slots via [`seed_entry_param_roots`].
//! 3. The IR-op switch in `translate.rs` calls [`emit_gc_set_root`]
//!    after every ref-producing op (blanket call at the bottom of
//!    `translate_instruction`, and again from `Op::Store` and
//!    `emit_block_param_copies` so the binding's pre-assigned slot
//!    reflects each write rather than only the definition-site value).
//! 4. [`emit_gc_pop_frame`] pops the frame before every `Return`
//!    terminator. `Unreachable` traps the program and skips the pop.
//!
//! For functions with zero ref-typed bindings (arithmetic-only
//! helpers — fibonacci is the canonical example), `assign_gc_root_slots`
//! returns an empty map and `setup_gc_frame` short-circuits before
//! allocating the frame local. The blanket `emit_gc_set_root` and
//! `emit_gc_pop_frame` helpers then no-op, leaving the emitted
//! bytecode shape unchanged.

use std::collections::HashMap;

use phoenix_ir::block::BlockId;
use phoenix_ir::instruction::{Op, ValueId};
use phoenix_ir::module::IrFunction;
use wasm_encoder::{Instruction, ValType};

use super::module_builder::ModuleBuilder;
use super::translate::FuncTranslateCtx;
use crate::error::CompileError;

/// Walk `func` and assign each ref-typed binding a unique shadow-stack
/// root slot. Entry-block params (which alias function params),
/// non-entry block params, and ref-result instruction results all get
/// slots. The total slot count is the size passed to
/// `phx_gc_push_frame` at function entry.
///
/// One slot per binding, not per write: `Op::Store` into a ref-typed
/// alloca and `Jump` / `Branch` into a ref-typed block param re-use
/// the binding's pre-assigned slot so the GC always sees the local's
/// current value. The pre-scan walks bindings in a fixed order
/// (entry-block params → non-entry block params → instructions), but
/// the order doesn't affect correctness — only the total count and
/// the per-binding uniqueness matter.
pub(super) fn assign_gc_root_slots(func: &IrFunction) -> HashMap<ValueId, u32> {
    let mut map: HashMap<ValueId, u32> = HashMap::new();
    let mut next_slot: u32 = 0;
    if let Some(entry) = func.blocks.first() {
        for (vid, ty) in &entry.params {
            if ty.is_ref_type() {
                map.insert(*vid, next_slot);
                next_slot += 1;
            }
        }
    }
    for block in func.blocks.iter().skip(1) {
        for (vid, ty) in &block.params {
            if ty.is_ref_type() {
                map.insert(*vid, next_slot);
                next_slot += 1;
            }
        }
    }
    for block in &func.blocks {
        for instr in &block.instructions {
            if let Some(vid) = instr.result
                && instr.result_type.is_ref_type()
            {
                map.insert(vid, next_slot);
                next_slot += 1;
            }
        }
    }
    map
}

/// Set up the shadow-stack frame at function entry: assign slots,
/// allocate a frame-pointer local, emit `phx_gc_push_frame(n_roots)`,
/// and seed the entry-block params' slots with their already-bound
/// local values. No-op (no instructions emitted, no local allocated)
/// when the function has zero ref-typed bindings.
pub(super) fn setup_gc_frame(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    func: &IrFunction,
) -> Result<(), CompileError> {
    // The seeding loop below assumes `func.blocks[0]` is the entry
    // block — the same invariant `translate_multi_block` relies on
    // for its br_table dispatch. Re-assert it here so a future IR
    // refactor that reorders blocks surfaces a localized panic before
    // we silently seed a non-entry block's params.
    debug_assert!(
        func.blocks
            .first()
            .map(|blk| blk.id == BlockId(0))
            .unwrap_or(true),
        "wasm32-linear: setup_gc_frame expects func.blocks[0].id == BlockId(0), \
         got {:?} (internal compiler bug — IR builder invariant violated)",
        func.blocks.first().map(|blk| blk.id),
    );
    let slot_map = assign_gc_root_slots(func);
    if slot_map.is_empty() {
        return Ok(());
    }
    let n_roots = slot_map.len();
    // `phx_gc_push_frame` takes a `usize` n_roots, which on wasm32 is
    // an i32 argument. Pinning the cast here keeps a wildly oversized
    // function (>2^31 ref bindings — practically impossible, but the
    // cast is silent so worth pinning) from quietly wrapping into a
    // negative value the runtime would reject as out-of-range.
    debug_assert!(
        n_roots <= i32::MAX as usize,
        "wasm32-linear: function `{}` has {n_roots} ref-typed bindings; \
         phx_gc_push_frame's i32 argument can't represent that (internal \
         compiler bug — IR is preposterously large)",
        func.name,
    );
    let push_frame_idx = b.require_phx_func("phx_gc_push_frame")?;
    let frame_local = ctx.allocate_temp_local(ValType::I32);
    ctx.emit(Instruction::I32Const(n_roots as i32));
    ctx.emit(Instruction::Call(push_frame_idx));
    ctx.emit(Instruction::LocalSet(frame_local));
    ctx.set_gc_frame_local(frame_local);
    ctx.install_gc_root_slot_map(slot_map);
    seed_entry_param_roots(ctx, b, func)
}

/// Write each ref-typed entry-block param's already-bound function-
/// parameter local into its shadow-stack slot. Without this, a function
/// like `fn f(s: String) { ...allocate... print(s) }` could see `s`
/// collected if the alloc fires before any later set_root touches the
/// entry-param slot. Runs once at function-entry after the frame is
/// pushed and `ctx.gc_root_slot_for` is populated.
///
/// Lookups below use `ok_or_else` rather than `?`: a missing entry
/// means an internal-bug shape (`assign_gc_root_slots` just populated
/// the slot map; `FuncTranslateCtx::new` just bound every entry-block
/// param), and `?`-ing it away would silently leave the slot at null —
/// exactly the miscompile the seeding exists to prevent. Surface it as
/// a CompileError so a future refactor that breaks the invariant gets a
/// clean diagnostic.
fn seed_entry_param_roots(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    func: &IrFunction,
) -> Result<(), CompileError> {
    let Some(entry) = func.blocks.first() else {
        return Ok(());
    };
    // Collect the (slot, local) pairs first so the immutable borrow of
    // the ctx is released before the mutating `emit_gc_set_root_raw`
    // call.
    let mut seeds: Vec<(u32, u32)> = Vec::new();
    for (vid, ty) in &entry.params {
        if !ty.is_ref_type() {
            continue;
        }
        let slot = ctx.gc_root_slot_of(*vid).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-linear: entry-block ref param {vid:?} has no slot \
                 in `gc_root_slot_for` (internal compiler bug — \
                 `assign_gc_root_slots` should have allocated one)"
            ))
        })?;
        let root_local = ctx.binding_root_local(*vid).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-linear: entry-block ref param {vid:?} has no \
                 binding (internal compiler bug — `FuncTranslateCtx::new` \
                 should have bound all entry params)"
            ))
        })?;
        seeds.push((slot, root_local));
    }
    for (slot, root_local) in seeds {
        emit_gc_set_root_raw(ctx, b, slot, root_local)?;
    }
    Ok(())
}

/// Emit `phx_gc_set_root(frame, slot, root_value_local)` if a frame
/// has been allocated for this function. No-op otherwise.
///
/// `vid` is looked up in `ctx.gc_root_slot_for`; if the binding has
/// no assigned slot (most commonly because the result type isn't a
/// ref), this is also a no-op — callers can blindly call this after
/// every binding-producing instruction and the helper filters.
///
/// The root value comes from the binding's *first* local (multi-slot
/// `StringRef` bindings use slot[0] which holds the i32 ptr; slot[1]
/// is the i32 length, not a pointer). Data-section pointers from
/// `Op::ConstString` are rooted too — the runtime's mark phase checks
/// each slot value against its allocation registry and ignores
/// non-registered pointers, so passing a data-section offset is safe.
pub(super) fn emit_gc_set_root(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    vid: ValueId,
) -> Result<(), CompileError> {
    if ctx.gc_frame_local().is_none() {
        return Ok(());
    }
    let Some(slot) = ctx.gc_root_slot_of(vid) else {
        return Ok(());
    };
    let Some(root_local) = ctx.binding_root_local(vid) else {
        return Ok(());
    };
    emit_gc_set_root_raw(ctx, b, slot, root_local)
}

/// Lower-level shadow-stack write: emit the three-arg
/// `phx_gc_set_root(frame, slot, root_local_value)` call without
/// going through the per-vid lookup. Used by `setup_gc_frame` to seed
/// entry-block params and (transitively, via [`emit_gc_set_root`]) by
/// `emit_block_param_copies` and `Op::Store` to re-root a binding
/// when the value providing the update is read from a known local.
///
/// **Contract asymmetry vs. [`emit_gc_set_root`] / [`emit_gc_pop_frame`]:**
/// this helper panics on `gc_frame_local == None` rather than no-op'ing
/// because every caller has already proven a frame exists — either by
/// being downstream of [`emit_gc_set_root`]'s frame-presence check
/// (which short-circuits before reaching here), or by sitting inside
/// the `slot_map.is_empty() → return` early-out in [`setup_gc_frame`]
/// (which guarantees a frame). The high-level helpers no-op because
/// they're called blindly by per-op codegen that doesn't know whether
/// the function has any ref bindings; this raw helper is only invoked
/// from sites that have already discharged that check, and a missing
/// frame at this point is an internal compiler bug worth panicking on.
fn emit_gc_set_root_raw(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    slot: u32,
    root_local: u32,
) -> Result<(), CompileError> {
    let frame_local = ctx
        .gc_frame_local()
        .expect("emit_gc_set_root_raw called without a frame allocated");
    let set_root_idx = b.require_phx_func("phx_gc_set_root")?;
    ctx.emit(Instruction::LocalGet(frame_local));
    ctx.emit(Instruction::I32Const(slot as i32));
    ctx.emit(Instruction::LocalGet(root_local));
    ctx.emit(Instruction::Call(set_root_idx));
    Ok(())
}

/// Emit `phx_gc_pop_frame(frame)` before a function-level exit. No-op
/// if the function has no allocated frame. Called from the terminator
/// translator before every `Return` — the frame must be popped on
/// *every* exit path so the runtime's per-thread frame-counter stays
/// in lockstep with the actual stack depth. `Unreachable` traps and
/// skips the pop (no further execution observes the frame).
pub(super) fn emit_gc_pop_frame(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
) -> Result<(), CompileError> {
    let Some(frame_local) = ctx.gc_frame_local() else {
        return Ok(());
    };
    let pop_frame_idx = b.require_phx_func("phx_gc_pop_frame")?;
    ctx.emit(Instruction::LocalGet(frame_local));
    ctx.emit(Instruction::Call(pop_frame_idx));
    Ok(())
}

// --- Nested (ad-hoc) shadow-stack frames ------------------------------
//
// The helpers above manage the *function-level* frame keyed off
// `ctx.gc_frame_local()` and the per-vid slot map. The helpers below
// manage an additional, manually-scoped frame whose pointer the caller
// holds in an explicit local. List functional methods that materialize
// ref-typed intermediates with no IR `ValueId` of their own (e.g.
// `flatMap`'s inner list, `sortBy`'s `key` element) push such a frame
// around their inner loop, root those intermediates into it, and pop it
// when the loop is done — so the comparator/closure's allocations can't
// sweep a live-but-vid-less buffer mid-method.

/// Push a fresh shadow-stack frame of `n_roots` slots and return the
/// temp local holding its frame pointer. The caller is responsible for
/// a matching [`emit_gc_pop_frame_at`] on every exit path out of the
/// scope the frame guards.
pub(super) fn emit_gc_push_frame_at(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    n_roots: u32,
) -> Result<u32, CompileError> {
    let push_idx = b.require_phx_func("phx_gc_push_frame")?;
    let frame_local = ctx.allocate_temp_local(ValType::I32);
    ctx.emit(Instruction::I32Const(n_roots as i32));
    ctx.emit(Instruction::Call(push_idx));
    ctx.emit(Instruction::LocalSet(frame_local));
    Ok(frame_local)
}

/// Emit `phx_gc_set_root(frame_local, slot, value_local)` against an
/// explicit ad-hoc frame. Unlike [`emit_gc_set_root`], this takes the
/// frame pointer and root value as raw locals rather than resolving a
/// vid — for intermediates that have no IR `ValueId`.
pub(super) fn emit_gc_set_root_at(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    frame_local: u32,
    slot: u32,
    value_local: u32,
) -> Result<(), CompileError> {
    let set_root_idx = b.require_phx_func("phx_gc_set_root")?;
    ctx.emit(Instruction::LocalGet(frame_local));
    ctx.emit(Instruction::I32Const(slot as i32));
    ctx.emit(Instruction::LocalGet(value_local));
    ctx.emit(Instruction::Call(set_root_idx));
    Ok(())
}

/// Pop an ad-hoc frame whose pointer is held in `frame_local`.
pub(super) fn emit_gc_pop_frame_at(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    frame_local: u32,
) -> Result<(), CompileError> {
    let pop_idx = b.require_phx_func("phx_gc_pop_frame")?;
    ctx.emit(Instruction::LocalGet(frame_local));
    ctx.emit(Instruction::Call(pop_idx));
    Ok(())
}

/// True if `op`'s ref-typed result needs its own shadow-stack root
/// entry on the way out of `translate_instruction`. The check is not
/// "did this op allocate?" — `Op::Load` aliases the alloca's pointer
/// rather than producing a fresh one, but the loaded vid is still a
/// distinct binding whose root must survive a subsequent `Op::Store`
/// overwriting the original slot, so it returns `true`.
///
/// The function is intentionally a tiny *skip-list* of ref-result
/// ops that are statically known not to need rooting:
///
/// - [`Op::ConstString`] yields a data-section offset; the runtime's
///   mark phase filters non-registered pointers, so rooting is wasted
///   work.
/// - [`Op::Alloca`]'s locals are zero-initialized — already null in
///   the slot post-`phx_gc_push_frame`. The first [`Op::Store`] into
///   the alloca emits its own `emit_gc_set_root` with the actual
///   value, so the seed write here would be redundant.
///
/// Every other ref-result op conservatively gets rooted. The skip-
/// list shape (rather than an explicit "yes, root me" enumeration)
/// means a future ref-result op (`Op::ListAlloc`, etc.) is rooted by
/// default without a manual update here.
pub(super) fn op_produces_heap_pointer(op: &Op) -> bool {
    !matches!(op, Op::ConstString(_) | Op::Alloca(_))
}
