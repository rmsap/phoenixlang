//! `json.decode<T>` synthesis: for every type demanded by a decode call
//! (plus, transitively, the field types of any struct target), a per-type
//! *node decoder* `__json_decode_<key>(node: I64) -> Result<T, JsonError>`,
//! and — for the call-site types only — a parse-wrapper *entry*
//! `__json_decode_entry_<key>(s: String) -> Result<T, JsonError>`.
//!
//! Covered so far: scalars (`Int`/`Float`/`Bool`/`String`), `Option<T>`,
//! `List<T>`, and non-generic structs and enums (adjacently tagged:
//! `{"type":"V"}` / `{"type":"V","value":[…]}`); sema's
//! `unsupported_json_decode_type` gate rejects the rest before it reaches here.

use std::collections::{BTreeMap, BTreeSet};

use phoenix_common::module_path::ModulePath;
use phoenix_sema::types::Type;

use super::decode_emit::{
    emit_decode_field, emit_get_field, emit_json_error, emit_missing_split, emit_result_err,
    emit_result_ok, result_ir,
};
use super::{encode_type_key, enum_variant_index, sanitize};
use crate::default_wrappers::with_synthetic_function;
use crate::instruction::{FuncId, Op, ValueId};
use crate::lower::LoweringContext;
use crate::module::IrFunction;
use crate::terminator::Terminator;
use crate::types::{IrType, LIST_TYPE, OPTION_ENUM};

// Node-kind tags mirror `phoenix_runtime::json::JSON_KIND_*` — the runtime
// is the ABI source of truth. Mirroring (rather than importing) keeps this
// compiler crate free of a production dependency on the runtime; the
// `kind_tags_match_runtime_abi` test pins the two in lockstep, the same
// scheme the list-header layout check uses. Decoders compare the result of
// the `json.kind` builtin against these.
const KIND_NULL: i64 = 0;
const KIND_BOOL: i64 = 1;
const KIND_INT: i64 = 2;
const KIND_FLOAT: i64 = 3;
const KIND_STRING: i64 = 4;
const KIND_ARRAY: i64 = 5;
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
        match &ty {
            // `Option<T>` / `List<T>` → decode `T`.
            Type::Generic(name, args)
                if (name == OPTION_ENUM || name == LIST_TYPE) && args.len() == 1 =>
            {
                queue.push(args[0].clone());
            }
            Type::Named(name) => {
                if let Some(info) = ctx.check.struct_info_by_name(name) {
                    let fields: Vec<(String, Type)> = info
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty.clone()))
                        .collect();
                    queue.extend(fields.iter().map(|(_, fty)| fty.clone()));
                    struct_fields.insert(key.clone(), fields);
                } else if let Some(info) = ctx.check.enum_info_by_name(name) {
                    // A non-generic enum decoder calls its variant field decoders.
                    queue.extend(
                        info.variants
                            .iter()
                            .flat_map(|(_, fts)| fts.iter().cloned()),
                    );
                }
            }
            _ => {}
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

/// Build a per-type node decoder body: scalars, `Option<T>`, `List<T>`, and
/// non-generic structs and enums. `node_ids` maps a type key to its node
/// decoder, so an aggregate decoder can call its field/variant/element
/// decoders; `struct_fields` is the demand walk's field snapshot for a
/// struct target (`None` for a non-struct). Both borrow caller-local maps,
/// so the emit closure captures them as-is; only an enum target's variants
/// need an owned snapshot up front (the lookup borrows `ctx.check`, and the
/// closure needs `ctx` mutably).
fn build_node_decoder(
    ctx: &mut LoweringContext<'_>,
    ty: &Type,
    fid: FuncId,
    node_ids: &BTreeMap<String, FuncId>,
    struct_fields: Option<&[(String, Type)]>,
) {
    let result_ty = result_ir(ctx, ty);
    // A non-struct `Type::Named` is an enum (sema's gate admits only structs
    // and enums).
    let enum_variants: Option<Vec<(String, Vec<Type>)>> = match ty {
        Type::Named(name) if struct_fields.is_none() => ctx
            .check
            .enum_info_by_name(name)
            .map(|i| i.variants.clone()),
        _ => None,
    };
    with_synthetic_function(ctx, fid, ModulePath::entry(), |ctx| {
        let start = ctx.create_block();
        ctx.switch_to_block(start);
        let node = ctx.add_block_param(start, IrType::I64);
        let result = if let Some(fields) = struct_fields {
            emit_struct_decode(ctx, fields, node, &result_ty, node_ids)
        } else if let Some(variants) = &enum_variants {
            emit_enum_decode(ctx, variants, node, &result_ty, node_ids)
        } else if let Type::Generic(name, args) = ty
            && name == OPTION_ENUM
            && args.len() == 1
        {
            emit_option_decode(ctx, &args[0], node, &result_ty, node_ids)
        } else if let Type::Generic(name, args) = ty
            && name == LIST_TYPE
            && args.len() == 1
        {
            emit_list_decode(ctx, &args[0], node, &result_ty, node_ids)
        } else {
            emit_scalar_decode(ctx, ty, node, &result_ty)
        };
        ctx.terminate(Terminator::Return(Some(result)));
    });
}

