//! wasm32-gc `Map<K,V>` type declaration and op lowering (§Phase 2.4
//! decision K.9).
//!
//! Per K.9, a map is an **ordered association over parallel arrays** —
//! *not* a hash table. Nothing about native's FNV-1a open-addressing
//! table is observable; only key-equality lookup and **insertion-order**
//! `keys()`/`values()` are contract. So a map is:
//!
//! ```text
//! $map_KV = (struct (field $len  i64)
//!                   (field $keys (ref null $arr_K))
//!                   (field $vals (ref null $arr_V)))
//! ```
//!
//! where `$arr_K` / `$arr_V` are exactly the K.7 `List<K>` / `List<V>`
//! backing arrays (the list-collection pass declares them for every
//! map's K and V). `keys()` / `values()` therefore just wrap `$keys` /
//! `$vals` as a `$list_K` / `$list_V` (O(1) view; the arrays are
//! immutable). Entries sit in the arrays in insertion order, so order
//! preservation is structural. `get` / `contains` / `set` / `remove`
//! are linear scans with per-`K` key-equality dispatch; `set` / `remove`
//! are copy-on-write (the immutable Map API). O(n) where native is
//! O(1) — accepted, consistent with K.7's O(n) lists; fixture maps are
//! tiny. The output is byte-identical to every other backend.
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
use super::module_builder::ModuleBuilder;
use super::translate::{FuncCtx, expect_result, single_slot};

/// `$map_KV` field index of `$len`.
const MAP_LEN: u32 = 0;
/// `$map_KV` field index of `$keys`.
const MAP_KEYS: u32 = 1;
/// `$map_KV` field index of `$vals`.
const MAP_VALS: u32 = 2;

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
    collect_map_kvs(ir_module, &mut kvs);
    let mut ordered: Vec<(IrType, IrType)> = kvs.into_iter().collect();
    ordered.sort_by_cached_key(|kv| format!("{kv:?}"));

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
        // A map is just a 3-field immutable struct (no subtyping).
        let map_idx = builder.declare_struct(&[len_field, keys_field, vals_field]);
        builder.record_map(k.clone(), v.clone(), map_idx);
    }
    Ok(())
}

