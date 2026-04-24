//! Placeholder-op resolution: rewrite the `Op::Unresolved*` variants
//! emitted by IR lowering for generic-context receivers / sources into
//! their concrete forms once monomorphization has substituted type
//! variables.
//!
//! Two resolvers live here today:
//!
//! - [`resolve_trait_bound_method_calls`] — [`Op::UnresolvedTraitMethod`]
//!   → [`Op::Call`] (via `module.method_index`).
//! - [`resolve_unresolved_dyn_allocs`] — [`Op::UnresolvedDynAlloc`]
//!   → [`Op::DynAlloc`] (registers the `(concrete, trait)` vtable on
//!   the module as a side effect).
//!
//! Both run inside
//! [`super::function_mono::clone_and_substitute_bodies`] against each
//! specialized function body after `substitute_types_in_fn` has erased
//! its `TypeVar`s. The verifier and the mono-pass debug assertion
//! ([`super::debug_assert_no_unresolved_placeholder_ops`]) together
//! enforce "no placeholder op survives into a concrete function."
//!
//! # Why a dedicated submodule
//!
//! Each placeholder currently costs five coordinated edits (enum
//! variant, verifier arm, mono-time resolver, Cranelift error arm,
//! IR-interp error arm). Colocating the resolvers here is the near-term
//! preparation for the dedicated `concretize` pass scheduled in
//! `docs/design-decisions.md`
//! (*Placeholder-op resolution via a dedicated concretize pass*):
//! when the third placeholder lands, promotion is a pass-boundary
//! change, not a structural reorg across files.
//!
//! The resolvers are intentionally not `pub` — only
//! [`super::function_mono`] drives them, and its ordering against
//! struct-mono is load-bearing (see the struct-mono invariant notes on
//! the individual functions).

use crate::instruction::{FuncId, Op};
use crate::module::{IrFunction, IrModule};
use crate::types::IrType;
use std::collections::HashMap;

/// Resolve trait-bound method calls (`animal.greet()` where `animal: T`
/// has `<T: Greet>`) inside a post-substitution specialized function
/// body.
///
/// IR lowering (`phoenix-ir/src/lower_expr.rs::lower_method_call`)
/// emits these as [`Op::UnresolvedTraitMethod`] because the receiver
/// is `Type::TypeVar` at that point and no impl is selected yet.
/// After function-mono's substitution, the receiver has a concrete IR
/// type — look it up in `method_index[(type_name, method_name)]` and
/// rewrite to a direct `Op::Call` (preserving the method's
/// `method_targs` so nested generic specialization can still fire
/// once the parser gains support for method-level generics on trait
/// methods; today those arrive empty and this branch is a no-op
/// passthrough, but keeping it wired avoids a second mono pass later).
///
/// **Generic-struct interaction.** For a receiver whose substituted
/// type is `StructRef(template, non_empty_args)` (e.g.
/// `StructRef("Container", [I64])`), this helper resolves to the
/// *template* method's FuncId by keying on the bare name.
/// Struct-monomorphization's `rewrite_method_calls` then picks up the
/// resulting `Op::Call`, sees the receiver's non-empty struct args,
/// and rewrites the target to the mangled specialized FuncId.  Both
/// passes cooperate to produce the final concrete `Op::Call`.
///
/// **Primitive receivers.** When the substituted receiver is a
/// primitive (`IrType::I64`/`F64`/`Bool`/`StringRef`), the type name is
/// recovered via [`primitive_type_name`] so that `impl Trait for Int`
/// and friends route correctly.
pub(super) fn resolve_trait_bound_method_calls(
    func: &mut IrFunction,
    method_index: &HashMap<(String, String), FuncId>,
) {
    // Collect rewrites under an immutable borrow, then apply.  Needed
    // because `instruction_result_type` reads the function while the
    // outer walk would otherwise hold a mutable borrow on the blocks.
    let mut rewrites: Vec<(usize, usize, Op)> = Vec::new();
    for (block_idx, block) in func.blocks.iter().enumerate() {
        for (instr_idx, instr) in block.instructions.iter().enumerate() {
            let Op::UnresolvedTraitMethod(method_name, method_targs, args) = &instr.op else {
                continue;
            };
            let Some(receiver) = args.first() else {
                debug_assert!(
                    false,
                    "Op::UnresolvedTraitMethod `{method_name}` has no receiver \
                     (args list is empty) in function `{}`",
                    func.name,
                );
                continue;
            };
            let recv_ty = func.instruction_result_type(*receiver).unwrap_or_else(|| {
                panic!(
                    "Op::UnresolvedTraitMethod `.{method_name}` receiver {receiver} \
                     has no recorded type in function `{}` — the value_types index \
                     is out of sync",
                    func.name,
                )
            });
            let type_name = match receiver_type_name(recv_ty) {
                Some(name) => name,
                None => panic!(
                    "Op::UnresolvedTraitMethod `.{method_name}` in function `{}` has a \
                     receiver typed {recv_ty} that cannot be mapped to a method-index \
                     key — sema should have rejected this impl shape",
                    func.name,
                ),
            };
            let &target_fid = method_index
                .get(&(type_name.clone(), method_name.clone()))
                .unwrap_or_else(|| {
                    panic!(
                        "Op::UnresolvedTraitMethod `.{method_name}` in function `{}`: \
                         method_index has no entry for ({type_name:?}, {method_name:?}) \
                         — a trait-impl registration step was skipped",
                        func.name,
                    )
                });
            let new_op = Op::Call(target_fid, method_targs.clone(), args.clone());
            rewrites.push((block_idx, instr_idx, new_op));
        }
    }
    for (block_idx, instr_idx, new_op) in rewrites {
        func.blocks[block_idx].instructions[instr_idx].op = new_op;
    }
}

