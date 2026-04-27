//! Struct-monomorphization pass.
//!
//! Walks every concrete function's IR types looking for
//! `StructRef(name, non_empty_args)` where `name` identifies a generic
//! struct template, registers a per-instantiation struct layout under
//! a mangled name, clones every method on the template into a parallel
//! specialized method, and rewrites every reference (`StructRef`,
//! `Op::StructAlloc`, `Op::DynAlloc`, method-dispatch `Op::Call`) to
//! the mangled form.
//!
//! Runs as a fixed-point worklist so recursive generic types
//! (`Node<T> { T val, Option<Node<T>> next }`) and nested
//! instantiations (`Container<List<Int>>`) both converge — each newly
//! specialized layout / method body is re-scanned for further generic
//! struct uses and enqueued.
//!
//! Unlike the function-mono pass, struct-mono does not need a separate
//! FuncId-assignment phase: the `rename_map` built during the
//! worklist tells the rewrite pass exactly which bare-name `StructRef`
//! maps to which mangled name, and specialized method FuncIds are
//! inserted into `method_index` as they're created.

use super::{mangle_type, substitute, substitute_types_in_fn};
use crate::instruction::{FuncId, Op};
use crate::module::{IrFunction, IrModule};
use crate::types::IrType;
use std::collections::{HashMap, HashSet, VecDeque};

/// One scheduled rekey of a `dyn_vtables` entry during struct-mono:
/// `(old_concrete_name, new_concrete_name, trait_name)`.  The trait name
/// is shared between the old and new keys by construction — the rekey
/// only changes the concrete type.  Accumulated during the DynAlloc
/// rewrite sweep and applied after all function bodies have been
/// rewritten.
type DynVtableRekey = (String, String, String);

/// `(template_name, concrete_args)` key used by struct-mono to identify
/// a particular instantiation of a generic struct template.
type StructInstKey = (String, Vec<IrType>);

/// Worklist of struct instantiations pending specialization.
type StructWorklist = VecDeque<StructInstKey>;

/// Dedup set paralleling [`StructWorklist`]: a key is inserted before
/// it's pushed onto the worklist to avoid re-enqueuing the same
/// instantiation when multiple use sites reference it.
type StructQueued = HashSet<StructInstKey>;

/// Map from `(template_name, concrete_args)` to the mangled
/// specialized-struct name (e.g. `"Container__i64"`).  Built during
/// `specialize_layouts_and_methods` and consumed by
/// `rewrite_all_references`.
type StructRenameMap = HashMap<StructInstKey, String>;

/// Top-level entry point for struct-mono.
pub(super) fn monomorphize_structs(module: &mut IrModule) {
    // Invariant: function-mono (if it ran) has already cleared every
    // `Op::Call` `type_args` vector in non-template functions.
    // struct-mono's call-rewriter depends on this — it only matches
    // calls whose `targs.is_empty()`, because a non-empty vector means a
    // still-unresolved user-generic call that must be handled by
    // function-mono first.  See module-level "Ordering" note.
    debug_assert_no_pending_generic_calls(module);

    let (worklist, queued) = seed_struct_worklist(module);
    let rename_map = specialize_layouts_and_methods(module, worklist, queued);
    let rekeys = rewrite_all_references(module, &rename_map);
    rekey_dyn_vtables(module, rekeys);
}

/// Debug-only check that every non-template function's `Op::Call` has an
/// empty `type_args` vector by the time struct-mono runs.  A violation
/// means function-mono was skipped or bypassed, and struct-mono's
/// call-rewriter would silently miss the call site.
fn debug_assert_no_pending_generic_calls(module: &IrModule) {
    if !cfg!(debug_assertions) {
        return;
    }
    for func in &module.functions {
        if func.is_generic_template {
            continue;
        }
        for (block_idx, block) in func.blocks.iter().enumerate() {
            for (instr_idx, instr) in block.instructions.iter().enumerate() {
                if let Op::Call(_, targs, _) = &instr.op {
                    debug_assert!(
                        targs.is_empty(),
                        "struct-mono precondition violated: non-template function `{}` \
                         block {} instr {} has `Op::Call` with non-empty type_args — \
                         function-mono must run before struct-mono (see module-level \
                         ordering note)",
                        func.name,
                        block_idx,
                        instr_idx,
                    );
                }
            }
        }
    }
}

