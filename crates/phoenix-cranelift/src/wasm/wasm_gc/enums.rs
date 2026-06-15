//! wasm32-gc enum type declaration (§Phase 2.4 decision K.4).
//!
//! Phoenix enums map to a WASM-GC subtype hierarchy: one open
//! `(sub (struct (field $tag i32)))` parent per concrete enum
//! instantiation, plus one final variant subtype per variant. This
//! module owns the *declaration* side — collecting every concrete
//! `EnumRef(name, args)` instantiation from the IR, monomorphizing
//! generic templates at codegen time, and emitting the parent +
//! variant type-section entries through [`ModuleBuilder`]'s narrow
//! enum accessors. The *query* side (`require_enum_parent_idx`,
//! `enum_by_parent_idx`, …) and the op lowering (`EnumAlloc` /
//! `EnumDiscriminant` / `EnumGetField`) live with the builder state and
//! the translator respectively.
//!
//! Split out of `module_builder.rs` so enum declaration is a single
//! self-contained responsibility, mirroring how `string_helpers`
//! owns string-helper synthesis.

use std::collections::{HashMap, HashSet};

use phoenix_ir::module::IrModule;
use phoenix_ir::types::IrType;

use crate::error::CompileError;

use super::module_builder::ModuleBuilder;

/// `(template_name, type_args)` identifying one concrete Phoenix enum
/// instantiation. `Option<Int>` and `Option<String>` are distinct keys.
pub(super) type EnumInstantiationKey = (String, Vec<IrType>);

/// Declare WASM-GC types for every Phoenix enum in the IR module per
/// §Phase 2.4 decision K.4: one open `(sub (struct (field $tag i32)))`
/// parent per concrete enum instantiation, then one final variant
/// subtype per variant. Parents are declared first so each variant's
/// `supertype_idx` can reference its parent (which must already exist
/// by type-section position).
///
/// Must run *after* `reserve_phoenix_structs` and
/// `declare_string_types` (so variant fields of those types can encode
/// their indices — a struct payload's index exists once reserved, even
/// though its body is defined later) and *before* any function signature
/// touching `IrType::EnumRef` is interned (so the signature can encode the
/// parent's `HeapType::Concrete(idx)`).
///
/// Field-type restriction for slice 3: variant fields can be
/// primitives (`Int` / `Float` / `Bool`), `StringRef`, `StructRef`, or
/// `EnumRef` (including self-recursive — the parent has already been
/// declared by the time variant fields are walked). Lists, maps,
/// closures, and dyn Trait as variant fields error here with a
/// per-slice diagnostic; each lands in the slice that pins its own
/// type-mapping decision.
pub(super) fn declare(
    builder: &mut ModuleBuilder,
    ir_module: &IrModule,
) -> Result<(), CompileError> {
    // Pass 0: collect every concrete enum instantiation appearing
    // anywhere in the IR. Phoenix doesn't monomorphize enum layouts
    // (templates with `__generic` placeholders live in `enum_layouts`);
    // we walk the IR for `EnumRef(name, args)` tuples and declare one
    // WASM enum per tuple. See §Phase 2.4 K.4 "Generic monomorphization
    // at codegen time".
    let mut instantiations_set: HashSet<EnumInstantiationKey> = HashSet::new();
    collect_enum_instantiations(ir_module, &mut instantiations_set);
    // Sort by the Debug-printed key so type-section layout is
    // deterministic across runs (IrType doesn't implement Ord, so we
    // can't use a BTreeSet; sorting by the formatted debug string is a
    // deterministic stable order).
    let mut instantiations: Vec<EnumInstantiationKey> = instantiations_set.into_iter().collect();
    instantiations.sort_by_cached_key(|k| format!("{k:?}"));

    // Substitute generics up front so the parent / variant passes both
    // see fully-concrete field type lists. The substitution matches
    // wasm32-linear's match-side heuristic — see the K.4 "Known
    // limitation" note for the case it doesn't cover.
    let mut concrete_variants: HashMap<EnumInstantiationKey, Vec<Vec<IrType>>> = HashMap::new();
    for inst in &instantiations {
        let (template_name, type_args) = inst;
        let template = ir_module.enum_layouts.get(template_name).ok_or_else(|| {
            CompileError::new(format!(
                "wasm32-gc: enum `{template_name}` referenced by `EnumRef` but \
                     missing from `IrModule::enum_layouts` (internal compiler bug)"
            ))
        })?;
        let concrete = substitute_generic_placeholders(template_name, template, type_args)?;
        concrete_variants.insert(inst.clone(), concrete);
    }

    // Pass 1: declare every parent. Each parent is an open
    // (`is_final = false`) struct with just the i32 `$tag` field. This
    // pass must precede pass 2 because variants reference their
    // parent's type-section index.
    let tag_field = wasm_encoder::FieldType {
        element_type: wasm_encoder::StorageType::Val(wasm_encoder::ValType::I32),
        mutable: false,
    };
    let mut parent_indices: HashMap<EnumInstantiationKey, u32> = HashMap::new();
    for inst in &instantiations {
        let parent_idx = builder.declare_enum_parent_struct(&[tag_field]);
        parent_indices.insert(inst.clone(), parent_idx);
    }

    // `$string`'s type-section index is stable here — string types are
    // declared before enums — so capture it once as a plain value
    // rather than re-borrowing the builder inside the field loop (which
    // would clash with the `&mut` variant-struct declaration below).
    let string_idx = builder.string_type_idx_if_set();

    // Pass 2: declare every variant, referencing its parent. The
    // template `enum_layouts` entry gives us variant names; concrete
    // field types come from the per-instantiation substitution.
    for inst in &instantiations {
        let (template_name, _) = inst;
        let template = &ir_module.enum_layouts[template_name];
        let concrete_fields_per_variant = &concrete_variants[inst];
        let parent_idx = parent_indices[inst];
        let mut variant_indices = Vec::with_capacity(template.len());
        for (variant_idx, (variant_name, _)) in template.iter().enumerate() {
            let concrete_fields = &concrete_fields_per_variant[variant_idx];
            let mut fields = Vec::with_capacity(concrete_fields.len() + 1);
            fields.push(tag_field);
            for (field_idx, field_ty) in concrete_fields.iter().enumerate() {
                fields.push(wasm_enum_field_type_for(
                    template_name,
                    variant_name,
                    field_idx,
                    field_ty,
                    &parent_indices,
                    string_idx,
                    builder.phx_struct_indices(),
                )?);
            }
            // All immutable borrows of `builder` (via `phx_struct_indices`)
            // are released now that `fields` is fully built and owned, so
            // the `&mut` declaration below doesn't conflict.
            let var_struct_idx = builder.declare_enum_variant_struct(&fields, parent_idx);
            variant_indices.push(var_struct_idx);
        }
        builder.record_enum(inst.clone(), parent_idx, variant_indices);
    }
    Ok(())
}

