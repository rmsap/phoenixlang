//! WASM lowering for `Op::BuiltinCall` — the `print` / `toString`
//! builtins plus the `List` / `Map` / `Result` / `Option` method surface.
//!
//! Split out of [`super::translate`]: `translate_instruction` dispatches
//! every `Op::BuiltinCall` to [`translate_builtin_call`], which fans out to
//! the per-collection / per-enum method translators here. Shared codegen
//! primitives (field load/store, stack-frame staging, closure calls, the
//! per-function [`FuncTranslateCtx`]) stay in `translate.rs` and are
//! imported below so the offset/layout machinery lives in one place.

use phoenix_ir::instruction::ValueId;
use phoenix_ir::types::{IrType, OPTION_ENUM, RESULT_ENUM};
use phoenix_runtime::gc::TypeTag;
use wasm_encoder::{BlockType, Instruction, ValType};

use super::gc_root::{
    emit_gc_pop_frame_at, emit_gc_push_frame_at, emit_gc_set_root, emit_gc_set_root_at,
};
use super::heap_layout::{
    align_up, compute_variant_field_offsets, i32_memarg, phx_field_size_bytes,
};
use super::module_builder::ModuleBuilder;
use super::translate::{
    FuncTranslateCtx, emit_alloc_stack_frame, emit_closure_call_raw, emit_field_load,
    emit_field_store, emit_list_elem_addr, emit_restore_stack_frame, emit_sret_string_call,
    expect_result, reject_placeholder_field_type,
};
use crate::error::CompileError;

/// Look up the variant index for `variant_name` within `enum_name`'s
/// declared layout in `ir_module.enum_layouts`. Used by the Option /
/// Result-constructing builtin helpers (`List.first` / `List.last` /
/// `List.find` / `Map.get` / `Result.ok` / etc.) to translate variant
/// *names* into the discriminant integers the WASM enum-layout uses
/// at offset 0.
fn find_variant_index(
    ir_module: &phoenix_ir::module::IrModule,
    enum_name: &str,
    variant_name: &str,
) -> Result<u32, CompileError> {
    let variants = ir_module.enum_layouts.get(enum_name).ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-linear: enum `{enum_name}` has no registered layout — \
             internal compiler bug; the stdlib enum should always be present \
             post-sema"
        ))
    })?;
    variants
        .iter()
        .position(|(name, _)| name == variant_name)
        .map(|i| i as u32)
        .ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-linear: enum `{enum_name}` has no variant `{variant_name}` \
                 — internal compiler bug; the stdlib enum should always declare \
                 this variant"
            ))
        })
}

/// Construct a stdlib enum value inline (no `Op::EnumAlloc` required).
/// Sizes the allocation from `payload_field_types` (concrete types,
/// supplied by the caller — these are stdlib enums with monomorphic
/// payloads at the construction site so no `__generic` placeholder
/// concerns apply), allocates via `phx_gc_alloc`, writes the
/// discriminant as i32 at offset 0, then stores each payload field at
/// its naturally-aligned offset. The resulting heap pointer is left
/// on the operand stack.
///
/// `payload_field_types` and `payload_field_locals` agree in length
/// and order — one entry per declared payload field. For `None` /
/// `Err`-with-no-payload-like shapes, both are empty (the variant has
/// only the 4-byte discriminant).
///
/// Shared by `List.first` / `List.last` / `List.find` / `Map.get` and
/// (planned) `Result.ok` / `Result.err` / `Option.okOr` so the enum
/// layout machinery stays in one place.
fn emit_enum_construct(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    enum_name_for_diag: &str,
    variant_idx: u32,
    payload_field_types: &[IrType],
    payload_field_locals: &[Vec<u32>],
) -> Result<(), CompileError> {
    if payload_field_types.len() != payload_field_locals.len() {
        return Err(CompileError::new(format!(
            "wasm32-linear: `emit_enum_construct({enum_name_for_diag})` payload \
             type/locals length mismatch ({} vs {}) — internal compiler bug",
            payload_field_types.len(),
            payload_field_locals.len()
        )));
    }
    // Defense in depth, mirroring `Op::EnumAlloc`: a placeholder slipping
    // through here would silently size as a 4-byte i32 and desync the
    // read side. Callers pass concrete monomorphic payload types, so this
    // should never fire — but if a future caller threads an unresolved
    // type, fail loudly at the construction site rather than emit garbled
    // bytes.
    for (i, ty) in payload_field_types.iter().enumerate() {
        reject_placeholder_field_type(
            ty,
            &format!("`emit_enum_construct({enum_name_for_diag})` payload field {i}"),
        )?;
    }
    let variant = compute_variant_field_offsets(payload_field_types)?;
    let total_size = align_up(variant.payload_end, variant.max_align);
    let alloc_idx = b.require_phx_func("phx_gc_alloc")?;
    ctx.emit(Instruction::I32Const(total_size as i32));
    ctx.emit(Instruction::I32Const(TypeTag::Enum as i32));
    ctx.emit(Instruction::Call(alloc_idx));
    let ptr_local = ctx.allocate_temp_local(ValType::I32);
    ctx.emit(Instruction::LocalSet(ptr_local));
    // Discriminant at offset 0.
    ctx.emit(Instruction::LocalGet(ptr_local));
    ctx.emit(Instruction::I32Const(variant_idx as i32));
    ctx.emit(Instruction::I32Store(i32_memarg(0)));
    // Payload fields.
    for (i, ty) in payload_field_types.iter().enumerate() {
        emit_field_store(
            ctx,
            ptr_local,
            variant.field_offsets[i],
            ty,
            &payload_field_locals[i],
        )?;
    }
    // Result on stack.
    ctx.emit(Instruction::LocalGet(ptr_local));
    Ok(())
}

/// Construct `Some(payload)` for the stdlib `Option` enum, leaving the
/// heap pointer on the operand stack. Resolves the `Some` variant
/// index from the IR module's `enum_layouts` to stay robust against any
/// future stdlib reordering.
fn emit_option_some(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    payload_ty: &IrType,
    payload_locals: &[u32],
) -> Result<(), CompileError> {
    let some_idx = find_variant_index(ir_module, OPTION_ENUM, "Some")?;
    let payload_locals_owned = vec![payload_locals.to_vec()];
    emit_enum_construct(
        ctx,
        b,
        OPTION_ENUM,
        some_idx,
        std::slice::from_ref(payload_ty),
        &payload_locals_owned,
    )
}

/// Construct `None` for the stdlib `Option` enum, leaving the heap
/// pointer on the operand stack. The variant has no payload — just
/// the 4-byte discriminant.
fn emit_option_none(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
) -> Result<(), CompileError> {
    let none_idx = find_variant_index(ir_module, OPTION_ENUM, "None")?;
    emit_enum_construct(ctx, b, OPTION_ENUM, none_idx, &[], &[])
}

/// Translate a `BuiltinCall(name, args)` by fanning out to the
/// per-builtin / per-collection translator. Handles `print` and
/// `toString`, the `List.<method>` surface, the `Map.<method>`
/// surface, and the `Result.<method>` / `Option.<method>` surface.
/// Methods not yet lowered fall through to their dispatcher's
/// catch-all and surface a clean "not yet supported" diagnostic.
pub(super) fn translate_builtin_call(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    name: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match name {
        "print" => translate_print_builtin(ctx, b, args),
        "toString" => translate_to_string_builtin(ctx, b, args, instr),
        // `List.<method>` dispatcher. Covers the indexing/iteration
        // primitives (`length` / `get`), the functional methods
        // (`map` / `filter` / `reduce` / `flatMap` / `sortBy` / `any` /
        // `all` / `find`), the structural ones (`take` / `drop` /
        // `push` / `contains` / `first` / `last`), and rejects anything
        // still unlowered with a per-method diagnostic.
        list_method if list_method.starts_with("List.") => translate_list_method_builtin(
            ctx,
            b,
            ir_module,
            list_method.strip_prefix("List.").unwrap(),
            args,
            instr,
        ),
        // `Result.<method>` / `Option.<method>` dispatchers. Cover the
        // discriminant-equality checks (`isOk` / `isErr` / `isSome` /
        // `isNone`) and the payload extractor `unwrapOr`. The
        // closure-taking payload transforms (`map` / `andThen` / ...)
        // need multi-block conditional emission plus closure handling
        // and ship in a later slice.
        result_method if result_method.starts_with("Result.") => translate_result_option_builtin(
            ctx,
            b,
            ir_module,
            "Result",
            result_method.strip_prefix("Result.").unwrap(),
            args,
            instr,
        ),
        option_method if option_method.starts_with("Option.") => translate_result_option_builtin(
            ctx,
            b,
            ir_module,
            "Option",
            option_method.strip_prefix("Option.").unwrap(),
            args,
            instr,
        ),
        // `Map.<method>` dispatcher. Covers the read-side surface
        // (`length` / `contains` / `keys` / `values`), the
        // fresh-map-returning `remove` / `set`, and the Option-
        // returning `get`.
        map_method if map_method.starts_with("Map.") => translate_map_method_builtin(
            ctx,
            b,
            ir_module,
            map_method.strip_prefix("Map.").unwrap(),
            args,
            instr,
        ),
        other => Err(CompileError::new(format!(
            "wasm32-linear: builtin `{other}` not yet supported \
             (Phase 2.4 PR 3c — see docs/design-decisions.md §Phase 2.4)"
        ))),
    }
}