/// Resolve `Op::UnresolvedDynAlloc` placeholders inside a post-
/// substitution specialized function body.
///
/// IR lowering (`phoenix-ir/src/lower_dyn.rs::coerce_to_expected`)
/// emits these when a value typed by a generic parameter flows into
/// a `dyn Trait` slot inside a generic function: the concrete type
/// is unknown, so vtable registration and concrete-name resolution
/// have to be deferred.  After `substitute_types_in_fn`, the
/// value's recorded type is concrete — derive the concrete type name,
/// register the `(concrete, trait)` vtable if it isn't registered
/// yet, and rewrite the op to `Op::DynAlloc(trait, concrete, value)`.
///
/// The vtable registration itself routes through
/// [`IrModule::register_dyn_vtable`] — the same helper the lowering-
/// time registration uses.  Method names are sourced from
/// `module.traits[trait].methods` (the IR-level trait registry) since
/// mono runs without a sema `CheckResult`.
///
/// # Struct-mono invariant (load-bearing)
///
/// When the substituted source type is a generic struct instance
/// (`StructRef("Container", [I64])`), [`receiver_type_name`] returns
/// the **bare** template name (`"Container"`), not the post-mangling
/// specialized name (`"Container__i64"`).  The vtable entry this
/// function inserts therefore points at the template method's
/// FuncId — which struct-mono will treat as an inert stub after it
/// runs.
///
/// **Struct-mono's `rekey_dyn_vtables` pass is responsible for
/// rekeying every `(bare_name, trait)` entry this function inserts
/// to `(mangled_name, trait)` and re-resolving each entry's FuncIds
/// through the mangled `method_index`.** The
/// [`super::function_mono::clone_and_substitute_bodies`] → `monomorphize_structs`
/// ordering in [`super::monomorphize`] is what makes this work; any
/// future refactor that reorders those two passes, or runs function-
/// mono without struct-mono, will silently install template FuncIds
/// into the emitted vtables and crash Cranelift after the verifier
/// happily accepts the module.
///
/// The long-term fix is the `concretize` pass (see
/// `docs/design-decisions.md` — *Placeholder-op resolution via a
/// dedicated concretize pass*): once concretize runs after both
/// mono passes, this function can consult the already-mangled
/// `method_index` and register the final key directly.
pub(super) fn resolve_unresolved_dyn_allocs(func: &mut IrFunction, module: &mut IrModule) {
    // Two-phase collect-then-apply so the immutable borrow for
    // `instruction_result_type` does not clash with the mutable borrow
    // needed to rewrite instructions.
    let mut rewrites: Vec<(usize, usize, Op)> = Vec::new();
    for (block_idx, block) in func.blocks.iter().enumerate() {
        for (instr_idx, instr) in block.instructions.iter().enumerate() {
            let Op::UnresolvedDynAlloc(trait_name, value) = &instr.op else {
                continue;
            };
            let actual_ty = func.instruction_result_type(*value).unwrap_or_else(|| {
                panic!(
                    "Op::UnresolvedDynAlloc trait=`{trait_name}` value={value} in function \
                     `{}` has no recorded type — value_types index is out of sync",
                    func.name,
                )
            });
            let concrete_name = match receiver_type_name(actual_ty) {
                Some(n) => n,
                None => panic!(
                    "Op::UnresolvedDynAlloc trait=`{trait_name}` in function `{}`: value \
                     type {actual_ty} cannot be mapped to a concrete type name — sema \
                     must reject this coercion shape before lowering",
                    func.name,
                ),
            };
            let method_names: Vec<String> = module
                .traits
                .get(trait_name)
                .unwrap_or_else(|| {
                    panic!(
                        "resolve_unresolved_dyn_allocs: trait `{trait_name}` has no \
                         IR-level metadata. `module.traits` is populated only for \
                         object-safe traits, so the most likely cause is sema's \
                         `object_safety_error` having allowed a non-object-safe trait \
                         into a `dyn` position; secondary cause is a missed trait-registry \
                         mirror during IR lowering"
                    )
                })
                .methods
                .iter()
                .map(|m| m.name.clone())
                .collect();
            module.register_dyn_vtable(trait_name, &concrete_name, &method_names);
            rewrites.push((
                block_idx,
                instr_idx,
                Op::DynAlloc(trait_name.clone(), concrete_name, *value),
            ));
        }
    }
    for (block_idx, instr_idx, new_op) in rewrites {
        func.blocks[block_idx].instructions[instr_idx].op = new_op;
    }
}