/// Map one Phoenix enum-variant field's `IrType` to a WASM-GC
/// `FieldType` for the containing variant's subtype declaration. Per
/// §Phase 2.4 decision K.4, slice 3 supports primitive fields,
/// `StringRef`, `StructRef`, and `EnumRef` (including self-recursive,
/// since all enum parents have already been declared by the time
/// variants are walked). Lists / maps / closures / dyn Trait error
/// with per-slice diagnostics. All fields are immutable — Phoenix enum
/// variants are immutable by language design (you can't mutate a field
/// of an existing variant value; you allocate a new one).
fn wasm_enum_field_type_for(
    enum_name: &str,
    variant_name: &str,
    field_idx: usize,
    field_ty: &IrType,
    enum_parents: &HashMap<EnumInstantiationKey, u32>,
    string_type_idx: Option<u32>,
    struct_indices: &HashMap<String, u32>,
) -> Result<wasm_encoder::FieldType, CompileError> {
    // An unresolved generic type parameter (`TypeVar` for user-defined
    // generic enum templates, or the `__generic` sentinel for builtins)
    // surviving into a concrete variant field means the position-counting
    // substitution couldn't resolve it — either a directly-generic field
    // (`enum Wrapper<T> { Bare(T) }`) or a nested one (`W(Option<T>)`,
    // whose field is an `EnumRef` *containing* the parameter rather than
    // being it, so `substitute_generic_placeholders` leaves it untouched).
    // Report it as the documented Known limitation rather than letting it
    // fall through to a misleading "struct `__generic` missing" / "enum
    // instantiation missed" / out-of-scope-`TypeVar` diagnostic. See
    // §Phase 2.4 K.4 Known limitation.
    if contains_generic_placeholder(field_ty) {
        return Err(CompileError::new(format!(
            "wasm32-gc slice 3: enum `{enum_name}` variant `{variant_name}` field \
             {field_idx} has unresolved generic type `{field_ty:?}` — user-defined \
             generic enums are not yet supported on wasm32-gc (§Phase 2.4 K.4 Known \
             limitation). The position-counting substitution heuristic only resolves \
             the stdlib `Option<T>` / `Result<T, E>` shapes; type parameters in \
             user-defined enums (whether a field is itself a parameter or nests one, \
             e.g. `enum Wrapper<T> {{ W(Option<T>) }}`) aren't substituted. The \
             limitation is shared with the wasm32-linear match-side path."
        )));
    }
    let val_type = match field_ty {
        IrType::I64 => wasm_encoder::ValType::I64,
        IrType::F64 => wasm_encoder::ValType::F64,
        IrType::Bool => wasm_encoder::ValType::I32,
        IrType::StringRef => {
            let idx = string_type_idx.ok_or_else(|| {
                CompileError::new(format!(
                    "wasm32-gc slice 3: enum `{enum_name}` variant \
                     `{variant_name}` field {field_idx} is `StringRef` but \
                     `declare_string_types` did not run — `scan_helper_needs` \
                     should have flagged `string_types` for any module with \
                     `IrType::StringRef` anywhere (internal compiler bug)"
                ))
            })?;
            wasm_encoder::ValType::Ref(wasm_encoder::RefType {
                nullable: true,
                heap_type: wasm_encoder::HeapType::Concrete(idx),
            })
        }
        IrType::StructRef(name, _) => {
            let idx = struct_indices.get(name).copied().ok_or_else(|| {
                CompileError::new(format!(
                    "wasm32-gc slice 3: enum `{enum_name}` variant \
                     `{variant_name}` field {field_idx} references struct \
                     `{name}`, which is missing from the declared structs \
                     (either not in `IrModule::struct_layouts` or declared \
                     after this enum, breaking the type-section ordering — \
                     §Phase 2.4 K.1 / K.4)"
                ))
            })?;
            wasm_encoder::ValType::Ref(wasm_encoder::RefType {
                nullable: true,
                heap_type: wasm_encoder::HeapType::Concrete(idx),
            })
        }
        IrType::EnumRef(name, args) => {
            let key = (name.clone(), args.clone());
            let idx = enum_parents.get(&key).copied().ok_or_else(|| {
                CompileError::new(format!(
                    "wasm32-gc slice 3: enum `{enum_name}` variant \
                     `{variant_name}` field {field_idx} references enum \
                     instantiation `{name}{args:?}`, which the \
                     enum-collection pass missed (the field type ought to \
                     have been walked too — internal compiler bug; K.4 \
                     `collect_enum_instantiations` should chase through \
                     nested EnumRef field types)"
                ))
            })?;
            wasm_encoder::ValType::Ref(wasm_encoder::RefType {
                nullable: true,
                heap_type: wasm_encoder::HeapType::Concrete(idx),
            })
        }
        other => {
            return Err(CompileError::new(format!(
                "wasm32-gc slice 3: enum `{enum_name}` variant \
                 `{variant_name}` field {field_idx} has type `{other:?}`, \
                 which is out of slice scope. Slice 3 supports Int / Float \
                 / Bool / StringRef / StructRef / EnumRef as variant \
                 fields; list / map / closure / dyn Trait fields land in \
                 follow-up slices (each carries its own type-mapping \
                 sub-decision under §Phase 2.4 decision K)"
            )));
        }
    };
    Ok(wasm_encoder::FieldType {
        element_type: wasm_encoder::StorageType::Val(val_type),
        mutable: false,
    })
}