/// Lower a `BuiltinCall("Map.<method>", args)`. `method` is the
/// stripped suffix (e.g. `"length"`, `"contains"`).
///
/// Methods that take a key arg (`contains` / `remove`) stage the key
/// onto the WASM shadow stack first (via [`emit_alloc_stack_frame`]
/// and [`emit_field_store`]), then pass `(map, key_ptr, key_size)` to
/// the runtime. SP is restored after the call returns so map literals
/// or chained method calls don't leak stack space.
fn translate_map_method_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    method: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match method {
        // `phx_map_length(map: i32) -> i64`. The result is the scalar
        // `Int`, so the store type is pinned to `I64` rather than read
        // from `instr.result_type` (matches `List.length`).
        "length" => emit_map_unary_call(
            ctx,
            b,
            "phx_map_length",
            "Map.length",
            args,
            instr,
            IrType::I64,
        ),
        // `phx_map_keys(map: i32) -> i32` (list pointer). The returned
        // list is GC-tracked; the blanket post-instruction
        // `emit_gc_set_root` at the bottom of `translate_instruction`
        // roots it, so the `ListRef` result type flows through verbatim.
        "keys" => emit_map_unary_call(
            ctx,
            b,
            "phx_map_keys",
            "Map.keys",
            args,
            instr,
            instr.result_type.clone(),
        ),
        // `phx_map_values(map: i32) -> i32` (list pointer). Same shape
        // as `keys`.
        "values" => emit_map_unary_call(
            ctx,
            b,
            "phx_map_values",
            "Map.values",
            args,
            instr,
            instr.result_type.clone(),
        ),
        // `phx_map_contains(map: i32, key_ptr: i32, key_size: i64) -> i32`.
        // C ABI widens the runtime's `i8` return to i32 on wasm32, so the
        // bool result lands as a single-slot i32 0/1 — exactly what
        // `IrType::Bool` flattens to, so the store type is pinned to
        // `Bool` rather than read from `instr.result_type`.
        "contains" => emit_map_key_staged_call(
            ctx,
            b,
            "phx_map_contains",
            "Map.contains",
            args,
            instr,
            IrType::Bool,
        ),
        // `phx_map_remove_raw(map: i32, key_ptr: i32, key_size: i64) -> i32`
        // returns a fresh map with the entry removed (or an equivalent
        // if the key was absent). The returned map pointer is GC-tracked;
        // the blanket post-instruction `emit_gc_set_root` roots it, so the
        // `MapRef` result type flows through verbatim.
        "remove" => emit_map_key_staged_call(
            ctx,
            b,
            "phx_map_remove_raw",
            "Map.remove",
            args,
            instr,
            instr.result_type.clone(),
        ),
        "get" => translate_map_get(ctx, b, ir_module, args, instr),
        "set" => translate_map_set(ctx, b, args, instr),
        other => Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"Map.{other}\")` not yet supported \
             (Phase 2.4 PR 3d)"
        ))),
    }
}

/// Emit a unary `phx_map_<m>(map: i32) -> R` runtime call: resolve the
/// single map-pointer arg, invoke `runtime_fn`, and store the result as
/// `result_ty`. Shared by `Map.length` / `Map.keys` / `Map.values` — the
/// methods that take no key and so need no shadow-stack staging.
///
/// `result_ty` is passed explicitly rather than read from
/// `instr.result_type` so scalar-returning `length` can pin `I64` while
/// the list-returning `keys` / `values` forward their `ListRef` result
/// type. A ref-typed result is rooted by the blanket post-instruction
/// `emit_gc_set_root`.
fn emit_map_unary_call(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    runtime_fn: &str,
    label: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    result_ty: IrType,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, &format!("BuiltinCall(\"{label}\")"))?;
    if args.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"{label}\")` requires 1 arg \
             (the map), got {} (internal compiler bug — IR verifier should \
             have caught this)",
            args.len()
        )));
    }
    let map_ptr_local = ctx.binding_of(args[0])?.single_local();
    let idx = b.require_phx_func(runtime_fn)?;
    ctx.emit(Instruction::LocalGet(map_ptr_local));
    ctx.emit(Instruction::Call(idx));
    ctx.emit_store_result(vid, result_ty)?;
    Ok(())
}

/// Emit a key-staged `phx_map_<m>(map, key_ptr, key_size) -> R` runtime
/// call: resolve `(map, key)` via [`resolve_map_key_call`], stage the key
/// onto the WASM shadow stack (via [`emit_alloc_stack_frame`] and
/// [`emit_field_store`]), invoke `runtime_fn`, store the result as
/// `result_ty`, then restore SP so map literals or chained method calls
/// don't leak stack space. Shared by `Map.contains` / `Map.remove`.
///
/// `result_ty` is passed explicitly so `contains` can pin `Bool` (the
/// `i8`-widened-to-i32 runtime return) while `remove` forwards its
/// `MapRef` result type — rooted by the blanket post-instruction
/// `emit_gc_set_root`. The result is stored *before* SP is restored:
/// `emit_restore_stack_frame` only touches the SP global, so a result
/// still pending on the operand stack would be orphaned across it.
fn emit_map_key_staged_call(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    runtime_fn: &str,
    label: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    result_ty: IrType,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, &format!("BuiltinCall(\"{label}\")"))?;
    let (map_ptr_local, key_locals, key_ty) = resolve_map_key_call(ctx, label, args)?;
    let key_size = phx_field_size_bytes(&key_ty)? as i64;
    let (saved_sp, key_frame_local) = emit_alloc_stack_frame(ctx, b, key_size as u32)?;
    emit_field_store(ctx, key_frame_local, 0, &key_ty, &key_locals)?;
    let idx = b.require_phx_func(runtime_fn)?;
    ctx.emit(Instruction::LocalGet(map_ptr_local));
    ctx.emit(Instruction::LocalGet(key_frame_local));
    ctx.emit(Instruction::I64Const(key_size));
    ctx.emit(Instruction::Call(idx));
    ctx.emit_store_result(vid, result_ty)?;
    emit_restore_stack_frame(ctx, b, saved_sp)?;
    Ok(())
}

/// Resolve `(map_ptr_local, key_locals, key_ir_type)` for the
/// 2-arg `Map.<m>(map, key)` shape. Centralizes the arity check and
/// the binding-lookup so [`emit_map_key_staged_call`] only handles the
/// runtime call shape.
fn resolve_map_key_call(
    ctx: &FuncTranslateCtx,
    label: &str,
    args: &[ValueId],
) -> Result<(u32, Vec<u32>, IrType), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"{label}\")` requires 2 args \
             (map, key), got {} (internal compiler bug — IR verifier should \
             have caught this)",
            args.len()
        )));
    }
    let map_ptr_local = ctx.binding_of(args[0])?.single_local();
    let key_binding = ctx.binding_of(args[1])?;
    let key_locals = key_binding.locals.clone();
    let key_ty = key_binding.ir_type.clone();
    Ok((map_ptr_local, key_locals, key_ty))
}

/// `Map.get(key)`: returns `Some(V)` if the key is present, `None`
/// otherwise. Stages the key on the shadow stack, calls
/// `phx_map_get_raw(map, key_ptr, key_size) -> i32` (value pointer or
/// null), branches on the returned pointer, and builds the Option
/// accordingly. The value-pointer lives inside the rooted map's
/// data region (`pairs` block), so loading the value across the
/// Option's `phx_gc_alloc` is safe — the map stays rooted.
fn translate_map_get(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    let vid = expect_result(instr, "BuiltinCall(\"Map.get\")")?;
    let (map_ptr_local, key_locals, key_ty) = resolve_map_key_call(ctx, "Map.get", args)?;
    let key_size = phx_field_size_bytes(&key_ty)? as i64;
    // Value type = the inner of the Option result.
    let value_ty = match &instr.result_type {
        IrType::EnumRef(name, targs) if name == OPTION_ENUM => {
            targs.first().cloned().ok_or_else(|| {
                CompileError::new(
                    "wasm32-linear: `Map.get` result `Option<V>` is missing the \
                     `V` type argument (internal compiler bug)"
                        .to_string(),
                )
            })?
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `Map.get` result type must be `Option<V>`, got \
                 `{other:?}` (internal compiler bug)"
            )));
        }
    };

    let (saved_sp, key_frame_local) = emit_alloc_stack_frame(ctx, b, key_size as u32)?;
    emit_field_store(ctx, key_frame_local, 0, &key_ty, &key_locals)?;
    let get_raw_idx = b.require_phx_func("phx_map_get_raw")?;
    ctx.emit(Instruction::LocalGet(map_ptr_local));
    ctx.emit(Instruction::LocalGet(key_frame_local));
    ctx.emit(Instruction::I64Const(key_size));
    ctx.emit(Instruction::Call(get_raw_idx));
    let val_ptr_local = ctx.allocate_temp_local(ValType::I32);
    ctx.emit(Instruction::LocalSet(val_ptr_local));
    // SP can be restored now — the returned value pointer lives in the
    // map's data region (not the staged key buffer), so it survives the
    // SP restore.
    emit_restore_stack_frame(ctx, b, saved_sp)?;

    let value_locals = ctx.allocate_locals_for_ir_type_anon(&value_ty)?;
    // if val_ptr == 0 → None, else → Some(load(val_ptr)).
    ctx.emit(Instruction::LocalGet(val_ptr_local));
    ctx.emit(Instruction::I32Eqz);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    emit_option_none(ctx, b, ir_module)?;
    ctx.emit(Instruction::Else);
    emit_field_load(ctx, val_ptr_local, 0, &value_ty)?;
    store_stack_into_locals(ctx, &value_locals);
    emit_option_some(ctx, b, ir_module, &value_ty, &value_locals)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Map.set(key, value)`: returns a fresh map with the entry inserted
/// or updated. Stages both key and value on the shadow stack — laid
/// out back-to-back as `[key_bytes | value_bytes]` in one frame so a
/// single SP restore covers both — and calls
/// `phx_map_set_raw(map, key_ptr, val_ptr, ks, vs, key_is_string)`.
/// The `key_is_string` flag tells the runtime whether to dereference
/// the key as a fat pointer for content-based hashing.
fn translate_map_set(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 3 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"Map.set\")` requires 3 args \
             (map, key, value), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"Map.set\")")?;
    let map_ptr_local = ctx.binding_of(args[0])?.single_local();
    let key_binding = ctx.binding_of(args[1])?;
    let key_locals = key_binding.locals.clone();
    let key_ty = key_binding.ir_type.clone();
    let val_binding = ctx.binding_of(args[2])?;
    let val_locals = val_binding.locals.clone();
    let val_ty = val_binding.ir_type.clone();
    let ks = phx_field_size_bytes(&key_ty)? as i64;
    let vs = phx_field_size_bytes(&val_ty)? as i64;
    let key_is_string = matches!(key_ty, IrType::StringRef) as i64;

    // Single frame holds `[key bytes (0..ks) | value bytes (ks..ks+vs)]`.
    let frame_size = (ks + vs) as u32;
    let (saved_sp, frame_local) = emit_alloc_stack_frame(ctx, b, frame_size)?;
    emit_field_store(ctx, frame_local, 0, &key_ty, &key_locals)?;
    emit_field_store(ctx, frame_local, ks as u32, &val_ty, &val_locals)?;
    // val_ptr = frame + ks.
    let val_ptr_local = ctx.allocate_temp_local(ValType::I32);
    ctx.emit(Instruction::LocalGet(frame_local));
    ctx.emit(Instruction::I32Const(ks as i32));
    ctx.emit(Instruction::I32Add);
    ctx.emit(Instruction::LocalSet(val_ptr_local));
    let set_raw_idx = b.require_phx_func("phx_map_set_raw")?;
    ctx.emit(Instruction::LocalGet(map_ptr_local));
    ctx.emit(Instruction::LocalGet(frame_local));
    ctx.emit(Instruction::LocalGet(val_ptr_local));
    ctx.emit(Instruction::I64Const(ks));
    ctx.emit(Instruction::I64Const(vs));
    ctx.emit(Instruction::I64Const(key_is_string));
    ctx.emit(Instruction::Call(set_raw_idx));
    // Restore SP before binding the result — the new map pointer is the
    // sole operand-stack value and `emit_restore_stack_frame` is
    // operand-stack-neutral (it only moves the SP global), so the
    // pointer survives the restore. Mirrors the restore-then-bind order
    // in `translate_map_get`.
    emit_restore_stack_frame(ctx, b, saved_sp)?;
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// Per-method dispatcher for `Result.<m>` / `Option.<m>` builtins.
/// Routes the simple discriminant-equality checks to
/// [`translate_enum_is_variant_builtin`] and the payload-handling
/// methods to their own helpers (currently `unwrapOr`; `unwrap` /
/// `map` / `andThen` / `mapErr` / `ok` / `err` / `orElse` / `filter`
/// / `okOr` / `unwrapOrElse` ship in later slices).
fn translate_result_option_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    enum_name: &str,
    method: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match (enum_name, method) {
        ("Result", "isOk") | ("Result", "isErr") | ("Option", "isSome") | ("Option", "isNone") => {
            translate_enum_is_variant_builtin(ctx, ir_module, enum_name, method, args, instr)
        }
        // `unwrapOr` extracts the positive variant's payload (`Some` for
        // Option, `Ok` for Result); the helper resolves that variant's
        // index from the IR layout rather than hardcoding it, matching
        // the construction-side helpers.
        (_, "unwrapOr") => translate_enum_unwrap_or(ctx, b, ir_module, enum_name, args, instr),
        _ => Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"{enum_name}.{method}\")` not yet supported \
             (Phase 2.4 PR 3d — payload-handling methods like `unwrap` / `map` / \
             `andThen` ship in a later slice)"
        ))),
    }
}

