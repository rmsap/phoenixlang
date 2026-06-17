//! §Phase-2.4-close open-addressing hash index for the wasm32-gc `Map`.
//!
//! The `$idx` array (the map's 4th field, see [`super::maps`]) is a
//! power-of-two open-addressing table holding *slot indices* into
//! `$keys`/`$vals` (or [`IDX_EMPTY`] for an empty slot). It makes
//! `get`/`contains` and construction-dedup O(1)/O(n) instead of
//! O(n)/O(n²). The hash is **not observable** — only key-equality is —
//! so any consistent hash works (we do *not* match native's FNV-1a).
//! Insertion order is still carried structurally by `$keys`/`$vals`;
//! the index is a pure acceleration structure.
//!
//! Split out of [`super::maps`] because it is a self-contained toolkit
//! (sizing + hashing + probe primitives) with no map-op bookkeeping of
//! its own — `maps.rs` owns the K.9 lowerings and the `$keys`/`$vals`
//! representation; this module owns *how* the index accelerates them.
//! It reaches back into `maps` for the shared `concrete_ref` / `bump` /
//! `emit_key_eq` primitives.

use phoenix_ir::types::IrType;
use wasm_encoder::{BlockType, Instruction, ValType};

use crate::error::CompileError;

use super::maps::{bump, concrete_ref, emit_key_eq};
use super::module_builder::{ModuleBuilder, STR_DATA, STR_LEN, STR_OFFSET};
use super::translate::{FuncCtx, single_slot};

/// Empty-slot sentinel in `$idx`.
const IDX_EMPTY: i32 = -1;

/// Smallest power of two ≥ `max(8, 2 * n)`. `idxlen >= 8` and
/// `idxlen >= 2*n` keeps the table at ≤50% load. Used to size every
/// freshly-built `$idx` from a compile-time-known length (the literal).
///
/// `n` is a map-literal pair count, which is bounded by the source size
/// and stays many orders of magnitude below `i32::MAX / 2`, so the `2 *
/// n` widening and the doubling loop below never overflow in practice.
/// (A pathological `n` near `i32::MAX / 2` would overflow `want`
/// negative and return `8`, an under-sized table → infinite probe loop;
/// the caller's input scale rules that out.) The `i64` arithmetic for
/// `want` keeps the multiply itself safe regardless.
pub(super) fn idx_len_for(n: i32) -> i32 {
    let want = (2i64 * n as i64).max(8);
    let mut cap: i64 = 8;
    while cap < want {
        cap <<= 1;
    }
    cap as i32
}

/// Runtime `idx_len_for(n)`: emit `cap = 8; while cap < max(8, 2*n)
/// { cap <<= 1 }`, leaving an i32 in a fresh scratch local (returned).
/// Used by the copy-on-write `set` / `remove` and `freeze`, where the
/// entry count is only known at run time.
pub(super) fn emit_idx_len_for(ctx: &mut FuncCtx, n: u32) -> u32 {
    // two_n = 2 * n ; want = max(8, two_n)
    let two_n = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(n));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Shl);
    ctx.emit(Instruction::LocalSet(two_n));
    let want = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(two_n));
    ctx.emit(Instruction::I32Const(8));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    ctx.emit(Instruction::LocalGet(two_n));
    ctx.emit(Instruction::Else);
    ctx.emit(Instruction::I32Const(8));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::LocalSet(want));
    // cap = 8; while cap < want { cap <<= 1 }
    let cap = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::I32Const(8));
    ctx.emit(Instruction::LocalSet(cap));
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(cap));
    ctx.emit(Instruction::LocalGet(want));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1));
    ctx.emit(Instruction::LocalGet(cap));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Shl);
    ctx.emit(Instruction::LocalSet(cap));
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);
    cap
}