/// Walk every IR type in the module, recursively collecting every
/// distinct `EnumRef(name, args)` tuple. Each tuple becomes one
/// concrete enum declaration in the WASM type section per the K.4
/// codegen-time monomorphization step. `scan_helper_needs` reuses the
/// same walk so its string-types backstop fires for exactly the
/// instantiations this pass will declare — keep the two callers in
/// sync if the walk's sources change.
///
/// Sources walked: function param/return types, block params,
/// instruction `result_type`s, struct field types, and (recursively)
/// the type-arg lists of nested EnumRef / StructRef / ListRef /
/// MapRef / ClosureRef. The walk doesn't dispatch on `Op::EnumAlloc`
/// directly because the alloc's result_type already carries the
/// concrete `EnumRef(name, args)` — visiting it as part of the
/// instruction's result_type covers both sides.
pub(super) fn collect_enum_instantiations(
    ir_module: &IrModule,
    result: &mut HashSet<EnumInstantiationKey>,
) {
    for func in ir_module.concrete_functions() {
        walk_type(&func.return_type, result);
        for ty in &func.param_types {
            walk_type(ty, result);
        }
        for block in &func.blocks {
            for (_, ty) in &block.params {
                walk_type(ty, result);
            }
            for instr in &block.instructions {
                walk_type(&instr.result_type, result);
            }
        }
    }
    // Struct field types — a struct might carry an enum-typed field
    // that no function/block param reaches directly.
    for variants in ir_module.struct_layouts.values() {
        for (_, ty) in variants {
            walk_type(ty, result);
        }
    }
    // Enum variant field types — recursive enums + cross-enum
    // references. Templates' `__generic` placeholders are skipped
    // (they're not concrete EnumRefs); concrete EnumRefs in user-
    // defined enums (e.g., `enum Foo { A(Option<Int>) }`) get picked
    // up here.
    for variants in ir_module.enum_layouts.values() {
        for (_, fields) in variants {
            for ty in fields {
                walk_type(ty, result);
            }
        }
    }
}

