//! wasm32-gc `Map<K,V>` type declaration and op lowering (§Phase 2.4
//! decision K.9).
//!
//! Per K.9, a map is an **insertion-ordered association over parallel
//! arrays** carrying an **open-addressing hash *index*** that
//! accelerates lookup and construction-dedup. Nothing about the hash is
//! observable; only key-equality lookup and **insertion-order**
//! `keys()`/`values()` are contract. So a map is:
//!
//! ```text
//! $arr_idx = (array (mut i32))   // shared; declared once
//! $map_KV  = (struct (field $len  i64)
//!                    (field $keys (ref null $arr_K))
//!                    (field $vals (ref null $arr_V))
//!                    (field $idx  (ref null $arr_idx)))
//! ```
//!
//! where `$arr_K` / `$arr_V` are exactly the K.7 `List<K>` / `List<V>`
//! backing arrays (the list-collection pass declares them for every
//! map's K and V). `keys()` / `values()` therefore just wrap `$keys` /
//! `$vals` as a `$list_K` / `$list_V` (O(1) view; the arrays are
//! immutable). Entries sit in the arrays in insertion order, so order
//! preservation is structural — **unchanged** by the index.
//!
//! `$idx` (§Phase-2.4 close, driven by the `hash_map_churn` bench) is a
//! power-of-two open-addressing table whose slots hold a *slot index*
//! into `$keys`/`$vals` (or `-1` for empty), sized to ≤50% load. It
//! makes `get` / `contains` O(1) (vs the prior O(n) linear scan) and
//! literal / `set` / `remove` / `MapBuilder.freeze` construction-dedup
//! O(n) (vs O(n²)). The hash is *not* matched to native's FNV-1a — it
//! need only be self-consistent, since equality (not hash) is the
//! contract. `set` / `remove` stay copy-on-write (the immutable Map
//! API) and rebuild the index from the result arrays. The output is
//! byte-identical to every other backend.
//!
//! Key types: Int / Bool / Float / String (scalars + String). `Float`
//! keys compare **byte-wise** (`i64.reinterpret_f64` + `i64.eq`,
//! matching the `Map<Float,V>` byte-wise decision — `NaN == NaN`,
//! `-0.0 ≠ +0.0`). Ref-typed keys (struct/enum) error until a fixture
//! needs them. `MapBuilder` is deferred (no matrix fixture).

use std::collections::HashSet;

use phoenix_ir::instruction::ValueId;
use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;
use wasm_encoder::{BlockType, HeapType, Instruction, RefType, ValType};

use crate::error::CompileError;

use super::enums::contains_generic_placeholder;
use super::map_hash_index::{
    emit_build_idx, emit_idx_len_for, emit_index_lookup, emit_index_store_at, emit_new_idx,
    idx_len_for,
};
use super::module_builder::ModuleBuilder;
use super::translate::{FuncCtx, expect_result, single_slot};

/// `$map_KV` field index of `$len`.
const MAP_LEN: u32 = 0;
/// `$map_KV` field index of `$keys`.
const MAP_KEYS: u32 = 1;
/// `$map_KV` field index of `$vals`.
const MAP_VALS: u32 = 2;
/// `$map_KV` field index of `$idx` — the open-addressing hash index
/// (§Phase 2.4 close; see [`declare`]). Holds `(ref null $arr_idx)`
/// where `$arr_idx = (array (mut i32))`. Each slot is a slot-index into
/// `$keys`/`$vals`, or `-1` for empty. Length is a power of two
/// ≥ `max(8, 2 * len)` (≤50% load). The index is *not* observable —
/// only key-equality is — so any consistent hash is fine.
const MAP_IDX: u32 = 3;

/// `$mapbuilder_KV` field index of `$len`.
const MB_LEN: u32 = 0;
/// `$mapbuilder_KV` field index of `$frozen`.
const MB_FROZEN: u32 = 1;
/// `$mapbuilder_KV` field index of `$keys`.
const MB_KEYS: u32 = 2;
/// `$mapbuilder_KV` field index of `$vals`.
const MB_VALS: u32 = 3;

/// Initial pair-slot capacity for a fresh `MapBuilder` — matches
/// native's `INITIAL_CAPACITY` (`phx_map_builder_alloc`).
const MB_INITIAL_CAPACITY: i32 = 8;

/// Declare a `$map_KV` struct per distinct concrete `(K, V)` per
/// §Phase 2.4 decision K.9. Reuses the K.7 `$arr_K` / `$arr_V` arrays
/// (declared by the list pass, which now treats every map's K and V as
/// a list element type), so this pass only adds the wrapper struct.
///
/// Must run *after* `declare_phoenix_lists` and *before* any function
/// signature touching `IrType::MapRef` is interned.
pub(super) fn declare(
    builder: &mut ModuleBuilder,
    ir_module: &IrModule,
) -> Result<(), CompileError> {
    let mut kvs: HashSet<(IrType, IrType)> = HashSet::new();
    let mut builder_kvs: HashSet<(IrType, IrType)> = HashSet::new();
    collect_map_kvs(ir_module, &mut kvs, &mut builder_kvs);
    // Every builder `(K, V)` also needs the `$map_KV` pair — `freeze()`
    // returns `Map<K, V>`. (The freeze site's `MapRef` result type makes
    // the walk catch this in practice; the union keeps it explicit.)
    for kv in &builder_kvs {
        kvs.insert(kv.clone());
    }
    let mut ordered: Vec<(IrType, IrType)> = kvs.into_iter().collect();
    ordered.sort_by_cached_key(|kv| format!("{kv:?}"));

    // Declare the single shared `$arr_idx = (array (mut i32))` open-
    // addressing hash-index backing array, *once*, before any `$map_KV`
    // (whose `$idx` field references it), and record its type-section
    // index on the builder for the lowering paths to read back.
    if !ordered.is_empty() {
        let idx_elem = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(ValType::I32),
            mutable: true,
        };
        let arr_idx = builder.declare_list_array(idx_elem);
        builder.record_map_idx_array(arr_idx);
    }

    for (k, v) in &ordered {
        let (arr_k, _) = builder.require_list_types(k)?;
        let (arr_v, _) = builder.require_list_types(v)?;
        let len_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(ValType::I64),
            mutable: false,
        };
        let keys_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(concrete_ref(arr_k)),
            mutable: false,
        };
        let vals_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(concrete_ref(arr_v)),
            mutable: false,
        };
        let arr_idx = builder.require_map_idx_array()?;
        let idx_field = wasm_encoder::FieldType {
            element_type: wasm_encoder::StorageType::Val(concrete_ref(arr_idx)),
            mutable: false,
        };
        // A map is a 4-field immutable struct (no subtyping): the K.9
        // ordered $keys / $vals plus the §Phase-2.4-close $idx hash index.
        let map_idx = builder.declare_struct(&[len_field, keys_field, vals_field, idx_field]);
        builder.record_map(k.clone(), v.clone(), map_idx);

        if builder_kvs.contains(&(k.clone(), v.clone())) {
            // `$mapbuilder_KV`: everything mutable — `set` bumps $len,
            // growth swaps $keys / $vals, freeze sets $frozen. Mirrors
            // the K.7 `$builder_T` shape with two arrays instead of one.
            let blen_field = wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(ValType::I64),
                mutable: true,
            };
            let frozen_field = wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(ValType::I32),
                mutable: true,
            };
            let bkeys_field = wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(concrete_ref(arr_k)),
                mutable: true,
            };
            let bvals_field = wasm_encoder::FieldType {
                element_type: wasm_encoder::StorageType::Val(concrete_ref(arr_v)),
                mutable: true,
            };
            let builder_idx =
                builder.declare_struct(&[blen_field, frozen_field, bkeys_field, bvals_field]);
            builder.record_map_builder(k.clone(), v.clone(), builder_idx);
        }
    }
    Ok(())
}