/// Compute a consistent hash of the key in `key_local`, leaving an i32
/// on the stack. Per-type dispatch mirrors [`emit_key_eq`]:
/// * Int  — fold the i64 (`xor(hi, lo)`) then a multiply-shift mix.
/// * Bool — the i32 value (mixed identically to keep distribution sane).
/// * Float — `i64.reinterpret_f64` then the Int path (so `-0.0`/`+0.0`
///   hash differently and `NaN` is self-consistent, matching the
///   byte-wise equality used for `Map<Float, _>`).
/// * String — FNV-1a over the `$bytes[offset..offset+len]` window.
///
/// The hash need not agree across backends — only key *equality* is
/// observable — so this is free to pick any stable function.
fn emit_map_hash(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    key_local: u32,
    key_ir: &IrType,
) -> Result<(), CompileError> {
    match key_ir {
        IrType::I64 => {
            emit_mix_i64(ctx, |c| c.emit(Instruction::LocalGet(key_local)));
        }
        IrType::Bool => {
            // Widen the i32 to i64 and run the same mix.
            emit_mix_i64(ctx, |c| {
                c.emit(Instruction::LocalGet(key_local));
                c.emit(Instruction::I64ExtendI32U);
            });
        }
        IrType::F64 => {
            emit_mix_i64(ctx, |c| {
                c.emit(Instruction::LocalGet(key_local));
                c.emit(Instruction::I64ReinterpretF64);
            });
        }
        IrType::StringRef => {
            emit_str_hash(ctx, b, key_local)?;
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `Map` key type `{other:?}` has no hash — the K.9 \
                 slice covers Int / Float / Bool / String keys (internal \
                 compiler bug; equality dispatch should have rejected it first)"
            )));
        }
    }
    Ok(())
}

/// Push an i64 via `push_i64`, fold it to an i32, and mix. Leaves an
/// i32 on the stack. `h64 = x ^ (x >>> 32)`; `h = (i32)h64 * 0x9E3779B1`;
/// `h = h ^ (h >>> 16)`.
fn emit_mix_i64(ctx: &mut FuncCtx, push_i64: impl Fn(&mut FuncCtx)) {
    // x ^ (x >>> 32)
    push_i64(ctx);
    push_i64(ctx);
    ctx.emit(Instruction::I64Const(32));
    ctx.emit(Instruction::I64ShrU);
    ctx.emit(Instruction::I64Xor);
    // fold to i32
    ctx.emit(Instruction::I32WrapI64);
    // * golden-ratio constant
    ctx.emit(Instruction::I32Const(0x9E37_79B1u32 as i32));
    ctx.emit(Instruction::I32Mul);
    // h ^ (h >>> 16) — split via a scratch to avoid recomputing the mul
    let h = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalSet(h));
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::I32Const(16));
    ctx.emit(Instruction::I32ShrU);
    ctx.emit(Instruction::I32Xor);
}

/// FNV-1a over a `$string`'s byte window. Leaves an i32 on the stack.
/// `h = 0x811C9DC5; for b in bytes { h = (h ^ b) * 0x01000193 }`.
fn emit_str_hash(ctx: &mut FuncCtx, b: &ModuleBuilder, s_local: u32) -> Result<(), CompileError> {
    let string_idx = b.require_string_type_idx()?;
    let bytes_idx = b.require_bytes_type_idx()?;
    let data = ctx.scratch_local(concrete_ref(bytes_idx));
    let off = ctx.scratch_local(ValType::I32);
    let len = ctx.scratch_local(ValType::I32);
    let h = ctx.scratch_local(ValType::I32);
    let i = ctx.scratch_local(ValType::I32);
    // data = s.$data; off = s.$offset; len = s.$len
    ctx.emit(Instruction::LocalGet(s_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_DATA,
    });
    ctx.emit(Instruction::LocalSet(data));
    ctx.emit(Instruction::LocalGet(s_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_OFFSET,
    });
    ctx.emit(Instruction::LocalSet(off));
    ctx.emit(Instruction::LocalGet(s_local));
    ctx.emit(Instruction::StructGet {
        struct_type_index: string_idx,
        field_index: STR_LEN,
    });
    ctx.emit(Instruction::LocalSet(len));
    // h = FNV offset basis; i = 0
    ctx.emit(Instruction::I32Const(0x811C_9DC5u32 as i32));
    ctx.emit(Instruction::LocalSet(h));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));
    // while i < len { h = (h ^ data[off+i]) * FNV_prime; i++ }
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32GeU);
    ctx.emit(Instruction::BrIf(1));
    // h = (h ^ data[off+i]) * 0x01000193
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::LocalGet(data));
    ctx.emit(Instruction::LocalGet(off));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::ArrayGetU(bytes_idx));
    ctx.emit(Instruction::I32Xor);
    ctx.emit(Instruction::I32Const(0x0100_0193));
    ctx.emit(Instruction::I32Mul);
    ctx.emit(Instruction::LocalSet(h));
    bump(ctx, i);
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::LocalGet(h));
    Ok(())
}