/// `Option.unwrapOr(default)` / `Result.unwrapOr(default)` — extract
/// the positive-variant payload if present, otherwise yield `default`.
/// The payload sits at the natural-alignment offset past the 4-byte
/// discriminant — see [`compute_variant_field_offsets`]'s shared layout
/// walk, used by both [`emit_enum_construct`] (alloc side) and this
/// helper (load side), so the read can't drift from any future write.
///
/// The positive variant's discriminant is resolved from the IR module's
/// `enum_layouts` via [`find_variant_index`] (`Some` for `Option`, `Ok`
/// for `Result`) rather than hardcoded — matching the robustness stance
/// of [`emit_option_some`] / [`emit_option_none`], so a future stdlib
/// reordering can't desync the load side from construction.
///
/// Implementation: load discriminant; if it equals the positive
/// variant's index, extract payload via `emit_field_load` into a
/// result-local set, else copy the default into that same set. Wraps
/// the two arms in `If`/`Else`/`End` (no block-result type — values
/// flow through allocated locals, so multi-slot `StringRef` payloads
/// don't need a multi-value block type).
fn translate_enum_unwrap_or(
    ctx: &mut FuncTranslateCtx,
    _b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    enum_name: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"{enum_name}.unwrapOr\")` requires 2 args \
             (receiver, default), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"enum.unwrapOr\")")?;
    // Positive variant: `Some` for Option, `Ok` for Result. Resolved
    // from the layout so this stays in lockstep with construction.
    let positive_variant = match enum_name {
        OPTION_ENUM => "Some",
        RESULT_ENUM => "Ok",
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `unwrapOr` on enum `{other}` — only `Option` / \
                 `Result` are supported (internal compiler bug)"
            )));
        }
    };
    let positive_idx = find_variant_index(ir_module, enum_name, positive_variant)?;
    let recv_local = ctx.binding_of(args[0])?.single_local();
    let default_binding = ctx.binding_of(args[1])?;
    let default_locals = default_binding.locals.clone();
    let payload_ty = instr.result_type.clone();
    // Per-site offset uses the *concrete* result_type (sema-annotated)
    // as the single payload field. Matches `emit_enum_construct`'s
    // walk shape so alloc-side and load-side offsets agree.
    let variant = compute_variant_field_offsets(std::slice::from_ref(&payload_ty))?;
    let payload_offset = variant.field_offsets[0];

    let result_locals = ctx.allocate_locals_for_ir_type_anon(&payload_ty)?;
    // `default` has the same type `T` as the unwrapped payload, so its
    // local count must match `result_locals` slot-for-slot — the
    // negative-branch copy below zips them and would silently leave
    // result slots uninitialized on a mismatch. Assert the invariant so
    // a future type-threading bug fails loudly rather than yielding a
    // half-copied (e.g. truncated `StringRef` fat-pointer) value.
    debug_assert_eq!(
        result_locals.len(),
        default_locals.len(),
        "unwrapOr: default/result local count mismatch for payload `{payload_ty:?}`"
    );
    // disc == positive_idx (Some / Ok)?
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::I32Load(i32_memarg(0)));
    ctx.emit(Instruction::I32Const(positive_idx as i32));
    ctx.emit(Instruction::I32Eq);
    ctx.emit(Instruction::If(BlockType::Empty));
    // Positive: load payload from recv at the computed offset.
    emit_field_load(ctx, recv_local, payload_offset, &payload_ty)?;
    store_stack_into_locals(ctx, &result_locals);
    ctx.emit(Instruction::Else);
    // Negative: copy default's locals.
    for (dst, src) in result_locals.iter().zip(default_locals.iter()) {
        ctx.emit(Instruction::LocalGet(*src));
        ctx.emit(Instruction::LocalSet(*dst));
    }
    ctx.emit(Instruction::End);
    // Push result_locals onto the stack and bind.
    for &local in &result_locals {
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit_store_result(vid, payload_ty)?;
    Ok(())
}

/// Lower a `BuiltinCall("Result.<m>", ...)` or
/// `BuiltinCall("Option.<m>", ...)` where `m` is a discriminant-
/// equality check (`isOk` / `isErr` / `isSome` / `isNone`).
///
/// The semantics:
///
/// - `isOk` / `isSome`: discriminant == the positive variant's index
///   (`Ok` for Result, `Some` for Option).
/// - `isErr` / `isNone`: discriminant != that index (negation of the
///   positive check — both stdlib enums are binary, so "not positive"
///   is exactly the negative variant).
///
/// The positive variant's index is resolved from the IR module's
/// `enum_layouts` via [`find_variant_index`] rather than hardcoded to
/// `0`, so this check stays in lockstep with the construction side
/// ([`emit_option_some`] / `Op::EnumAlloc`) and [`translate_enum_unwrap_or`]
/// — a future stdlib reordering can't desync the discriminant tested
/// here from the one written at construction. Multi-payload non-stdlib
/// enums route through `Op::EnumDiscriminant` directly at the IR level;
/// nothing in user code can reach this builtin path with a
/// non-`Result`/-`Option` receiver.
///
/// All four methods return `Bool` (single i32 0/1) and have no GC-
/// ref-typed result, so the blanket post-instruction `emit_gc_set_root`
/// is a no-op for them.
fn translate_enum_is_variant_builtin(
    ctx: &mut FuncTranslateCtx,
    ir_module: &phoenix_ir::module::IrModule,
    enum_name: &str,
    method: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    // `positive_variant`: the variant whose discriminant `isOk`/`isSome`
    // test for equality. `isErr`/`isNone` are the negation.
    let (positive_variant, positive_check) = match (enum_name, method) {
        ("Result", "isOk") => ("Ok", true),
        ("Result", "isErr") => ("Ok", false),
        ("Option", "isSome") => ("Some", true),
        ("Option", "isNone") => ("Some", false),
        _ => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `BuiltinCall(\"{enum_name}.{method}\")` not yet \
                 supported (Phase 2.4 PR 3d — see docs/design-decisions.md §Phase 2.4 \
                 for the remaining stdlib-enum method surface)"
            )));
        }
    };
    let positive_idx = find_variant_index(ir_module, enum_name, positive_variant)?;
    let vid = expect_result(instr, "BuiltinCall(\"enum.is_variant\")")?;
    let receiver_vid = *args.first().ok_or_else(|| {
        CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"{enum_name}.{method}\")` requires 1 arg \
             (the {enum_name} receiver), got 0 (internal compiler bug — IR \
             verifier should have caught this)"
        ))
    })?;
    let recv_ptr_local = ctx.binding_of(receiver_vid)?.single_local();
    // Discriminant is i32 at offset 0 of the enum payload — the same
    // load shape `Op::EnumDiscriminant` uses. Comparing with i32.eq /
    // i32.ne keeps the result on the operand stack as an i32 0/1
    // which lines up exactly with `Bool`'s wasm-encoding.
    ctx.emit(Instruction::LocalGet(recv_ptr_local));
    ctx.emit(Instruction::I32Load(i32_memarg(0)));
    ctx.emit(Instruction::I32Const(positive_idx as i32));
    if positive_check {
        ctx.emit(Instruction::I32Eq);
    } else {
        ctx.emit(Instruction::I32Ne);
    }
    ctx.emit_store_result(vid, IrType::Bool)?;
    Ok(())
}