/// Walk every IR type, collecting each `MapRef`'s `(K, V)` into `kvs`
/// and each `MapBuilderRef`'s `(K, V)` into `builder_kvs`. Mirrors the
/// K.4 / K.7 collection sources.
fn collect_map_kvs(
    ir_module: &IrModule,
    kvs: &mut HashSet<(IrType, IrType)>,
    builder_kvs: &mut HashSet<(IrType, IrType)>,
) {
    let mut walk = |ty: &IrType| walk_type(ty, kvs, builder_kvs);
    for func in ir_module.concrete_functions() {
        walk(&func.return_type);
        for ty in &func.param_types {
            walk(ty);
        }
        for block in &func.blocks {
            for (_, ty) in &block.params {
                walk(ty);
            }
            for instr in &block.instructions {
                walk(&instr.result_type);
            }
        }
    }
    for fields in ir_module.struct_layouts.values() {
        for (_, ty) in fields {
            walk(ty);
        }
    }
    for variants in ir_module.enum_layouts.values() {
        for (_, fields) in variants {
            for ty in fields {
                walk(ty);
            }
        }
    }
}

fn walk_type(
    ty: &IrType,
    kvs: &mut HashSet<(IrType, IrType)>,
    builder_kvs: &mut HashSet<(IrType, IrType)>,
) {
    match ty {
        // A `MapRef` declares the `$map_KV` struct; a `MapBuilderRef`
        // additionally declares the `$mapbuilder_KV` (the `declare`
        // union ensures the `$map_KV` it freezes into exists too).
        IrType::MapRef(k, v) => {
            if !contains_generic_placeholder(k) && !contains_generic_placeholder(v) {
                kvs.insert(((**k).clone(), (**v).clone()));
            }
            walk_type(k, kvs, builder_kvs);
            walk_type(v, kvs, builder_kvs);
        }
        IrType::MapBuilderRef(k, v) => {
            if !contains_generic_placeholder(k) && !contains_generic_placeholder(v) {
                builder_kvs.insert(((**k).clone(), (**v).clone()));
            }
            walk_type(k, kvs, builder_kvs);
            walk_type(v, kvs, builder_kvs);
        }
        IrType::StructRef(_, args) | IrType::EnumRef(_, args) => {
            for arg in args {
                walk_type(arg, kvs, builder_kvs);
            }
        }
        IrType::ListRef(inner) | IrType::ListBuilderRef(inner) => {
            walk_type(inner, kvs, builder_kvs)
        }
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            for p in param_types {
                walk_type(p, kvs, builder_kvs);
            }
            walk_type(return_type, kvs, builder_kvs);
        }
        _ => {}
    }
}

// ───────────────────────── K.9 map lowerings ─────────────────────────

/// Receiver facts: `$map_KV` struct idx, `(K, V)` IrTypes, the key's
/// ValType, and the `$arr_K` / `$arr_V` indices.
struct MapInfo {
    map_idx: u32,
    key_ir: IrType,
    val_ir: IrType,
    key_vt: ValType,
    arr_k: u32,
    arr_v: u32,
    arr_idx: u32,
}

fn map_info(ctx: &FuncCtx, b: &ModuleBuilder, recv: ValueId) -> Result<MapInfo, CompileError> {
    let map_idx = concrete_ref_idx(ctx, recv, "`Map` builtin receiver")?;
    let (k, v) = b
        .map_kv_by_struct_idx(map_idx)
        .ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: `Map` builtin receiver `{recv:?}` is bound to type \
                 index {map_idx}, which is not a recorded map instantiation \
                 (internal compiler bug)"
            ))
        })?
        .clone();
    let (arr_k, _) = b.require_list_types(&k)?;
    let (arr_v, _) = b.require_list_types(&v)?;
    let arr_idx = b.require_map_idx_array()?;
    Ok(MapInfo {
        map_idx,
        key_vt: single_slot(&k, b, "map key")?,
        key_ir: k,
        val_ir: v,
        arr_k,
        arr_v,
        arr_idx,
    })
}

