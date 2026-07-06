//! JSON encoder synthesis.
//!
//! For every type reachable from a `json.encode(value)` call, this pass
//! synthesizes a per-type encoder function `__json_encode_<key>(v: T) ->
//! String` and records it in [`crate::module::IrModule::json_encoders`].
//! Because the synthesized routines are ordinary IR, all five backends
//! execute them uniformly — there is no per-backend serialization logic.
//! Each `json.encode` call site then lowers to an `Op::Call` of the
//! encoder for its argument's static type (see `lower_method_call`).
//!
//! Runs as a Pass 1.5 step (after declaration registration, before user
//! bodies are lowered) so encoder stubs exist before any call site — or
//! any sibling encoder — needs to reference them. Mirrors the two-pass
//! shape of [`crate::default_wrappers`].
//!
//! Covered so far: scalars (`Int`/`Float`/`Bool`/`String`), non-generic
//! structs, `Option<T>` (None → null, Some(x) → encode(x)), non-generic
//! enums (adjacently tagged), `List<T>` (array), and `Map<String, V>`
//! (object). Non-`String`-key maps and generic enums other than `Option`
//! are deferred; sema's `unsupported_json_encode_type` gate keeps
//! unsupported shapes from reaching here.

use std::collections::BTreeMap;

use phoenix_common::module_path::ModulePath;
use phoenix_sema::types::Type;

use crate::default_wrappers::with_synthetic_function;
use crate::instruction::{FuncId, Op, ValueId};
use crate::lower::LoweringContext;
use crate::module::IrFunction;
use crate::terminator::Terminator;
use crate::types::{IrType, JSON_ERROR_ENUM, LIST_TYPE, MAP_TYPE, OPTION_ENUM, RESULT_ENUM};

/// Synthesize an encoder for every type demanded by a `json.encode` call
/// (transitively through struct fields). No-op when the program has no
/// `json.encode` sites.
pub(crate) fn synthesize_json_encoders(ctx: &mut LoweringContext<'_>) {
    // Deterministic order (sorted by key) so `FuncId` allocation is stable
    // across rebuilds — same discipline the monomorphizer relies on.
    let demanded = collect_demanded_types(ctx);
    if demanded.is_empty() {
        return;
    }
    // Pass A: register every encoder stub up front so a struct encoder can
    // call its field encoders (and any mutual references resolve).
    for (key, ty) in &demanded {
        let fid = register_encoder_stub(ctx, key, ty);
        ctx.module.json_encoders.insert(key.clone(), fid);
    }
    // Pass B: lower each encoder body.
    for (key, ty) in &demanded {
        let fid = ctx.module.json_encoders[key];
        build_encoder_body(ctx, ty, fid);
    }
}

/// Collect every type needing an encoder: the argument types of all
/// `json.encode` calls, plus (transitively) the field types of any struct
/// among them. Keyed by [`encode_type_key`] so each type is synthesized
/// once.
fn collect_demanded_types(ctx: &LoweringContext<'_>) -> BTreeMap<String, Type> {
    let mut demanded: BTreeMap<String, Type> = BTreeMap::new();
    let mut queue: Vec<Type> = ctx.check.json_encode_types.values().cloned().collect();
    while let Some(ty) = queue.pop() {
        let key = encode_type_key(&ty);
        if demanded.contains_key(&key) {
            continue;
        }
        demanded.insert(key, ty.clone());
        match &ty {
            // `Option<T>` / `List<T>` → encode `T`; `Map<K, V>` → `K` and `V`.
            Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
                queue.push(args[0].clone());
            }
            Type::Generic(name, args) if name == LIST_TYPE && args.len() == 1 => {
                queue.push(args[0].clone());
            }
            Type::Generic(name, args) if name == MAP_TYPE && args.len() == 2 => {
                queue.push(args[0].clone());
                queue.push(args[1].clone());
            }
            // Struct → its field types; non-generic enum → its variant field
            // types. (Sema's gate guarantees only these shapes reach here.)
            Type::Named(name) => {
                if let Some(info) = ctx.check.struct_info_by_name(name) {
                    for f in &info.fields {
                        queue.push(f.ty.clone());
                    }
                } else if let Some(info) = ctx.check.enum_info_by_name(name) {
                    queue.extend(info.variants.iter().flat_map(|(_, fts)| fts).cloned());
                }
            }
            _ => {}
        }
    }
    demanded
}