/// Lower a `BuiltinCall("List.<method>", args)`. `method` is the
/// stripped suffix (e.g. `"length"`, `"get"`). Dispatches the
/// indexing/iteration primitives (`length` / `get`), the functional
/// methods (`map` / `filter` / `reduce` / `flatMap` / `sortBy` / `any`
/// / `all` / `find`), and the structural methods (`take` / `drop` /
/// `push` / `contains` / `first` / `last`) to their per-method
/// translators. Any still-unlowered method falls through to the
/// catch-all with a clean "not yet supported" diagnostic.
fn translate_list_method_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    method: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match method {
        "length" => {
            // `phx_list_length(list) -> i64`. Returns 0 for an empty
            // list. Result vid is i64; the blanket post-instruction
            // `emit_gc_set_root` is a no-op for value types.
            let vid = expect_result(instr, "BuiltinCall(\"List.length\")")?;
            if args.len() != 1 {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `BuiltinCall(\"List.length\")` requires 1 arg \
                     (the list), got {} (internal compiler bug — IR verifier should \
                     have caught this)",
                    args.len()
                )));
            }
            let list_vid = args[0];
            let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
            let length_idx = b.require_phx_func("phx_list_length")?;
            ctx.emit(Instruction::LocalGet(list_ptr_local));
            ctx.emit(Instruction::Call(length_idx));
            ctx.emit_store_result(vid, IrType::I64)?;
            Ok(())
        }
        "get" => {
            // `phx_list_get_raw(list, index) -> *const u8` returns a
            // pointer to the element's bytes inside the list's data
            // region. Load the element value(s) from offset 0 via
            // `emit_field_load`, which handles single-slot scalars and
            // the multi-slot `StringRef` fat pointer uniformly. The
            // element's `IrType` comes from `instr.result_type` —
            // sema annotated it from the list's element type.
            let vid = expect_result(instr, "BuiltinCall(\"List.get\")")?;
            if args.len() != 2 {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `BuiltinCall(\"List.get\")` requires 2 args \
                     (list, index), got {} (internal compiler bug — IR verifier \
                     should have caught this)",
                    args.len()
                )));
            }
            let list_vid = args[0];
            let idx_vid = args[1];
            let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
            let idx_local = ctx.binding_of(idx_vid)?.single_local();
            let get_raw_idx = b.require_phx_func("phx_list_get_raw")?;
            ctx.emit(Instruction::LocalGet(list_ptr_local));
            ctx.emit(Instruction::LocalGet(idx_local));
            ctx.emit(Instruction::Call(get_raw_idx));
            // Stash the data-region pointer in a local so
            // `emit_field_load` can re-read it for each slot (a
            // multi-slot `StringRef` needs the base pointer twice —
            // once for ptr at offset 0 and once for len at offset 4).
            let data_ptr_local = ctx.allocate_temp_local(ValType::I32);
            ctx.emit(Instruction::LocalSet(data_ptr_local));
            let elem_ty = instr.result_type.clone();
            emit_field_load(ctx, data_ptr_local, 0, &elem_ty)?;
            ctx.emit_store_result(vid, elem_ty)?;
            Ok(())
        }
        "reduce" => translate_list_reduce(ctx, b, args, instr),
        "map" => translate_list_map(ctx, b, args, instr),
        "filter" => translate_list_filter(ctx, b, args, instr),
        "flatMap" => translate_list_flatmap(ctx, b, args, instr),
        "sortBy" => translate_list_sortby(ctx, b, args, instr),
        // `take(n)` / `drop(n)`: pure runtime calls returning a fresh
        // list. The count arg is an i64 `Int`; the result is a
        // GC-rooted list (blanket post-instruction set_root).
        "take" => translate_list_take_drop(ctx, b, "take", "phx_list_take", args, instr),
        "drop" => translate_list_take_drop(ctx, b, "drop", "phx_list_drop", args, instr),
        "push" => translate_list_push(ctx, b, args, instr),
        "contains" => translate_list_contains(ctx, b, args, instr),
        "any" => translate_list_any_all(ctx, b, args, instr, true),
        "all" => translate_list_any_all(ctx, b, args, instr, false),
        "first" => translate_list_first_last(ctx, b, ir_module, args, instr, true),
        "last" => translate_list_first_last(ctx, b, ir_module, args, instr, false),
        "find" => translate_list_find(ctx, b, ir_module, args, instr),
        other => Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.{other}\")` not yet supported \
             (Phase 2.4 PR 3d — see docs/phases/phase-2.md §2.4 PR 3d)"
        ))),
    }
}

/// Helper: extract the element `IrType` from a list-typed binding.
fn list_elem_type(ctx: &FuncTranslateCtx, list_vid: ValueId) -> Result<IrType, CompileError> {
    match &ctx.binding_of(list_vid)?.ir_type {
        IrType::ListRef(t) => Ok(t.as_ref().clone()),
        other => Err(CompileError::new(format!(
            "wasm32-linear: expected a `ListRef` receiver for a list functional \
             method, got `{other:?}` (internal compiler bug)"
        ))),
    }
}

/// `List.take(n)` / `List.drop(n)`: `runtime_fn(list, n) -> list`.
/// `n` is an i64 `Int`. The fresh list is GC-rooted by the blanket
/// post-instruction set_root.
fn translate_list_take_drop(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    method_name: &str,
    runtime_fn: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.{method_name}\")` requires 2 args \
             (list, n), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.take/drop\")")?;
    let list_ptr_local = ctx.binding_of(args[0])?.single_local();
    let n_local = ctx.binding_of(args[1])?.single_local();
    let fn_idx = b.require_phx_func(runtime_fn)?;
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::LocalGet(n_local));
    ctx.emit(Instruction::Call(fn_idx));
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `br_if` / `br` depth that exits a [`emit_breakable_loop`] body.
const LOOP_BREAK: u32 = 1;
/// `br` depth that continues a [`emit_breakable_loop`] body.
const LOOP_CONTINUE: u32 = 0;

/// `(block (loop body))` skeleton where `body` handles bounds, work,
/// advance, and the trailing `br LOOP_CONTINUE`. Use when the body
/// needs to short-circuit (`List.any`/`all`) or when the loop shape
/// isn't a simple `for i in 0..len` walk (`sortBy`'s decreasing inner
/// counter). Distinct from [`emit_list_loop`], which owns the bounds
/// check itself and forbids the body from branching. Nesting is
/// sound — inner depths shift by 2 — which [`translate_list_sortby`]
/// relies on.
fn emit_breakable_loop<F>(ctx: &mut FuncTranslateCtx, body: F) -> Result<(), CompileError>
where
    F: FnOnce(&mut FuncTranslateCtx) -> Result<(), CompileError>,
{
    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    body(ctx)?;
    ctx.emit(Instruction::End); // close loop
    ctx.emit(Instruction::End); // close exit block
    Ok(())
}

/// `List.any(pred)` / `List.all(pred)`: short-circuiting boolean fold.
/// `any` seeds `false` and ORs each `pred(elem)`, breaking at the first
/// `true`; `all` seeds `true` and ANDs, breaking at the first `false`.
/// Matches the interpreter, so a side-effecting predicate observes the
/// same call sequence under wasm and tree-walk.
///
/// ## Bool canonicalization
///
/// The `i32.or` / `i32.and` combine and the `i32.eqz` short-circuit
/// test rely on Phoenix `Bool` values being exactly `0` or `1`. This is
/// not a property of [`wasm_valtypes_for`] (which only maps `Bool` to
/// `ValType::I32`) — it's an invariant maintained by every site that
/// *produces* a Bool: comparison ops (`i32.eq`/`i64.eq` etc. return 0
/// or 1), `i32.eqz`, the seed `I32Const(0|1)` below, and the user
/// predicate's return value (whose codegen must respect the same
/// invariant — `LowerBoolNot`, comparisons, and Bool literals all do).
///
/// Hand-rolled around [`emit_breakable_loop`] so the body can `br_if`
/// out — [`emit_list_loop`] forbids branching.
fn translate_list_any_all(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    is_any: bool,
) -> Result<(), CompileError> {
    let method = if is_any { "any" } else { "all" };
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.{method}\")` requires 2 args \
             (list, predicate), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.any/all\")")?;
    let list_vid = args[0];
    let closure_vid = args[1];
    let elem_ty = list_elem_type(ctx, list_vid)?;
    let elem_size = phx_field_size_bytes(&elem_ty)?;
    let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
    let closure_ptr_local = ctx.binding_of(closure_vid)?.single_local();
    let closure_ir_ty = ctx.binding_of(closure_vid)?.ir_type.clone();

    // result = seed (0 for any, 1 for all).
    let result_local = ctx.allocate_temp_local(ValType::I32);
    ctx.emit(Instruction::I32Const(if is_any { 0 } else { 1 }));
    ctx.emit(Instruction::LocalSet(result_local));

    let elem_locals = ctx.allocate_locals_for_ir_type_anon(&elem_ty)?;
    let length_idx = b.require_phx_func("phx_list_length")?;
    let len_local = ctx.allocate_temp_local(ValType::I64);
    let i_local = ctx.allocate_temp_local(ValType::I64);
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::Call(length_idx));
    ctx.emit(Instruction::LocalSet(len_local));
    ctx.emit(Instruction::I64Const(0));
    ctx.emit(Instruction::LocalSet(i_local));

    emit_breakable_loop(ctx, |ctx| {
        // i >= len → break out of the loop.
        ctx.emit(Instruction::LocalGet(i_local));
        ctx.emit(Instruction::LocalGet(len_local));
        ctx.emit(Instruction::I64GeS);
        ctx.emit(Instruction::BrIf(LOOP_BREAK));
        // pred = closure(list[i]) → i32 on stack.
        let addr = emit_list_elem_addr(ctx, list_ptr_local, i_local, elem_size);
        emit_field_load(ctx, addr, 0, &elem_ty)?;
        store_stack_into_locals(ctx, &elem_locals);
        let call_args = vec![elem_locals.clone()];
        emit_closure_call_raw(ctx, b, closure_ptr_local, &closure_ir_ty, &call_args)?;
        // result = result OR/AND pred (predicate result is on the stack).
        ctx.emit(Instruction::LocalGet(result_local));
        if is_any {
            ctx.emit(Instruction::I32Or);
        } else {
            ctx.emit(Instruction::I32And);
        }
        ctx.emit(Instruction::LocalSet(result_local));
        // Short-circuit: `any` breaks once result is true, `all` once
        // it is false (i32.eqz flips the test).
        ctx.emit(Instruction::LocalGet(result_local));
        if !is_any {
            ctx.emit(Instruction::I32Eqz);
        }
        ctx.emit(Instruction::BrIf(LOOP_BREAK));
        // i += 1; continue.
        ctx.emit(Instruction::LocalGet(i_local));
        ctx.emit(Instruction::I64Const(1));
        ctx.emit(Instruction::I64Add);
        ctx.emit(Instruction::LocalSet(i_local));
        ctx.emit(Instruction::Br(LOOP_CONTINUE));
        Ok(())
    })?;

    // Bind the result Bool.
    ctx.emit(Instruction::LocalGet(result_local));
    ctx.emit_store_result(vid, IrType::Bool)?;
    Ok(())
}

