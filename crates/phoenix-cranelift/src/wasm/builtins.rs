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
        // `ListBuilder.<method>` / `MapBuilder.<method>` — the Phase 2.7
        // transient-mutable accumulators. Both route to the merged
        // runtime's `phx_{list,map}_builder_*` functions, the same shape
        // native uses. Placed after the `List.`/`Map.` arms above;
        // ordering is harmless because the `.` guards collision —
        // `"ListBuilder.alloc"` does not start with `"List."`.
        lb if lb.starts_with("ListBuilder.") => translate_list_builder_method_builtin(
            ctx,
            b,
            lb.strip_prefix("ListBuilder.").unwrap(),
            args,
            instr,
        ),
        mb if mb.starts_with("MapBuilder.") => translate_map_builder_method_builtin(
            ctx,
            b,
            mb.strip_prefix("MapBuilder.").unwrap(),
            args,
            instr,
        ),
        string_method if string_method.starts_with("String.") => translate_string_method_builtin(
            ctx,
            b,
            string_method.strip_prefix("String.").unwrap(),
            args,
            instr,
        ),
        other => Err(CompileError::new(format!(
            "wasm32-linear: builtin `{other}` not yet supported \
             (Phase 2.4 PR 3c — see docs/design-decisions.md §Phase 2.4)"
        ))),
    }
}

/// `String.<method>` dispatcher. Today only `length` is wired up —
/// the only string method any matrix fixture actually calls. The
/// transform-returning methods (`trim` / `toLowerCase` / `toUpperCase`
/// → fresh String), the predicate methods (`contains` / `startsWith` /
/// `endsWith` → Bool), and the `Int`-returning `indexOf` share the
/// existing shadow-stack and predicate machinery and can be wired up
/// opportunistically when a fixture or user program reaches for them.
fn translate_string_method_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    method: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match method {
        "length" => {
            // `phx_str_length(ptr: i32, len: i32) -> i64`. The
            // receiver is a `StringRef` → 2 slots `[ptr, len]`; push
            // both in declaration order and the runtime returns the
            // byte length.
            let vid = expect_result(instr, "BuiltinCall(\"String.length\")")?;
            if args.len() != 1 {
                return Err(CompileError::new(format!(
                    "wasm32-linear: `BuiltinCall(\"String.length\")` requires 1 arg \
                     (the string), got {} (internal compiler bug — IR verifier should \
                     have caught this)",
                    args.len()
                )));
            }
            let recv_vid = args[0];
            ctx.emit_load_all(recv_vid)?;
            let idx = b.require_phx_func("phx_str_length")?;
            ctx.emit(Instruction::Call(idx));
            ctx.emit_store_result(vid, IrType::I64)?;
            Ok(())
        }
        other => Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"String.{other}\")` not yet supported \
             (Phase 2.4 — opportunistic enablement)"
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
/// methods to their own helpers. The full method surface the
/// interpreter recognizes is now lowered (`is*` / `unwrap` /
/// `unwrapOr` / `unwrapOrElse` / `map` / `mapErr` / `andThen` /
/// `orElse` / `filter` / `ok` / `err` / `okOr`), so the catch-all
/// arm is defensive: a valid program never reaches it (an unknown
/// method is rejected earlier by the type checker as "no method `m`
/// on type `Option`/`Result`").
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
        // `unwrap` extracts the positive variant's payload or panics
        // (Phoenix's `phx_panic`) with a static message reserved in the
        // user-data region. Shared between Option and Result.
        (_, "unwrap") => translate_enum_unwrap(ctx, b, ir_module, enum_name, args, instr),
        // `Result.ok` / `Result.err`: convert one side of a `Result`
        // into an `Option`. Pure branching, no closure.
        ("Result", "ok") => translate_result_to_option(ctx, b, ir_module, "Ok", args, instr),
        ("Result", "err") => translate_result_to_option(ctx, b, ir_module, "Err", args, instr),
        // `Option.okOr`: convert `Option<T>` into `Result<T, E>` by
        // tagging the `None` case with a caller-supplied default `Err`
        // value. Pure branching + enum construct, no closure.
        ("Option", "okOr") => translate_option_okor(ctx, b, ir_module, args, instr),
        // Closure-payload transforms: each invokes a user closure on
        // the matching variant's payload (or in the `*OrElse` family,
        // the *non*-matching variant's payload) and rebuilds the result
        // enum from the closure's return.
        ("Option", "map") => translate_option_map(ctx, b, ir_module, args, instr),
        ("Option", "andThen") => translate_option_and_then(ctx, b, ir_module, args, instr),
        ("Option", "orElse") => translate_option_or_else(ctx, b, ir_module, args, instr),
        ("Option", "unwrapOrElse") => {
            translate_enum_unwrap_or_else(ctx, b, ir_module, OPTION_ENUM, args, instr)
        }
        ("Option", "filter") => translate_option_filter(ctx, b, ir_module, args, instr),
        ("Result", "map") => translate_result_map(ctx, b, ir_module, args, instr),
        ("Result", "mapErr") => translate_result_map_err(ctx, b, ir_module, args, instr),
        ("Result", "andThen") => translate_result_and_then(ctx, b, ir_module, args, instr),
        ("Result", "orElse") => translate_result_or_else(ctx, b, ir_module, args, instr),
        ("Result", "unwrapOrElse") => {
            translate_enum_unwrap_or_else(ctx, b, ir_module, RESULT_ENUM, args, instr)
        }
        // Unreachable for a valid program: every method the sema layer
        // and IR interpreter recognize on `Option`/`Result` is dispatched
        // above. An unknown method is rejected earlier by the type checker
        // ("no method `m` on type `Option`/`Result`"), so reaching here
        // means the IR is malformed.
        _ => Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"{enum_name}.{method}\")` reached the \
             dispatcher catch-all — the type checker should have rejected an \
             unknown `{enum_name}` method (internal compiler bug)"
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

/// `Option.unwrap()` / `Result.unwrap()` — extract the positive
/// variant's payload, or trap via `phx_panic` on the negative variant.
/// The panic message is reserved per call site in the user-data
/// region (each unwrap call site gets its own offset — the messages
/// are short and dedup isn't worth the bookkeeping).
///
/// The negative arm ends in `phx_panic` followed by `unreachable`,
/// satisfying WASM's branch-result-typing rules without a multi-value
/// block type — the typed result is materialized in result-locals on
/// the positive arm only, and pushed onto the stack after the
/// `If`/`Else`/`End` closes.
fn translate_enum_unwrap(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    enum_name: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"{enum_name}.unwrap\")` requires 1 arg \
             (receiver), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"enum.unwrap\")")?;
    let (positive_variant, panic_msg) = match enum_name {
        OPTION_ENUM => ("Some", "called Option.unwrap on None"),
        RESULT_ENUM => ("Ok", "called Result.unwrap on Err"),
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `unwrap` on enum `{other}` — only `Option` / \
                 `Result` are supported (internal compiler bug)"
            )));
        }
    };
    let positive_idx = find_variant_index(ir_module, enum_name, positive_variant)?;
    let recv_local = ctx.binding_of(args[0])?.single_local();
    let payload_ty = instr.result_type.clone();
    let variant = compute_variant_field_offsets(std::slice::from_ref(&payload_ty))?;
    let payload_offset = variant.field_offsets[0];

    let result_locals = ctx.allocate_locals_for_ir_type_anon(&payload_ty)?;
    let (msg_off, msg_len) = b.reserve_user_data(panic_msg.as_bytes())?;
    let panic_idx = b.require_phx_func("phx_panic")?;

    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::I32Load(i32_memarg(0)));
    ctx.emit(Instruction::I32Const(positive_idx as i32));
    ctx.emit(Instruction::I32Eq);
    ctx.emit(Instruction::If(BlockType::Empty));
    // Positive arm: extract payload into result_locals.
    emit_field_load(ctx, recv_local, payload_offset, &payload_ty)?;
    store_stack_into_locals(ctx, &result_locals);
    ctx.emit(Instruction::Else);
    // Negative arm: panic + unreachable (no value materialized — WASM
    // accepts `unreachable` as any type, so the empty-result block
    // shape is well-typed even though only the positive arm writes
    // to result_locals).
    ctx.emit(Instruction::I32Const(msg_off as i32));
    ctx.emit(Instruction::I32Const(msg_len as i32));
    ctx.emit(Instruction::Call(panic_idx));
    ctx.emit(Instruction::Unreachable);
    ctx.emit(Instruction::End);
    for &local in &result_locals {
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit_store_result(vid, payload_ty)?;
    Ok(())
}