/// A stable, collision-free key per encodable type: a scalar's name, a
/// struct's or non-generic enum's qualified name (`"models.user::User"`), or
/// an `Option<T>` parameterized by its element key (`"Option<Int>"`). Shared
/// with the `json.encode` dispatch in `lower_method_call`.
pub(crate) fn encode_type_key(ty: &Type) -> String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::String => "String".to_string(),
        // Generic collections need a distinct encoder per instantiation.
        Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
            format!("Option<{}>", encode_type_key(&args[0]))
        }
        Type::Generic(name, args) if name == LIST_TYPE && args.len() == 1 => {
            format!("List<{}>", encode_type_key(&args[0]))
        }
        Type::Generic(name, args) if name == MAP_TYPE && args.len() == 2 => format!(
            "Map<{},{}>",
            encode_type_key(&args[0]),
            encode_type_key(&args[1])
        ),
        // Both structs and non-generic enums key on their qualified name.
        Type::Named(name) => name.clone(),
        other => unreachable!(
            "json encode: unsupported type reached synthesis ({other:?}) — \
             sema's `unsupported_json_encode_type` gate should reject it"
        ),
    }
}

/// Readable name prefix for a type's encoder. Not guaranteed unique on its
/// own — the `::`→`__` mangling can alias distinct keys (e.g. `a::b` and a
/// struct literally named `a__b`) — so `register_encoder_stub` appends the
/// assigned `FuncId` to make the final IR function name collision-free.
/// (Dispatch never relies on the name: `IrModule::json_encoders` is keyed by
/// the unmangled `key`.)
fn encoder_fn_name(key: &str) -> String {
    // Map every non-alphanumeric character (`::`, `<`, `>`, …) to `_` so the
    // name is a valid object symbol; the FuncId suffix added by
    // `register_encoder_stub` carries uniqueness.
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("__json_encode_{safe}")
}

/// Pass A: append the encoder's stub (one param `v: T`, returns a string).
fn register_encoder_stub(ctx: &mut LoweringContext<'_>, key: &str, ty: &Type) -> FuncId {
    let param_ir = ctx.lower_type(ty);
    let stub = IrFunction::new(
        FuncId(u32::MAX), // filled in by `push_concrete`
        encoder_fn_name(key),
        vec![param_ir],
        vec!["v".to_string()],
        IrType::StringRef,
        None,
    );
    let fid = ctx.module.push_concrete(stub);
    // Suffix with the (globally unique) FuncId so the IR symbol name is
    // unique even when `encoder_fn_name` mangling aliases two keys.
    let func = ctx.module.functions[fid.index()].func_mut();
    func.name = format!("{}_{}", func.name, fid.0);
    fid
}

/// Pass B: lower the encoder body. Encoders reference no user scope (they
/// only emit ops over the single parameter), so they lower in the entry
/// module.
fn build_encoder_body(ctx: &mut LoweringContext<'_>, ty: &Type, fid: FuncId) {
    let param_ir = ctx.module.functions[fid.index()].func().param_types[0].clone();
    with_synthetic_function(ctx, fid, ModulePath::entry(), |ctx| {
        let entry = ctx.create_block();
        ctx.switch_to_block(entry);
        let param = ctx.add_block_param(entry, param_ir);
        let result = emit_encode(ctx, ty, param);
        ctx.terminate(Terminator::Return(Some(result)));
    });
}

/// Emit the ops that encode `param` (of type `ty`) to a JSON string,
/// returning the result value.
fn emit_encode(ctx: &mut LoweringContext<'_>, ty: &Type, param: ValueId) -> ValueId {
    match ty {
        // `toString` already renders `Int`/`Float`/`Bool` as valid JSON
        // numbers / `true`/`false`, byte-identically across backends.
        Type::Int | Type::Float | Type::Bool => ctx.emit(
            Op::BuiltinCall("toString".to_string(), vec![param]),
            IrType::StringRef,
            None,
        ),
        Type::String => ctx.emit(
            Op::BuiltinCall("json.escapeString".to_string(), vec![param]),
            IrType::StringRef,
            None,
        ),
        // `Option<T>`: None → `null`, Some(x) → encode(x).
        Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
            emit_option_encode(ctx, &args[0], param)
        }
        // `List<T>` → array.
        Type::Generic(name, args) if name == LIST_TYPE && args.len() == 1 => {
            emit_list_encode(ctx, &args[0], param)
        }
        // `Map<K, V>` → object (string keys) or `[k, v]` pairs (other keys).
        Type::Generic(name, args) if name == MAP_TYPE && args.len() == 2 => {
            emit_map_encode(ctx, &args[0], &args[1], param)
        }
        // A `Type::Named` is either a struct (object) or a non-generic enum
        // (adjacently tagged).
        Type::Named(name) => {
            if ctx.check.struct_info_by_name(name).is_some() {
                emit_struct_encode(ctx, name, param)
            } else {
                emit_enum_encode(ctx, name, param)
            }
        }
        other => unreachable!("json encode body: unsupported type {other:?}"),
    }
}