/// `Op::MapAlloc(pairs)` — a map literal. Build `$keys` / `$vals`
/// arrays of the literal length, inserting each pair with
/// last-wins-on-duplicate / first-position-kept (matching native's
/// `from_pairs`); `$len` may end < the array length if the literal
/// had duplicate keys (the K.7 `$len < capacity` invariant).
pub(super) fn translate_map_alloc(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    pairs: &[(ValueId, ValueId)],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "Op::MapAlloc")?;
    let (k_ir, v_ir) = match &instr.result_type {
        IrType::MapRef(k, v) => ((**k).clone(), (**v).clone()),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `Op::MapAlloc` result type is `{other:?}`, expected \
                 `MapRef` (internal compiler bug)"
            )));
        }
    };
    let map_idx = b.require_map_idx(&k_ir, &v_ir)?;
    let (arr_k, _) = b.require_list_types(&k_ir)?;
    let (arr_v, _) = b.require_list_types(&v_ir)?;
    let arr_idx = b.require_map_idx_array()?;
    let key_vt = single_slot(&k_ir, b, "map key")?;
    let val_vt = single_slot(&v_ir, b, "map value")?;
    let cap = pairs.len() as i32;
    // `$idx` sized for the *literal* length (an upper bound on the final
    // entry count after dedup), kept ≤50% load and a power of two.
    let idxlen_const = idx_len_for(cap);

    let keys_arr = ctx.scratch_local(concrete_ref(arr_k));
    let vals_arr = ctx.scratch_local(concrete_ref(arr_v));
    let len = ctx.scratch_local(ValType::I32);
    let idxlen = ctx.scratch_local(ValType::I32);
    let k_scratch = ctx.scratch_local(key_vt);
    let v_scratch = ctx.scratch_local(val_vt);
    let found = ctx.scratch_local(ValType::I32);
    // Probe index/slot scratch hoisted out of the per-pair loop so the
    // literal builder reuses one pair across all pairs. (The `mask` and
    // `cur` scratch that `emit_index_lookup` mints internally are *not*
    // hoisted — they add a few locals per unrolled pair, negligible since
    // map literals are small. The append path reuses the lookup's `h` via
    // `emit_index_store_at`, so it mints nothing.)
    let probe_h = ctx.scratch_local(ValType::I32);
    let probe_slot = ctx.scratch_local(ValType::I32);

    // keys/vals = array.new_default(cap); idx = array.new(-1, idxlen); len = 0
    ctx.emit(Instruction::I32Const(cap));
    ctx.emit(Instruction::ArrayNewDefault(arr_k));
    ctx.emit(Instruction::LocalSet(keys_arr));
    ctx.emit(Instruction::I32Const(cap));
    ctx.emit(Instruction::ArrayNewDefault(arr_v));
    ctx.emit(Instruction::LocalSet(vals_arr));
    ctx.emit(Instruction::I32Const(idxlen_const));
    ctx.emit(Instruction::LocalSet(idxlen));
    let idx_arr = emit_new_idx(ctx, arr_idx, idxlen);
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(len));

    for (kv, vv) in pairs {
        // k_scratch = key; v_scratch = val
        let kl = ctx.binding_of(*kv)?;
        let vl = ctx.binding_of(*vv)?;
        ctx.emit(Instruction::LocalGet(kl));
        ctx.emit(Instruction::LocalSet(k_scratch));
        ctx.emit(Instruction::LocalGet(vl));
        ctx.emit(Instruction::LocalSet(v_scratch));
        // found = existing slot of this key in the entries so far, or -1
        emit_index_lookup(
            ctx, b, keys_arr, arr_k, idx_arr, arr_idx, idxlen, k_scratch, &k_ir, found, probe_h,
            probe_slot,
        )?;
        // if found >= 0 { vals[found] = v } else { append + index it }
        ctx.emit(Instruction::LocalGet(found));
        ctx.emit(Instruction::I32Const(0));
        ctx.emit(Instruction::I32GeS);
        ctx.emit(Instruction::If(BlockType::Empty));
        ctx.emit(Instruction::LocalGet(vals_arr));
        ctx.emit(Instruction::LocalGet(found));
        ctx.emit(Instruction::LocalGet(v_scratch));
        ctx.emit(Instruction::ArraySet(arr_v));
        ctx.emit(Instruction::Else);
        // keys[len]=k; vals[len]=v; idx[probe_h] = len; len++. The lookup
        // miss above already left `probe_h` at this key's empty home slot,
        // so store there directly — no re-hash, no re-probe.
        emit_append(ctx, keys_arr, arr_k, len, k_scratch);
        emit_append(ctx, vals_arr, arr_v, len, v_scratch);
        emit_index_store_at(ctx, idx_arr, arr_idx, probe_h, len);
        bump(ctx, len);
        ctx.emit(Instruction::End);
    }

    emit_wrap_map(ctx, len, keys_arr, vals_arr, idx_arr, map_idx, vid);
    Ok(())
}

/// `Map.length(map) -> Int` — `struct.get $len`.
pub(super) fn translate_map_length(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("Map.length", args, 1)?;
    let vid = expect_result(instr, "Map.length")?;
    let info = map_info(ctx, b, args[0])?;
    let recv = ctx.binding_of(args[0])?;
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: info.map_idx,
        field_index: MAP_LEN,
    });
    let local = ctx.allocate_local(vid, ValType::I64);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `Map.contains(map, k) -> Bool` — scan, `found >= 0`.
pub(super) fn translate_map_contains(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("Map.contains", args, 2)?;
    let vid = expect_result(instr, "Map.contains")?;
    let info = map_info(ctx, b, args[0])?;
    let recv = ctx.binding_of(args[0])?;
    let probe = ctx.binding_of(args[1])?;
    let keys = load_field_arr(ctx, recv, &info, MAP_KEYS, info.arr_k);
    let (idx_arr, idxlen) = emit_load_idx(ctx, recv, &info);
    let found = ctx.scratch_local(ValType::I32);
    let h = ctx.scratch_local(ValType::I32);
    let slot = ctx.scratch_local(ValType::I32);
    emit_index_lookup(
        ctx,
        b,
        keys,
        info.arr_k,
        idx_arr,
        info.arr_idx,
        idxlen,
        probe,
        &info.key_ir,
        found,
        h,
        slot,
    )?;
    // found >= 0 → 1
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::I32GeS);
    let local = ctx.allocate_local(vid, ValType::I32);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `Map.get(map, k) -> Option<V>` — scan; `Some(vals[found])` or `None`.