/// Open-addressing lookup of `probe` against `idx_arr` (slot indices
/// into `keys_arr`/`$vals`). Sets `found` to the matching `$keys` slot,
/// or `IDX_EMPTY` (-1) on a miss. `idxlen` must be a power of two
/// (mask = `idxlen - 1`). `h` and `slot` are caller-owned i32 scratch
/// (reused across a loop of lookups, e.g. the literal/freeze builders).
///
/// `h = hash(probe) & mask;`
/// `loop { slot = idx[h];`
/// `       if slot == -1 { found = -1; break }`
/// `       if keys[slot] eq probe { found = slot; break }`
/// `       h = (h + 1) & mask }`
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_index_lookup(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    keys_arr: u32,
    arr_k: u32,
    idx_arr: u32,
    arr_idx: u32,
    idxlen: u32,
    probe: u32,
    key_ir: &IrType,
    found: u32,
    h: u32,
    slot: u32,
) -> Result<(), CompileError> {
    // mask = idxlen - 1 (idxlen is a power of two)
    let mask = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(idxlen));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Sub);
    ctx.emit(Instruction::LocalSet(mask));
    // h = hash(probe) & mask
    emit_map_hash(ctx, b, probe, key_ir)?;
    ctx.emit(Instruction::LocalGet(mask));
    ctx.emit(Instruction::I32And);
    ctx.emit(Instruction::LocalSet(h));
    // found = -1 (default; overwritten on a hit)
    ctx.emit(Instruction::I32Const(IDX_EMPTY));
    ctx.emit(Instruction::LocalSet(found));
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    // slot = idx[h]
    ctx.emit(Instruction::LocalGet(idx_arr));
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::ArrayGet(arr_idx));
    ctx.emit(Instruction::LocalSet(slot));
    // if slot == -1 { found = -1; break } (found already -1)
    ctx.emit(Instruction::LocalGet(slot));
    ctx.emit(Instruction::I32Const(IDX_EMPTY));
    ctx.emit(Instruction::I32Eq);
    ctx.emit(Instruction::BrIf(1));
    // cur = keys[slot]; if key_eq(cur, probe) { found = slot; break }
    let cur = ctx.scratch_local(single_slot(key_ir, b, "map key")?);
    ctx.emit(Instruction::LocalGet(keys_arr));
    ctx.emit(Instruction::LocalGet(slot));
    ctx.emit(Instruction::ArrayGet(arr_k));
    ctx.emit(Instruction::LocalSet(cur));
    emit_key_eq(ctx, b, key_ir, cur, probe)?;
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(slot));
    ctx.emit(Instruction::LocalSet(found));
    ctx.emit(Instruction::Br(2)); // break the outer block
    ctx.emit(Instruction::End);
    // h = (h + 1) & mask
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalGet(mask));
    ctx.emit(Instruction::I32And);
    ctx.emit(Instruction::LocalSet(h));
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);
    Ok(())
}

/// Store `slot_val` into the index slot a just-completed
/// [`emit_index_lookup`] miss left in `h`. On a miss that lookup advances
/// `h` to the probe's first empty home slot — exactly where the new entry
/// belongs — so the append path records it with a single `array.set`, no
/// re-hash and no re-probe.
///
/// **Contract:** the caller must have run [`emit_index_lookup`] for the
/// same key into the same `h`, taken the `found < 0` (miss) branch, and
/// not mutated `idx_arr` since. Use [`emit_index_insert`] instead when no
/// prior lookup established `h` (e.g. [`emit_build_idx`] over keys already
/// known unique).
pub(super) fn emit_index_store_at(
    ctx: &mut FuncCtx,
    idx_arr: u32,
    arr_idx: u32,
    h: u32,
    slot_val: u32,
) {
    ctx.emit(Instruction::LocalGet(idx_arr));
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::LocalGet(slot_val));
    ctx.emit(Instruction::ArraySet(arr_idx));
}