/// `Result.ok()` (`tag = "Ok"`) → `Option<T>` carrying the Ok payload.
/// `Result.err()` (`tag = "Err"`) → `Option<E>` carrying the Err payload.
///
/// In both cases the matching arm wraps the extracted payload in
/// `Some`; the mismatching arm yields `None`. The block result is
/// `i32` (the Option pointer); both arms leave that pointer on the
/// stack — no result-locals dance needed because Option pointers are
/// single-slot regardless of payload type.
fn translate_result_to_option(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    tag: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 1 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"Result.{}\")` requires 1 arg \
             (receiver), got {} (internal compiler bug)",
            tag.to_lowercase(),
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"Result.ok/err\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    // `ok` reads `T` (type arg 0), `err` reads `E` (type arg 1) — both via
    // the shared `enum_targ` extractor that the closure-payload transforms
    // use, so the receiver-shape diagnostic stays consistent across sites.
    let targ_idx = match tag {
        "Ok" => 0,
        "Err" => 1,
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `translate_result_to_option` got tag `{other}` — \
                 only `Ok` / `Err` are supported (internal compiler bug)"
            )));
        }
    };
    let payload_ty = enum_targ(&recv_binding.ir_type, RESULT_ENUM, targ_idx)?;
    let payload_idx = find_variant_index(ir_module, RESULT_ENUM, tag)?;
    let variant = compute_variant_field_offsets(std::slice::from_ref(&payload_ty))?;
    let payload_offset = variant.field_offsets[0];

    let payload_locals = ctx.allocate_locals_for_ir_type_anon(&payload_ty)?;
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::I32Load(i32_memarg(0)));
    ctx.emit(Instruction::I32Const(payload_idx as i32));
    ctx.emit(Instruction::I32Eq);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // Matching arm: build Some(payload).
    emit_field_load(ctx, recv_local, payload_offset, &payload_ty)?;
    store_stack_into_locals(ctx, &payload_locals);
    emit_option_some(ctx, b, ir_module, &payload_ty, &payload_locals)?;
    ctx.emit(Instruction::Else);
    emit_option_none(ctx, b, ir_module)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Option.okOr(default_err)` — convert `Option<T>` to
/// `Result<T, E>`: `Some(t)` becomes `Ok(t)`, `None` becomes
/// `Err(default_err)`.
///
/// Block result is `i32` (the Result pointer); the matching arm
/// extracts the `Some` payload and re-wraps as `Ok`, the negative arm
/// constructs `Err` with the caller's default value (already in
/// locals — no staging needed). The `Err` payload type comes from
/// `args[1]`'s binding (the concrete `E`).
fn translate_option_okor(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"Option.okOr\")` requires 2 args \
             (receiver, default_err), got {} (internal compiler bug)",
            args.len()
        )));
    }
    let vid = expect_result(instr, "BuiltinCall(\"Option.okOr\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let some_ty = match &recv_binding.ir_type {
        IrType::EnumRef(name, targs) if name == OPTION_ENUM => {
            targs.first().cloned().ok_or_else(|| {
                CompileError::new(
                    "wasm32-linear: `Option.okOr` receiver missing type arg `T` \
                     (internal compiler bug)"
                        .to_string(),
                )
            })?
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-linear: `Option.okOr` receiver must be `Option<T>`, got \
                 `{other:?}` (internal compiler bug)"
            )));
        }
    };
    let err_binding = ctx.binding_of(args[1])?;
    let err_locals = err_binding.locals.clone();
    let err_ty = err_binding.ir_type.clone();
    let some_idx = find_variant_index(ir_module, OPTION_ENUM, "Some")?;
    let ok_idx = find_variant_index(ir_module, RESULT_ENUM, "Ok")?;
    let err_idx = find_variant_index(ir_module, RESULT_ENUM, "Err")?;
    let some_variant = compute_variant_field_offsets(std::slice::from_ref(&some_ty))?;
    let some_offset = some_variant.field_offsets[0];

    let payload_locals = ctx.allocate_locals_for_ir_type_anon(&some_ty)?;
    let payload_locals_owned = vec![payload_locals.clone()];
    let err_locals_owned = vec![err_locals];

    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::I32Load(i32_memarg(0)));
    ctx.emit(Instruction::I32Const(some_idx as i32));
    ctx.emit(Instruction::I32Eq);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // Some path: extract T, build Ok(T).
    emit_field_load(ctx, recv_local, some_offset, &some_ty)?;
    store_stack_into_locals(ctx, &payload_locals);
    emit_enum_construct(
        ctx,
        b,
        RESULT_ENUM,
        ok_idx,
        std::slice::from_ref(&some_ty),
        &payload_locals_owned,
    )?;
    ctx.emit(Instruction::Else);
    // None path: build Err(default_err).
    emit_enum_construct(
        ctx,
        b,
        RESULT_ENUM,
        err_idx,
        std::slice::from_ref(&err_ty),
        &err_locals_owned,
    )?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