/// Seed: walk every non-template function's type annotations and enqueue
/// every `(template_name, concrete_args)` pair that names a generic
/// struct declared in `module.struct_type_params`.  Returns the worklist
/// and a parallel `HashSet` used to dedup enqueue attempts across the
/// entire pass.
fn seed_struct_worklist(module: &IrModule) -> (StructWorklist, StructQueued) {
    let mut worklist: StructWorklist = VecDeque::new();
    let mut queued: StructQueued = HashSet::new();
    for func in &module.functions {
        if func.is_generic_template {
            continue;
        }
        enqueue_types_from_fn(func, module, &mut worklist, &mut queued);
    }
    (worklist, queued)
}

/// Drain the worklist, specializing each `(template, args)` pair by:
/// registering a per-instantiation struct layout under the mangled name,
/// cloning + substituting every method on the template into a parallel
/// specialized method, and enqueuing any newly-exposed nested generic
/// struct uses.  Returns the `rename_map` used by Pass 2 to rewrite
/// references in concrete function bodies.
///
/// **Clone-bypass note.** Specialized methods are created by cloning an
/// existing `IrFunction` and mutating its fields, rather than going
/// through [`IrFunction::new`] and the usual `fresh_value` /
/// `add_block_param` entry points.  This preserves the parallel
/// `value_types` index (which the clone carries with it) but joins the
/// small set of sites that bypass the three canonical allocation paths
/// — see known-issues.md's *`IrFunction.value_types` parallel-index
/// invariant* entry.  If `IrFunction::new` ever starts tracking state
/// that clone doesn't copy, this call site will need to be revisited
/// alongside the monomorphization template-clone at
/// `clone_and_substitute_bodies`.
fn specialize_layouts_and_methods(
    module: &mut IrModule,
    mut worklist: StructWorklist,
    mut queued: StructQueued,
) -> StructRenameMap {
    let mut rename_map: StructRenameMap = HashMap::new();

    while let Some((template_name, concrete_args)) = worklist.pop_front() {
        let mangled = mangle_struct_instantiation(&template_name, &concrete_args);
        // Skip if we've already registered this pair (worklist can enqueue
        // duplicates when multiple use sites reference the same
        // instantiation; `queued` dedups enqueue-order, but the specialized
        // layout registration is the source of truth).
        if module.struct_layouts.contains_key(&mangled)
            && rename_map.contains_key(&(template_name.clone(), concrete_args.clone()))
        {
            continue;
        }
        rename_map.insert(
            (template_name.clone(), concrete_args.clone()),
            mangled.clone(),
        );

        // Build the TypeVar → concrete-type substitution map.
        let type_params = module
            .struct_type_params
            .get(&template_name)
            .cloned()
            .unwrap_or_default();
        let subst: HashMap<String, IrType> = type_params
            .iter()
            .cloned()
            .zip(concrete_args.iter().cloned())
            .collect();

        specialize_one_struct(
            module,
            &template_name,
            &mangled,
            &subst,
            &mut worklist,
            &mut queued,
        );
    }

    rename_map
}