/// Insert `slot_val` (a `$keys`/`$vals` slot index) into `idx_arr` for
/// the key in `probe`: hash `probe`, probe linearly from `hash & mask` to
/// the first empty slot, and store `slot_val` there. For the append path
/// after an [`emit_index_lookup`] miss, prefer [`emit_index_store_at`],
/// which reuses the `h` that lookup already left at the empty home slot.
/// This full form is for callers with no preceding lookup —
/// [`emit_build_idx`] rebuilding an index over keys already known unique.
/// `h` is caller-owned i32 scratch.
#[allow(clippy::too_many_arguments)]
fn emit_index_insert(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    idx_arr: u32,
    arr_idx: u32,
    idxlen: u32,
    probe: u32,
    slot_val: u32,
    key_ir: &IrType,
    h: u32,
) -> Result<(), CompileError> {
    let mask = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(idxlen));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Sub);
    ctx.emit(Instruction::LocalSet(mask));
    emit_map_hash(ctx, b, probe, key_ir)?;
    ctx.emit(Instruction::LocalGet(mask));
    ctx.emit(Instruction::I32And);
    ctx.emit(Instruction::LocalSet(h));
    // while idx[h] != -1 { h = (h+1) & mask }
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(idx_arr));
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::ArrayGet(arr_idx));
    ctx.emit(Instruction::I32Const(IDX_EMPTY));
    ctx.emit(Instruction::I32Eq);
    ctx.emit(Instruction::BrIf(1)); // empty → home found
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalGet(mask));
    ctx.emit(Instruction::I32And);
    ctx.emit(Instruction::LocalSet(h));
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);
    // idx[h] = slot_val
    ctx.emit(Instruction::LocalGet(idx_arr));
    ctx.emit(Instruction::LocalGet(h));
    ctx.emit(Instruction::LocalGet(slot_val));
    ctx.emit(Instruction::ArraySet(arr_idx));
    Ok(())
}

/// `array.new` an `$arr_idx` of length `idxlen` filled with `IDX_EMPTY`
/// (-1), returning its local. `array.new` (not `_default`) so every slot
/// starts at -1 rather than 0 (slot 0 is a valid `$keys` index).
pub(super) fn emit_new_idx(ctx: &mut FuncCtx, arr_idx: u32, idxlen: u32) -> u32 {
    let idx = ctx.scratch_local(concrete_ref(arr_idx));
    ctx.emit(Instruction::I32Const(IDX_EMPTY));
    ctx.emit(Instruction::LocalGet(idxlen));
    ctx.emit(Instruction::ArrayNew(arr_idx));
    ctx.emit(Instruction::LocalSet(idx));
    idx
}

/// Build a fresh `$idx` of length `idxlen` and populate it by
/// hash-inserting slot `i` for each `keys_arr[0..len]`. The keys are
/// assumed already deduped (each present once, in insertion order — the
/// copy-on-write `set`/`remove` source), so no equality check is needed:
/// every entry gets its own home slot. Returns the populated idx local.
/// `idxlen` must be a power of two and ≥ `2 * len`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_build_idx(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    keys_arr: u32,
    arr_k: u32,
    arr_idx: u32,
    len: u32,
    idxlen: u32,
    key_ir: &IrType,
) -> Result<u32, CompileError> {
    let idx_arr = emit_new_idx(ctx, arr_idx, idxlen);
    let i = ctx.scratch_local(ValType::I32);
    let cur = ctx.scratch_local(single_slot(key_ir, b, "map key")?);
    let h = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1));
    // cur = keys[i]; idx-insert (i) for cur
    ctx.emit(Instruction::LocalGet(keys_arr));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::ArrayGet(arr_k));
    ctx.emit(Instruction::LocalSet(cur));
    emit_index_insert(ctx, b, idx_arr, arr_idx, idxlen, cur, i, key_ir, h)?;
    bump(ctx, i);
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);
    Ok(idx_arr)
}
