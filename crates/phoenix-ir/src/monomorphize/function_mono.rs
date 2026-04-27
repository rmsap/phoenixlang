//! Function-monomorphization passes.
//!
//! Specializes each `(template_FuncId, concrete_type_args)` pair
//! reachable from non-template call sites and rewrites those call sites
//! to target the specialized `FuncId`.  Runs before struct-mono — see
//! the ordering note in [`super`]'s module doc.
//!
//! ## Passes
//!
//! - [`collect_seed`] — Seed: every `Op::Call` with non-empty
//!   `type_args` in a non-template caller, sorted for determinism.
//! - [`assign_specialization_ids`] — Pass A: BFS-walk template bodies
//!   to assign fresh FuncIds to every reachable specialization.
//! - [`clone_and_substitute_bodies`] — Pass B: clone each template
//!   body, substitute `TypeVar`s, resolve trait-bound method
//!   placeholders, and rewrite internal generic call targets.
//! - [`rewrite_root_call_sites`] — Pass C: point root generic call
//!   sites at their specialized `FuncId`s.

use super::placeholder_resolution::{
    resolve_trait_bound_method_calls, resolve_unresolved_dyn_allocs,
};
use super::{
    SpecKey, SpecMap, SpecOrder, contains_dyn_ref, contains_type_var, mangle, substitute,
    substitute_types_in_fn,
};
use crate::instruction::{FuncId, Op};
use crate::module::{IrFunction, IrModule};
use crate::types::IrType;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};

/// Collect the BFS seed: every `(caller, block, instr, callee, type_args)`
/// for generic calls in non-template functions. Sorted for determinism so
/// that FuncId assignment order is reproducible across builds.
pub(super) fn collect_seed(module: &IrModule) -> Vec<SpecKey> {
    /// `(caller, block, instr)` position key for deterministic ordering.
    type Pos = (FuncId, u32, u32);
    let mut seed: Vec<(Pos, SpecKey)> = Vec::new();
    for caller in module.concrete_functions() {
        for (block_idx, block) in caller.blocks.iter().enumerate() {
            for (instr_idx, instr) in block.instructions.iter().enumerate() {
                if let Op::Call(callee, targs, _) = &instr.op
                    && !targs.is_empty()
                {
                    let pos = (caller.id, block_idx as u32, instr_idx as u32);
                    seed.push((pos, (*callee, targs.clone())));
                }
            }
        }
    }
    // Sort by position so enqueue order is deterministic across builds.
    seed.sort_by_key(|(pos, _)| *pos);
    seed.into_iter().map(|(_, k)| k).collect()
}

/// Pass A. Assign a fresh `FuncId` to every reachable `(template, targs)`
/// pair, BFS-walking template bodies to discover nested generic calls.
///
/// Returns the specialization map and an insertion-ordered list used by
/// Pass B to clone bodies in the same order.
pub(super) fn assign_specialization_ids(
    module: &IrModule,
    seed: Vec<SpecKey>,
) -> (SpecMap, SpecOrder) {
    let mut specialized: SpecMap = HashMap::new();
    let mut order: SpecOrder = Vec::new();
    let mut worklist: VecDeque<SpecKey> = seed.into_iter().collect();
    let base_id = module.functions.len() as u32;

    while let Some((orig_id, targs)) = worklist.pop_front() {
        debug_assert!(
            !targs.iter().any(contains_type_var),
            "monomorphization reached a call with unresolved TypeVar in type_args: \
             callee={orig_id:?}, targs={targs:?}. The outer template's substitution \
             failed to resolve this, which indicates either a sema bug or a call-site \
             type-arg recorded with a TypeVar that isn't one of the outer's parameters."
        );
        // Hard check (not debug_assert!) because the consequence of a
        // regression here is silent miscompile in release builds: mono
        // would proceed to specialize on a `dyn Trait` type argument with
        // no vtable-keyed specialization strategy, and Cranelift would
        // emit codegen that reads past the end of a non-existent vtable.
        // MVP scope (docs/design-decisions.md: "Dynamic dispatch via dyn
        // Trait") excludes this; sema's `check_call_type_args` is
        // expected to reject it. Remove this gate only once a
        // vtable-keyed specialization strategy lands.
        if targs.iter().any(contains_dyn_ref) {
            panic!(
                "monomorphization reached a call with a `dyn Trait` concrete type argument: \
                 callee={orig_id:?}, targs={targs:?}. MVP scope excludes generic \
                 specialization at `dyn Trait`; sema is expected to reject it."
            );
        }

        // `Entry` lets us check-or-insert without cloning `targs` twice.
        // `HashMap::len()` is stable across the vacant-branch `insert`, so
        // computing `new_id` up front is fine.
        let new_id = FuncId(base_id + specialized.len() as u32);
        match specialized.entry((orig_id, targs.clone())) {
            Entry::Occupied(_) => continue,
            Entry::Vacant(v) => {
                v.insert(new_id);
            }
        }

        // Build substitution for this specialization. The template's
        // `type_param_names` and `targs` are parallel lists.
        let orig = module.functions[orig_id.index()].func();
        let subst: HashMap<String, IrType> = orig
            .type_param_names
            .iter()
            .cloned()
            .zip(targs.iter().cloned())
            .collect();

        // Walk the template's body for nested generic calls. For each
        // Op::Call with non-empty type_args, substitute any TypeVars in
        // its recorded type args using `subst`, then enqueue the
        // resolved specialization.
        for block in &orig.blocks {
            for instr in &block.instructions {
                let Op::Call(inner_callee, inner_targs, _) = &instr.op else {
                    continue;
                };
                if inner_targs.is_empty() {
                    continue;
                }
                let resolved: Vec<IrType> =
                    inner_targs.iter().map(|t| substitute(t, &subst)).collect();
                if !specialized.contains_key(&(*inner_callee, resolved.clone())) {
                    worklist.push_back((*inner_callee, resolved));
                }
            }
        }

        order.push((orig_id, targs, new_id));
    }

    (specialized, order)
}