/// Emit a struct encoder: `{"f0":<enc f0>,"f1":<enc f1>,…}`, fields in
/// declaration order, each value encoded by its own per-type encoder.
fn emit_struct_encode(ctx: &mut LoweringContext<'_>, struct_name: &str, param: ValueId) -> ValueId {
    // Snapshot (name, type) pairs so we don't hold a borrow of `ctx.check`
    // across the `ctx.emit` calls below.
    let fields: Vec<(String, Type)> = ctx
        .check
        .struct_info_by_name(struct_name)
        .expect("struct demanded for json encode must be registered in sema")
        .fields
        .iter()
        .map(|f| (f.name.clone(), f.ty.clone()))
        .collect();

    // The positional `StructGetField(param, i)` below pairs with sema's
    // declaration-order field list: IR struct layout preserves declaration
    // order, so index `i` here is the same field as `fields[i]`. (The
    // interpreter side is name-keyed and so is independent of this.)
    let mut acc = ctx.emit(Op::ConstString("{".to_string()), IrType::StringRef, None);
    for (i, (fname, fty)) in fields.iter().enumerate() {
        // Field-name keys are identifiers, so a raw `"name":` is already
        // valid JSON. (`@jsonName` wire keys, which may need escaping, land
        // with the annotation slice.)
        let prefix = if i == 0 {
            format!("\"{fname}\":")
        } else {
            format!(",\"{fname}\":")
        };
        let prefix_v = ctx.emit(Op::ConstString(prefix), IrType::StringRef, None);
        acc = ctx.emit(Op::StringConcat(acc, prefix_v), IrType::StringRef, None);

        let field_ir = ctx.lower_type(fty);
        let fieldval = ctx.emit(Op::StructGetField(param, i as u32), field_ir, None);
        let enc_fid = encoder_for(ctx, fty);
        let enc = ctx.emit(
            Op::Call(enc_fid, Vec::new(), vec![fieldval]),
            IrType::StringRef,
            None,
        );
        acc = ctx.emit(Op::StringConcat(acc, enc), IrType::StringRef, None);
    }
    let close = ctx.emit(Op::ConstString("}".to_string()), IrType::StringRef, None);
    ctx.emit(Op::StringConcat(acc, close), IrType::StringRef, None)
}

/// The discriminant index of `variant` in `enum_name`'s IR layout, looked up
/// rather than assuming sema's variant order matches the layout order. The two
/// agree today (the layout is built from sema's variant list), but resolving
/// the index keeps every enum encoder correct if that ever changes.
fn enum_variant_index(ctx: &LoweringContext<'_>, enum_name: &str, variant: &str) -> u32 {
    ctx.module
        .enum_layouts
        .get(enum_name)
        .and_then(|vs| vs.iter().position(|(n, _)| n == variant))
        .unwrap_or_else(|| unreachable!("{enum_name} layout missing variant `{variant}`"))
        as u32
}

/// Emit an `Option<T>` encoder: `None` → `null`, `Some(x)` → `encode(x)`.
/// Branches on the discriminant and merges both arms' strings through a
/// block parameter; leaves the context positioned in the merge block (its
/// parameter is the returned value).
fn emit_option_encode(ctx: &mut LoweringContext<'_>, inner: &Type, param: ValueId) -> ValueId {
    let some_idx = enum_variant_index(ctx, OPTION_ENUM, "Some");
    let disc = ctx.emit(Op::EnumDiscriminant(param), IrType::I64, None);
    let some_idx_v = ctx.emit(Op::ConstI64(some_idx as i64), IrType::I64, None);
    let is_some = ctx.emit(Op::IEq(disc, some_idx_v), IrType::Bool, None);

    let some_blk = ctx.create_block();
    let none_blk = ctx.create_block();
    let merge = ctx.create_block();
    let result = ctx.add_block_param(merge, IrType::StringRef);
    ctx.terminate(Terminator::Branch {
        condition: is_some,
        true_block: some_blk,
        true_args: Vec::new(),
        false_block: none_blk,
        false_args: Vec::new(),
    });

    // Some(x) → encode the inner value.
    ctx.switch_to_block(some_blk);
    let inner_ir = ctx.lower_type(inner);
    let inner_val = ctx.emit(Op::EnumGetField(param, some_idx, 0), inner_ir, None);
    let enc_fid = encoder_for(ctx, inner);
    let enc = ctx.emit(
        Op::Call(enc_fid, Vec::new(), vec![inner_val]),
        IrType::StringRef,
        None,
    );
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![enc],
    });

    // None → the JSON literal `null`.
    ctx.switch_to_block(none_blk);
    let null = ctx.emit(Op::ConstString("null".to_string()), IrType::StringRef, None);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![null],
    });

    ctx.switch_to_block(merge);
    result
}