pub(super) fn translate_map_get(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("Map.get", args, 2)?;
    let vid = expect_result(instr, "Map.get")?;
    let info = map_info(ctx, b, args[0])?;
    // Option<V> output enum (K.4); V from the receiver.
    let opt_args = vec![info.val_ir.clone()];
    let opt_parent = b.require_enum_parent_idx("Option", &opt_args)?;
    let some_idx = b.require_enum_variant_idx("Option", &opt_args, 0)?;
    let none_idx = b.require_enum_variant_idx("Option", &opt_args, 1)?;

    let recv = ctx.binding_of(args[0])?;
    let probe = ctx.binding_of(args[1])?;
    let keys = load_field_arr(ctx, recv, &info, MAP_KEYS, info.arr_k);
    let vals = load_field_arr(ctx, recv, &info, MAP_VALS, info.arr_v);
    let (idx_arr, idxlen) = emit_load_idx(ctx, recv, &info);
    let found = ctx.scratch_local(ValType::I32);
    let h = ctx.scratch_local(ValType::I32);
    let slot = ctx.scratch_local(ValType::I32);
    emit_index_lookup(
        ctx,
        b,
        keys,
        info.arr_k,
        idx_arr,
        info.arr_idx,
        idxlen,
        probe,
        &info.key_ir,
        found,
        h,
        slot,
    )?;

    let opt_ref = concrete_ref(opt_parent);
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::If(BlockType::Result(opt_ref)));
    // Some(vals[found]): tag 0, value
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(vals));
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::ArrayGet(info.arr_v));
    ctx.emit(Instruction::StructNew(some_idx));
    ctx.emit(Instruction::Else);
    // None: tag 1
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::StructNew(none_idx));
    ctx.emit(Instruction::End);
    let local = ctx.allocate_local(vid, opt_ref);
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// `Map.set(map, k, v) -> Map` — copy-on-write: fresh arrays; overwrite
/// in place if the key exists (position kept), else append.
pub(super) fn translate_map_set(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("Map.set", args, 3)?;
    let vid = expect_result(instr, "Map.set")?;
    let info = map_info(ctx, b, args[0])?;
    let recv = ctx.binding_of(args[0])?;
    let new_k = ctx.binding_of(args[1])?;
    let new_v = ctx.binding_of(args[2])?;

    let (len, keys) = emit_load_len_keys(ctx, recv, &info);
    let vals = load_field_arr(ctx, recv, &info, MAP_VALS, info.arr_v);
    // Copy into fresh arrays of capacity len + 1 (room for an append).
    let cap = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(cap));
    let nkeys = fresh_copy(ctx, cap, keys, len, info.arr_k);
    let nvals = fresh_copy(ctx, cap, vals, len, info.arr_v);
    let nlen = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::LocalSet(nlen));
    // Fresh `$idx` sized for len + 1 (the max post-set entry count),
    // populated from the existing (already-deduped) entries.
    let idxlen = emit_idx_len_for(ctx, cap);
    let idx_arr = emit_build_idx(
        ctx,
        b,
        nkeys,
        info.arr_k,
        info.arr_idx,
        nlen,
        idxlen,
        &info.key_ir,
    )?;

    let found = ctx.scratch_local(ValType::I32);
    let h = ctx.scratch_local(ValType::I32);
    let slot = ctx.scratch_local(ValType::I32);
    emit_index_lookup(
        ctx,
        b,
        nkeys,
        info.arr_k,
        idx_arr,
        info.arr_idx,
        idxlen,
        new_k,
        &info.key_ir,
        found,
        h,
        slot,
    )?;
    // if found >= 0 { nvals[found] = v } else { append + index; nlen++ }
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(nvals));
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::LocalGet(new_v));
    ctx.emit(Instruction::ArraySet(info.arr_v));
    ctx.emit(Instruction::Else);
    // nkeys[nlen]=k; nvals[nlen]=v; idx[h] = nlen; nlen++. The lookup miss
    // above already left `h` at this key's empty home slot, so store there
    // directly — no re-hash, no re-probe.
    emit_append(ctx, nkeys, info.arr_k, nlen, new_k);
    emit_append(ctx, nvals, info.arr_v, nlen, new_v);
    emit_index_store_at(ctx, idx_arr, info.arr_idx, h, nlen);
    bump(ctx, nlen);
    ctx.emit(Instruction::End);

    emit_wrap_map(ctx, nlen, nkeys, nvals, idx_arr, info.map_idx, vid);
    Ok(())
}