/// Emit an `Option<T>` decoder: `null` → `Ok(None)`, otherwise decode `T` and
/// wrap it in `Some` (propagating a `T` decode error). Leaves the context in
/// the merge block; returns its parameter.
fn emit_option_decode(
    ctx: &mut LoweringContext<'_>,
    inner: &Type,
    node: ValueId,
    result_ty: &IrType,
    node_ids: &BTreeMap<String, FuncId>,
) -> ValueId {
    let some_idx = enum_variant_index(ctx, OPTION_ENUM, "Some");
    let none_idx = enum_variant_index(ctx, OPTION_ENUM, "None");
    let option_ir = match result_ty {
        IrType::EnumRef(_, args) => args[0].clone(),
        other => unreachable!("Option decode: unexpected result type {other:?}"),
    };

    let merge = ctx.create_block();
    let result = ctx.add_block_param(merge, result_ty.clone());

    let kind = ctx.emit(
        Op::BuiltinCall("json.kind".to_string(), vec![node]),
        IrType::I64,
        None,
    );
    let null_kind = ctx.emit(Op::ConstI64(KIND_NULL), IrType::I64, None);
    let is_null = ctx.emit(Op::IEq(kind, null_kind), IrType::Bool, None);
    let null_blk = ctx.create_block();
    let some_blk = ctx.create_block();
    ctx.terminate(Terminator::Branch {
        condition: is_null,
        true_block: null_blk,
        true_args: Vec::new(),
        false_block: some_blk,
        false_args: Vec::new(),
    });

    // null → Ok(None).
    ctx.switch_to_block(null_blk);
    let none = ctx.emit(
        Op::EnumAlloc(OPTION_ENUM.to_string(), none_idx, Vec::new()),
        option_ir.clone(),
        None,
    );
    let ok_none = emit_result_ok(ctx, result_ty, none);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![ok_none],
    });

    // else → decode T (a `T` decode error propagates to merge), wrap Some.
    ctx.switch_to_block(some_blk);
    let v = emit_decode_field(ctx, node, inner, result_ty, merge, node_ids);
    let some = ctx.emit(
        Op::EnumAlloc(OPTION_ENUM.to_string(), some_idx, vec![v]),
        option_ir,
        None,
    );
    let ok_some = emit_result_ok(ctx, result_ty, some);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![ok_some],
    });

    ctx.switch_to_block(merge);
    result
}