/// `List.push(elem)`: `phx_list_push_raw(list, &elem, es) -> list`.
/// Stages the element value into a shadow-stack scratch frame and
/// passes its address (push copies the bytes by value, so the staged
/// element needs no rooting). Returns a fresh GC-rooted list.
fn translate_list_push(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.push\")` requires 2 args \
             (list, elem), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.push\")")?;
    let list_ptr_local = ctx.binding_of(args[0])?.single_local();
    let elem_binding = ctx.binding_of(args[1])?;
    let elem_locals = elem_binding.locals.clone();
    let elem_ty = elem_binding.ir_type.clone();
    let es = phx_field_size_bytes(&elem_ty)?;

    let (saved_sp, frame_local) = emit_alloc_stack_frame(ctx, b, es)?;
    emit_field_store(ctx, frame_local, 0, &elem_ty, &elem_locals)?;
    let push_idx = b.require_phx_func("phx_list_push_raw")?;
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::LocalGet(frame_local));
    ctx.emit(Instruction::I64Const(es as i64));
    ctx.emit(Instruction::Call(push_idx));
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    emit_restore_stack_frame(ctx, b, saved_sp)?;
    Ok(())
}

/// `List.contains(elem)`: `phx_list_contains(list, &elem, es, is_float,
/// is_string) -> i8`. Stages the element on the shadow stack; the two
/// flags use [`IrType::float_flag`] / [`IrType::string_flag`] so
/// cranelift and wasm encode them identically. The runtime treats
/// `is_string` as authoritative, which is what makes
/// `List<String>.contains` compare by content on wasm32 (where a
/// `StringRef` is 8 bytes, indistinguishable from `Int` / `Float` by
/// size).
fn translate_list_contains(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.contains\")` requires 2 args \
             (list, elem), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.contains\")")?;
    let list_ptr_local = ctx.binding_of(args[0])?.single_local();
    let elem_binding = ctx.binding_of(args[1])?;
    let elem_locals = elem_binding.locals.clone();
    let elem_ty = elem_binding.ir_type.clone();
    let es = phx_field_size_bytes(&elem_ty)?;

    let (saved_sp, frame_local) = emit_alloc_stack_frame(ctx, b, es)?;
    emit_field_store(ctx, frame_local, 0, &elem_ty, &elem_locals)?;
    let contains_idx = b.require_phx_func("phx_list_contains")?;
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::LocalGet(frame_local));
    ctx.emit(Instruction::I64Const(es as i64));
    ctx.emit(Instruction::I32Const(elem_ty.float_flag() as i32));
    ctx.emit(Instruction::I32Const(elem_ty.string_flag() as i32));
    ctx.emit(Instruction::Call(contains_idx));
    ctx.emit_store_result(vid, IrType::Bool)?;
    emit_restore_stack_frame(ctx, b, saved_sp)?;
    Ok(())
}

/// `List.first` / `List.last`: return `Some(elem)` if the list is
/// non-empty, `None` otherwise.
///
/// Lowered as: load `len`; if `len == 0` build `None`, else build
/// `Some(list[idx])` where `idx = 0` for first and `len - 1` for last.
/// Both branches end with the constructed Option pointer on the
/// operand stack, then `emit_store_result` binds it.
///
/// GC: elements live in the rooted input list, so the loaded element
/// stays reachable across the `phx_gc_alloc` triggered by Some-
/// construction. The constructed Option is rooted by the blanket
/// post-instruction `emit_gc_set_root`.
fn translate_list_first_last(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
    is_first: bool,
) -> Result<(), CompileError> {
    let method = if is_first { "first" } else { "last" };
    if args.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.{method}\")` requires 1 arg \
             (list), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.first/last\")")?;
    let list_vid = args[0];
    let elem_ty = list_elem_type(ctx, list_vid)?;
    let elem_size = phx_field_size_bytes(&elem_ty)?;
    let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
    // Load length.
    let length_idx = b.require_phx_func("phx_list_length")?;
    let len_local = ctx.allocate_temp_local(ValType::I64);
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::Call(length_idx));
    ctx.emit(Instruction::LocalSet(len_local));
    // Allocate locals (declared before any block — WASM locals are
    // function-scoped, not block-scoped).
    let idx_local = ctx.allocate_temp_local(ValType::I64);
    let elem_locals = ctx.allocate_locals_for_ir_type_anon(&elem_ty)?;
    // is_empty = len == 0
    ctx.emit(Instruction::LocalGet(len_local));
    ctx.emit(Instruction::I64Eqz);
    // Branch: if empty → None on stack; else → Some(list[idx]) on stack.
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    emit_option_none(ctx, b, ir_module)?;
    ctx.emit(Instruction::Else);
    // Non-empty path. idx = 0 for first; len - 1 for last.
    if is_first {
        ctx.emit(Instruction::I64Const(0));
    } else {
        ctx.emit(Instruction::LocalGet(len_local));
        ctx.emit(Instruction::I64Const(1));
        ctx.emit(Instruction::I64Sub);
    }
    ctx.emit(Instruction::LocalSet(idx_local));
    let elem_addr = emit_list_elem_addr(ctx, list_ptr_local, idx_local, elem_size);
    emit_field_load(ctx, elem_addr, 0, &elem_ty)?;
    store_stack_into_locals(ctx, &elem_locals);
    emit_option_some(ctx, b, ir_module, &elem_ty, &elem_locals)?;
    ctx.emit(Instruction::End); // close if/else
    // The if/else block result (i32 Option pointer) is on the stack.
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `List.find(pred)`: return `Some(first matching element)` or `None`.
///
/// Short-circuits at the first match via [`emit_breakable_loop`] — the
/// predicate is not called on elements past the match, matching the
/// interpreter's first-match-and-return, so a side-effecting or partial
/// predicate observes the same call sequence (and the same traps) under
/// wasm and tree-walk. The matched element is captured into
/// `found_elem_locals` before the break and carried to the post-loop
/// Option construction; the input list stays rooted, so the element
/// bytes are transitively reachable through it during the iteration.
/// The Option result is rooted by the blanket post-instruction
/// `emit_gc_set_root`.
fn translate_list_find(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.find\")` requires 2 args \
             (list, predicate), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.find\")")?;
    let list_vid = args[0];
    let closure_vid = args[1];
    let elem_ty = list_elem_type(ctx, list_vid)?;
    let elem_size = phx_field_size_bytes(&elem_ty)?;
    let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
    let closure_binding = ctx.binding_of(closure_vid)?;
    let closure_ptr_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    // found = 0; found_elem_locals hold the match (only valid when found==1).
    let found_local = ctx.allocate_temp_local(ValType::I32);
    ctx.emit(Instruction::I32Const(0));
    ctx.emit(Instruction::LocalSet(found_local));
    let elem_locals = ctx.allocate_locals_for_ir_type_anon(&elem_ty)?;
    let found_elem_locals = ctx.allocate_locals_for_ir_type_anon(&elem_ty)?;
    let found_elem_locals_for_body = found_elem_locals.clone();

    // Manual bounds tracking around `emit_breakable_loop` so the body
    // can `br_if` out at the first match — `emit_list_loop` owns its
    // bounds check and forbids the body from branching. Mirrors the
    // `translate_list_any_all` loop shape.
    let length_idx = b.require_phx_func("phx_list_length")?;
    let len_local = ctx.allocate_temp_local(ValType::I64);
    let i_local = ctx.allocate_temp_local(ValType::I64);
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::Call(length_idx));
    ctx.emit(Instruction::LocalSet(len_local));
    ctx.emit(Instruction::I64Const(0));
    ctx.emit(Instruction::LocalSet(i_local));

    emit_breakable_loop(ctx, |ctx| {
        // i >= len → break out of the loop.
        ctx.emit(Instruction::LocalGet(i_local));
        ctx.emit(Instruction::LocalGet(len_local));
        ctx.emit(Instruction::I64GeS);
        ctx.emit(Instruction::BrIf(LOOP_BREAK));
        // elem = list[i]
        let addr = emit_list_elem_addr(ctx, list_ptr_local, i_local, elem_size);
        emit_field_load(ctx, addr, 0, &elem_ty)?;
        store_stack_into_locals(ctx, &elem_locals);
        // pred = closure(elem) → i32 on the stack.
        let call_args = vec![elem_locals.clone()];
        emit_closure_call_raw(ctx, b, closure_ptr_local, &closure_ir_ty, &call_args)?;
        // if pred: capture elem and set found (the `If` consumes the
        // predicate result on the stack).
        ctx.emit(Instruction::If(BlockType::Empty));
        for (dst, src) in found_elem_locals_for_body.iter().zip(elem_locals.iter()) {
            ctx.emit(Instruction::LocalGet(*src));
            ctx.emit(Instruction::LocalSet(*dst));
        }
        ctx.emit(Instruction::I32Const(1));
        ctx.emit(Instruction::LocalSet(found_local));
        ctx.emit(Instruction::End); // close if
        // Break once found — leaves the predicate uncalled on the rest.
        ctx.emit(Instruction::LocalGet(found_local));
        ctx.emit(Instruction::BrIf(LOOP_BREAK));
        // i += 1; continue.
        ctx.emit(Instruction::LocalGet(i_local));
        ctx.emit(Instruction::I64Const(1));
        ctx.emit(Instruction::I64Add);
        ctx.emit(Instruction::LocalSet(i_local));
        ctx.emit(Instruction::Br(LOOP_CONTINUE));
        Ok(())
    })?;
    // After loop: if found → Some(found_elem); else → None.
    ctx.emit(Instruction::LocalGet(found_local));
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    emit_option_some(ctx, b, ir_module, &elem_ty, &found_elem_locals)?;
    ctx.emit(Instruction::Else);
    emit_option_none(ctx, b, ir_module)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// Emit the shared list-iteration loop skeleton and run `body` once