/// Map a receiver IR type to the string key used in
/// `IrModule::method_index`.  Returns `None` when the type cannot be
/// keyed — closures, maps, lists, and raw `dyn Trait` receivers all
/// fall into this bucket (none of them is a legal trait-bound
/// method-call receiver post-substitution).
fn receiver_type_name(ty: &IrType) -> Option<String> {
    match ty {
        IrType::StructRef(n, _) | IrType::EnumRef(n, _) => Some(n.clone()),
        IrType::I64 | IrType::F64 | IrType::Bool | IrType::StringRef => {
            Some(primitive_type_name(ty).to_string())
        }
        IrType::TypeVar(name) => {
            debug_assert!(
                false,
                "receiver_type_name called with a residual TypeVar({name}) — \
                 substitution should have erased this"
            );
            None
        }
        IrType::Void
        | IrType::ListRef(_)
        | IrType::MapRef(_, _)
        | IrType::ClosureRef { .. }
        | IrType::DynRef(_) => None,
    }
}

/// Surface name for a primitive IR type — the key used to register
/// impls in `method_index` (e.g. `impl Display for Int` is keyed
/// `("Int", "toString")`).  Mirrors the inverse mapping in
/// `lower_expr.rs::lower_method_call`.
fn primitive_type_name(ty: &IrType) -> &'static str {
    match ty {
        IrType::I64 => "Int",
        IrType::F64 => "Float",
        IrType::Bool => "Bool",
        IrType::StringRef => "String",
        _ => unreachable!("primitive_type_name called on non-primitive {ty}"),
    }
}