/// `Map.remove(map, k) -> Map` — copy-on-write: fresh arrays holding
/// every entry whose key differs from `k`, order preserved.
pub(super) fn translate_map_remove(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("Map.remove", args, 2)?;
    let vid = expect_result(instr, "Map.remove")?;
    let info = map_info(ctx, b, args[0])?;
    let recv = ctx.binding_of(args[0])?;
    let probe = ctx.binding_of(args[1])?;

    let (len, keys) = emit_load_len_keys(ctx, recv, &info);
    let vals = load_field_arr(ctx, recv, &info, MAP_VALS, info.arr_v);
    // Fresh arrays of capacity len (the result is at most len entries).
    let nkeys = ctx.scratch_local(concrete_ref(info.arr_k));
    let nvals = ctx.scratch_local(concrete_ref(info.arr_v));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayNewDefault(info.arr_k));
    ctx.emit(Instruction::LocalSet(nkeys));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayNewDefault(info.arr_v));
    ctx.emit(Instruction::LocalSet(nvals));
    let nlen = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(nlen));
    let i = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));
    let cur_k = ctx.scratch_local(info.key_vt);

    // for i in 0..len: if keys[i] != probe { copy it across }
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1));
    // cur_k = keys[i]
    ctx.emit(Instruction::LocalGet(keys));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::ArrayGet(info.arr_k));
    ctx.emit(Instruction::LocalSet(cur_k));
    // if !key_eq(cur_k, probe) { keep }
    emit_key_eq(ctx, b, &info.key_ir, cur_k, probe)?;
    ctx.emit(Instruction::I32Eqz);
    ctx.emit(Instruction::If(BlockType::Empty));
    // nkeys[nlen] = cur_k
    ctx.emit(Instruction::LocalGet(nkeys));
    ctx.emit(Instruction::LocalGet(nlen));
    ctx.emit(Instruction::LocalGet(cur_k));
    ctx.emit(Instruction::ArraySet(info.arr_k));
    // nvals[nlen] = vals[i]
    ctx.emit(Instruction::LocalGet(nvals));
    ctx.emit(Instruction::LocalGet(nlen));
    ctx.emit(Instruction::LocalGet(vals));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::ArrayGet(info.arr_v));
    ctx.emit(Instruction::ArraySet(info.arr_v));
    bump(ctx, nlen);
    ctx.emit(Instruction::End);
    bump(ctx, i);
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);

    // Build a fresh `$idx` over the surviving (already-deduped) entries.
    // Sized from `nlen` — the *exact* survivor count, since `remove` only
    // shrinks (no append follows). (Contrast `set`, which sizes from
    // `len + 1` to leave room for the one entry its append may add.)
    let idxlen = emit_idx_len_for(ctx, nlen);
    let idx_arr = emit_build_idx(
        ctx,
        b,
        nkeys,
        info.arr_k,
        info.arr_idx,
        nlen,
        idxlen,
        &info.key_ir,
    )?;

    emit_wrap_map(ctx, nlen, nkeys, nvals, idx_arr, info.map_idx, vid);
    Ok(())
}

/// `Map.keys(map) -> List<K>` / `Map.values(map) -> List<V>` — wrap the
/// `$keys` / `$vals` array as a list (O(1) view; arrays are immutable).
pub(super) fn translate_map_keys_or_values(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    keys: bool,
) -> Result<(), CompileError> {
    let what = if keys { "Map.keys" } else { "Map.values" };
    expect_args(what, args, 1)?;
    let vid = expect_result(instr, what)?;
    let info = map_info(ctx, b, args[0])?;
    // Result type is List<K> or List<V>; resolve its $list struct.
    let out_elem = match &instr.result_type {
        IrType::ListRef(e) => (**e).clone(),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `{what}` result type is `{other:?}`, expected \
                 `ListRef` (internal compiler bug)"
            )));
        }
    };
    let (_, out_list) = b.require_list_types(&out_elem)?;
    let field = if keys { MAP_KEYS } else { MAP_VALS };
    let recv = ctx.binding_of(args[0])?;
    // struct.new $list(map.$len, map.$keys|$vals)
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: info.map_idx,
        field_index: MAP_LEN,
    });
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: info.map_idx,
        field_index: field,
    });
    ctx.emit(Instruction::StructNew(out_list));
    let local = ctx.allocate_local(vid, concrete_ref(out_list));
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

// ─────────────── MapBuilder lowerings ───────────────
//
// `MapBuilder<K,V>` is the two-array analogue of the K.7
// `ListBuilder<T>`: an *append-only* accumulator of `(k, v)` pairs with
// no dedup during the build phase (duplicates are stored verbatim), and
// a `freeze()` that replays the pairs through the same
// scan-and-overwrite-or-append dedup `translate_map_set` uses to
// produce a K.9 `$map_KV`. Last value wins; the key keeps its
// first-insertion position. This matches native `phx_map_builder_*`
// byte-for-byte (the runtime defers dedup to `phx_map_from_pairs`).
//
// `$mapbuilder_KV = (struct (mut $len i64) (mut $frozen i32)
//                          (mut $keys (ref null $arr_K))
//                          (mut $vals (ref null $arr_V)))`.
//
// Everything is synthesized inline (decision I) — no runtime merge,
// unlike wasm32-linear's `phx_map_builder_*` calls. GC rooting follows
// the host VM's tracing (the same discipline as the wasm-gc
// `ListBuilder` / `Map` ops): builder fields stay reachable through the
// builder handle for the duration of each op.

/// `MapBuilder.alloc() -> MapBuilder<K,V>` — a fresh builder: length 0,
/// unfrozen, capacity-8 key and value arrays (native parity). Mirrors
/// `ListBuilder.alloc`.
pub(super) fn translate_map_builder_alloc(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"MapBuilder.alloc\")")?;
    let (k_ir, v_ir) = match &instr.result_type {
        IrType::MapBuilderRef(k, v) => ((**k).clone(), (**v).clone()),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `MapBuilder.alloc` result type is `{other:?}`, \
                 expected `MapBuilderRef` (internal compiler bug)"
            )));
        }
    };
    let builder_idx = b.require_map_builder_idx(&k_ir, &v_ir)?;
    let (arr_k, _) = b.require_list_types(&k_ir)?;
    let (arr_v, _) = b.require_list_types(&v_ir)?;
    // struct.new $mapbuilder_KV(0, 0, new_default $arr_K(8), new_default $arr_V(8))
    ctx.emit(Instruction::I64Const(0)); // $len
    ctx.emit(Instruction::I32Const(0)); // $frozen
    ctx.emit(Instruction::I32Const(MB_INITIAL_CAPACITY));
    ctx.emit(Instruction::ArrayNewDefault(arr_k));
    ctx.emit(Instruction::I32Const(MB_INITIAL_CAPACITY));
    ctx.emit(Instruction::ArrayNewDefault(arr_v));
    ctx.emit(Instruction::StructNew(builder_idx));
    let local = ctx.allocate_local(vid, concrete_ref(builder_idx));
    ctx.emit(Instruction::LocalSet(local));
    Ok(())
}

/// Receiver facts for a `MapBuilder` op: the `$mapbuilder_KV` struct
/// index, `(K, V)` IrTypes, the key ValType, and the `$arr_K` / `$arr_V`
/// indices. The two-array analogue of `MapInfo`.
struct MapBuilderInfo {
    builder_idx: u32,
    key_ir: IrType,
    key_vt: ValType,
    val_vt: ValType,
    arr_k: u32,
    arr_v: u32,
}