/// per element inside it.
///
/// Allocates an i64 counter local `i` (initialized to 0) and an i64
/// `len` local (from `phx_list_length(list)`), then emits:
///
/// ```text
/// (block            ;; $exit
///   (loop           ;; $loop
///     local.get i; local.get len; i64.ge_s; br_if 1   ;; i >= len → exit
///     <body>                                          ;; caller-emitted
///     local.get i; i64.const 1; i64.add; local.set i  ;; i += 1
///     br 0                                            ;; continue
///   )
/// )
/// ```
///
/// `body` is invoked with the loop counter local (`i`) and the loop's
/// element-size (the input list's `elem_size`); it emits the per-
/// element work (load element, call closure, store result). Because
/// the block/loop is fully nested and the caller's `body` only ever
/// branches to the loop's own labels (none — it falls through), the
/// br-depths stay self-contained regardless of any enclosing
/// function-level dispatcher.
fn emit_list_loop(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    list_ptr_local: u32,
    mut body: impl FnMut(&mut FuncTranslateCtx, &mut ModuleBuilder, u32) -> Result<(), CompileError>,
) -> Result<(), CompileError> {
    let len_local = ctx.allocate_temp_local(ValType::I64);
    let i_local = ctx.allocate_temp_local(ValType::I64);
    let length_idx = b.require_phx_func("phx_list_length")?;
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::Call(length_idx));
    ctx.emit(Instruction::LocalSet(len_local));
    ctx.emit(Instruction::I64Const(0));
    ctx.emit(Instruction::LocalSet(i_local));

    ctx.emit(Instruction::Block(BlockType::Empty));
    ctx.emit(Instruction::Loop(BlockType::Empty));
    // i >= len → break to $exit (depth 1).
    ctx.emit(Instruction::LocalGet(i_local));
    ctx.emit(Instruction::LocalGet(len_local));
    ctx.emit(Instruction::I64GeS);
    ctx.emit(Instruction::BrIf(1));
    // Body.
    body(ctx, b, i_local)?;
    // i += 1; continue.
    ctx.emit(Instruction::LocalGet(i_local));
    ctx.emit(Instruction::I64Const(1));
    ctx.emit(Instruction::I64Add);
    ctx.emit(Instruction::LocalSet(i_local));
    ctx.emit(Instruction::Br(0));
    ctx.emit(Instruction::End); // close loop
    ctx.emit(Instruction::End); // close block
    Ok(())
}

/// `List.reduce(list, init, closure)`: fold over elements.
///
/// `args = [list, init, closure]`. The accumulator lives in the
/// *result vid's* locals (so the final accumulator is already the
/// instruction's bound result — no post-loop copy), seeded from
/// `init`. Each iteration loads the element, calls
/// `closure(acc, elem)`, and writes the result back into the acc
/// locals.
///
/// GC: when the accumulator is ref-typed, it's re-rooted on the
/// shadow stack (via the result vid's pre-assigned slot) after the
/// seed and after each update, so an allocating closure can't sweep
/// the live accumulator mid-fold. Elements loaded from the input list
/// need no separate rooting — they stay reachable transitively through
/// the input list, which is itself a rooted method argument.
fn translate_list_reduce(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 3 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.reduce\")` requires 3 args \
             (list, init, closure), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.reduce\")")?;
    let list_vid = args[0];
    let init_vid = args[1];
    let closure_vid = args[2];
    let elem_ty = list_elem_type(ctx, list_vid)?;
    let elem_size = phx_field_size_bytes(&elem_ty)?;
    let acc_ty = instr.result_type.clone();

    let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
    let closure_ptr_local = ctx.binding_of(closure_vid)?.single_local();
    let closure_ir_ty = ctx.binding_of(closure_vid)?.ir_type.clone();

    // Accumulator locals = the result binding's locals, seeded from
    // `init`. Allocating the result binding up front lets the fold
    // mutate it in place and leaves the final value already bound.
    let acc_locals = ctx.allocate_locals_for_ir_type(vid, acc_ty.clone())?;
    let init_locals = ctx.binding_of(init_vid)?.locals.clone();
    if init_locals.len() != acc_locals.len() {
        return Err(CompileError::new(
            "wasm32-linear: `List.reduce` init/acc slot-count mismatch (internal \
             compiler bug)"
                .to_string(),
        ));
    }
    for (dst, src) in acc_locals.iter().zip(init_locals.iter()) {
        ctx.emit(Instruction::LocalGet(*src));
        ctx.emit(Instruction::LocalSet(*dst));
    }
    // Root the seeded accumulator (no-op for value-typed acc).
    emit_gc_set_root(ctx, b, vid)?;

    let elem_locals = ctx.allocate_locals_for_ir_type_anon(&elem_ty)?;
    let acc_locals_for_body = acc_locals.clone();
    emit_list_loop(ctx, b, list_ptr_local, |ctx, b, i_local| {
        // Load element into elem_locals.
        let addr_local = emit_list_elem_addr(ctx, list_ptr_local, i_local, elem_size);
        emit_field_load(ctx, addr_local, 0, &elem_ty)?;
        store_stack_into_locals(ctx, &elem_locals);
        // new_acc = closure(acc, elem).
        let call_args = vec![acc_locals_for_body.clone(), elem_locals.clone()];
        emit_closure_call_raw(ctx, b, closure_ptr_local, &closure_ir_ty, &call_args)?;
        store_stack_into_locals(ctx, &acc_locals_for_body);
        // Re-root the updated accumulator (no-op for value types).
        emit_gc_set_root(ctx, b, vid)?;
        Ok(())
    })?;
    Ok(())
}

/// `List.map(list, closure)`: apply closure to each element, collect
/// results into a new list.
///
/// Allocates the output list (length = input length, element size =
/// closure return type's size) before the loop and roots it via the
/// result vid's slot so it survives any allocation inside the closure.
/// Each iteration loads the input element, calls the closure, and
/// stores the result at the matching index in the output list.
fn translate_list_map(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.map\")` requires 2 args \
             (list, closure), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.map\")")?;
    let list_vid = args[0];
    let closure_vid = args[1];
    let in_elem_ty = list_elem_type(ctx, list_vid)?;
    let in_elem_size = phx_field_size_bytes(&in_elem_ty)?;
    // Output element type = the map result's list element type.
    let out_elem_ty = match &instr.result_type {
        IrType::ListRef(t) => t.as_ref().clone(),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `List.map` result type must be `ListRef`, got \
                 `{other:?}` (internal compiler bug)"
            )));
        }
    };
    let out_elem_size = phx_field_size_bytes(&out_elem_ty)?;

    let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
    let closure_ptr_local = ctx.binding_of(closure_vid)?.single_local();
    let closure_ir_ty = ctx.binding_of(closure_vid)?.ir_type.clone();

    // Allocate output list (length = input length).
    let length_idx = b.require_phx_func("phx_list_length")?;
    let list_alloc_idx = b.require_phx_func("phx_list_alloc")?;
    ctx.emit(Instruction::I64Const(out_elem_size as i64));
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::Call(length_idx)); // len (i64)
    ctx.emit(Instruction::Call(list_alloc_idx)); // phx_list_alloc(out_es, len)
    let out_ptr_local = ctx.allocate_local(vid, ValType::I32, instr.result_type.clone());
    ctx.emit(Instruction::LocalSet(out_ptr_local));
    // Root the output list for the duration of the loop.
    emit_gc_set_root(ctx, b, vid)?;

    let in_elem_locals = ctx.allocate_locals_for_ir_type_anon(&in_elem_ty)?;
    let out_elem_locals = ctx.allocate_locals_for_ir_type_anon(&out_elem_ty)?;
    emit_list_loop(ctx, b, list_ptr_local, |ctx, b, i_local| {
        // elem = in[i].
        let in_addr = emit_list_elem_addr(ctx, list_ptr_local, i_local, in_elem_size);
        emit_field_load(ctx, in_addr, 0, &in_elem_ty)?;
        store_stack_into_locals(ctx, &in_elem_locals);
        // result = closure(elem).
        let call_args = vec![in_elem_locals.clone()];
        emit_closure_call_raw(ctx, b, closure_ptr_local, &closure_ir_ty, &call_args)?;
        store_stack_into_locals(ctx, &out_elem_locals);
        // out[i] = result.
        let out_addr = emit_list_elem_addr(ctx, out_ptr_local, i_local, out_elem_size);
        emit_field_store(ctx, out_addr, 0, &out_elem_ty, &out_elem_locals)?;
        Ok(())
    })?;
    Ok(())
}

/// `List.filter(list, closure)`: keep elements where the predicate
/// returns true.
///
/// Allocates an output list with capacity = input length, walks the
/// input writing matching elements contiguously while tracking an
/// `out_count`, then patches the output list's length field (i64 at
/// offset 0) to `out_count` at the end. Roots the output list via the
/// result vid's slot for the loop's duration.
fn translate_list_filter(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.filter\")` requires 2 args \
             (list, closure), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.filter\")")?;
    let list_vid = args[0];
    let closure_vid = args[1];
    let elem_ty = list_elem_type(ctx, list_vid)?;
    let elem_size = phx_field_size_bytes(&elem_ty)?;

    let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
    let closure_ptr_local = ctx.binding_of(closure_vid)?.single_local();
    let closure_ir_ty = ctx.binding_of(closure_vid)?.ir_type.clone();

    // Allocate output list sized to input length (worst case: all kept).
    let length_idx = b.require_phx_func("phx_list_length")?;
    let list_alloc_idx = b.require_phx_func("phx_list_alloc")?;
    ctx.emit(Instruction::I64Const(elem_size as i64));
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::Call(length_idx));
    ctx.emit(Instruction::Call(list_alloc_idx));
    let out_ptr_local = ctx.allocate_local(vid, ValType::I32, instr.result_type.clone());
    ctx.emit(Instruction::LocalSet(out_ptr_local));
    emit_gc_set_root(ctx, b, vid)?;

    // out_count: number of elements written so far (i64).
    let out_count_local = ctx.allocate_temp_local(ValType::I64);
    ctx.emit(Instruction::I64Const(0));
    ctx.emit(Instruction::LocalSet(out_count_local));

    let elem_locals = ctx.allocate_locals_for_ir_type_anon(&elem_ty)?;
    let pred_local = ctx.allocate_temp_local(ValType::I32);
    emit_list_loop(ctx, b, list_ptr_local, |ctx, b, i_local| {
        // elem = in[i].
        let in_addr = emit_list_elem_addr(ctx, list_ptr_local, i_local, elem_size);
        emit_field_load(ctx, in_addr, 0, &elem_ty)?;
        store_stack_into_locals(ctx, &elem_locals);
        // pred = closure(elem)  (Bool → i32 0/1).
        let call_args = vec![elem_locals.clone()];
        emit_closure_call_raw(ctx, b, closure_ptr_local, &closure_ir_ty, &call_args)?;
        ctx.emit(Instruction::LocalSet(pred_local));
        // if pred != 0: out[out_count] = elem; out_count += 1.
        ctx.emit(Instruction::LocalGet(pred_local));
        ctx.emit(Instruction::If(BlockType::Empty));
        let out_addr = emit_list_elem_addr(ctx, out_ptr_local, out_count_local, elem_size);
        emit_field_store(ctx, out_addr, 0, &elem_ty, &elem_locals)?;
        ctx.emit(Instruction::LocalGet(out_count_local));
        ctx.emit(Instruction::I64Const(1));
        ctx.emit(Instruction::I64Add);
        ctx.emit(Instruction::LocalSet(out_count_local));
        ctx.emit(Instruction::End); // close if
        Ok(())
    })?;
    // Patch the output list's length field (i64 @ offset 0) to the
    // actual kept count.
    ctx.emit(Instruction::LocalGet(out_ptr_local));
    ctx.emit(Instruction::LocalGet(out_count_local));
    ctx.emit(Instruction::I64Store(wasm_encoder::MemArg {
        offset: 0,
        align: 3, // log2(8) — i64
        memory_index: 0,
    }));
    Ok(())
}

