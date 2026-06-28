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
//! This slice covers scalars (`Int`/`Float`/`Bool`/`String`) and
//! non-generic structs of supported field types. `Option`, enums, `List`,
//! and `Map` are added by later slices; sema's
//! `unsupported_json_encode_type` gate keeps unsupported shapes from
//! reaching here.

use std::collections::BTreeMap;

use phoenix_common::module_path::ModulePath;
use phoenix_sema::types::Type;

use crate::default_wrappers::with_synthetic_function;
use crate::instruction::{FuncId, Op, ValueId};
use crate::lower::LoweringContext;
use crate::module::IrFunction;
use crate::terminator::Terminator;
use crate::types::IrType;

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
        if let Type::Named(name) = &ty
            && let Some(info) = ctx.check.struct_info_by_name(name)
        {
            for f in &info.fields {
                queue.push(f.ty.clone());
            }
        }
    }
    demanded
}

/// A stable, collision-free key per encodable type: the scalar's name or a
/// struct's qualified name (`"models.user::User"`). Shared with the
/// `json.encode` dispatch in `lower_method_call`.
pub(crate) fn encode_type_key(ty: &Type) -> String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::String => "String".to_string(),
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
    format!("__json_encode_{}", key.replace("::", "__"))
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
        Type::Named(name) => emit_struct_encode(ctx, name, param),
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
        let field_key = encode_type_key(fty);
        let enc_fid = *ctx.module.json_encoders.get(&field_key).unwrap_or_else(|| {
            unreachable!(
                "struct `{struct_name}` field `{fname}` has no synthesized encoder for \
                 `{field_key}` — demand collection missed a transitive field type"
            )
        });
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