fn map_builder_info(
    ctx: &FuncCtx,
    b: &ModuleBuilder,
    recv: ValueId,
) -> Result<MapBuilderInfo, CompileError> {
    let builder_idx = concrete_ref_idx(ctx, recv, "`MapBuilder` builtin receiver")?;
    let (k, v) = b
        .map_builder_kv_by_struct_idx(builder_idx)
        .ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: `MapBuilder` receiver `{recv:?}` is bound to type \
                 index {builder_idx}, which is not a recorded map-builder \
                 instantiation (internal compiler bug)"
            ))
        })?
        .clone();
    let (arr_k, _) = b.require_list_types(&k)?;
    let (arr_v, _) = b.require_list_types(&v)?;
    Ok(MapBuilderInfo {
        builder_idx,
        key_vt: single_slot(&k, b, "map-builder key")?,
        val_vt: single_slot(&v, b, "map-builder value")?,
        key_ir: k,
        arr_k,
        arr_v,
    })
}

/// `MapBuilder.set(builder, k, v)` (Void) — in-place **append** with no
/// dedup (dedup happens at freeze), 2× growth at capacity. Mirrors
/// `ListBuilder.push` over two parallel arrays. Set on a frozen builder
/// traps (native aborts with `builder was already frozen`).
pub(super) fn translate_map_builder_set(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
) -> Result<(), CompileError> {
    expect_args("MapBuilder.set", args, 3)?;
    let info = map_builder_info(ctx, b, args[0])?;
    let bi = info.builder_idx;
    let recv = ctx.binding_of(args[0])?;
    let new_k = ctx.binding_of(args[1])?;
    let new_v = ctx.binding_of(args[2])?;

    // if builder.$frozen { trap }
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_FROZEN,
    });
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::Unreachable);
    ctx.emit(Instruction::End);

    // grow 2× if $len == capacity (== array.len($keys)). The key and
    // value arrays grow in lockstep, so testing one capacity suffices.
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_LEN,
    });
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_KEYS,
    });
    ctx.emit(Instruction::ArrayLen);
    ctx.emit(Instruction::I64ExtendI32U);
    ctx.emit(Instruction::I64Eq);
    ctx.emit(Instruction::If(BlockType::Empty));
    {
        // new_cap = capacity * 2 (capacity starts at 8, only doubles —
        // never 0, so native's saturating min-1 guard is unneeded here).
        let new_cap = ctx.scratch_local(ValType::I32);
        ctx.emit(Instruction::LocalGet(recv));
        ctx.emit(Instruction::StructGet {
            struct_type_index: bi,
            field_index: MB_KEYS,
        });
        ctx.emit(Instruction::ArrayLen);
        ctx.emit(Instruction::I32Const(1));
        ctx.emit(Instruction::I32Shl);
        ctx.emit(Instruction::LocalSet(new_cap));
        // len (i32) for the copy length, read once.
        let len_i32 = ctx.scratch_local(ValType::I32);
        ctx.emit(Instruction::LocalGet(recv));
        ctx.emit(Instruction::StructGet {
            struct_type_index: bi,
            field_index: MB_LEN,
        });
        ctx.emit(Instruction::I32WrapI64);
        ctx.emit(Instruction::LocalSet(len_i32));
        emit_grow_field(ctx, recv, bi, MB_KEYS, info.arr_k, new_cap, len_i32);
        emit_grow_field(ctx, recv, bi, MB_VALS, info.arr_v, new_cap, len_i32);
    }
    ctx.emit(Instruction::End);

    // builder.$keys[$len] = k ; builder.$vals[$len] = v
    let pos = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_LEN,
    });
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::LocalSet(pos));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_KEYS,
    });
    ctx.emit(Instruction::LocalGet(pos));
    ctx.emit(Instruction::LocalGet(new_k));
    ctx.emit(Instruction::ArraySet(info.arr_k));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_VALS,
    });
    ctx.emit(Instruction::LocalGet(pos));
    ctx.emit(Instruction::LocalGet(new_v));
    ctx.emit(Instruction::ArraySet(info.arr_v));

    // builder.$len += 1
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_LEN,
    });
    ctx.emit(Instruction::I64Const(1));
    ctx.emit(Instruction::I64Add);
    ctx.emit(Instruction::StructSet {
        struct_type_index: bi,
        field_index: MB_LEN,
    });
    Ok(())
}

/// Grow one builder array field in place: allocate a fresh
/// `array.new_default(new_cap)`, `array.copy` the first `len` elements
/// over, and `struct.set` it back. `new_cap` / `len` are i32 scratch
/// locals the caller owns (shared across the key/value field grow).
fn emit_grow_field(
    ctx: &mut FuncCtx,
    recv: u32,
    builder_idx: u32,
    field: u32,
    arr: u32,
    new_cap: u32,
    len: u32,
) {
    let grown = ctx.scratch_local(concrete_ref(arr));
    ctx.emit(Instruction::LocalGet(new_cap));
    ctx.emit(Instruction::ArrayNewDefault(arr));
    ctx.emit(Instruction::LocalSet(grown));
    // array.copy grown[0..len] <- builder.$field[0..len]
    ctx.emit(Instruction::LocalGet(grown));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: builder_idx,
        field_index: field,
    });
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayCopy {
        array_type_index_dst: arr,
        array_type_index_src: arr,
    });
    // builder.$field = grown
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::LocalGet(grown));
    ctx.emit(Instruction::StructSet {
        struct_type_index: builder_idx,
        field_index: field,
    });
}