// --- Closure-payload transforms ---------------------------------------
//
// The methods below share a structural pattern: load the receiver's
// discriminant, branch on it, run a closure on one side's payload (or
// emit a passthrough/short-circuit on the other), and produce a result
// enum or unwrapped value. Common helpers below cover the shared bits.

/// Pull the closure's user-facing return type out of its `IrType`.
fn closure_return_type(closure_ir_ty: &IrType) -> Result<IrType, CompileError> {
    match closure_ir_ty {
        IrType::ClosureRef { return_type, .. } => Ok((**return_type).clone()),
        other => Err(CompileError::new(format!(
            "wasm32-linear: expected `ClosureRef` for closure, got `{other:?}` \
             (internal compiler bug)"
        ))),
    }
}

/// Extract one type-arg from an `EnumRef` (e.g. `T` from `Option<T>`,
/// `E` from `Result<T, E>`). `targ_idx` is 0 for the first type arg.
fn enum_targ(ty: &IrType, expected_name: &str, targ_idx: usize) -> Result<IrType, CompileError> {
    match ty {
        IrType::EnumRef(name, targs) if name == expected_name => {
            targs.get(targ_idx).cloned().ok_or_else(|| {
                CompileError::new(format!(
                    "wasm32-linear: `{expected_name}` receiver missing type arg \
                     [{targ_idx}] (internal compiler bug)"
                ))
            })
        }
        other => Err(CompileError::new(format!(
            "wasm32-linear: expected `{expected_name}<...>` receiver, got \
             `{other:?}` (internal compiler bug)"
        ))),
    }
}

/// If `ty` is ref-typed, push a 1-slot ad-hoc shadow frame and root
/// `value_local`'s first slot into it, returning the frame local for
/// a later pop. No-op (returns `None`) for value-typed payloads. Used
/// to keep closure results alive across the subsequent enum-construct
/// `phx_gc_alloc`.
fn maybe_root_ref_payload(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ty: &IrType,
    value_local: u32,
) -> Result<Option<u32>, CompileError> {
    if !ty.is_ref_type() {
        return Ok(None);
    }
    let frame = emit_gc_push_frame_at(ctx, b, 1)?;
    emit_gc_set_root_at(ctx, b, frame, 0, value_local)?;
    Ok(Some(frame))
}