/// Pop the top-of-stack value slots into `locals`, last slot first.
/// WASM `local.set` pops the operand stack top, so to store a multi-
/// slot value pushed in declaration order (`[slot0, slot1, ...]`) we
/// `local.set` in reverse. Single-slot values collapse to one set.
fn store_stack_into_locals(ctx: &mut FuncTranslateCtx, locals: &[u32]) {
    for &local in locals.iter().rev() {
        ctx.emit(Instruction::LocalSet(local));
    }
}

/// `List.flatMap(list, closure)`: apply the closure to each element
/// (it must return a `List<U>`), concatenate the resulting lists.
///
/// `args = [list, closure]`. The output starts as an empty `List<U>`
/// and grows via `phx_list_push_raw` — which is *immutable-append*:
/// it allocates a fresh `length + 1` list and returns it, so the output
/// pointer changes on every push and must be re-stored + re-rooted.
///
/// GC: the output list is rooted via the result vid's slot (re-rooted
/// after each push, since the pointer moves). The per-element inner
/// list returned by the closure has no IR vid, so it's rooted in a
/// dedicated 1-slot ad-hoc shadow frame for the duration of the inner
/// push loop — `phx_list_push_raw` allocates, and without rooting the
/// inner list its elements could be swept before they're all copied.
/// Elements pushed from the inner list don't need separate rooting:
/// `push_raw` copies their bytes by value, and the pointed-to objects
/// become reachable through the (rooted) output list.
fn translate_list_flatmap(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.flatMap\")` requires 2 args \
             (list, closure), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.flatMap\")")?;
    let list_vid = args[0];
    let closure_vid = args[1];
    let in_elem_ty = list_elem_type(ctx, list_vid)?;
    let in_elem_size = phx_field_size_bytes(&in_elem_ty)?;
    // Output element type = the flatMap result's list element type
    // (also the element type of each inner list the closure returns).
    let out_elem_ty = match &instr.result_type {
        IrType::ListRef(t) => t.as_ref().clone(),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `List.flatMap` result type must be `ListRef`, got \
                 `{other:?}` (internal compiler bug)"
            )));
        }
    };
    let out_elem_size = phx_field_size_bytes(&out_elem_ty)?;

    let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
    let closure_ptr_local = ctx.binding_of(closure_vid)?.single_local();
    let closure_ir_ty = ctx.binding_of(closure_vid)?.ir_type.clone();

    // Output: start with an empty list (`phx_list_alloc(out_es, 0)`).
    let list_alloc_idx = b.require_phx_func("phx_list_alloc")?;
    ctx.emit(Instruction::I64Const(out_elem_size as i64));
    ctx.emit(Instruction::I64Const(0));
    ctx.emit(Instruction::Call(list_alloc_idx));
    let out_ptr_local = ctx.allocate_local(vid, ValType::I32, instr.result_type.clone());
    ctx.emit(Instruction::LocalSet(out_ptr_local));
    emit_gc_set_root(ctx, b, vid)?;

    // Ad-hoc 1-slot frame rooting the closure's per-element inner list
    // across the push loop's allocations.
    let inner_frame_local = emit_gc_push_frame_at(ctx, b, 1)?;
    let inner_list_local = ctx.allocate_temp_local(ValType::I32);
    let in_elem_locals = ctx.allocate_locals_for_ir_type_anon(&in_elem_ty)?;
    let push_raw_idx = b.require_phx_func("phx_list_push_raw")?;

    emit_list_loop(ctx, b, list_ptr_local, |ctx, b, i_local| {
        // elem = in[i]; inner = closure(elem).
        let in_addr = emit_list_elem_addr(ctx, list_ptr_local, i_local, in_elem_size);
        emit_field_load(ctx, in_addr, 0, &in_elem_ty)?;
        store_stack_into_locals(ctx, &in_elem_locals);
        let call_args = vec![in_elem_locals.clone()];
        emit_closure_call_raw(ctx, b, closure_ptr_local, &closure_ir_ty, &call_args)?;
        ctx.emit(Instruction::LocalSet(inner_list_local));
        // Root the inner list before the push loop (push allocates).
        emit_gc_set_root_at(ctx, b, inner_frame_local, 0, inner_list_local)?;
        // Inner loop: for j in 0..inner_len, push inner[j] onto out.
        emit_list_loop(ctx, b, inner_list_local, |ctx, b, j_local| {
            // out = phx_list_push_raw(out, &inner[j], out_es). The
            // element pointer is inner[j]'s address in the inner list's
            // data region — `push_raw` copies its bytes by value.
            let inner_addr = emit_list_elem_addr(ctx, inner_list_local, j_local, out_elem_size);
            ctx.emit(Instruction::LocalGet(out_ptr_local));
            ctx.emit(Instruction::LocalGet(inner_addr));
            ctx.emit(Instruction::I64Const(out_elem_size as i64));
            ctx.emit(Instruction::Call(push_raw_idx));
            // push_raw returns a fresh list ptr — update + re-root out.
            ctx.emit(Instruction::LocalSet(out_ptr_local));
            emit_gc_set_root(ctx, b, vid)?;
            Ok(())
        })
    })?;
    emit_gc_pop_frame_at(ctx, b, inner_frame_local)?;
    Ok(())
}