/// `MapBuilder.freeze(builder) -> Map<K,V>` — build the K.9 `$map_KV`
/// with last-wins dedup. Replays the builder's appended `(k, v)` pairs
/// through the same scan-and-overwrite-or-append logic as
/// `translate_map_set`: for each appended pair, scan the result keys
/// built so far; if the key is already present overwrite its value
/// (keeping its first-insertion position), else append. Matches native
/// `phx_map_builder_freeze` byte-for-byte. Double-freeze traps.
///
/// This linear-scan dedup is **O(n²)** in the pair count — same as the
/// incremental `translate_map_set` it mirrors, and unlike the AST/IR
/// interpreters' hashed O(n) `freeze`. Cheap for the small maps wasm-gc
/// fixtures build; revisit with a hashed table if a large-map wasm
/// fixture ever lands.
pub(super) fn translate_map_builder_freeze(
    ctx: &mut FuncCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_args("MapBuilder.freeze", args, 1)?;
    let vid = expect_result(instr, "MapBuilder.freeze")?;
    let info = map_builder_info(ctx, b, args[0])?;
    let bi = info.builder_idx;
    // Result type is Map<K,V>; resolve its $map_KV struct.
    let map_idx = match &instr.result_type {
        IrType::MapRef(k, v) => b.require_map_idx(k, v)?,
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `MapBuilder.freeze` result type is `{other:?}`, \
                 expected `MapRef` (internal compiler bug)"
            )));
        }
    };
    let recv = ctx.binding_of(args[0])?;

    // if builder.$frozen { trap }
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_FROZEN,
    });
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::Unreachable);
    ctx.emit(Instruction::End);
    // builder.$frozen = 1
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::StructSet {
        struct_type_index: bi,
        field_index: MB_FROZEN,
    });

    // src_len = builder.$len (i32); src_keys / src_vals = the buffers.
    let src_len = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_LEN,
    });
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::LocalSet(src_len));
    let src_keys = ctx.scratch_local(concrete_ref(info.arr_k));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_KEYS,
    });
    ctx.emit(Instruction::LocalSet(src_keys));
    let src_vals = ctx.scratch_local(concrete_ref(info.arr_v));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: bi,
        field_index: MB_VALS,
    });
    ctx.emit(Instruction::LocalSet(src_vals));

    // Fresh result arrays of capacity src_len (the deduped result is at
    // most src_len entries). nlen = 0.
    let nkeys = ctx.scratch_local(concrete_ref(info.arr_k));
    let nvals = ctx.scratch_local(concrete_ref(info.arr_v));
    ctx.emit(Instruction::LocalGet(src_len));
    ctx.emit(Instruction::ArrayNewDefault(info.arr_k));
    ctx.emit(Instruction::LocalSet(nkeys));
    ctx.emit(Instruction::LocalGet(src_len));
    ctx.emit(Instruction::ArrayNewDefault(info.arr_v));
    ctx.emit(Instruction::LocalSet(nvals));
    let nlen = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(nlen));

    // Fresh `$idx` sized for src_len (the max post-dedup entry count),
    // built up incrementally as the replay appends unique keys. This is
    // the O(n) hash-insert dedup that replaces the prior O(n²) scan.
    let arr_idx = b.require_map_idx_array()?;
    let idxlen = emit_idx_len_for(ctx, src_len);
    let idx_arr = emit_new_idx(ctx, arr_idx, idxlen);

    // Replay loop: for i in 0..src_len { k = src_keys[i]; v = src_vals[i];
    //   found = idx-lookup k in nkeys[0..nlen];
    //   if found >= 0 { nvals[found] = v } else { append (k,v); index; nlen++ } }
    let i = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(i));
    let cur_k = ctx.scratch_local(info.key_vt);
    let cur_v = ctx.scratch_local(info.val_vt);
    let found = ctx.scratch_local(ValType::I32);
    let probe_h = ctx.scratch_local(ValType::I32);
    let probe_slot = ctx.scratch_local(ValType::I32);

    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    // i >= src_len → done
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::LocalGet(src_len));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1));
    // cur_k = src_keys[i]; cur_v = src_vals[i]
    ctx.emit(Instruction::LocalGet(src_keys));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::ArrayGet(info.arr_k));
    ctx.emit(Instruction::LocalSet(cur_k));
    ctx.emit(Instruction::LocalGet(src_vals));
    ctx.emit(Instruction::LocalGet(i));
    ctx.emit(Instruction::ArrayGet(info.arr_v));
    ctx.emit(Instruction::LocalSet(cur_v));
    // found = index of cur_k in nkeys[0..nlen], or -1
    emit_index_lookup(
        ctx,
        b,
        nkeys,
        info.arr_k,
        idx_arr,
        arr_idx,
        idxlen,
        cur_k,
        &info.key_ir,
        found,
        probe_h,
        probe_slot,
    )?;
    // if found >= 0 { nvals[found] = cur_v } else { append + index; nlen++ }
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(nvals));
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::LocalGet(cur_v));
    ctx.emit(Instruction::ArraySet(info.arr_v));
    ctx.emit(Instruction::Else);
    // The lookup miss above already left `probe_h` at cur_k's empty home
    // slot, so store there directly — no re-hash, no re-probe.
    emit_append(ctx, nkeys, info.arr_k, nlen, cur_k);
    emit_append(ctx, nvals, info.arr_v, nlen, cur_v);
    emit_index_store_at(ctx, idx_arr, arr_idx, probe_h, nlen);
    bump(ctx, nlen);
    ctx.emit(Instruction::End);
    // i += 1
    bump(ctx, i);
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);

    emit_wrap_map(ctx, nlen, nkeys, nvals, idx_arr, map_idx, vid);
    Ok(())
}

// ── shared emit helpers ──

/// Load `$len` (wrapped to i32) and `$keys` into fresh scratch locals,
/// returning their indices.
fn emit_load_len_keys(ctx: &mut FuncCtx, recv: u32, info: &MapInfo) -> (u32, u32) {
    let len = ctx.scratch_local(ValType::I32);
    let keys = ctx.scratch_local(concrete_ref(info.arr_k));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: info.map_idx,
        field_index: MAP_LEN,
    });
    ctx.emit(Instruction::I32WrapI64);
    ctx.emit(Instruction::LocalSet(len));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: info.map_idx,
        field_index: MAP_KEYS,
    });
    ctx.emit(Instruction::LocalSet(keys));
    (len, keys)
}