/// Counterpart to [`maybe_root_ref_payload`] — pop the ad-hoc frame
/// if one was pushed. Cheap no-op for the `None` (value-typed) case.
fn maybe_pop_ref_frame(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    frame: Option<u32>,
) -> Result<(), CompileError> {
    if let Some(f) = frame {
        emit_gc_pop_frame_at(ctx, b, f)?;
    }
    Ok(())
}

/// Validate that a closure-payload transform got exactly its two
/// arguments — `(receiver, closure)`. Every transform in this section
/// has the same arity, so the check and its diagnostic live here
/// rather than being copy-pasted into each translator. `method` is the
/// `Enum.method` name used in the error message.
fn expect_receiver_and_closure(args: &[ValueId], method: &str) -> Result<(), CompileError> {
    if args.len() != 2 {
        return Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"{method}\")` requires 2 args \
             (receiver, closure), got {} (internal compiler bug)",
            args.len()
        )));
    }
    Ok(())
}

/// Emit the discriminant-equality prelude shared by every
/// closure-payload transform: load the receiver's discriminant (the
/// `i32` at offset 0), compare it against `variant_idx`, and leave the
/// boolean on the stack for a following `If`.
fn emit_discriminant_eq(ctx: &mut FuncTranslateCtx, recv_local: u32, variant_idx: u32) {
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::I32Load(i32_memarg(0)));
    ctx.emit(Instruction::I32Const(variant_idx as i32));
    ctx.emit(Instruction::I32Eq);
}

/// `Option.map(f: T -> U)` -> `Option<U>`.
///
/// `Some(t)` → `Some(f(t))`, `None` → `None`. The closure runs on the
/// extracted `T` payload; its `U` return value is wrapped in a fresh
/// `Some`. When `U` is ref-typed, the closure result is rooted in an
/// ad-hoc shadow frame across the `emit_option_some` allocation so an
/// allocating closure can't be swept by the wrap-side `phx_gc_alloc`.
fn translate_option_map(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, "Option.map")?;
    let vid = expect_result(instr, "BuiltinCall(\"Option.map\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let t_ty = enum_targ(&recv_binding.ir_type, OPTION_ENUM, 0)?;
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    let u_ty = closure_return_type(&closure_ir_ty)?;

    let some_idx = find_variant_index(ir_module, OPTION_ENUM, "Some")?;
    let variant_in = compute_variant_field_offsets(std::slice::from_ref(&t_ty))?;
    let t_offset = variant_in.field_offsets[0];

    let t_locals = ctx.allocate_locals_for_ir_type_anon(&t_ty)?;
    let u_locals = ctx.allocate_locals_for_ir_type_anon(&u_ty)?;

    emit_discriminant_eq(ctx, recv_local, some_idx);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // Some path: extract T, call f(T) → U, wrap as Some(U).
    emit_field_load(ctx, recv_local, t_offset, &t_ty)?;
    store_stack_into_locals(ctx, &t_locals);
    let call_args = vec![t_locals];
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    store_stack_into_locals(ctx, &u_locals);
    let root_frame = maybe_root_ref_payload(ctx, b, &u_ty, u_locals[0])?;
    emit_option_some(ctx, b, ir_module, &u_ty, &u_locals)?;
    maybe_pop_ref_frame(ctx, b, root_frame)?;
    ctx.emit(Instruction::Else);
    emit_option_none(ctx, b, ir_module)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Option.andThen(f: T -> Option<U>)` -> `Option<U>`. The closure
/// already returns the final `Option<U>`, so the positive arm hands
/// the closure result straight through (no rewrap). `None` produces a
/// fresh `None` on the negative arm.
fn translate_option_and_then(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, "Option.andThen")?;
    let vid = expect_result(instr, "BuiltinCall(\"Option.andThen\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let t_ty = enum_targ(&recv_binding.ir_type, OPTION_ENUM, 0)?;
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    let some_idx = find_variant_index(ir_module, OPTION_ENUM, "Some")?;
    let variant_in = compute_variant_field_offsets(std::slice::from_ref(&t_ty))?;
    let t_offset = variant_in.field_offsets[0];
    let t_locals = ctx.allocate_locals_for_ir_type_anon(&t_ty)?;

    emit_discriminant_eq(ctx, recv_local, some_idx);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    emit_field_load(ctx, recv_local, t_offset, &t_ty)?;
    store_stack_into_locals(ctx, &t_locals);
    let call_args = vec![t_locals];
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    // Closure return is already Option<U>; leave on stack.
    ctx.emit(Instruction::Else);
    emit_option_none(ctx, b, ir_module)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Option.orElse(f: () -> Option<T>)` -> `Option<T>`. The closure
/// takes no user arg; it runs on the `None` path and its return is
/// the result. The `Some` path passes the receiver pointer through
/// unchanged — the input and output types match exactly.
fn translate_option_or_else(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, "Option.orElse")?;
    let vid = expect_result(instr, "BuiltinCall(\"Option.orElse\")")?;
    let recv_local = ctx.binding_of(args[0])?.single_local();
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    let some_idx = find_variant_index(ir_module, OPTION_ENUM, "Some")?;

    emit_discriminant_eq(ctx, recv_local, some_idx);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // Some path: passthrough receiver (same Option<T> type).
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::Else);
    // None path: call f() → Option<T>, leave on stack.
    let call_args: Vec<Vec<u32>> = Vec::new();
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Option.filter(pred: T -> Bool)` -> `Option<T>`. On `Some(t)`,
/// runs the predicate: keep as `Some(t)` if true, otherwise yield
/// `None`. `None` is passed through.
fn translate_option_filter(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, "Option.filter")?;
    let vid = expect_result(instr, "BuiltinCall(\"Option.filter\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let t_ty = enum_targ(&recv_binding.ir_type, OPTION_ENUM, 0)?;
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    let some_idx = find_variant_index(ir_module, OPTION_ENUM, "Some")?;
    let variant_in = compute_variant_field_offsets(std::slice::from_ref(&t_ty))?;
    let t_offset = variant_in.field_offsets[0];
    let t_locals = ctx.allocate_locals_for_ir_type_anon(&t_ty)?;

    emit_discriminant_eq(ctx, recv_local, some_idx);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // Some path: extract T, call pred(T) → Bool. The predicate's `Bool`
    // result is left on the stack to drive the keep/drop `If` directly.
    emit_field_load(ctx, recv_local, t_offset, &t_ty)?;
    store_stack_into_locals(ctx, &t_locals);
    let call_args = vec![t_locals.clone()];
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // pred true: rebuild Some(t). `t` stays reachable through the
    // (rooted) receiver across the predicate call, so this re-root is
    // belt-and-suspenders: it pins `t` directly across the Some
    // allocation rather than relying on the receiver root. No-op for
    // value-typed `t`.
    let root_frame = maybe_root_ref_payload(ctx, b, &t_ty, t_locals[0])?;
    emit_option_some(ctx, b, ir_module, &t_ty, &t_locals)?;
    maybe_pop_ref_frame(ctx, b, root_frame)?;
    ctx.emit(Instruction::Else);
    emit_option_none(ctx, b, ir_module)?;
    ctx.emit(Instruction::End);
    ctx.emit(Instruction::Else);
    emit_option_none(ctx, b, ir_module)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Option.unwrapOrElse` / `Result.unwrapOrElse`: extract the