/// Walk every IR type, collecting each `MapRef` / `MapBuilderRef`'s
/// `(K, V)`. Mirrors the K.4 / K.7 collection sources.
fn collect_map_kvs(ir_module: &IrModule, kvs: &mut HashSet<(IrType, IrType)>) {
    let mut walk = |ty: &IrType| walk_type(ty, kvs);
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

fn walk_type(ty: &IrType, kvs: &mut HashSet<(IrType, IrType)>) {
    match ty {
        // `MapBuilderRef`'s `(K, V)` is collected here alongside `MapRef`
        // (mirroring `lists.rs`) so its `$map_KV` struct exists, even
        // though `MapBuilder` op lowering is still deferred — the struct
        // is harmless if unused, and a freeze's result `MapRef` would
        // declare it anyway. Recurse into K/V regardless, to reach any
        // nested concrete `MapRef`.
        IrType::MapRef(k, v) | IrType::MapBuilderRef(k, v) => {
            if !contains_generic_placeholder(k) && !contains_generic_placeholder(v) {
                kvs.insert(((**k).clone(), (**v).clone()));
            }
            walk_type(k, kvs);
            walk_type(v, kvs);
        }
        IrType::StructRef(_, args) | IrType::EnumRef(_, args) => {
            for arg in args {
                walk_type(arg, kvs);
            }
        }
        IrType::ListRef(inner) | IrType::ListBuilderRef(inner) => walk_type(inner, kvs),
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            for p in param_types {
                walk_type(p, kvs);
            }
            walk_type(return_type, kvs);
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
    Ok(MapInfo {
        map_idx,
        key_vt: single_slot(&k, b, "map key")?,
        key_ir: k,
        val_ir: v,
        arr_k,
        arr_v,
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
    let key_vt = single_slot(&k_ir, b, "map key")?;
    let val_vt = single_slot(&v_ir, b, "map value")?;
    let cap = pairs.len() as i32;

    let keys_arr = ctx.scratch_local(concrete_ref(arr_k));
    let vals_arr = ctx.scratch_local(concrete_ref(arr_v));
    let len = ctx.scratch_local(ValType::I32);
    let k_scratch = ctx.scratch_local(key_vt);
    let v_scratch = ctx.scratch_local(val_vt);
    let found = ctx.scratch_local(ValType::I32);
    let scan_i = ctx.scratch_local(ValType::I32);
    // Scan scratch hoisted out of the per-pair loop below so the literal
    // builder reuses one `cur` local rather than minting one per pair.
    let scan_cur = ctx.scratch_local(key_vt);

    // keys/vals = array.new_default(cap); len = 0
    ctx.emit(Instruction::I32Const(cap));
    ctx.emit(Instruction::ArrayNewDefault(arr_k));
    ctx.emit(Instruction::LocalSet(keys_arr));
    ctx.emit(Instruction::I32Const(cap));
    ctx.emit(Instruction::ArrayNewDefault(arr_v));
    ctx.emit(Instruction::LocalSet(vals_arr));
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
        // found = index of existing key, or -1
        emit_scan_for_key(
            ctx, b, keys_arr, arr_k, len, k_scratch, &k_ir, found, scan_i, scan_cur,
        )?;
        // if found >= 0 { vals[found] = v } else { keys[len]=k; vals[len]=v; len++ }
        ctx.emit(Instruction::LocalGet(found));
        ctx.emit(Instruction::I32Const(0));
        ctx.emit(Instruction::I32GeS);
        ctx.emit(Instruction::If(BlockType::Empty));
        ctx.emit(Instruction::LocalGet(vals_arr));
        ctx.emit(Instruction::LocalGet(found));
        ctx.emit(Instruction::LocalGet(v_scratch));
        ctx.emit(Instruction::ArraySet(arr_v));
        ctx.emit(Instruction::Else);
        // keys[len]=k; vals[len]=v (both at `len`); then bump once.
        emit_append(ctx, keys_arr, arr_k, len, k_scratch);
        emit_append(ctx, vals_arr, arr_v, len, v_scratch);
        bump(ctx, len);
        ctx.emit(Instruction::End);
    }

    emit_wrap_map(ctx, len, keys_arr, vals_arr, map_idx, vid);
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
    let (len, keys) = emit_load_len_keys(ctx, recv, &info);
    let found = ctx.scratch_local(ValType::I32);
    let scan_i = ctx.scratch_local(ValType::I32);
    let cur = ctx.scratch_local(info.key_vt);
    emit_scan_for_key(
        ctx,
        b,
        keys,
        info.arr_k,
        len,
        probe,
        &info.key_ir,
        found,
        scan_i,
        cur,
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
    let (len, keys) = emit_load_len_keys(ctx, recv, &info);
    let vals = ctx.scratch_local(concrete_ref(info.arr_v));
    ctx.emit(Instruction::LocalGet(recv));
    ctx.emit(Instruction::StructGet {
        struct_type_index: info.map_idx,
        field_index: MAP_VALS,
    });
    ctx.emit(Instruction::LocalSet(vals));
    let found = ctx.scratch_local(ValType::I32);
    let scan_i = ctx.scratch_local(ValType::I32);
    let cur = ctx.scratch_local(info.key_vt);
    emit_scan_for_key(
        ctx,
        b,
        keys,
        info.arr_k,
        len,
        probe,
        &info.key_ir,
        found,
        scan_i,
        cur,
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

    let found = ctx.scratch_local(ValType::I32);
    let scan_i = ctx.scratch_local(ValType::I32);
    let cur = ctx.scratch_local(info.key_vt);
    emit_scan_for_key(
        ctx,
        b,
        nkeys,
        info.arr_k,
        nlen,
        new_k,
        &info.key_ir,
        found,
        scan_i,
        cur,
    )?;
    // if found >= 0 { nvals[found] = v } else { nkeys[nlen]=k; nvals[nlen]=v; nlen++ }
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(nvals));
    ctx.emit(Instruction::LocalGet(found));
    ctx.emit(Instruction::LocalGet(new_v));
    ctx.emit(Instruction::ArraySet(info.arr_v));
    ctx.emit(Instruction::Else);
    // nkeys[nlen]=k; nvals[nlen]=v (both at `nlen`); then bump once.
    emit_append(ctx, nkeys, info.arr_k, nlen, new_k);
    emit_append(ctx, nvals, info.arr_v, nlen, new_v);
    bump(ctx, nlen);
    ctx.emit(Instruction::End);

    emit_wrap_map(ctx, nlen, nkeys, nvals, info.map_idx, vid);
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

    emit_wrap_map(ctx, nlen, nkeys, nvals, info.map_idx, vid);
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
fn bump(ctx: &mut FuncCtx, pos: u32) {
    ctx.emit(Instruction::LocalGet(pos));
    ctx.emit(Instruction::I32Const(1));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(pos));
}

/// Scan `keys_arr[0..len]` for the first element key-equal to the value
/// in `probe`, storing its index in `found` (or -1). `scan_i` and `cur`
/// are caller-owned scratch (i32 index, key-typed current element) —
/// passed in rather than allocated here so a caller that scans in a loop
/// (the literal builder) reuses one set instead of minting locals per
/// iteration.
#[allow(clippy::too_many_arguments)]
fn emit_scan_for_key(
    ctx: &mut FuncCtx,
    b: &ModuleBuilder,
    keys_arr: u32,
    arr_k: u32,
    len: u32,
    probe: u32,
    key_ir: &IrType,
    found: u32,
    scan_i: u32,
    cur: u32,
) -> Result<(), CompileError> {
    ctx.emit(Instruction::I32Const(-1));
    ctx.emit(Instruction::LocalSet(found));
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(scan_i));
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    // scan_i >= len → done
    ctx.emit(Instruction::LocalGet(scan_i));
    ctx.emit(Instruction::LocalGet(len));
    ctx.emit(Instruction::I32GeS);
    ctx.emit(Instruction::BrIf(1));
    // cur = keys[scan_i]
    ctx.emit(Instruction::LocalGet(keys_arr));
    ctx.emit(Instruction::LocalGet(scan_i));
    ctx.emit(Instruction::ArrayGet(arr_k));
    ctx.emit(Instruction::LocalSet(cur));
    // if key_eq(cur, probe) { found = scan_i; break }
    emit_key_eq(ctx, b, key_ir, cur, probe)?;
    ctx.emit(Instruction::If(BlockType::Empty));
    ctx.emit(Instruction::LocalGet(scan_i));
    ctx.emit(Instruction::LocalSet(found));
    ctx.emit(Instruction::Br(2)); // break the outer block
    ctx.emit(Instruction::End);
    bump(ctx, scan_i);
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::End);
    Ok(())
}

/// Push `a`, push `b`, emit the key-equality op for `key_ir`, leaving
/// an i32 0/1 on the stack. Matches native's `keys_equal`: value
/// compare for Int/Bool, byte-wise (`reinterpret` + `i64.eq`) for
/// Float, `phx_str_eq` content compare for String.
fn emit_key_eq(
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

/// `result = struct.new $map_KV(extend_i64_u(len_i32), keys, vals)`.
fn emit_wrap_map(
    ctx: &mut FuncCtx,
    len_i32: u32,
    keys_arr: u32,
    vals_arr: u32,
    map_idx: u32,
    vid: ValueId,
) {
    ctx.emit(Instruction::LocalGet(len_i32));
    ctx.emit(Instruction::I64ExtendI32U);
    ctx.emit(Instruction::LocalGet(keys_arr));
    ctx.emit(Instruction::LocalGet(vals_arr));
    ctx.emit(Instruction::StructNew(map_idx));
    let local = ctx.allocate_local(vid, concrete_ref(map_idx));
    ctx.emit(Instruction::LocalSet(local));
}

// ── small valtype/util helpers ──

/// A nullable ref to the concrete type at `idx` — the WASM binding type
/// for every map field/value here (the `$arr_K`/`$arr_V` arrays, the
/// `$list_K` view, the `$map_KV` wrapper, an `Option` parent).
fn concrete_ref(idx: u32) -> ValType {
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