/// `List.sortBy(list, comparator)`: return a new list sorted by the
/// comparator closure `(a, b) -> Int` (negative = `a` before `b`).
///
/// Uses **stable insertion sort** rather than native's bottom-up merge
/// sort. The output is byte-identical for any total-order comparator:
/// insertion sort is stable (ties keep input order via the `cmp <= 0`
/// stop condition), matching the interpreter and native backends. It
/// trades native's O(n log n) for O(n²), acceptable for the
/// small-list use the method sees today; the WASM port favors a
/// structured-control-flow shape that maps cleanly to nested
/// `block`/`loop`s over replicating merge sort's many-edged CFG.
///
/// The sort runs in place on a fresh copy of the input
/// (`phx_list_take(list, len)` — Phoenix lists are immutable, so the
/// input is never mutated). The copy is bound to the result vid and
/// rooted via its slot.
///
/// GC: when the element type is a ref, the `key` being inserted is
/// held in locals across the comparator call (which may allocate) —
/// and after the first shift `copy[i]` is overwritten, so `key` is no
/// longer reachable through the rooted copy. A dedicated 1-slot ad-hoc
/// shadow frame roots `key` for the outer loop's duration. Value-typed
/// elements (the common case) skip the frame entirely. Shifted
/// elements (`copy[j] → copy[j+1]`) stay reachable through the rooted
/// copy throughout.
///
/// The 1-slot frame is sufficient because today every Phoenix ref type
/// occupies at most one *pointer* slot: single-slot refs (`List`, `Map`,
/// `Closure`, `Struct`, `Enum`) are a bare GC pointer in `key_locals[0]`,
/// and the only 2-slot ref — `StringRef` — stores the heap pointer in
/// `key_locals[0]` and the (non-pointer) `len` in `key_locals[1]`.
/// Rooting slot 0 keeps the underlying object live in every current
/// case. A future 2-pointer ref type (e.g. a fat reference whose second
/// slot is itself a GC pointer) would need this frame widened to 2 slots
/// — `phx_field_size_bytes` adding a non-`StringRef` 2-slot ref-type
/// branch is the tripwire for the rewrite.
fn translate_list_sortby(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"List.sortBy\")` requires 2 args \
             (list, comparator), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"List.sortBy\")")?;
    let list_vid = args[0];
    let closure_vid = args[1];
    let elem_ty = list_elem_type(ctx, list_vid)?;
    let elem_size = phx_field_size_bytes(&elem_ty)?;
    // `is_ref_type()` returns true for the `__generic` placeholder
    // (`StructRef("__generic", [])`), so a sortBy over a placeholder-typed
    // empty list would take the ref branch. Sema closes off that path in
    // practice today — `let xs = []` is rejected as ambiguous
    // (`phoenix-sema/src/check_stmt.rs::check_let`'s "cannot infer type"
    // diagnostic) and `let xs: List<T> = []` is pinned to the annotation
    // by `pin_inferred_type_to_annotation` — so every sortBy reachable
    // from user code carries a concrete element type. The placeholder
    // branch is therefore unreachable today; it is kept defensively (and
    // is safe by construction: `len == 0` on the only path that could
    // reach it, so the inner loop never executes and the rooted slot
    // never participates in a collection). The GC-root emitter paired
    // with `gc_roots::is_tracked_ref`'s placeholder skip makes this
    // safe; see that file's docstring for the joint contract.
    let elem_is_ref = elem_ty.is_ref_type();

    let list_ptr_local = ctx.binding_of(list_vid)?.single_local();
    let closure_ptr_local = ctx.binding_of(closure_vid)?.single_local();
    let closure_ir_ty = ctx.binding_of(closure_vid)?.ir_type.clone();

    // len = phx_list_length(list); copy = phx_list_take(list, len).
    let length_idx = b.require_phx_func("phx_list_length")?;
    let take_idx = b.require_phx_func("phx_list_take")?;
    let len_local = ctx.allocate_temp_local(ValType::I64);
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::Call(length_idx));
    ctx.emit(Instruction::LocalSet(len_local));
    ctx.emit(Instruction::LocalGet(list_ptr_local));
    ctx.emit(Instruction::LocalGet(len_local));
    ctx.emit(Instruction::Call(take_idx));
    let copy_local = ctx.allocate_local(vid, ValType::I32, instr.result_type.clone());
    ctx.emit(Instruction::LocalSet(copy_local));
    emit_gc_set_root(ctx, b, vid)?; // root the copy

    // Optional ad-hoc frame rooting the `key` element (ref types only).
    let key_frame_local = if elem_is_ref {
        Some(emit_gc_push_frame_at(ctx, b, 1)?)
    } else {
        None
    };

    let key_locals = ctx.allocate_locals_for_ir_type_anon(&elem_ty)?;
    let cmp_a_locals = ctx.allocate_locals_for_ir_type_anon(&elem_ty)?;
    // Executable tripwire for the docstring's single-pointer-slot
    // assumption. Today every ref type either occupies one slot (a
    // bare GC pointer in slot 0) or is exactly `StringRef` (slot 0 =
    // heap pointer, slot 1 = non-pointer `len`). A future ref type
    // whose second slot is also a GC pointer would need this frame
    // widened to 2 slots; this assert fires before that case silently
    // miscompiles.
    debug_assert!(
        !elem_is_ref || matches!(&elem_ty, IrType::StringRef) || key_locals.len() == 1,
        "sortBy: ref element type {:?} occupies {} slots — the ad-hoc \
         key frame only roots slot 0, but this type may have a pointer \
         in another slot. Widen the frame to match (see the docstring's \
         tripwire note).",
        elem_ty,
        key_locals.len(),
    );
    let i_local = ctx.allocate_temp_local(ValType::I64);
    let j_local = ctx.allocate_temp_local(ValType::I64);
    let jp1_local = ctx.allocate_temp_local(ValType::I64);
    let cmp_result_local = ctx.allocate_temp_local(ValType::I64);

    // i = 1 (element 0 is trivially sorted).
    ctx.emit(Instruction::I64Const(1));
    ctx.emit(Instruction::LocalSet(i_local));

    // Outer loop: for i in 1..len.
    emit_breakable_loop(ctx, |ctx| {
        // i >= len → break outer.
        ctx.emit(Instruction::LocalGet(i_local));
        ctx.emit(Instruction::LocalGet(len_local));
        ctx.emit(Instruction::I64GeS);
        ctx.emit(Instruction::BrIf(LOOP_BREAK));
        // key = copy[i].
        let key_addr = emit_list_elem_addr(ctx, copy_local, i_local, elem_size);
        emit_field_load(ctx, key_addr, 0, &elem_ty)?;
        store_stack_into_locals(ctx, &key_locals);
        if let Some(frame) = key_frame_local {
            emit_gc_set_root_at(ctx, b, frame, 0, key_locals[0])?;
        }
        // j = i - 1.
        ctx.emit(Instruction::LocalGet(i_local));
        ctx.emit(Instruction::I64Const(1));
        ctx.emit(Instruction::I64Sub);
        ctx.emit(Instruction::LocalSet(j_local));

        // Inner loop: while j >= 0 && cmp(copy[j], key) > 0:
        //                  copy[j+1] = copy[j]; j -= 1.
        // Inner LOOP_BREAK / LOOP_CONTINUE refer to the inner loop —
        // emit_breakable_loop's nesting shifts the outer depths by 2.
        emit_breakable_loop(ctx, |ctx| {
            // j < 0 → break inner.
            ctx.emit(Instruction::LocalGet(j_local));
            ctx.emit(Instruction::I64Const(0));
            ctx.emit(Instruction::I64LtS);
            ctx.emit(Instruction::BrIf(LOOP_BREAK));
            // cmp_a = copy[j].
            let cj_addr = emit_list_elem_addr(ctx, copy_local, j_local, elem_size);
            emit_field_load(ctx, cj_addr, 0, &elem_ty)?;
            store_stack_into_locals(ctx, &cmp_a_locals);
            // c = comparator(copy[j], key) → i64 on stack.
            let cmp_args = vec![cmp_a_locals.clone(), key_locals.clone()];
            emit_closure_call_raw(ctx, b, closure_ptr_local, &closure_ir_ty, &cmp_args)?;
            ctx.emit(Instruction::LocalSet(cmp_result_local));
            // c <= 0 → stop shifting (stable: ties keep left/original order).
            ctx.emit(Instruction::LocalGet(cmp_result_local));
            ctx.emit(Instruction::I64Const(0));
            ctx.emit(Instruction::I64LeS);
            ctx.emit(Instruction::BrIf(LOOP_BREAK));
            // copy[j+1] = copy[j] (cmp_a_locals still holds copy[j]).
            ctx.emit(Instruction::LocalGet(j_local));
            ctx.emit(Instruction::I64Const(1));
            ctx.emit(Instruction::I64Add);
            ctx.emit(Instruction::LocalSet(jp1_local));
            let shift_dst = emit_list_elem_addr(ctx, copy_local, jp1_local, elem_size);
            emit_field_store(ctx, shift_dst, 0, &elem_ty, &cmp_a_locals)?;
            // j -= 1.
            ctx.emit(Instruction::LocalGet(j_local));
            ctx.emit(Instruction::I64Const(1));
            ctx.emit(Instruction::I64Sub);
            ctx.emit(Instruction::LocalSet(j_local));
            ctx.emit(Instruction::Br(LOOP_CONTINUE));
            Ok(())
        })?;

        // copy[j+1] = key.
        ctx.emit(Instruction::LocalGet(j_local));
        ctx.emit(Instruction::I64Const(1));
        ctx.emit(Instruction::I64Add);
        ctx.emit(Instruction::LocalSet(jp1_local));
        let insert_dst = emit_list_elem_addr(ctx, copy_local, jp1_local, elem_size);
        emit_field_store(ctx, insert_dst, 0, &elem_ty, &key_locals)?;
        // i += 1.
        ctx.emit(Instruction::LocalGet(i_local));
        ctx.emit(Instruction::I64Const(1));
        ctx.emit(Instruction::I64Add);
        ctx.emit(Instruction::LocalSet(i_local));
        ctx.emit(Instruction::Br(LOOP_CONTINUE));
        Ok(())
    })?;

    if let Some(frame) = key_frame_local {
        emit_gc_pop_frame_at(ctx, b, frame)?;
    }
    Ok(())
}

/// Translate `print(value)` — dispatch on the value's Phoenix
/// [`IrType`] to the matching `phx_print_*` runtime export.
fn translate_print_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
) -> Result<(), CompileError> {
    let arg = *args.first().ok_or_else(|| {
        CompileError::new(
            "wasm32-linear: `print` builtin called with zero arguments — \
             IR verifier should have caught this"
                .to_string(),
        )
    })?;
    let arg_ir_ty = ctx.binding_of(arg)?.ir_type.clone();
    match arg_ir_ty {
        IrType::I64 => {
            let idx = b.require_phx_func("phx_print_i64")?;
            ctx.emit_load_all(arg)?;
            ctx.emit(Instruction::Call(idx));
        }
        IrType::Bool => {
            let idx = b.require_phx_func("phx_print_bool")?;
            ctx.emit_load_all(arg)?;
            ctx.emit(Instruction::Call(idx));
        }
        IrType::StringRef => {
            // `phx_print_str(ptr: i32, len: i32) -> ()` — push the
            // fat pointer's two slots in declaration order. Works
            // uniformly for `Op::ConstString` data-section pointers
            // (decision H) and heap pointers produced by runtime ops
            // (`phx_str_concat`, `phx_i64_to_str`, …) because the
            // runtime treats the fat pointer as a borrowed slice.
            let idx = b.require_phx_func("phx_print_str")?;
            ctx.emit_load_all(arg)?;
            ctx.emit(Instruction::Call(idx));
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `print` on argument of IR type `{other:?}` \
                 not yet supported (Phase 2.4 PR 3c — see docs/design-decisions.md §Phase 2.4)"
            )));
        }
    }
    Ok(())
}

/// Translate `toString(value)` — convert a primitive Phoenix value to
/// a heap-allocated `String` via the runtime's `phx_*_to_str` family.
/// These runtime functions are declared `extern "C" fn(val) ->
/// PhxFatPtr` in Rust source; the wasm32-wasip1 C ABI lowers the
/// struct return via an implicit *sret* pointer as the first
/// argument. The SP-management dance lives in
/// [`emit_sret_string_call`] and is shared with `Op::StringConcat`.
///
/// `toString(String)` is the identity — no runtime call needed; the
/// arg's two slots are aliased into the result binding so the rest
/// of the translator can treat `toString` uniformly without the
/// caller having to know whether the source operand was already a
/// String.
fn translate_to_string_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    // Validate IR shape first — bail before doing any plumbing work
    // (argument-type inspection, runtime-fn lookup, SP-global lookup)
    // so an IR malformation surfaces as a clean diagnostic instead of
    // accidentally leaving partial state in the builder.
    let vid = expect_result(instr, "Op::BuiltinCall(\"toString\")")?;
    // Hard arity check rather than debug_assert + `args.first()`: in
    // release builds with `args.len() > 1`, the silent-truncation
    // shape (debug_assert no-ops, `args[0]` is used) would silently
    // drop the extra args. The IR verifier should prevent this, but
    // a one-line guard keeps debug/release behavior identical on the
    // arity edge.
    if args.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `toString` builtin takes exactly one argument; \
             got {} (IR verifier should have caught this)",
            args.len(),
        )));
    }
    let arg = args[0];
    // Resolve dispatch by *reference* — `IrType` carries owned
    // `String`/`Vec` payloads on its reference variants, so cloning
    // for the dispatch table is wasted work. The borrow released at
    // the end of this match lets the mutating builder calls below
    // run unobstructed.
    let runtime_fn_name = match &ctx.binding_of(arg)?.ir_type {
        // `toString(String)` is the source-level identity: alias
        // `vid` to the arg's existing binding (same locals, same
        // IrType). No runtime call, no SP plumbing, no new locals,
        // no `local.get`/`local.set` copies — and no shadow-stack
        // rooting needed (the source binding is already rooted by
        // its defining op). Future reads of `vid` resolve via
        // `binding_of` to the same locals the arg already owns.
        IrType::StringRef => {
            debug_assert_ne!(
                vid, arg,
                "Op::BuiltinCall(toString) result must differ from its arg \
                 (single-assignment IR invariant)"
            );
            debug_assert_eq!(
                ctx.binding_of(arg)?.locals.len(),
                2,
                "StringRef arg must be 2 slots"
            );
            ctx.alias_binding(vid, arg)?;
            return Ok(());
        }
        IrType::I64 => "phx_i64_to_str",
        IrType::F64 => "phx_f64_to_str",
        IrType::Bool => "phx_bool_to_str",
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `toString` on argument of IR type `{other:?}` \
                 not yet supported (only `Int` / `Float` / `Bool` / `String` \
                 lower today)"
            )));
        }
    };
    let runtime_idx = b.require_phx_func(runtime_fn_name)?;
    emit_sret_string_call(ctx, b, runtime_idx, &[arg], vid)
}