/// Load `$idx` and its length (`array.len`) into fresh scratch locals,
/// returning `(idx_local, idxlen_local)`. The table length is a power of
/// two by construction, so callers use `idxlen - 1` as the probe mask.
fn emit_load_idx(ctx: &mut FuncCtx, recv: u32, info: &MapInfo) -> (u32, u32) {
    let idx = ctx.scratch_local(concrete_ref(info.arr_idx));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: info.map_idx,
        field_index: MAP_IDX,
    });
    ctx.emit(Instruction::LocalSet(idx));
    let idxlen = ctx.scratch_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(idx));
    ctx.emit(Instruction::ArrayLen);
    ctx.emit(Instruction::LocalSet(idxlen));
    (idx, idxlen)
}

/// Load a map array field into a fresh scratch local.
fn load_field_arr(ctx: &mut FuncCtx, recv: u32, info: &MapInfo, field: u32, arr: u32) -> u32 {
    let local = ctx.scratch_local(concrete_ref(arr));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: info.map_idx,
        field_index: field,
    });
    ctx.emit(Instruction::LocalSet(local));
    local
}

/// Allocate `array.new_default(cap)` and `array.copy` the first `len`
/// elements from `src`, returning the new array's local.
fn fresh_copy(ctx: &mut FuncCtx, cap: u32, src: u32, len: u32, arr: u32) -> u32 {
    let new = ctx.scratch_local(concrete_ref(arr));
    ctx.emit(Instruction::LocalGet(cap));
    ctx.emit(Instruction::ArrayNewDefault(arr));
    ctx.emit(Instruction::LocalSet(new));
    ctx.emit(Instruction::LocalGet(new));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(src));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::ArrayCopy {
        array_type_index_dst: arr,
        array_type_index_src: arr,
    });
    new
}

/// `arr[pos] = val`. Does *not* bump `pos`: the key and value arrays are
/// both written at the same `pos`, so the caller bumps `pos` once after
/// both `emit_append` calls.
fn emit_append(ctx: &mut FuncCtx, arr_local: u32, arr: u32, pos: u32, val: u32) {
    ctx.emit(Instruction::LocalGet(arr_local));
    ctx.emit(Instruction::LocalGet(pos));
    ctx.emit(Instruction::LocalGet(val));
    ctx.emit(Instruction::ArraySet(arr));
}

/// `pos += 1`.
pub(super) fn bump(ctx: &mut FuncCtx, pos: u32) {
    ctx.emit(Instruction::LocalGet(pos));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(pos));
}

/// Push `a`, push `b`, emit the key-equality op for `key_ir`, leaving
/// an i32 0/1 on the stack. Matches native's `keys_equal`: value
/// compare for Int/Bool, byte-wise (`reinterpret` + `i64.eq`) for
/// Float, `phx_str_eq` content compare for String.
pub(super) fn emit_key_eq(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    key_ir: &IrType,
    a: u32,
    bl: u32,
) -> Result<(), CompileError> {
    match key_ir {
        IrType::I64 => {
            ctx.emit(Instruction::LocalGet(a));
            ctx.emit(Instruction::LocalGet(bl));
            ctx.emit(Instruction::I64Eq);
        }
        IrType::Bool => {
            ctx.emit(Instruction::LocalGet(a));
            ctx.emit(Instruction::LocalGet(bl));
            ctx.emit(Instruction::I32Eq);
        }
        IrType::F64 => {
            // Byte-wise: reinterpret to i64 and compare bits.
            ctx.emit(Instruction::LocalGet(a));
            ctx.emit(Instruction::I64ReinterpretF64);
            ctx.emit(Instruction::LocalGet(bl));
            ctx.emit(Instruction::I64ReinterpretF64);
            ctx.emit(Instruction::I64Eq);
        }
        IrType::StringRef => {
            let str_eq = b.require_str_eq_idx()?;
            ctx.emit(Instruction::LocalGet(a));
            ctx.emit(Instruction::LocalGet(bl));
            ctx.emit(Instruction::Call(str_eq));
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc: `Map` key type `{other:?}` is not yet supported — \
                 the K.9 slice covers Int / Float / Bool / String keys; \
                 struct/enum keys land when a fixture needs them (the \
                 identity-vs-structural choice is deferred with them)"
            )));
        }
    }
    Ok(())
}

/// `result = struct.new $map_KV(extend_i64_u(len_i32), keys, vals, idx)`.
fn emit_wrap_map(
    ctx: &mut FuncCtx,
    len_i32: u32,
    keys_arr: u32,
    vals_arr: u32,
    idx_arr: u32,
    map_idx: u32,
    vid: ValueId,
) {
    ctx.emit(Instruction::LocalGet(len_i32));
    ctx.emit(Instruction::I64ExtendI32U);
    ctx.emit(Instruction::LocalGet(keys_arr));
    ctx.emit(Instruction::LocalGet(vals_arr));
    ctx.emit(Instruction::LocalGet(idx_arr));
    ctx.emit(Instruction::StructNew(map_idx));
    let local = ctx.allocate_local(vid, concrete_ref(map_idx));
    ctx.emit(Instruction::LocalSet(local));
}

// ── small valtype/util helpers ──

/// A nullable ref to the concrete type at `idx` — the WASM binding type
/// for every map field/value here (the `$arr_K`/`$arr_V` arrays, the
/// `$list_K` view, the `$map_KV` wrapper, an `Option` parent).
pub(super) fn concrete_ref(idx: u32) -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Concrete(idx),
    })
}

fn concrete_ref_idx(ctx: &FuncCtx, vid: ValueId, what: &str) -> Result<u32, CompileError> {
    match ctx.binding_type_of(vid)? {
        ValType::Ref(RefType {
            heap_type: HeapType::Concrete(idx),
            ..
        }) => Ok(idx),
        other => Err(CompileError::new(format!(
            "wasm32-gc: {what} lowered to `{other:?}`, expected a concrete ref \
             (internal compiler bug)"
        ))),
    }
}

fn expect_args(name: &str, args: &[ValueId], n: usize) -> Result<(), CompileError> {
    if args.len() == n {
        Ok(())
    } else {
        Err(CompileError::new(format!(
            "wasm32-gc: `{name}` takes {n} args but got {} (IR verifier should \
             have caught this)",
            args.len()
        )))
    }
}