/// positive-variant payload, or call the closure (no user args for
/// Option; the Err payload for Result) and use its return.
fn translate_enum_unwrap_or_else(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    enum_name: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, &format!("{enum_name}.unwrapOrElse"))?;
    let vid = expect_result(instr, "BuiltinCall(\"enum.unwrapOrElse\")")?;
    let positive_variant = match enum_name {
        OPTION_ENUM => "Some",
        RESULT_ENUM => "Ok",
        _ => unreachable!("dispatcher restricts to Option/Result"),
    };
    let positive_idx = find_variant_index(ir_module, enum_name, positive_variant)?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let recv_ir_type = recv_binding.ir_type.clone();
    let payload_ty = instr.result_type.clone();
    let variant = compute_variant_field_offsets(std::slice::from_ref(&payload_ty))?;
    let payload_offset = variant.field_offsets[0];
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();

    let result_locals = ctx.allocate_locals_for_ir_type_anon(&payload_ty)?;

    emit_discriminant_eq(ctx, recv_local, positive_idx);
    ctx.emit(Instruction::If(BlockType::Empty));
    // Positive arm: extract payload.
    emit_field_load(ctx, recv_local, payload_offset, &payload_ty)?;
    store_stack_into_locals(ctx, &result_locals);
    ctx.emit(Instruction::Else);
    // Negative arm: call closure. For Result, closure takes the Err
    // payload as its single user arg; for Option, the closure takes
    // no user args.
    let call_args: Vec<Vec<u32>> = if enum_name == RESULT_ENUM {
        let err_ty = enum_targ(&recv_ir_type, RESULT_ENUM, 1)?;
        let err_variant = compute_variant_field_offsets(std::slice::from_ref(&err_ty))?;
        let err_offset = err_variant.field_offsets[0];
        let err_locals = ctx.allocate_locals_for_ir_type_anon(&err_ty)?;
        emit_field_load(ctx, recv_local, err_offset, &err_ty)?;
        store_stack_into_locals(ctx, &err_locals);
        vec![err_locals]
    } else {
        Vec::new()
    };
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    store_stack_into_locals(ctx, &result_locals);
    ctx.emit(Instruction::End);
    for &local in &result_locals {
        ctx.emit(Instruction::LocalGet(local));
    }
    ctx.emit_store_result(vid, payload_ty)?;
    Ok(())
}