/// Emit a `List<T>` decoder: require a JSON array, decode each element into a
/// `ListBuilder<T>` (a `T` decode error propagates), then freeze it into the
/// list. The builder is threaded through the loop as a block param so it stays
/// GC-rooted across iterations (the same discipline the encode accumulator
/// uses). Leaves the context in the merge block; returns its parameter.
fn emit_list_decode(
    ctx: &mut LoweringContext<'_>,
    elem: &Type,
    node: ValueId,
    result_ty: &IrType,
    node_ids: &BTreeMap<String, FuncId>,
) -> ValueId {
    let elem_ir = ctx.lower_type(elem);
    // The list type is the `Ok` payload of `Result<List<T>, JsonError>`.
    let list_ir = match result_ty {
        IrType::EnumRef(_, args) => args[0].clone(),
        other => unreachable!("List decode: unexpected result type {other:?}"),
    };
    let builder_ir = IrType::ListBuilderRef(Box::new(elem_ir));

    let merge = ctx.create_block();
    let result = ctx.add_block_param(merge, result_ty.clone());

    // Require a JSON array.
    let kind = ctx.emit(
        Op::BuiltinCall("json.kind".to_string(), vec![node]),
        IrType::I64,
        None,
    );
    let arr_kind = ctx.emit(Op::ConstI64(KIND_ARRAY), IrType::I64, None);
    let is_arr = ctx.emit(Op::IEq(kind, arr_kind), IrType::Bool, None);
    let arr_blk = ctx.create_block();
    let not_arr = ctx.create_block();
    ctx.terminate(Terminator::Branch {
        condition: is_arr,
        true_block: arr_blk,
        true_args: Vec::new(),
        false_block: not_arr,
        false_args: Vec::new(),
    });
    ctx.switch_to_block(not_arr);
    let m = ctx.emit(
        Op::ConstString("expected array".to_string()),
        IrType::StringRef,
        None,
    );
    let je = emit_json_error(ctx, "TypeMismatch", m);
    let err = emit_result_err(ctx, result_ty, je);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![err],
    });

    // Iterate `0..len`, decoding each element into a fresh `ListBuilder`.
    ctx.switch_to_block(arr_blk);
    let len = ctx.emit(
        Op::BuiltinCall("json.arrayLen".to_string(), vec![node]),
        IrType::I64,
        None,
    );
    let builder = ctx.emit(
        Op::BuiltinCall("ListBuilder.alloc".to_string(), Vec::new()),
        builder_ir.clone(),
        None,
    );
    let zero = ctx.emit(Op::ConstI64(0), IrType::I64, None);
    let one = ctx.emit(Op::ConstI64(1), IrType::I64, None);

    let header = ctx.create_block();
    let body = ctx.create_block();
    let freeze_blk = ctx.create_block();
    let h_i = ctx.add_block_param(header, IrType::I64);
    let h_b = ctx.add_block_param(header, builder_ir.clone());
    let b_i = ctx.add_block_param(body, IrType::I64);
    let b_b = ctx.add_block_param(body, builder_ir.clone());
    let f_b = ctx.add_block_param(freeze_blk, builder_ir);
    ctx.terminate(Terminator::Jump {
        target: header,
        args: vec![zero, builder],
    });

    // Header: while i < len.
    ctx.switch_to_block(header);
    let cond = ctx.emit(Op::ILt(h_i, len), IrType::Bool, None);
    ctx.terminate(Terminator::Branch {
        condition: cond,
        true_block: body,
        true_args: vec![h_i, h_b],
        false_block: freeze_blk,
        false_args: vec![h_b],
    });

    // Body: decode element `i` (in-bounds, so never the missing sentinel) and
    // push it; on a decode error `emit_decode_field` jumps to `merge`. The
    // array handle `node` dominates every block here, so it is read directly;
    // only the builder needs threading through block params.
    ctx.switch_to_block(body);
    let elem_node = ctx.emit(
        Op::BuiltinCall("json.arrayGet".to_string(), vec![node, b_i]),
        IrType::I64,
        None,
    );
    let v = emit_decode_field(ctx, elem_node, elem, result_ty, merge, node_ids);
    ctx.emit(
        Op::BuiltinCall("ListBuilder.push".to_string(), vec![b_b, v]),
        IrType::Void,
        None,
    );
    let i_next = ctx.emit(Op::IAdd(b_i, one), IrType::I64, None);
    ctx.terminate(Terminator::Jump {
        target: header,
        args: vec![i_next, b_b],
    });

    // Freeze: builder → List, wrap Ok.
    ctx.switch_to_block(freeze_blk);
    let list = ctx.emit(
        Op::BuiltinCall("ListBuilder.freeze".to_string(), vec![f_b]),
        list_ir,
        None,
    );
    let ok = emit_result_ok(ctx, result_ty, list);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![ok],
    });

    ctx.switch_to_block(merge);
    result
}