/// Emit a non-generic enum encoder using the adjacently-tagged wire form:
/// a unit variant → `{"type":"V"}`, a variant with fields → `{"type":"V",
/// "value":[<enc f0>,…]}`. Dispatches on the discriminant via a chained-
/// branch ladder; the sema-order last variant (not the one with the highest
/// discriminant) is the fallthrough — `disc_idx` decouples sema order from
/// layout discriminants, and the bijection makes any fallthrough choice
/// correct. Merges every arm's string through one block parameter.
fn emit_enum_encode(ctx: &mut LoweringContext<'_>, enum_name: &str, param: ValueId) -> ValueId {
    // Snapshot the variants (name + field sema types) to drop the borrow on
    // `ctx.check` before emitting.
    let variants: Vec<(String, Vec<Type>)> = ctx
        .check
        .enum_info_by_name(enum_name)
        .expect("enum demanded for json encode must be registered in sema")
        .variants
        .clone();

    // An uninhabited enum (zero variants) can never be constructed, so this
    // encoder body is unreachable — but it still has to be valid IR returning
    // a `StringRef`. Emit a placeholder and skip the variant ladder, whose
    // `variants.len() - 1` would otherwise underflow.
    if variants.is_empty() {
        return ctx.emit(Op::ConstString("null".to_string()), IrType::StringRef, None);
    }

    // The discriminant each variant carries comes from the IR layout, not from
    // sema's enumeration order — resolved per variant so this matches the
    // values `EnumDiscriminant`/`EnumGetField` actually see (see
    // `enum_variant_index`). The block layout below is still indexed by sema
    // order; only the tested constant and field-extraction index are mapped.
    let disc_idx: Vec<u32> = variants
        .iter()
        .map(|(vname, _)| enum_variant_index(ctx, enum_name, vname))
        .collect();

    let disc = ctx.emit(Op::EnumDiscriminant(param), IrType::I64, None);
    let merge = ctx.create_block();
    let result = ctx.add_block_param(merge, IrType::StringRef);
    let var_blocks: Vec<_> = (0..variants.len()).map(|_| ctx.create_block()).collect();

    // Discriminant ladder: test every variant but the last by its layout
    // discriminant; the last is the fallthrough. The variants partition the
    // discriminants bijectively, so falling through to the last is correct
    // whatever order they appear in.
    let last = variants.len() - 1;
    for i in 0..last {
        let k = ctx.emit(Op::ConstI64(disc_idx[i] as i64), IrType::I64, None);
        let eq = ctx.emit(Op::IEq(disc, k), IrType::Bool, None);
        let next = ctx.create_block();
        ctx.terminate(Terminator::Branch {
            condition: eq,
            true_block: var_blocks[i],
            true_args: Vec::new(),
            false_block: next,
            false_args: Vec::new(),
        });
        ctx.switch_to_block(next);
    }
    ctx.terminate(Terminator::Jump {
        target: var_blocks[last],
        args: Vec::new(),
    });

    // Fill each variant block, jumping to `merge` with the encoded string.
    for (i, (vname, field_tys)) in variants.iter().enumerate() {
        ctx.switch_to_block(var_blocks[i]);
        let s = emit_enum_variant_body(ctx, param, disc_idx[i], vname, field_tys);
        ctx.terminate(Terminator::Jump {
            target: merge,
            args: vec![s],
        });
    }

    ctx.switch_to_block(merge);
    result
}

/// Emit the adjacently-tagged string for one enum variant in the current
/// block: a unit variant → `{"type":"V"}`, a variant with fields → `{"type":
/// "V","value":[<enc f0>,…]}`. `var_idx` is the discriminant index used to
/// extract each field from `param`. Returns the value holding the string.
fn emit_enum_variant_body(
    ctx: &mut LoweringContext<'_>,
    param: ValueId,
    var_idx: u32,
    vname: &str,
    field_tys: &[Type],
) -> ValueId {
    if field_tys.is_empty() {
        // Variant names are identifiers, so raw quoting is valid JSON.
        return ctx.emit(
            Op::ConstString(format!("{{\"type\":\"{vname}\"}}")),
            IrType::StringRef,
            None,
        );
    }
    let mut acc = ctx.emit(
        Op::ConstString(format!("{{\"type\":\"{vname}\",\"value\":[")),
        IrType::StringRef,
        None,
    );
    for (j, fty) in field_tys.iter().enumerate() {
        if j > 0 {
            let comma = ctx.emit(Op::ConstString(",".to_string()), IrType::StringRef, None);
            acc = ctx.emit(Op::StringConcat(acc, comma), IrType::StringRef, None);
        }
        let field_ir = ctx.lower_type(fty);
        let fval = ctx.emit(Op::EnumGetField(param, var_idx, j as u32), field_ir, None);
        let enc_fid = encoder_for(ctx, fty);
        let enc = ctx.emit(
            Op::Call(enc_fid, Vec::new(), vec![fval]),
            IrType::StringRef,
            None,
        );
        acc = ctx.emit(Op::StringConcat(acc, enc), IrType::StringRef, None);
    }
    let close = ctx.emit(Op::ConstString("]}".to_string()), IrType::StringRef, None);
    ctx.emit(Op::StringConcat(acc, close), IrType::StringRef, None)
}

/// Look up the synthesized encoder for `ty`, panicking with a clear message
/// if demand collection missed it.
fn encoder_for(ctx: &LoweringContext<'_>, ty: &Type) -> FuncId {
    let key = encode_type_key(ty);
    *ctx.module.json_encoders.get(&key).unwrap_or_else(|| {
        unreachable!(
            "no synthesized encoder for `{key}` — demand collection missed a \
             transitive component type"
        )
    })
}

