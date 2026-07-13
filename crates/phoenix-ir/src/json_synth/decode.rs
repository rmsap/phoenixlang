//! `json.decode<T>` synthesis: for every type demanded by a decode call
//! (plus, transitively, the field types of any struct target), a per-type
//! *node decoder* `__json_decode_<key>(node: I64) -> Result<T, JsonError>`,
//! and — for the call-site types only — a parse-wrapper *entry*
//! `__json_decode_entry_<key>(s: String) -> Result<T, JsonError>`.
//!
//! Covered so far: scalars (`Int`/`Float`/`Bool`/`String`) and non-generic
//! structs; sema's `unsupported_json_decode_type` gate rejects the rest
//! before it reaches here.

use std::collections::{BTreeMap, BTreeSet};

use phoenix_common::module_path::ModulePath;
use phoenix_sema::types::Type;

use super::{encode_type_key, enum_variant_index, sanitize};
use crate::default_wrappers::with_synthetic_function;
use crate::instruction::{FuncId, Op, ValueId};
use crate::lower::LoweringContext;
use crate::module::IrFunction;
use crate::terminator::Terminator;
use crate::types::{IrType, JSON_ERROR_ENUM, RESULT_ENUM};

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
const KIND_OBJECT: i64 = 6;

/// Synthesize a decoder for every type demanded by a `json.decode<T>` call
/// (plus, transitively, the field types of any struct target). No-op when
/// the program has no `json.decode` sites.
pub(crate) fn synthesize_json_decoders(ctx: &mut LoweringContext<'_>) {
    // Only the argument types of a `json.decode<T>` call get an entry (parse
    // wrapper); struct field types are decoded from an existing DOM node and
    // never re-parse.
    let entry_keys: BTreeSet<String> = ctx
        .check
        .json_decode_types
        .values()
        .map(encode_type_key)
        .collect();
    // Demand collection: the decoded types plus (transitively) any struct's
    // field types — a struct decoder calls its field decoders. Each struct's
    // field list is snapshotted here (one `struct_info_by_name` lookup per
    // struct), shared by the queue seeding and Pass B's body builds.
    let mut demanded: BTreeMap<String, Type> = BTreeMap::new();
    let mut struct_fields: BTreeMap<String, Vec<(String, Type)>> = BTreeMap::new();
    let mut queue: Vec<Type> = ctx.check.json_decode_types.values().cloned().collect();
    while let Some(ty) = queue.pop() {
        let key = encode_type_key(&ty);
        if demanded.contains_key(&key) {
            continue;
        }
        if let Type::Named(name) = &ty
            && let Some(info) = ctx.check.struct_info_by_name(name)
        {
            let fields: Vec<(String, Type)> = info
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.ty.clone()))
                .collect();
            queue.extend(fields.iter().map(|(_, fty)| fty.clone()));
            struct_fields.insert(key.clone(), fields);
        }
        demanded.insert(key, ty);
    }
    if demanded.is_empty() {
        return;
    }
    // Pass A: register the node-decoder stubs for every type (so a struct
    // decoder can call its field decoders, and any cycle resolves), plus an
    // entry stub for the call-site types.
    let mut node_ids: BTreeMap<String, FuncId> = BTreeMap::new();
    for (key, ty) in &demanded {
        let node =
            register_decoder_stub(ctx, &format!("__json_decode_{}", sanitize(key)), ty, false);
        node_ids.insert(key.clone(), node);
        if entry_keys.contains(key) {
            let entry = register_decoder_stub(
                ctx,
                &format!("__json_decode_entry_{}", sanitize(key)),
                ty,
                true,
            );
            ctx.module.json_decoders.insert(key.clone(), entry);
        }
    }
    // Pass B: build the node-decoder bodies (+ entry bodies where present).
    for (key, ty) in &demanded {
        let node = node_ids[key];
        build_node_decoder(
            ctx,
            ty,
            node,
            &node_ids,
            struct_fields.get(key).map(Vec::as_slice),
        );
        if let Some(&entry) = ctx.module.json_decoders.get(key) {
            build_decode_entry(ctx, ty, entry, node);
        }
    }
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

/// Build a per-type node decoder body. This slice handles scalars and
/// non-generic structs. `node_ids` maps a type key to its node decoder, so a
/// struct decoder can call its field decoders; `struct_fields` is the demand
/// walk's field snapshot for a struct target (`None` for a scalar) — owned
/// data, so nothing borrows `ctx.check` inside the emit closure.
fn build_node_decoder(
    ctx: &mut LoweringContext<'_>,
    ty: &Type,
    fid: FuncId,
    node_ids: &BTreeMap<String, FuncId>,
    struct_fields: Option<&[(String, Type)]>,
) {
    let result_ty = result_ir(ctx, ty);
    with_synthetic_function(ctx, fid, ModulePath::entry(), |ctx| {
        let start = ctx.create_block();
        ctx.switch_to_block(start);
        let node = ctx.add_block_param(start, IrType::I64);
        let result = match struct_fields {
            Some(fields) => emit_struct_decode(ctx, fields, node, &result_ty, node_ids),
            None => emit_scalar_decode(ctx, ty, node, &result_ty),
        };
        ctx.terminate(Terminator::Return(Some(result)));
    });
}