/// Emit a non-generic enum decoder (adjacently tagged): require an object,
/// read the `"type"` string discriminator, dispatch to the matching variant,
/// and decode its `"value"` array. Leaves the context in the merge block;
/// returns its parameter.
fn emit_enum_decode(
    ctx: &mut LoweringContext<'_>,
    variants: &[(String, Vec<Type>)],
    node: ValueId,
    result_ty: &IrType,
    node_ids: &BTreeMap<String, FuncId>,
) -> ValueId {
    // The enum name lives in the `Ok` payload type of `Result<E, JsonError>`.
    let enum_ir = match result_ty {
        IrType::EnumRef(_, args) => args[0].clone(),
        other => unreachable!("enum decode: unexpected result type {other:?}"),
    };
    let enum_name = match &enum_ir {
        IrType::EnumRef(n, _) => n.clone(),
        other => unreachable!("enum decode: Ok payload is not an enum: {other:?}"),
    };

    let merge = ctx.create_block();
    let result = ctx.add_block_param(merge, result_ty.clone());
    let err_to = |ctx: &mut LoweringContext<'_>, variant: &str, msg: &str| {
        let m = ctx.emit(Op::ConstString(msg.to_string()), IrType::StringRef, None);
        let je = emit_json_error(ctx, variant, m);
        let err = emit_result_err(ctx, result_ty, je);
        ctx.terminate(Terminator::Jump {
            target: merge,
            args: vec![err],
        });
    };

    // Require an object.
    let kind = ctx.emit(
        Op::BuiltinCall("json.kind".to_string(), vec![node]),
        IrType::I64,
        None,
    );
    let obj = ctx.emit(Op::ConstI64(KIND_OBJECT), IrType::I64, None);
    let is_obj = ctx.emit(Op::IEq(kind, obj), IrType::Bool, None);
    let obj_blk = ctx.create_block();
    let not_obj = ctx.create_block();
    ctx.terminate(Terminator::Branch {
        condition: is_obj,
        true_block: obj_blk,
        true_args: Vec::new(),
        false_block: not_obj,
        false_args: Vec::new(),
    });
    ctx.switch_to_block(not_obj);
    err_to(ctx, "TypeMismatch", "expected object");

    // Read the `"type"` discriminator (a string).
    ctx.switch_to_block(obj_blk);
    let tag_node = emit_get_field(ctx, node, "type");
    let (has_type, no_type) = emit_missing_split(ctx, tag_node);
    ctx.switch_to_block(no_type);
    err_to(ctx, "MissingField", "type");
    ctx.switch_to_block(has_type);
    let tk = ctx.emit(
        Op::BuiltinCall("json.kind".to_string(), vec![tag_node]),
        IrType::I64,
        None,
    );
    let str_kind = ctx.emit(Op::ConstI64(KIND_STRING), IrType::I64, None);
    let is_str = ctx.emit(Op::IEq(tk, str_kind), IrType::Bool, None);
    let read_tag = ctx.create_block();
    let bad_tag = ctx.create_block();
    ctx.terminate(Terminator::Branch {
        condition: is_str,
        true_block: read_tag,
        true_args: Vec::new(),
        false_block: bad_tag,
        false_args: Vec::new(),
    });
    ctx.switch_to_block(bad_tag);
    err_to(
        ctx,
        "TypeMismatch",
        "expected a string \"type\" discriminator",
    );

    // Dispatch on the tag string: one variant block per variant, chained.
    ctx.switch_to_block(read_tag);
    let tag = ctx.emit(
        Op::BuiltinCall("json.asStr".to_string(), vec![tag_node]),
        IrType::StringRef,
        None,
    );
    let variant_blocks: Vec<_> = variants.iter().map(|_| ctx.create_block()).collect();
    for (i, (vname, _)) in variants.iter().enumerate() {
        let vn = ctx.emit(Op::ConstString(vname.clone()), IrType::StringRef, None);
        let eq = ctx.emit(Op::StringEq(tag, vn), IrType::Bool, None);
        let next = ctx.create_block();
        ctx.terminate(Terminator::Branch {
            condition: eq,
            true_block: variant_blocks[i],
            true_args: Vec::new(),
            false_block: next,
            false_args: Vec::new(),
        });
        ctx.switch_to_block(next);
    }
    // No variant matched — name the offending tag in the message.
    let prefix = ctx.emit(
        Op::ConstString("unknown enum variant: ".to_string()),
        IrType::StringRef,
        None,
    );
    let msg = ctx.emit(Op::StringConcat(prefix, tag), IrType::StringRef, None);
    let je = emit_json_error(ctx, "TypeMismatch", msg);
    let err = emit_result_err(ctx, result_ty, je);
    ctx.terminate(Terminator::Jump {
        target: merge,
        args: vec![err],
    });

    // Per-variant decode.
    for (i, (vname, field_tys)) in variants.iter().enumerate() {
        ctx.switch_to_block(variant_blocks[i]);
        let variant_idx = enum_variant_index(ctx, &enum_name, vname);
        if field_tys.is_empty() {
            let built = ctx.emit(
                Op::EnumAlloc(enum_name.clone(), variant_idx, Vec::new()),
                enum_ir.clone(),
                None,
            );
            let ok = emit_result_ok(ctx, result_ty, built);
            ctx.terminate(Terminator::Jump {
                target: merge,
                args: vec![ok],
            });
            continue;
        }
        // Variant with fields: decode the `"value"` array.
        let value_node = emit_get_field(ctx, node, "value");
        let (have_value, no_value) = emit_missing_split(ctx, value_node);
        ctx.switch_to_block(no_value);
        err_to(ctx, "MissingField", "value");
        ctx.switch_to_block(have_value);
        let vk = ctx.emit(
            Op::BuiltinCall("json.kind".to_string(), vec![value_node]),
            IrType::I64,
            None,
        );
        let arr_kind = ctx.emit(Op::ConstI64(KIND_ARRAY), IrType::I64, None);
        let is_arr = ctx.emit(Op::IEq(vk, arr_kind), IrType::Bool, None);
        let decode_fields = ctx.create_block();
        let bad_value = ctx.create_block();
        ctx.terminate(Terminator::Branch {
            condition: is_arr,
            true_block: decode_fields,
            true_args: Vec::new(),
            false_block: bad_value,
            false_args: Vec::new(),
        });
        ctx.switch_to_block(bad_value);
        err_to(ctx, "TypeMismatch", "expected a \"value\" array");
        ctx.switch_to_block(decode_fields);
        let mut field_vals: Vec<ValueId> = Vec::with_capacity(field_tys.len());
        for (j, fty) in field_tys.iter().enumerate() {
            let idx = ctx.emit(Op::ConstI64(j as i64), IrType::I64, None);
            let elem = ctx.emit(
                Op::BuiltinCall("json.arrayGet".to_string(), vec![value_node, idx]),
                IrType::I64,
                None,
            );
            let (present, absent) = emit_missing_split(ctx, elem);
            ctx.switch_to_block(absent);
            err_to(ctx, "TypeMismatch", "too few elements in \"value\" array");
            ctx.switch_to_block(present);
            let v = emit_decode_field(ctx, elem, fty, result_ty, merge, node_ids);
            field_vals.push(v);
        }
        let built = ctx.emit(
            Op::EnumAlloc(enum_name.clone(), variant_idx, field_vals),
            enum_ir.clone(),
            None,
        );
        let ok = emit_result_ok(ctx, result_ty, built);
        ctx.terminate(Terminator::Jump {
            target: merge,
            args: vec![ok],
        });
    }

    ctx.switch_to_block(merge);
    result
}