/// `Result.map(f: T -> U)` -> `Result<U, E>`. Ok arm runs the closure
/// and wraps the result as Ok(U); Err arm passes the receiver pointer
/// through unchanged (the Err(E) layout is identical between
/// `Result<T, E>` and `Result<U, E>`).
fn translate_result_map(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, "Result.map")?;
    let vid = expect_result(instr, "BuiltinCall(\"Result.map\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let t_ty = enum_targ(&recv_binding.ir_type, RESULT_ENUM, 0)?;
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    let u_ty = closure_return_type(&closure_ir_ty)?;
    let ok_idx = find_variant_index(ir_module, RESULT_ENUM, "Ok")?;
    let variant_in = compute_variant_field_offsets(std::slice::from_ref(&t_ty))?;
    let t_offset = variant_in.field_offsets[0];
    let t_locals = ctx.allocate_locals_for_ir_type_anon(&t_ty)?;
    let u_locals = ctx.allocate_locals_for_ir_type_anon(&u_ty)?;

    emit_discriminant_eq(ctx, recv_local, ok_idx);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // Ok path: extract T, call f → U, build Ok(U) of new Result type.
    emit_field_load(ctx, recv_local, t_offset, &t_ty)?;
    store_stack_into_locals(ctx, &t_locals);
    let call_args = vec![t_locals];
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    store_stack_into_locals(ctx, &u_locals);
    let root_frame = maybe_root_ref_payload(ctx, b, &u_ty, u_locals[0])?;
    let u_locals_owned = vec![u_locals.clone()];
    emit_enum_construct(
        ctx,
        b,
        RESULT_ENUM,
        ok_idx,
        std::slice::from_ref(&u_ty),
        &u_locals_owned,
    )?;
    maybe_pop_ref_frame(ctx, b, root_frame)?;
    ctx.emit(Instruction::Else);
    // Err path: passthrough receiver (Err(E) layout is independent of T/U).
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Result.mapErr(f: E -> E2)` -> `Result<T, E2>`. Err arm runs the
/// closure and wraps as Err(E2); Ok arm passes the receiver pointer
/// through unchanged.
fn translate_result_map_err(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, "Result.mapErr")?;
    let vid = expect_result(instr, "BuiltinCall(\"Result.mapErr\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let e_ty = enum_targ(&recv_binding.ir_type, RESULT_ENUM, 1)?;
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    let e2_ty = closure_return_type(&closure_ir_ty)?;
    let ok_idx = find_variant_index(ir_module, RESULT_ENUM, "Ok")?;
    let err_idx = find_variant_index(ir_module, RESULT_ENUM, "Err")?;
    let variant_in = compute_variant_field_offsets(std::slice::from_ref(&e_ty))?;
    let e_offset = variant_in.field_offsets[0];
    let e_locals = ctx.allocate_locals_for_ir_type_anon(&e_ty)?;
    let e2_locals = ctx.allocate_locals_for_ir_type_anon(&e2_ty)?;

    emit_discriminant_eq(ctx, recv_local, ok_idx);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // Ok path: passthrough.
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::Else);
    // Err path: extract E, call f → E2, build Err(E2).
    emit_field_load(ctx, recv_local, e_offset, &e_ty)?;
    store_stack_into_locals(ctx, &e_locals);
    let call_args = vec![e_locals];
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    store_stack_into_locals(ctx, &e2_locals);
    let root_frame = maybe_root_ref_payload(ctx, b, &e2_ty, e2_locals[0])?;
    let e2_locals_owned = vec![e2_locals.clone()];
    emit_enum_construct(
        ctx,
        b,
        RESULT_ENUM,
        err_idx,
        std::slice::from_ref(&e2_ty),
        &e2_locals_owned,
    )?;
    maybe_pop_ref_frame(ctx, b, root_frame)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Result.andThen(f: T -> Result<U, E>)` -> `Result<U, E>`. Ok arm
/// calls the closure; its return (already a Result) is the answer.
/// Err arm passes the receiver pointer through unchanged.
fn translate_result_and_then(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, "Result.andThen")?;
    let vid = expect_result(instr, "BuiltinCall(\"Result.andThen\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let t_ty = enum_targ(&recv_binding.ir_type, RESULT_ENUM, 0)?;
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    let ok_idx = find_variant_index(ir_module, RESULT_ENUM, "Ok")?;
    let variant_in = compute_variant_field_offsets(std::slice::from_ref(&t_ty))?;
    let t_offset = variant_in.field_offsets[0];
    let t_locals = ctx.allocate_locals_for_ir_type_anon(&t_ty)?;

    emit_discriminant_eq(ctx, recv_local, ok_idx);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    emit_field_load(ctx, recv_local, t_offset, &t_ty)?;
    store_stack_into_locals(ctx, &t_locals);
    let call_args = vec![t_locals];
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    ctx.emit(Instruction::Else);
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
    Ok(())
}

/// `Result.orElse(f: E -> Result<T, E2>)` -> `Result<T, E2>`. Err
/// arm calls the closure with the extracted Err payload; the return
/// (already a Result) is the answer. Ok arm passes the receiver
/// pointer through unchanged.
fn translate_result_or_else(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    ir_module: &phoenix_ir::module::IrModule,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    expect_receiver_and_closure(args, "Result.orElse")?;
    let vid = expect_result(instr, "BuiltinCall(\"Result.orElse\")")?;
    let recv_binding = ctx.binding_of(args[0])?;
    let recv_local = recv_binding.single_local();
    let e_ty = enum_targ(&recv_binding.ir_type, RESULT_ENUM, 1)?;
    let closure_binding = ctx.binding_of(args[1])?;
    let closure_local = closure_binding.single_local();
    let closure_ir_ty = closure_binding.ir_type.clone();
    let ok_idx = find_variant_index(ir_module, RESULT_ENUM, "Ok")?;
    let variant_in = compute_variant_field_offsets(std::slice::from_ref(&e_ty))?;
    let e_offset = variant_in.field_offsets[0];
    let e_locals = ctx.allocate_locals_for_ir_type_anon(&e_ty)?;

    emit_discriminant_eq(ctx, recv_local, ok_idx);
    ctx.emit(Instruction::If(BlockType::Result(ValType::I32)));
    // Ok path: passthrough.
    ctx.emit(Instruction::LocalGet(recv_local));
    ctx.emit(Instruction::Else);
    // Err path: extract E, call f → Result<T, E2>.
    emit_field_load(ctx, recv_local, e_offset, &e_ty)?;
    store_stack_into_locals(ctx, &e_locals);
    let call_args = vec![e_locals];
    emit_closure_call_raw(ctx, b, closure_local, &closure_ir_ty, &call_args)?;
    ctx.emit(Instruction::End);
    ctx.emit_store_result(vid, instr.result_type.clone())?;
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

/// `ListBuilder.<method>` → the merged runtime's `phx_list_builder_*`
/// (§Phase 2.7 decision F). `alloc` derives the element size from the
/// result `ListBuilderRef(T)`; `push` stages the element on the shadow
/// stack and passes its address (the runtime copies by value, so no
/// rooting); `freeze` hands the handle back for a one-shot memcpy into a
/// fresh `List<T>`. Use-after-freeze is enforced in the runtime.
fn translate_list_builder_method_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    method: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match method {
        "alloc" => {
            let vid = expect_result(instr, "BuiltinCall(\"ListBuilder.alloc\")")?;
            let elem_ty = match &instr.result_type {
                IrType::ListBuilderRef(t) => (**t).clone(),
                other => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `ListBuilder.alloc` result type is `{other:?}`, \
                         expected `ListBuilderRef` (internal compiler bug)"
                    )));
                }
            };
            let es = phx_field_size_bytes(&elem_ty)?;
            let idx = b.require_phx_func("phx_list_builder_alloc")?;
            ctx.emit(Instruction::I64Const(es as i64));
            ctx.emit(Instruction::Call(idx));
            ctx.emit_store_result(vid, instr.result_type.clone())?;
            Ok(())
        }
        "push" => {
            // args[0] = handle, args[1] = element. No result (in-place).
            let handle = ctx.binding_of(args[0])?.single_local();
            let elem_binding = ctx.binding_of(args[1])?;
            let elem_locals = elem_binding.locals.clone();
            let elem_ty = elem_binding.ir_type.clone();
            let es = phx_field_size_bytes(&elem_ty)?;
            let (saved_sp, frame_local) = emit_alloc_stack_frame(ctx, b, es)?;
            emit_field_store(ctx, frame_local, 0, &elem_ty, &elem_locals)?;
            let idx = b.require_phx_func("phx_list_builder_push")?;
            ctx.emit(Instruction::LocalGet(handle));
            ctx.emit(Instruction::LocalGet(frame_local));
            ctx.emit(Instruction::I64Const(es as i64));
            ctx.emit(Instruction::Call(idx));
            emit_restore_stack_frame(ctx, b, saved_sp)?;
            Ok(())
        }
        "freeze" => {
            let vid = expect_result(instr, "BuiltinCall(\"ListBuilder.freeze\")")?;
            let handle = ctx.binding_of(args[0])?.single_local();
            let idx = b.require_phx_func("phx_list_builder_freeze")?;
            ctx.emit(Instruction::LocalGet(handle));
            ctx.emit(Instruction::Call(idx));
            ctx.emit_store_result(vid, instr.result_type.clone())?;
            Ok(())
        }
        other => Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"ListBuilder.{other}\")` not yet supported"
        ))),
    }
}