/// Emit a comma-joined sequence with `open`/`close` brackets, calling
/// `emit_item(ctx, i)` for each index `i` in `0..count` to produce that
/// item's already-encoded string.
///
/// The accumulator threads through **block parameters** (not an `alloca`)
/// so it is a live SSA ref the backend roots on the shadow stack across the
/// per-item allocations. The first item is emitted before the loop so the
/// loop body can unconditionally prepend the separator; an empty sequence
/// short-circuits to `open+close`. Leaves the context in the merge block;
/// the returned value is that block's parameter (the finished string).
fn emit_join_loop(
    ctx: &mut LoweringContext<'_>,
    count: ValueId,
    open: &str,
    sep: &str,
    close: &str,
    mut emit_item: impl FnMut(&mut LoweringContext<'_>, ValueId) -> ValueId,
) -> ValueId {
    let zero = ctx.emit(Op::ConstI64(0), IrType::I64, None);
    let one = ctx.emit(Op::ConstI64(1), IrType::I64, None);
    let has_any = ctx.emit(Op::IGt(count, zero), IrType::Bool, None);

    let init_blk = ctx.create_block();
    let empty_blk = ctx.create_block();
    let header = ctx.create_block();
    let body = ctx.create_block();
    let close_blk = ctx.create_block();
    let done = ctx.create_block();

    // Loop-carried values: (index, accumulator) through header/body; the
    // accumulator alone through the close path; the result through `done`.
    let h_i = ctx.add_block_param(header, IrType::I64);
    let h_acc = ctx.add_block_param(header, IrType::StringRef);
    let b_i = ctx.add_block_param(body, IrType::I64);
    let b_acc = ctx.add_block_param(body, IrType::StringRef);
    let c_acc = ctx.add_block_param(close_blk, IrType::StringRef);
    let d_res = ctx.add_block_param(done, IrType::StringRef);

    ctx.terminate(Terminator::Branch {
        condition: has_any,
        true_block: init_blk,
        true_args: Vec::new(),
        false_block: empty_blk,
        false_args: Vec::new(),
    });

    // Empty: `open` + `close` (e.g. `[]` / `{}`).
    ctx.switch_to_block(empty_blk);
    let empty_str = ctx.emit(
        Op::ConstString(format!("{open}{close}")),
        IrType::StringRef,
        None,
    );
    ctx.terminate(Terminator::Jump {
        target: done,
        args: vec![empty_str],
    });

    // Init: `open` + item(0), then enter the loop at index 1.
    ctx.switch_to_block(init_blk);
    let open_v = ctx.emit(Op::ConstString(open.to_string()), IrType::StringRef, None);
    let item0 = emit_item(ctx, zero);
    let acc0 = ctx.emit(Op::StringConcat(open_v, item0), IrType::StringRef, None);
    ctx.terminate(Terminator::Jump {
        target: header,
        args: vec![one, acc0],
    });

    // Header: while i < count.
    ctx.switch_to_block(header);
    let cond = ctx.emit(Op::ILt(h_i, count), IrType::Bool, None);
    ctx.terminate(Terminator::Branch {
        condition: cond,
        true_block: body,
        true_args: vec![h_i, h_acc],
        false_block: close_blk,
        false_args: vec![h_acc],
    });

    // Body: acc + sep + item(i); i += 1.
    ctx.switch_to_block(body);
    let sep_v = ctx.emit(Op::ConstString(sep.to_string()), IrType::StringRef, None);
    let acc_sep = ctx.emit(Op::StringConcat(b_acc, sep_v), IrType::StringRef, None);
    let item = emit_item(ctx, b_i);
    let acc_next = ctx.emit(Op::StringConcat(acc_sep, item), IrType::StringRef, None);
    let i_next = ctx.emit(Op::IAdd(b_i, one), IrType::I64, None);
    ctx.terminate(Terminator::Jump {
        target: header,
        args: vec![i_next, acc_next],
    });

    // Close: acc + `close`.
    ctx.switch_to_block(close_blk);
    let close_v = ctx.emit(Op::ConstString(close.to_string()), IrType::StringRef, None);
    let result = ctx.emit(Op::StringConcat(c_acc, close_v), IrType::StringRef, None);
    ctx.terminate(Terminator::Jump {
        target: done,
        args: vec![result],
    });

    ctx.switch_to_block(done);
    d_res
}

/// Emit a `List<T>` encoder: a JSON array of each element's encoding.
fn emit_list_encode(ctx: &mut LoweringContext<'_>, elem: &Type, param: ValueId) -> ValueId {
    let len = ctx.emit(
        Op::BuiltinCall("List.length".to_string(), vec![param]),
        IrType::I64,
        None,
    );
    let elem_ir = ctx.lower_type(elem);
    let enc_fid = encoder_for(ctx, elem);
    emit_join_loop(ctx, len, "[", ",", "]", move |ctx, i| {
        let e = ctx.emit(
            Op::BuiltinCall("List.get".to_string(), vec![param, i]),
            elem_ir.clone(),
            None,
        );
        ctx.emit(
            Op::Call(enc_fid, Vec::new(), vec![e]),
            IrType::StringRef,
            None,
        )
    })
}

/// Emit a `Map<K, V>` encoder. String keys produce a JSON object
/// (`{"k":v,…}`). Iterates the map's `keys()` and `values()` lists in
/// parallel (the runtime returns them in matching insertion order). Sema
/// guarantees `key_ty == String` for this slice; maps with other key types
/// (which serialize as `[k, v]` pairs) are a deferred follow-up.
fn emit_map_encode(
    ctx: &mut LoweringContext<'_>,
    key_ty: &Type,
    val_ty: &Type,
    param: ValueId,
) -> ValueId {
    debug_assert!(
        matches!(key_ty, Type::String),
        "this slice only synthesizes Map encoders for String keys; \
         sema's gate should reject other key types"
    );
    let val_ir = ctx.lower_type(val_ty);
    let key_enc = encoder_for(ctx, key_ty);
    let val_enc = encoder_for(ctx, val_ty);
    let keys = ctx.emit(
        Op::BuiltinCall("Map.keys".to_string(), vec![param]),
        ctx.lower_type(&list_of(key_ty)),
        None,
    );
    let vals = ctx.emit(
        Op::BuiltinCall("Map.values".to_string(), vec![param]),
        ctx.lower_type(&list_of(val_ty)),
        None,
    );
    let len = ctx.emit(
        Op::BuiltinCall("List.length".to_string(), vec![keys]),
        IrType::I64,
        None,
    );
    let str_ir = ctx.lower_type(&Type::String);

    emit_join_loop(ctx, len, "{", ",", "}", move |ctx, i| {
        // `"k":v` — the String encoder quotes/escapes the key.
        let k = ctx.emit(
            Op::BuiltinCall("List.get".to_string(), vec![keys, i]),
            str_ir.clone(),
            None,
        );
        let v = ctx.emit(
            Op::BuiltinCall("List.get".to_string(), vec![vals, i]),
            val_ir.clone(),
            None,
        );
        let kenc = ctx.emit(
            Op::Call(key_enc, Vec::new(), vec![k]),
            IrType::StringRef,
            None,
        );
        let venc = ctx.emit(
            Op::Call(val_enc, Vec::new(), vec![v]),
            IrType::StringRef,
            None,
        );
        let colon = ctx.emit(Op::ConstString(":".to_string()), IrType::StringRef, None);
        let kc = ctx.emit(Op::StringConcat(kenc, colon), IrType::StringRef, None);
        ctx.emit(Op::StringConcat(kc, venc), IrType::StringRef, None)
    })
}

/// `List<T>` sema type for a given element type — used to type the
/// `Map.keys` / `Map.values` results.
fn list_of(elem: &Type) -> Type {
    Type::Generic(LIST_TYPE.to_string(), vec![elem.clone()])
}

// ── JSON decode ─────────────────────────────────────────────
//
// Node-kind tags mirror `phoenix_runtime::json::JSON_KIND_*` — the runtime
// is the ABI source of truth. Mirroring (rather than importing) keeps this
// compiler crate free of a production dependency on the runtime; the
// `kind_tags_match_runtime_abi` test pins the two in lockstep, the same
// scheme the list-header layout check uses. Decoders compare the result of
// the `json.kind` builtin against these.
const KIND_BOOL: i64 = 1;
const KIND_INT: i64 = 2;
const KIND_FLOAT: i64 = 3;
const KIND_STRING: i64 = 4;

/// Synthesize a decoder for every type demanded by a `json.decode<T>` call.
/// No-op when the program has no `json.decode` sites. This slice covers the
/// scalar types (sema's `unsupported_json_decode_type` gate rejects the
/// rest before they reach here).
pub(crate) fn synthesize_json_decoders(ctx: &mut LoweringContext<'_>) {
    let demanded: BTreeMap<String, Type> = ctx
        .check
        .json_decode_types
        .values()
        .map(|t| (encode_type_key(t), t.clone()))
        .collect();
    if demanded.is_empty() {
        return;
    }
    // Pass A: register the node-decoder + entry stubs for every type.
    let mut node_ids: BTreeMap<String, FuncId> = BTreeMap::new();
    for (key, ty) in &demanded {
        let node =
            register_decoder_stub(ctx, &format!("__json_decode_{}", sanitize(key)), ty, false);
        node_ids.insert(key.clone(), node);
        let entry = register_decoder_stub(
            ctx,
            &format!("__json_decode_entry_{}", sanitize(key)),
            ty,
            true,
        );
        ctx.module.json_decoders.insert(key.clone(), entry);
    }
    // Pass B: build the node-decoder + entry bodies.
    for (key, ty) in &demanded {
        let node = node_ids[key];
        let entry = ctx.module.json_decoders[key];
        build_node_decoder(ctx, ty, node);
        build_decode_entry(ctx, ty, entry, node);
    }
}

/// Map a type key to a symbol-safe suffix (shares the `encoder_fn_name`
/// scheme so the two families read alike in IR dumps).
fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Register a decoder stub. An entry (`is_entry`) takes the input `String`;
/// a node decoder takes a DOM node handle (`I64`). Both return
/// `Result<T, JsonError>`.
fn register_decoder_stub(
    ctx: &mut LoweringContext<'_>,
    name: &str,
    ty: &Type,
    is_entry: bool,
) -> FuncId {
    let param = if is_entry {
        IrType::StringRef
    } else {
        IrType::I64
    };
    let ret = result_ir(ctx, ty);
    let stub = IrFunction::new(
        FuncId(u32::MAX),
        name.to_string(),
        vec![param],
        vec![if is_entry { "s" } else { "node" }.to_string()],
        ret,
        None,
    );
    let fid = ctx.module.push_concrete(stub);
    let func = ctx.module.functions[fid.index()].func_mut();
    func.name = format!("{}_{}", func.name, fid.0);
    fid
}

/// `Result<T, JsonError>` as an IR type.
fn result_ir(ctx: &LoweringContext<'_>, ty: &Type) -> IrType {
    let t = ctx.lower_type(ty);
    IrType::EnumRef(
        RESULT_ENUM.to_string(),
        vec![t, IrType::EnumRef(JSON_ERROR_ENUM.to_string(), Vec::new())],
    )
}

/// Build the entry body: parse the input string, propagate a parse error,
/// otherwise decode the root node — freeing the DOM on both paths.
fn build_decode_entry(ctx: &mut LoweringContext<'_>, ty: &Type, entry: FuncId, node_dec: FuncId) {
    let result_ty = result_ir(ctx, ty);
    with_synthetic_function(ctx, entry, ModulePath::entry(), |ctx| {
        let start = ctx.create_block();
        ctx.switch_to_block(start);
        let s = ctx.add_block_param(start, IrType::StringRef);

        let dom = ctx.emit(
            Op::BuiltinCall("json.parse".to_string(), vec![s]),
            IrType::I64,
            None,
        );
        let failed = ctx.emit(
            Op::BuiltinCall("json.parseFailed".to_string(), vec![dom]),
            IrType::Bool,
            None,
        );
        let perr = ctx.create_block();
        let ok = ctx.create_block();
        let done = ctx.create_block();
        let result = ctx.add_block_param(done, result_ty.clone());
        ctx.terminate(Terminator::Branch {
            condition: failed,
            true_block: perr,
            true_args: Vec::new(),
            false_block: ok,
            false_args: Vec::new(),
        });

        // Parse failed → Err(ParseError(message)).
        ctx.switch_to_block(perr);
        let msg = ctx.emit(
            Op::BuiltinCall("json.parseError".to_string(), vec![dom]),
            IrType::StringRef,
            None,
        );
        let je = emit_json_error(ctx, "ParseError", msg);
        let err_res = emit_result_err(ctx, &result_ty, je);
        ctx.emit_void(Op::BuiltinCall("json.free".to_string(), vec![dom]), None);
        ctx.terminate(Terminator::Jump {
            target: done,
            args: vec![err_res],
        });

        // Parsed → decode the root node, then free.
        ctx.switch_to_block(ok);
        let root = ctx.emit(
            Op::BuiltinCall("json.root".to_string(), vec![dom]),
            IrType::I64,
            None,
        );
        let decoded = ctx.emit(
            Op::Call(node_dec, Vec::new(), vec![root]),
            result_ty.clone(),
            None,
        );
        ctx.emit_void(Op::BuiltinCall("json.free".to_string(), vec![dom]), None);
        ctx.terminate(Terminator::Jump {
            target: done,
            args: vec![decoded],
        });

        ctx.switch_to_block(done);
        ctx.terminate(Terminator::Return(Some(result)));
    });
}

/// Build a per-type node decoder body. This slice handles the scalars.
fn build_node_decoder(ctx: &mut LoweringContext<'_>, ty: &Type, fid: FuncId) {
    let result_ty = result_ir(ctx, ty);
    with_synthetic_function(ctx, fid, ModulePath::entry(), |ctx| {
        let start = ctx.create_block();
        ctx.switch_to_block(start);
        let node = ctx.add_block_param(start, IrType::I64);
        let result = emit_scalar_decode(ctx, ty, node, &result_ty);
        ctx.terminate(Terminator::Return(Some(result)));
    });
}

/// Emit a scalar decoder: check the node's kind, extract on match, else
/// `Err(TypeMismatch)`. Leaves the context in the merge block and returns
/// its parameter.
fn emit_scalar_decode(
    ctx: &mut LoweringContext<'_>,
    ty: &Type,
    node: ValueId,
    result_ty: &IrType,
) -> ValueId {
    let (kinds, extract, value_ir, name): (&[i64], &str, IrType, &str) = match ty {
        Type::Int => (&[KIND_INT], "json.asInt", IrType::I64, "Int"),
        // A JSON integer is also a valid Float.
        Type::Float => (
            &[KIND_INT, KIND_FLOAT],
            "json.asFloat",
            IrType::F64,
            "Float",
        ),
        Type::Bool => (&[KIND_BOOL], "json.asBool", IrType::Bool, "Bool"),
        Type::String => (&[KIND_STRING], "json.asStr", IrType::StringRef, "String"),
        other => unreachable!("scalar decode: unsupported type {other:?}"),
    };

    let kind = ctx.emit(
        Op::BuiltinCall("json.kind".to_string(), vec![node]),
        IrType::I64,
        None,
    );
    let ok_blk = ctx.create_block();
    let err_blk = ctx.create_block();
    let merge = ctx.create_block();
    let result = ctx.add_block_param(merge, result_ty.clone());

    // Chain of kind tests: any match jumps to `ok_blk`; the final miss falls
    // through to `err_blk`.
    for (i, &k) in kinds.iter().enumerate() {
        let kv = ctx.emit(Op::ConstI64(k), IrType::I64, None);
        let eq = ctx.emit(Op::IEq(kind, kv), IrType::Bool, None);
        let next = if i + 1 < kinds.len() {
            ctx.create_block()
        } else {
            err_blk
        };
        ctx.terminate(Terminator::Branch {
            condition: eq,
            true_block: ok_blk,
            true_args: Vec::new(),
            false_block: next,
            false_args: Vec::new(),
        });
        if next != err_blk {
            ctx.switch_to_block(next);
        }
    }

    // Match → Ok(extract(node)).
    ctx.switch_to_block(ok_blk);
    let v = ctx.emit(
        Op::BuiltinCall(extract.to_string(), vec![node]),
        value_ir,
        None,
    );
    let ok = emit_result_ok(ctx, result_ty, v);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![ok],
    });

    // Miss → Err(TypeMismatch("expected <T>")).
    ctx.switch_to_block(err_blk);
    let msg = ctx.emit(
        Op::ConstString(format!("expected {name}")),
        IrType::StringRef,
        None,
    );
    let je = emit_json_error(ctx, "TypeMismatch", msg);
    let err = emit_result_err(ctx, result_ty, je);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![err],
    });

    ctx.switch_to_block(merge);
    result
}

