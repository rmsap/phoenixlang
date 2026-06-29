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
//! structs, `Option<T>` (None → null, Some(x) → encode(x)), and non-generic
//! enums (adjacently tagged). `List`, `Map`, and generic enums other than
//! `Option` are added by later slices; sema's `unsupported_json_encode_type`
//! gate keeps unsupported shapes from reaching here.

use std::collections::BTreeMap;

use phoenix_common::module_path::ModulePath;
use phoenix_sema::types::Type;

use crate::default_wrappers::with_synthetic_function;
use crate::instruction::{FuncId, Op, ValueId};
use crate::lower::LoweringContext;
use crate::module::IrFunction;
use crate::terminator::Terminator;
use crate::types::{IrType, OPTION_ENUM};

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
            // `Option<T>` → encode `T`.
            Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
                queue.push(args[0].clone());
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
        // `Option<T>` needs a distinct encoder per element type.
        Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
            format!("Option<{}>", encode_type_key(&args[0]))
        }
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