/// Specialize a single `(template, mangled, subst)`: register the
/// specialized layout and clone + substitute every method on the
/// template.  Enqueues any nested generic struct uses exposed by either
/// the specialized layout or the specialized method bodies.
fn specialize_one_struct(
    module: &mut IrModule,
    template_name: &str,
    mangled: &str,
    subst: &HashMap<String, IrType>,
    worklist: &mut StructWorklist,
    queued: &mut StructQueued,
) {
    // Specialize the layout.
    let template_layout = module
        .struct_layouts
        .get(template_name)
        .cloned()
        .unwrap_or_default();
    let specialized_layout: Vec<(String, IrType)> = template_layout
        .into_iter()
        .map(|(fname, fty)| (fname, substitute(&fty, subst)))
        .collect();
    for (_, fty) in &specialized_layout {
        enqueue_generic_struct_refs(fty, module, worklist, queued);
    }
    module
        .struct_layouts
        .insert(mangled.to_string(), specialized_layout);

    // Specialize methods. Snapshot first because we mutate
    // `method_index` during the loop.
    let template_methods: Vec<(String, FuncId)> = module
        .method_index
        .iter()
        .filter_map(|((t, m), fid)| {
            if t == template_name {
                Some((m.clone(), *fid))
            } else {
                None
            }
        })
        .collect();
    for (method_name, template_fid) in template_methods {
        let new_fid = FuncId(module.functions.len() as u32);
        let mut new_fn = module.functions[template_fid.index()].clone();
        new_fn.id = new_fid;
        new_fn.name = format!("{mangled}.{method_name}");
        new_fn.is_generic_template = false;
        // Apply the struct's type-param substitution to every type
        // annotation in the method body.
        substitute_types_in_fn(&mut new_fn, subst);
        // Enqueue any nested generic structs exposed by the
        // substituted body before moving the function.
        enqueue_types_from_fn(&new_fn, module, worklist, queued);
        module.functions.push(new_fn);
        module
            .method_index
            .insert((mangled.to_string(), method_name), new_fid);
    }
}

/// Rewrite every concrete function's references to generic structs
/// (method calls, StructAlloc, DynAlloc, and then StructRef types
/// themselves) to the mangled-name form.  Also rewrites `struct_layouts`
/// field types for consistency with the post-mono invariant.  Returns
/// the accumulated dyn-vtable rekey list to be consumed by
/// [`rekey_dyn_vtables`].
///
/// Order matters: call-site and DynAlloc rewriting must read receiver
/// types before those types are rewritten, because they key on the
/// original `(template_name, args)` pair to pick the right mangled
/// destination.
fn rewrite_all_references(
    module: &mut IrModule,
    rename_map: &StructRenameMap,
) -> Vec<DynVtableRekey> {
    let mut dyn_vtable_rekeys: Vec<DynVtableRekey> = Vec::new();
    for func_idx in 0..module.functions.len() {
        if module.functions[func_idx].is_generic_template {
            continue;
        }
        rewrite_method_calls(module, func_idx, rename_map);
        rewrite_struct_alloc(module, func_idx, rename_map);
        rewrite_dyn_alloc(module, func_idx, rename_map, &mut dyn_vtable_rekeys);
    }
    // Rewrite StructRef types themselves (erases the args).
    for func_idx in 0..module.functions.len() {
        if module.functions[func_idx].is_generic_template {
            continue;
        }
        let func = &mut module.functions[func_idx];
        func.for_each_type_mut(|ty| rewrite_struct_refs_in_type(ty, rename_map));
    }

    // Also rewrite StructRef types inside specialized struct_layouts
    // field-type slots, so a `Nested<T> { Pair<T> p }` specialization
    // stores `StructRef("Pair__i64", [])` rather than the unresolved
    // `StructRef("Pair", [I64])`. Cranelift's current layout code
    // treats all StructRefs as 1-slot opaque pointers regardless of
    // args, so this isn't strictly necessary for codegen — it's a
    // consistency guard so any future consumer that inspects layout
    // field types sees fully-resolved references.
    let layout_names: Vec<String> = module.struct_layouts.keys().cloned().collect();
    for name in layout_names {
        let mut layout = module.struct_layouts.remove(&name).unwrap();
        for (_, fty) in &mut layout {
            rewrite_struct_refs_in_type(fty, rename_map);
        }
        module.struct_layouts.insert(name, layout);
    }

    dyn_vtable_rekeys
}