/// `MapBuilder.<method>` → the merged runtime's `phx_map_builder_*`
/// (§Phase 2.7 decision F). `alloc` derives key/value sizes (and the
/// key-is-string flag) from the result `MapBuilderRef(K, V)`; `set`
/// stages the key and value into one shadow-stack frame (key at offset
/// 0, value at `key_size`) and passes both addresses; `freeze` builds
/// the hash table in one pass via `phx_map_builder_freeze`.
fn translate_map_builder_method_builtin(
    ctx: &mut FuncTranslateCtx,
    b: &mut ModuleBuilder,
    method: &str,
    args: &[ValueId],
    instr: &phoenix_ir::instruction::Instruction,
) -> Result<(), CompileError> {
    match method {
        "alloc" => {
            let vid = expect_result(instr, "BuiltinCall(\"MapBuilder.alloc\")")?;
            let (k_ty, v_ty) = match &instr.result_type {
                IrType::MapBuilderRef(k, v) => ((**k).clone(), (**v).clone()),
                other => {
                    return Err(CompileError::new(format!(
                        "wasm32-linear: `MapBuilder.alloc` result type is `{other:?}`, \
                         expected `MapBuilderRef` (internal compiler bug)"
                    )));
                }
            };
            let ks = phx_field_size_bytes(&k_ty)?;
            let vs = phx_field_size_bytes(&v_ty)?;
            let idx = b.require_phx_func("phx_map_builder_alloc")?;
            ctx.emit(Instruction::I64Const(ks as i64));
            ctx.emit(Instruction::I64Const(vs as i64));
            ctx.emit(Instruction::I64Const(k_ty.string_flag() as i64));
            ctx.emit(Instruction::Call(idx));
            ctx.emit_store_result(vid, instr.result_type.clone())?;
            Ok(())
        }
        "set" => {
            // args[0] = handle, args[1] = key, args[2] = value. No result.
            let handle = ctx.binding_of(args[0])?.single_local();
            let k_binding = ctx.binding_of(args[1])?;
            let v_binding = ctx.binding_of(args[2])?;
            let (k_locals, k_ty) = (k_binding.locals.clone(), k_binding.ir_type.clone());
            let (v_locals, v_ty) = (v_binding.locals.clone(), v_binding.ir_type.clone());
            let ks = phx_field_size_bytes(&k_ty)?;
            let vs = phx_field_size_bytes(&v_ty)?;
            // One combined frame: key at offset 0, value packed directly
            // at offset `ks` with no alignment padding. The value may land
            // unaligned (e.g. a 1-byte key before an 8-byte value), which
            // is fine — linear-memory wasm permits unaligned stores and the
            // runtime `memcpy`s `key_size`/`val_size` bytes back out, so the
            // frame is just a byte buffer (same idiom as `List.contains`).
            let (saved_sp, frame_local) = emit_alloc_stack_frame(ctx, b, ks + vs)?;
            emit_field_store(ctx, frame_local, 0, &k_ty, &k_locals)?;
            emit_field_store(ctx, frame_local, ks, &v_ty, &v_locals)?;
            let idx = b.require_phx_func("phx_map_builder_set")?;
            ctx.emit(Instruction::LocalGet(handle)); // handle
            ctx.emit(Instruction::LocalGet(frame_local)); // key_ptr
            ctx.emit(Instruction::LocalGet(frame_local)); // value_ptr = frame + ks
            ctx.emit(Instruction::I32Const(ks as i32));
            ctx.emit(Instruction::I32Add);
            ctx.emit(Instruction::I64Const(ks as i64));
            ctx.emit(Instruction::I64Const(vs as i64));
            ctx.emit(Instruction::Call(idx));
            emit_restore_stack_frame(ctx, b, saved_sp)?;
            Ok(())
        }
        "freeze" => {
            let vid = expect_result(instr, "BuiltinCall(\"MapBuilder.freeze\")")?;
            let handle = ctx.binding_of(args[0])?.single_local();
            let idx = b.require_phx_func("phx_map_builder_freeze")?;
            ctx.emit(Instruction::LocalGet(handle));
            ctx.emit(Instruction::Call(idx));
            ctx.emit_store_result(vid, instr.result_type.clone())?;
            Ok(())
        }
        other => Err(CompileError::new(format!(
            "wasm32-linear: `BuiltinCall(\"MapBuilder.{other}\")` not yet supported"
        ))),
    }
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
/// and the two 2-slot refs — `StringRef` (`ptr`, `len`) and `DynRef`
/// (`data_ptr`, `vtable_ptr`) — both keep their only GC pointer in
/// `key_locals[0]`; slot 1 is a non-pointer (`len` / data-section vtable
/// offset). Rooting slot 0 keeps the underlying object live in every
/// current case. A future 2-*pointer* ref type (whose second slot is
/// itself a GC pointer) would need this frame widened to 2 slots — that
/// type, not merely any 2-slot ref, is the tripwire for the rewrite.
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
    // list would take the ref branch. A placeholder element reaches here in
    // exactly one shape: a *generic-annotated empty* literal, `let xs:
    // List<T> = []` inside a generic function. Sema's
    // `pin_inferred_type_to_annotation` (§Phase 2.4 K.12) only refines
    // *toward a concrete* annotation, so a concrete `let xs: List<Int> =
    // []` is pinned to `List<Int>`, but a type-var annotation `List<T>` is
    // left for monomorphization — which, finding no source for the empty
    // literal's element, lowers it to `List<__generic>`. (An un-annotated
    // `let xs = []` is still rejected outright as ambiguous by
    // `check_stmt.rs::check_let`'s "cannot infer type" diagnostic.)
    //
    // Sorting such a list is therefore *reachable* but safe by
    // construction: lists are immutable (growth goes through `ListBuilder`,
    // whose `freeze()` produces a list with a concrete element type), so a
    // `__generic`-element list is *always empty*. `len == 0` on the only
    // path that reaches the ref branch, so the inner loop never executes
    // and the rooted slot never participates in a collection. The GC-root
    // emitter paired with `gc_roots::is_tracked_ref`'s placeholder skip
    // makes this safe; see that file's docstring for the joint contract.
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
    // assumption. Today every ref type either occupies one slot (a bare
    // GC pointer in slot 0) or is one of the two 2-slot fat pointers
    // whose slot 1 is a non-pointer: `StringRef` (slot 1 = `len`) and
    // `DynRef` (slot 1 = data-section vtable offset). A future ref type
    // whose second slot is also a GC pointer would need this frame
    // widened to 2 slots; this assert fires before that case silently
    // miscompiles.
    debug_assert!(
        !elem_is_ref
            || matches!(&elem_ty, IrType::StringRef | IrType::DynRef(_))
            || key_locals.len() == 1,
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