/// Emit a struct decoder: require an object, decode each field (missing
/// required field → `Err(MissingField)`, a field decode error propagates),
/// then build the struct. Leaves the context in the merge block; returns its
/// parameter.
fn emit_struct_decode(
    ctx: &mut LoweringContext<'_>,
    fields: &[(String, Type)],
    node: ValueId,
    result_ty: &IrType,
    node_ids: &BTreeMap<String, FuncId>,
) -> ValueId {
    let merge = ctx.create_block();
    let result = ctx.add_block_param(merge, result_ty.clone());
    let jump_err = |ctx: &mut LoweringContext<'_>, je: ValueId| {
        let err = emit_result_err(ctx, result_ty, je);
        ctx.terminate(Terminator::Jump {
            target: merge,
            args: vec![err],
        });
    };

    // The value must be a JSON object.
    let kind = ctx.emit(
        Op::BuiltinCall("json.kind".to_string(), vec![node]),
        IrType::I64,
        None,
    );
    let obj_kind = ctx.emit(Op::ConstI64(KIND_OBJECT), IrType::I64, None);
    let is_obj = ctx.emit(Op::IEq(kind, obj_kind), IrType::Bool, None);
    let fields_blk = ctx.create_block();
    let not_obj = ctx.create_block();
    ctx.terminate(Terminator::Branch {
        condition: is_obj,
        true_block: fields_blk,
        true_args: Vec::new(),
        false_block: not_obj,
        false_args: Vec::new(),
    });
    ctx.switch_to_block(not_obj);
    let msg = ctx.emit(
        Op::ConstString("expected object".to_string()),
        IrType::StringRef,
        None,
    );
    let je = emit_json_error(ctx, "TypeMismatch", msg);
    jump_err(ctx, je);

    // Decode each field along the happy path (a linear chain of blocks); each
    // field's decoded value dominates the final struct build.
    ctx.switch_to_block(fields_blk);
    let ok_idx = enum_variant_index(ctx, RESULT_ENUM, "Ok");
    let err_idx = enum_variant_index(ctx, RESULT_ENUM, "Err");
    let mut field_vals: Vec<ValueId> = Vec::with_capacity(fields.len());
    for (fname, fty) in fields {
        let key = ctx.emit(Op::ConstString(fname.clone()), IrType::StringRef, None);
        let child = ctx.emit(
            Op::BuiltinCall("json.getField".to_string(), vec![node, key]),
            IrType::I64,
            None,
        );
        let missing = ctx.emit(
            Op::BuiltinCall("json.isMissing".to_string(), vec![child]),
            IrType::Bool,
            None,
        );
        let present_blk = ctx.create_block();
        let missing_blk = ctx.create_block();
        ctx.terminate(Terminator::Branch {
            condition: missing,
            true_block: missing_blk,
            true_args: Vec::new(),
            false_block: present_blk,
            false_args: Vec::new(),
        });
        // Missing required field → Err(MissingField(name)).
        ctx.switch_to_block(missing_blk);
        let msg = ctx.emit(Op::ConstString(fname.clone()), IrType::StringRef, None);
        let je = emit_json_error(ctx, "MissingField", msg);
        jump_err(ctx, je);

        // Present → decode; propagate the field's error on failure.
        ctx.switch_to_block(present_blk);
        let field_result_ty = result_ir(ctx, fty);
        let decoder = node_ids[&encode_type_key(fty)];
        let r = ctx.emit(
            Op::Call(decoder, Vec::new(), vec![child]),
            field_result_ty,
            None,
        );
        let disc = ctx.emit(Op::EnumDiscriminant(r), IrType::I64, None);
        let err_disc = ctx.emit(Op::ConstI64(err_idx as i64), IrType::I64, None);
        let is_err = ctx.emit(Op::IEq(disc, err_disc), IrType::Bool, None);
        let ok_blk = ctx.create_block();
        let prop_blk = ctx.create_block();
        ctx.terminate(Terminator::Branch {
            condition: is_err,
            true_block: prop_blk,
            true_args: Vec::new(),
            false_block: ok_blk,
            false_args: Vec::new(),
        });
        // Field error → re-wrap the field's JsonError as this struct's error.
        ctx.switch_to_block(prop_blk);
        let je = ctx.emit(
            Op::EnumGetField(r, err_idx, 0),
            IrType::EnumRef(JSON_ERROR_ENUM.to_string(), Vec::new()),
            None,
        );
        jump_err(ctx, je);

        // Ok → extract the field value and continue to the next field.
        ctx.switch_to_block(ok_blk);
        let field_ir = ctx.lower_type(fty);
        let v = ctx.emit(Op::EnumGetField(r, ok_idx, 0), field_ir, None);
        field_vals.push(v);
    }

    // All fields decoded → build the struct and wrap in Ok.
    let payload_ir = result_ok_payload_ir(result_ty);
    let struct_name = match &payload_ir {
        IrType::StructRef(name, _) => name.clone(),
        other => unreachable!("struct decode: unexpected Ok payload type {other:?}"),
    };
    let built = ctx.emit(Op::StructAlloc(struct_name, field_vals), payload_ir, None);
    let ok = emit_result_ok(ctx, result_ty, built);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![ok],
    });

    ctx.switch_to_block(merge);
    result
}

/// The Ok-payload IR type of a `Result<T, JsonError>` result type (a
/// `StructRef` for a struct decoder).
fn result_ok_payload_ir(result_ty: &IrType) -> IrType {
    match result_ty {
        IrType::EnumRef(_, args) if !args.is_empty() => args[0].clone(),
        other => unreachable!("struct decode: unexpected result type {other:?}"),
    }
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
        assert_eq!(super::KIND_OBJECT, rt::JSON_KIND_OBJECT);
        // Tags the decoders don't consume yet, pinned by value so a
        // runtime renumbering is caught now, not when the composite-decode
        // slices mirror them here (replace these with mirror asserts then).
        assert_eq!(rt::JSON_KIND_NULL, 0);
        assert_eq!(rt::JSON_KIND_ARRAY, 5);
    }
}