fn walk_type(ty: &IrType, result: &mut HashSet<EnumInstantiationKey>) {
    match ty {
        IrType::EnumRef(name, args) => {
            // Only record fully-concrete instantiations. An `EnumRef`
            // whose args still carry a `__generic` placeholder (e.g.
            // `Option<T>` inside a generic enum template's variant
            // field) is not a real instantiation — declaring a parent
            // for it would emit junk types and surface as a confusing
            // diagnostic when its placeholder field is walked. The
            // nested-generic limitation is reported with a clear
            // message in `wasm_enum_field_type_for` instead. See
            // §Phase 2.4 K.4 Known limitation.
            if !args.iter().any(contains_generic_placeholder) {
                result.insert((name.clone(), args.clone()));
            }
            for arg in args {
                walk_type(arg, result);
            }
        }
        IrType::StructRef(_, args) => {
            for arg in args {
                walk_type(arg, result);
            }
        }
        IrType::ListRef(inner) | IrType::ListBuilderRef(inner) => walk_type(inner, result),
        IrType::MapRef(k, v) | IrType::MapBuilderRef(k, v) => {
            walk_type(k, result);
            walk_type(v, result);
        }
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            for p in param_types {
                walk_type(p, result);
            }
            walk_type(return_type, result);
        }
        _ => {}
    }
}

/// Whether `ty` is, or transitively contains, an unresolved generic
/// type parameter. Two spellings exist: builtin generic enums
/// (`Option` / `Result`) carry the `StructRef("__generic", [])`
/// sentinel ([`IrType::is_generic_placeholder`]), while user-defined
/// generic enum *templates* carry `IrType::TypeVar(_)` (they aren't
/// monomorphized into `enum_layouts`). Either surviving into a concrete
/// variant field means the position-counting substitution couldn't
/// resolve it. The recursive arms catch the parameter nested inside
/// enum / struct type args, list / map element types, and closure
/// signatures. See §Phase 2.4 K.4 Known limitation.
pub(super) fn contains_generic_placeholder(ty: &IrType) -> bool {
    if ty.is_generic_placeholder() || matches!(ty, IrType::TypeVar(_)) {
        return true;
    }
    match ty {
        IrType::EnumRef(_, args) | IrType::StructRef(_, args) => {
            args.iter().any(contains_generic_placeholder)
        }
        IrType::ListRef(inner) | IrType::ListBuilderRef(inner) => {
            contains_generic_placeholder(inner)
        }
        IrType::MapRef(k, v) | IrType::MapBuilderRef(k, v) => {
            contains_generic_placeholder(k) || contains_generic_placeholder(v)
        }
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            param_types.iter().any(contains_generic_placeholder)
                || contains_generic_placeholder(return_type)
        }
        _ => false,
    }
}

/// Substitute `IrType::StructRef("__generic", [])` placeholders in a
/// generic enum template's variant fields by walking all variants in
/// declaration order and consuming the next slot of `type_args` for
/// each placeholder. Matches wasm32-linear's match-side heuristic; see
/// the K.4 "Known limitation" note for the case it doesn't cover
/// (user-defined generic enums with type params repeated across
/// variants in non-declaration order).
fn substitute_generic_placeholders(
    template_name: &str,
    template: &[(String, Vec<IrType>)],
    type_args: &[IrType],
) -> Result<Vec<Vec<IrType>>, CompileError> {
    let mut result = Vec::with_capacity(template.len());
    let mut placeholder_cursor: usize = 0;
    for (variant_name, fields) in template {
        let mut concrete = Vec::with_capacity(fields.len());
        for field in fields {
            if field.is_generic_placeholder() {
                if placeholder_cursor >= type_args.len() {
                    return Err(CompileError::new(format!(
                        "wasm32-gc slice 3: enum `{template_name}` variant \
                         `{variant_name}` has more `__generic` placeholders \
                         than the {} type args available — this is the \
                         known limitation of the position-counting \
                         substitution heuristic (§Phase 2.4 K.4 Known \
                         limitation). User-defined generic enums where \
                         type parameters repeat across variants in \
                         non-declaration order are not yet supported on \
                         wasm32-gc; the limitation is shared with the \
                         wasm32-linear match-side path.",
                        type_args.len()
                    )));
                }
                concrete.push(type_args[placeholder_cursor].clone());
                placeholder_cursor += 1;
            } else {
                concrete.push(field.clone());
            }
        }
        result.push(concrete);
    }
    Ok(result)
}