/// Emit a struct decoder: require an object, decode each field (an absent
/// `Option` field decodes to `None` — absent ≡ null, see design-decisions
/// §Phase 4.6 B; any other missing field → `Err(MissingField)`; a field
/// decode error propagates), then build the struct. Leaves the context in
/// the merge block; returns its parameter.
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
    let mut field_vals: Vec<ValueId> = Vec::with_capacity(fields.len());
    for (fname, fty) in fields {
        let child = emit_get_field(ctx, node, fname);
        let (present_blk, missing_blk) = emit_missing_split(ctx, child);
        let is_option =
            matches!(fty, Type::Generic(n, args) if n == OPTION_ENUM && args.len() == 1);
        if is_option {
            // Absent Option field ≡ null → None. The absent and decoded
            // paths converge on a per-field block whose param is the value.
            let field_ir = ctx.lower_type(fty);
            let cont = ctx.create_block();
            let fv = ctx.add_block_param(cont, field_ir.clone());
            ctx.switch_to_block(missing_blk);
            let none_idx = enum_variant_index(ctx, OPTION_ENUM, "None");
            let none = ctx.emit(
                Op::EnumAlloc(OPTION_ENUM.to_string(), none_idx, Vec::new()),
                field_ir,
                None,
            );
            ctx.terminate(Terminator::Jump {
                target: cont,
                args: vec![none],
            });
            ctx.switch_to_block(present_blk);
            let v = emit_decode_field(ctx, child, fty, result_ty, merge, node_ids);
            ctx.terminate(Terminator::Jump {
                target: cont,
                args: vec![v],
            });
            ctx.switch_to_block(cont);
            field_vals.push(fv);
        } else {
            // Missing required field → Err(MissingField(name)).
            ctx.switch_to_block(missing_blk);
            let msg = ctx.emit(Op::ConstString(fname.clone()), IrType::StringRef, None);
            let je = emit_json_error(ctx, "MissingField", msg);
            jump_err(ctx, je);

            // Present → decode; propagate the field's error on failure.
            ctx.switch_to_block(present_blk);
            let v = emit_decode_field(ctx, child, fty, result_ty, merge, node_ids);
            field_vals.push(v);
        }
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
        assert_eq!(super::KIND_NULL, rt::JSON_KIND_NULL);
        assert_eq!(super::KIND_ARRAY, rt::JSON_KIND_ARRAY);
    }
}