/// Rekey `dyn_vtables` entries for generic structs and rewrite each
/// entry's method `FuncId`s to point at the specialized methods.  When
/// a concrete generic struct is coerced into `dyn Trait`, the
/// lowering-time vtable registration used the template method `FuncId`s;
/// post-mono those templates are inert stubs (filtered out of the
/// Cranelift `func_ids` map), so we re-resolve through the mangled
/// `method_index` now.
///
/// Multiple DynAlloc sites may share the same `(bare_name, trait)`
/// template key (e.g. `Box<Int>` and `Box<String>` both registered
/// under `("Box", "Show")` at lowering time).  Fan the template entry
/// out per-instantiation via `get` + `clone` — only drop the template
/// after processing all rekeys so later iterations can still read it.
fn rekey_dyn_vtables(module: &mut IrModule, rekeys: Vec<DynVtableRekey>) {
    let mut template_keys_to_drop: HashSet<(String, String)> = HashSet::new();
    for (old_concrete, new_concrete, trait_name) in rekeys {
        let Some(entry) = module
            .dyn_vtables
            .get(&(old_concrete.clone(), trait_name.clone()))
            .cloned()
        else {
            continue;
        };
        template_keys_to_drop.insert((old_concrete, trait_name.clone()));
        let remapped: Vec<(String, FuncId)> = entry
            .iter()
            .map(|(method_name, _template_fid)| {
                let specialized = module
                    .method_index
                    .get(&(new_concrete.clone(), method_name.clone()))
                    .copied()
                    .unwrap_or_else(|| {
                        unreachable!(
                            "struct-mono: vtable rekey for `{new_concrete}: dyn {trait_name}` \
                             found no specialized method `{method_name}` in method_index"
                        )
                    });
                (method_name.clone(), specialized)
            })
            .collect();
        module
            .dyn_vtables
            .insert((new_concrete, trait_name), remapped);
    }
    for key in template_keys_to_drop {
        module.dyn_vtables.remove(&key);
    }
}

/// Enqueue every `(template_name, concrete_args)` pair reachable from
/// `ty` that names a generic struct declared in `module.struct_type_params`.
/// Recurses into nested container / closure types.
fn enqueue_generic_struct_refs(
    ty: &IrType,
    module: &IrModule,
    worklist: &mut StructWorklist,
    queued: &mut StructQueued,
) {
    match ty {
        IrType::StructRef(name, args) if !args.is_empty() => {
            if module.struct_type_params.contains_key(name) {
                let key = (name.clone(), args.clone());
                if queued.insert(key.clone()) {
                    worklist.push_back(key);
                }
            }
            // Recurse into args anyway — nested generics like
            // `Container<Box<Int>>` need Box<Int> enqueued too.
            for a in args {
                enqueue_generic_struct_refs(a, module, worklist, queued);
            }
        }
        IrType::StructRef(_, _) => {}
        IrType::EnumRef(_, args) => {
            for a in args {
                enqueue_generic_struct_refs(a, module, worklist, queued);
            }
        }
        IrType::ListRef(inner) => {
            enqueue_generic_struct_refs(inner, module, worklist, queued);
        }
        IrType::MapRef(k, v) => {
            enqueue_generic_struct_refs(k, module, worklist, queued);
            enqueue_generic_struct_refs(v, module, worklist, queued);
        }
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            for p in param_types {
                enqueue_generic_struct_refs(p, module, worklist, queued);
            }
            enqueue_generic_struct_refs(return_type, module, worklist, queued);
        }
        _ => {}
    }
}

/// Enqueue every generic-struct use referenced by any type annotation
/// of `func`: parameter types, return type, block-parameter types, and
/// per-instruction result types.  Shared between the seed pass and the
/// post-method-clone re-seed.
fn enqueue_types_from_fn(
    func: &IrFunction,
    module: &IrModule,
    worklist: &mut StructWorklist,
    queued: &mut StructQueued,
) {
    for pt in &func.param_types {
        enqueue_generic_struct_refs(pt, module, worklist, queued);
    }
    enqueue_generic_struct_refs(&func.return_type, module, worklist, queued);
    for block in &func.blocks {
        for (_, bp_ty) in &block.params {
            enqueue_generic_struct_refs(bp_ty, module, worklist, queued);
        }
        for instr in &block.instructions {
            enqueue_generic_struct_refs(&instr.result_type, module, worklist, queued);
        }
    }
}