/// Pass B. Clone each template body, substitute TypeVars, and rewrite
/// internal `Op::Call` destinations (with their embedded `type_args`) to
/// the matching specialization. Pushes the clones into `module.functions`.
pub(super) fn clone_and_substitute_bodies(
    module: &mut IrModule,
    specialized: &SpecMap,
    order: &SpecOrder,
) {
    // Collect specialized functions before pushing to preserve `orig_id`
    // indexing into `module.functions` (which must not grow during the loop).
    let mut new_funcs: Vec<IrFunction> = Vec::with_capacity(order.len());

    for (orig_id, targs, new_id) in order {
        let orig = module.functions[orig_id.index()].func();
        let subst: HashMap<String, IrType> = orig
            .type_param_names
            .iter()
            .cloned()
            .zip(targs.iter().cloned())
            .collect();

        // Specializations are always concrete (the whole point of mono):
        // we collect them as bare `IrFunction`s and push via
        // `push_concrete` below. `push_concrete` will assign each
        // specialization's id from its slot index — push order matches
        // `new_id` allocation order, so the id reaches the value that
        // pass A wrote into `specialized`.
        let mut spec_fn = orig.clone();
        spec_fn.id = *new_id;
        spec_fn.name = mangle(&orig.name, targs);
        spec_fn.type_param_names = Vec::new();
        substitute_types_in_fn(&mut spec_fn, &subst);

        // Resolve trait-bound method calls on a type-variable receiver
        // (`animal.greet()` where `animal: T` has `<T: Greet>`). IR
        // lowering emits these as `Op::UnresolvedTraitMethod` because
        // it doesn't know which impl to call yet.  Now that types are
        // substituted, the receiver is concrete — look it up in
        // `method_index` and rewrite to a direct `Op::Call`.  Leaves
        // generic-struct-receiver calls pointing at the template
        // FuncId; struct-mono's `rewrite_method_calls` picks them up
        // from there.
        resolve_trait_bound_method_calls(&mut spec_fn, &module.method_index);

        // Resolve `dyn Trait` coercions on a type-variable source
        // (`let d: dyn Drawable = x` where `x: T` has `<T: Drawable>`).
        // Same structural story as above: lowering emits a
        // placeholder because vtable registration needs a concrete
        // name; mono supplies the name and registers the vtable.
        resolve_unresolved_dyn_allocs(&mut spec_fn, module);

        // Rewrite internal generic Op::Call targets and clear their
        // type_args (since the callee is now a concrete specialization).
        for block in spec_fn.blocks.iter_mut() {
            for instr in block.instructions.iter_mut() {
                let Op::Call(callee, call_targs, _) = &mut instr.op else {
                    continue;
                };
                if call_targs.is_empty() {
                    continue;
                }
                let resolved: Vec<IrType> =
                    call_targs.iter().map(|t| substitute(t, &subst)).collect();
                let spec_id = specialized.get(&(*callee, resolved.clone())).copied();
                debug_assert!(
                    spec_id.is_some(),
                    "Pass A should have enqueued every nested generic call, but no \
                     specialization exists for callee={callee:?} targs={resolved:?} \
                     reached from template {orig_id:?} at spec {new_id:?}"
                );
                if let Some(spec_id) = spec_id {
                    *callee = spec_id;
                    call_targs.clear();
                }
            }
        }

        new_funcs.push(spec_fn);
    }

    for spec in new_funcs {
        let name = spec.name.clone();
        let expected_id = spec.id;
        let pushed = module.push_concrete(spec);
        debug_assert_eq!(
            pushed, expected_id,
            "push order desync: pass A allocated {expected_id:?} but push_concrete \
             assigned {pushed:?}"
        );
        module.function_index.insert(name, pushed);
    }
}

/// Pass C. Rewrite every generic `Op::Call` in non-template callers to
/// point at the specialized `FuncId` and clear its `type_args`.
pub(super) fn rewrite_root_call_sites(module: &mut IrModule, specialized: &SpecMap) {
    for func in module.concrete_functions_mut() {
        for block in func.blocks.iter_mut() {
            for instr in block.instructions.iter_mut() {
                let Op::Call(callee, call_targs, _) = &mut instr.op else {
                    continue;
                };
                if call_targs.is_empty() {
                    continue;
                }
                if let Some(&spec_id) = specialized.get(&(*callee, call_targs.clone())) {
                    *callee = spec_id;
                    call_targs.clear();
                }
            }
        }
    }
}