/// `Ok(value)` as a `Result<T, JsonError>` (`result_ty`).
fn emit_result_ok(ctx: &mut LoweringContext<'_>, result_ty: &IrType, value: ValueId) -> ValueId {
    let ok_idx = enum_variant_index(ctx, RESULT_ENUM, "Ok");
    ctx.emit(
        Op::EnumAlloc(RESULT_ENUM.to_string(), ok_idx, vec![value]),
        result_ty.clone(),
        None,
    )
}

/// `Err(json_error)` as a `Result<T, JsonError>` (`result_ty`).
fn emit_result_err(ctx: &mut LoweringContext<'_>, result_ty: &IrType, je: ValueId) -> ValueId {
    let err_idx = enum_variant_index(ctx, RESULT_ENUM, "Err");
    ctx.emit(
        Op::EnumAlloc(RESULT_ENUM.to_string(), err_idx, vec![je]),
        result_ty.clone(),
        None,
    )
}

/// A `JsonError::<variant>(message)` value.
fn emit_json_error(ctx: &mut LoweringContext<'_>, variant: &str, msg: ValueId) -> ValueId {
    let idx = enum_variant_index(ctx, JSON_ERROR_ENUM, variant);
    ctx.emit(
        Op::EnumAlloc(JSON_ERROR_ENUM.to_string(), idx, vec![msg]),
        IrType::EnumRef(JSON_ERROR_ENUM.to_string(), Vec::new()),
        None,
    )
}

#[cfg(test)]
mod tests {
    use phoenix_runtime::json as rt;

    /// The `KIND_*` mirror must match the runtime's `JSON_KIND_*` ABI (see
    /// the module comment on the constants).
    #[test]
    fn kind_tags_match_runtime_abi() {
        assert_eq!(super::KIND_BOOL, rt::JSON_KIND_BOOL);
        assert_eq!(super::KIND_INT, rt::JSON_KIND_INT);
        assert_eq!(super::KIND_FLOAT, rt::JSON_KIND_FLOAT);
        assert_eq!(super::KIND_STRING, rt::JSON_KIND_STRING);
        // Tags the scalar decoders don't consume yet, pinned by value so a
        // runtime renumbering is caught now, not when the composite-decode
        // slices mirror them here (replace these with mirror asserts then).
        assert_eq!(rt::JSON_KIND_NULL, 0);
        assert_eq!(rt::JSON_KIND_ARRAY, 5);
        assert_eq!(rt::JSON_KIND_OBJECT, 6);
    }
}