/// Compute the mangled name for a `(template_name, concrete_args)` pair
/// using the shared [`mangle_type`] grammar on each arg.
fn mangle_struct_instantiation(template_name: &str, args: &[IrType]) -> String {
    let mut s = String::from(template_name);
    for a in args {
        s.push_str("__");
        s.push_str(&mangle_type(a));
    }
    s
}

/// Rewrite `Op::Call` whose callee is a method on a generic struct,
/// redirecting to the specialized method registered under the mangled
/// struct name.  Must run before struct-ref types are rewritten, because
/// it reads the receiver's IR type from the function's `value_types`
/// index to figure out which instantiation's method to call.
///
/// **Second-stage role for trait-bound method calls.**  Function-mono's
/// [`crate::monomorphize::function_mono`] rewrites
/// [`Op::UnresolvedTraitMethod`] placeholders with a generic-struct
/// receiver to an `Op::Call` targeting the *template* method's FuncId
/// (keyed by bare name in `method_index`).  This helper then sees that
/// `Op::Call`, notices the receiver's non-empty struct args, and
/// promotes the target to the mangled specialization's FuncId.  The
/// two passes cooperate — neither alone produces the final concrete
/// call on a generic-struct receiver.
fn rewrite_method_calls(module: &mut IrModule, func_idx: usize, rename_map: &StructRenameMap) {
    // Snapshot the data we need so we can mutably borrow the function.
    let mut rewrites: Vec<(usize, usize, FuncId)> = Vec::new();
    {
        let func = &module.functions[func_idx];
        for (block_idx, block) in func.blocks.iter().enumerate() {
            for (instr_idx, instr) in block.instructions.iter().enumerate() {
                if let Op::Call(callee_fid, targs, args) = &instr.op
                    && targs.is_empty()
                    && let Some(first_arg) = args.first()
                {
                    // Is the callee a method on a generic struct?
                    let callee_name = &module.functions[callee_fid.index()].name;
                    let (ty_name, method_name) = match callee_name.rsplit_once('.') {
                        Some((t, m)) => (t, m),
                        None => continue,
                    };
                    if !module.struct_type_params.contains_key(ty_name) {
                        continue;
                    }
                    // Read the receiver's StructRef args from the
                    // function's value_types index (populated at emit
                    // time via IrFunction::value_types).
                    let recv_ty = func.instruction_result_type(*first_arg);
                    let Some(IrType::StructRef(recv_name, recv_args)) = recv_ty else {
                        continue;
                    };
                    if recv_name != ty_name || recv_args.is_empty() {
                        continue;
                    }
                    let key = (ty_name.to_string(), recv_args.clone());
                    let Some(mangled) = rename_map.get(&key) else {
                        continue;
                    };
                    let Some(&specialized_fid) = module
                        .method_index
                        .get(&(mangled.clone(), method_name.to_string()))
                    else {
                        continue;
                    };
                    rewrites.push((block_idx, instr_idx, specialized_fid));
                }
            }
        }
    }
    // Apply rewrites.
    let func = &mut module.functions[func_idx];
    for (block_idx, instr_idx, new_fid) in rewrites {
        let instr = &mut func.blocks[block_idx].instructions[instr_idx];
        if let Op::Call(callee, _, _) = &mut instr.op {
            *callee = new_fid;
        }
    }
}

/// Rewrite every `Op::StructAlloc(name, ...)` whose recorded result
/// type carries non-empty generic args to use the mangled name from
/// `rename_map`.  The original lowering emits `StructAlloc("Container", ...)`
/// even at a `Container<Int>` call site; this rewrite points it at
/// `StructAlloc("Container__i64", ...)` so the Cranelift backend reads
/// the specialized layout by name.
fn rewrite_struct_alloc(module: &mut IrModule, func_idx: usize, rename_map: &StructRenameMap) {
    let mut rewrites: Vec<(usize, usize, String)> = Vec::new();
    {
        let func = &module.functions[func_idx];
        for (block_idx, block) in func.blocks.iter().enumerate() {
            for (instr_idx, instr) in block.instructions.iter().enumerate() {
                if let Op::StructAlloc(name, _) = &instr.op
                    && let IrType::StructRef(result_name, result_args) = &instr.result_type
                    && result_name == name
                    && !result_args.is_empty()
                    && let Some(mangled) = rename_map.get(&(name.clone(), result_args.clone()))
                {
                    rewrites.push((block_idx, instr_idx, mangled.clone()));
                }
            }
        }
    }
    let func = &mut module.functions[func_idx];
    for (block_idx, instr_idx, mangled) in rewrites {
        if let Op::StructAlloc(name, _) = &mut func.blocks[block_idx].instructions[instr_idx].op {
            *name = mangled;
        }
    }
}

/// Rewrite every `Op::DynAlloc(trait, concrete, value)` whose receiver
/// value has a generic StructRef type to use the mangled concrete name.
/// Accumulates the corresponding `(old_concrete, new_concrete, trait)`
/// rekey actions in `dyn_vtable_rekeys` for post-pass vtable updates.
fn rewrite_dyn_alloc(
    module: &mut IrModule,
    func_idx: usize,
    rename_map: &StructRenameMap,
    dyn_vtable_rekeys: &mut Vec<DynVtableRekey>,
) {
    let mut rewrites: Vec<(usize, usize, String)> = Vec::new();
    {
        let func = &module.functions[func_idx];
        for (block_idx, block) in func.blocks.iter().enumerate() {
            for (instr_idx, instr) in block.instructions.iter().enumerate() {
                let Op::DynAlloc(trait_name, concrete, value) = &instr.op else {
                    continue;
                };
                let Some(IrType::StructRef(recv_name, recv_args)) =
                    func.instruction_result_type(*value)
                else {
                    continue;
                };
                if recv_name != concrete || recv_args.is_empty() {
                    continue;
                }
                let Some(mangled) = rename_map.get(&(concrete.clone(), recv_args.clone())) else {
                    continue;
                };
                rewrites.push((block_idx, instr_idx, mangled.clone()));
                dyn_vtable_rekeys.push((concrete.clone(), mangled.clone(), trait_name.clone()));
            }
        }
    }
    let func = &mut module.functions[func_idx];
    for (block_idx, instr_idx, mangled) in rewrites {
        if let Op::DynAlloc(_, concrete, _) = &mut func.blocks[block_idx].instructions[instr_idx].op
        {
            *concrete = mangled;
        }
    }
}

/// Recursively rewrite every `StructRef(template, args)` where `(template,
/// args)` is in `rename_map` to `StructRef(mangled, Vec::new())`.  Walks
/// into nested generic / list / map / closure types.
fn rewrite_struct_refs_in_type(ty: &mut IrType, rename_map: &StructRenameMap) {
    match ty {
        IrType::StructRef(name, args) if !args.is_empty() => {
            // Recurse first so nested args get rewritten before we
            // consult the rename_map (lookups key on the *post-recurse*
            // args).
            for a in args.iter_mut() {
                rewrite_struct_refs_in_type(a, rename_map);
            }
            if let Some(mangled) = rename_map.get(&(name.clone(), args.clone())) {
                *name = mangled.clone();
                args.clear();
            }
        }
        IrType::StructRef(_, _) => {}
        IrType::EnumRef(_, args) => {
            for a in args.iter_mut() {
                rewrite_struct_refs_in_type(a, rename_map);
            }
        }
        IrType::ListRef(inner) => rewrite_struct_refs_in_type(inner, rename_map),
        IrType::MapRef(k, v) => {
            rewrite_struct_refs_in_type(k, rename_map);
            rewrite_struct_refs_in_type(v, rename_map);
        }
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            for p in param_types.iter_mut() {
                rewrite_struct_refs_in_type(p, rename_map);
            }
            rewrite_struct_refs_in_type(return_type, rename_map);
        }
        _ => {}
    }
}
