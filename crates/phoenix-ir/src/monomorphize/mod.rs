//! Monomorphization pass for generic functions and generic structs.
//!
//! Collects every concrete instantiation of every generic function from
//! call sites embedded in the IR (the middle `Vec<IrType>` of
//! [`Op::Call`]), emits one specialized [`IrFunction`] per
//! `(template, concrete_type_args)` pair, and rewrites the call sites to
//! point at the specialized functions.  A second stage does the same
//! for generic struct templates — see [`struct_mono`].
//!
//! Runs inside [`crate::lower::lower`] between AST lowering and module
//! return, so all downstream consumers (verifier, interpreter, Cranelift
//! backend) see only monomorphized IR where every `IrType::TypeVar` has
//! been substituted with a concrete type and every `Op::Call`'s
//! `type_args` vector is empty.
//!
//! ## Algorithm (BFS worklist, two-pass)
//!
//! 1. **Seed**: walk every `Op::Call` in every non-template function and
//!    enqueue `(callee_FuncId, concrete_targs)` for each one whose
//!    `type_args` is non-empty. Iteration is deterministic (ordered by
//!    `(caller_FuncId, block, instr)`).
//! 2. **Pass A — ID assignment**: drain the worklist, assigning a fresh
//!    `FuncId` to each new `(template, targs)` pair. While walking a
//!    template's body, discover its own generic calls; substitute any
//!    inner `TypeVar` args using the current specialization's bindings and
//!    recursively enqueue. Does not clone bodies yet.
//! 3. **Pass B — body cloning**: iterate the assigned specializations in
//!    insertion order. Clone each template body, substitute all `TypeVar`
//!    occurrences with concrete types, resolve any
//!    [`Op::UnresolvedTraitMethod`] placeholders into direct `Op::Call`s
//!    via the module's `method_index`, and rewrite internal generic
//!    `Op::Call` destinations (and their embedded `type_args`) to their
//!    specialized `FuncId`s.
//! 4. **Pass C — root call-site rewriting**: replace `Op::Call` targets in
//!    non-generic functions with their specialized `FuncId`s and clear
//!    their `type_args` vectors.
//! 5. **Pass D — TypeVar erasure**: erase any residual `IrType::TypeVar`
//!    in non-template functions (from unresolved sema inference, e.g.
//!    empty list literals) to the `GENERIC_PLACEHOLDER` sentinel so the
//!    Cranelift backend's use-site inference can resolve them.
//!
//! Generic templates are kept in `module.functions` as inert
//! [`crate::module::FunctionSlot::Template`] slots to preserve the
//! `FuncId`-as-vector-index invariant relied on across the codebase.
//! Every downstream pass iterates via
//! [`crate::module::IrModule::concrete_functions`] to skip them — the
//! tagged-slot type makes the filter type-system-enforced rather than
//! convention-only.
//!
//! ## Integration with earlier passes
//!
//! The concrete type arguments this pass consumes are populated by sema's
//! `Checker::record_inferred_type_args` (in `phoenix-sema/src/check_expr_call.rs`)
//! into `ResolvedModule.call_type_args`, keyed by each call expression's
//! source span. IR lowering (`crate::lower_expr`) looks them up at call
//! sites via `LowerContext::resolve_call_type_args` and embeds them into
//! the middle slot of `Op::Call`. This pass reads that middle slot, rewrites
//! the call to target a specialized `FuncId`, and clears the slot.
//!
//! ## Determinism
//!
//! FuncId assignment is stable across builds. The seed set is sorted by
//! `(caller_FuncId, block_idx, instr_idx)` before being drained as a
//! VecDeque BFS (see [`function_mono::collect_seed`]), and new FuncIds
//! are assigned via `FuncId(base_id + specialized.len() as u32)` at
//! insertion time — so the only thing that can perturb the assignment
//! is the insertion order, which is deterministic. The `SpecMap`
//! `HashMap` is only ever consulted via direct lookup (never iterated
//! for output), so its non-deterministic iteration order does not leak
//! into the generated IR.
//!
//! ## MVP scope (deliberate omissions)
//!
//! - Only direct calls (`Op::Call`) are specialized; closures over
//!   generic parameters and first-class generic function values are not.
//! - Cross-module instantiation is not handled (single-module compilation).
//!
//! Trait-bounded method-call specialization (`x.method()` where
//! `x: T` has `<T: Trait>`) is handled during Pass B: IR lowering
//! emits [`Op::UnresolvedTraitMethod`] when the receiver type is a
//! `TypeVar`, and this pass rewrites it to a direct `Op::Call` once
//! the receiver's concrete type is known.  See
//! [`function_mono`].
//!
//! ## Why Phoenix uses two generic strategies side-by-side
//!
//! Phoenix generics are resolved in **two different ways** depending on
//! the callee, and this file is only responsible for one of them:
//!
//! - **User-defined generics** (this pass). Each call site carries fully
//!   resolved concrete type arguments (produced by sema; embedded in
//!   `Op::Call`'s middle slot during lowering). This pass clones one
//!   specialized body per `(template, targs)` pair, substitutes
//!   `IrType::TypeVar` with concrete types, and rewrites the call to
//!   target the specialized `FuncId`. After this pass, no user-generic
//!   call carries type args and no user-generic body contains `TypeVar`.
//!
//! - **Stdlib generics** (`List<T>`, `Map<K,V>`, `Option<T>`,
//!   `Result<T,E>`). These remain single functions in the IR, their
//!   element types represented by `StructRef(GENERIC_PLACEHOLDER)`. The
//!   Cranelift backend recovers the concrete element type at each
//!   *use site* (from the receiver's static type), and uses it to pick
//!   the right load/store/alloc shape. No monomorphization happens for
//!   these; they're value-uniform by construction (all reference types
//!   have the same calling convention, so one implementation serves all
//!   element types).
//!
//! Pass D below bridges the two strategies. If sema leaves an
//! `IrType::TypeVar` in a non-template function (e.g., an empty list
//! literal whose element type was never constrained), Pass D erases it
//! to `GENERIC_PLACEHOLDER` so the backend's use-site inference path
//! handles it uniformly with the stdlib-generic case. This is the *only*
//! reason the sentinel reaches non-template bodies.
//!
//! The design tradeoff: user generics can be at arbitrary value types
//! (including `Int` and `Float` with different Cranelift reprs), so they
//! *must* be monomorphized. Stdlib generics are restricted to reference
//! types, which share a single repr, so they *need not* be — and one
//! implementation per stdlib method keeps codegen simple. Unifying on a
//! single strategy (all-monomorphize or all-placeholder) would either
//! explode compile times for the stdlib or block user generics over
//! value types, so the split stays.
//!
//! ## Symbol-safe name mangling
//!
//! Specialized function names are produced by [`mangle`] and use only
//! characters matching `[A-Za-z0-9_]`. See that function's docs for the
//! grammar. The names flow through to Cranelift symbol names via
//! `phoenix-cranelift/src/context.rs`, so no external sanitization is
//! required there.
//!
//! ## Struct-mono: dual-path TypeVar substitution
//!
//! Field types on generic structs are substituted in **two distinct
//! places**, and both are necessary:
//!
//! 1. **At lowering time**, inside a concrete use-site method body.
//!    When lowering `c.value` where `c: Container<Int>`, sema types the
//!    receiver as `Generic("Container", [Int])`. The layout in
//!    `struct_layouts["Container"]` still holds `TypeVar("T")` for
//!    `value` at this point (struct-mono hasn't run yet), so
//!    [`crate::lower_expr::LoweringContext::resolve_field_type`]
//!    substitutes `T → Int` to emit an `Op::StructGetField` with a
//!    fully-resolved `result_type`. Without this, the concrete use-site
//!    body would carry `TypeVar` in its value-allocator type index — a
//!    verifier violation.
//!
//! 2. **At struct-mono time**, inside a *template* method body. The
//!    template's `self` parameter is `StructRef("Container", [TypeVar(T)])`
//!    per `register_method`, and sema types `self.value` as just
//!    `Named("Container")` (no concrete args — `self` refers to the
//!    parametric form). `resolve_field_type` takes the no-op path at
//!    lowering time, and the `StructGetField` result type stays as
//!    `TypeVar("T")`. [`struct_mono::monomorphize_structs`] clones the
//!    template at each concrete instantiation and runs
//!    [`substitute_types_in_fn`] over the clone's `param_types` /
//!    `return_type` / per-value type index / block params —
//!    substituting `T → Int`, etc. — so the specialized body is
//!    fully concrete.
//!
//! Both paths funnel through the single [`substitute`] function, so
//! they share a common definition of how `TypeVar` is replaced inside
//! nested types. Removing either path would miscompile a real case:
//! the lowering-time path handles use-site field access on concrete
//! values, and the mono-time path handles the parametric receiver's
//! own body. They cover complementary, non-overlapping cases.
//!
//! ## Ordering: function-mono → struct-mono
//!
//! Struct-mono runs *after* function-mono inside [`monomorphize`].
//! Function-mono resolves every `Op::Call`'s `type_args` vector down to
//! the empty vector (rewriting the callee to a specialized `FuncId`);
//! struct-mono's call-rewriter depends on this invariant because it
//! only matches calls whose `targs.is_empty()`. A non-empty vector at
//! struct-mono entry would silently skip the rewrite and miscompile a
//! generic method call on a generic struct. This ordering is enforced
//! at runtime in debug builds by
//! [`struct_mono::monomorphize_structs`].

mod function_mono;
mod placeholder_resolution;
mod struct_mono;

use crate::instruction::{FuncId, Op};
use crate::module::{IrFunction, IrModule};
use crate::types::IrType;
use std::collections::HashMap;

/// A `(template_FuncId, concrete_type_args)` key, keyed by the template's
/// original `FuncId` and the concrete type-argument vector substituted in.
pub(super) type SpecKey = (FuncId, Vec<IrType>);

/// Map from `SpecKey` to the fresh `FuncId` assigned to that specialization.
pub(super) type SpecMap = HashMap<SpecKey, FuncId>;

/// Ordered record of `(template, targs, new_id)` triples in insertion
/// order, used by Pass B to clone template bodies in the same sequence.
pub(super) type SpecOrder = Vec<(FuncId, Vec<IrType>, FuncId)>;

/// Substitute every `IrType::TypeVar(name)` in `ty` using `subst`. Types
/// not referenced by the substitution map pass through unchanged.
///
/// Exposed `pub(crate)` so IR lowering can reuse the same substitution
/// rules at field-access sites (see
/// [`crate::lower_expr::LoweringContext::resolve_field_type`]); keeping
/// the single implementation prevents the lowering-time and
/// mono-time paths from drifting on edge cases like `ClosureRef` or
/// nested `EnumRef`s.
pub(crate) fn substitute(ty: &IrType, subst: &HashMap<String, IrType>) -> IrType {
    match ty {
        IrType::TypeVar(name) => subst.get(name).cloned().unwrap_or_else(|| ty.clone()),
        IrType::ListRef(inner) => IrType::ListRef(Box::new(substitute(inner, subst))),
        IrType::MapRef(k, v) => IrType::MapRef(
            Box::new(substitute(k, subst)),
            Box::new(substitute(v, subst)),
        ),
        IrType::ClosureRef {
            param_types,
            return_type,
        } => IrType::ClosureRef {
            param_types: param_types.iter().map(|t| substitute(t, subst)).collect(),
            return_type: Box::new(substitute(return_type, subst)),
        },
        IrType::EnumRef(name, args) => IrType::EnumRef(
            name.clone(),
            args.iter().map(|t| substitute(t, subst)).collect(),
        ),
        IrType::StructRef(name, args) => IrType::StructRef(
            name.clone(),
            args.iter().map(|t| substitute(t, subst)).collect(),
        ),
        // Value types, strings, trait objects: no inner types that can
        // contain TypeVar. Pass through. (DynRef carries only a trait
        // name, and trait-object generics on the trait itself are out of
        // scope — see docs/design-decisions.md multi-bound section.)
        IrType::I64
        | IrType::F64
        | IrType::Bool
        | IrType::Void
        | IrType::StringRef
        | IrType::DynRef(_) => ty.clone(),
    }
}

/// Apply `subst` to every type annotation inside `func`. Uses
/// [`IrFunction::for_each_type_mut`] so all four parallel type
/// annotations (params / return / block params / per-value index /
/// instruction result types) stay in sync.
pub(super) fn substitute_types_in_fn(func: &mut IrFunction, subst: &HashMap<String, IrType>) {
    func.for_each_type_mut(|ty| *ty = substitute(ty, subst));
}

/// Symbol-safe encoding of an [`IrType`] for name mangling. The output
/// matches `[A-Za-z0-9_]`, suitable for use directly as a Cranelift
/// symbol name fragment.
///
/// Grammar:
/// - `I64` → `i64`, `F64` → `f64`, `Bool` → `bool`, `Void` → `void`
/// - `StringRef` → `str`
/// - `StructRef(name, [])` → `s_{name}`
/// - `StructRef(name, [arg1, …, argN])` →
///   `s_{name}__{mangle(arg1)}__…__{mangle(argN)}_E`
/// - `EnumRef(name, [])` → `e_{name}`
/// - `EnumRef(name, [arg1, …, argN])` →
///   `e_{name}__{mangle(arg1)}__…__{mangle(argN)}_E`
/// - `ListRef(T)` → `L_{mangle(T)}_E`
/// - `MapRef(K, V)` → `M_{mangle(K)}_{mangle(V)}_E`
/// - `ClosureRef((P1, …, Pn) -> R)` → `C{n}_{mangle(P1)}_…_{mangle(Pn)}_{mangle(R)}_E`
/// - `TypeVar(name)` → `T_{name}` (must only appear in templates, never in specialized names)
///
/// The `_E` end-marker makes nested encodings unambiguous without
/// requiring a length prefix.
///
/// `EnumRef` uses `__` (double underscore) to delimit its name from its
/// first arg and to separate subsequent args, because Phoenix identifiers
/// forbid `__` (the same invariant [`mangle`] relies on). A single-`_`
/// separator would not be injective: `EnumRef("Opt", [StructRef("foo_i64")])`
/// and `EnumRef("Opt", [StructRef("foo"), I64])` would both produce
/// `e_Opt_s_foo_i64_E`. Other constructors (`List`/`Map`/`Closure`) avoid
/// this by having a fixed arity (or a leading arity prefix), but `EnumRef`
/// is variadic, so it needs a name/arg delimiter that cannot appear inside
/// either segment.
///
/// Examples:
/// - `EnumRef("Option", [])` → `e_Option`
/// - `EnumRef("Option", [I64])` → `e_Option__i64_E`
/// - `EnumRef("Result", [StringRef, I64])` → `e_Result__str__i64_E`
/// - `ListRef(EnumRef("Option", [I64]))` → `L_e_Option__i64_E_E`
pub(crate) fn mangle_type(ty: &IrType) -> String {
    match ty {
        IrType::I64 => "i64".to_string(),
        IrType::F64 => "f64".to_string(),
        IrType::Bool => "bool".to_string(),
        IrType::Void => "void".to_string(),
        IrType::StringRef => "str".to_string(),
        IrType::StructRef(name, args) => {
            if args.is_empty() {
                format!("s_{name}")
            } else {
                let mut s = format!("s_{name}");
                for a in args {
                    s.push_str("__");
                    s.push_str(&mangle_type(a));
                }
                s.push_str("_E");
                s
            }
        }
        IrType::EnumRef(name, args) => {
            if args.is_empty() {
                format!("e_{name}")
            } else {
                let mut s = format!("e_{name}");
                for a in args {
                    s.push_str("__");
                    s.push_str(&mangle_type(a));
                }
                s.push_str("_E");
                s
            }
        }
        IrType::DynRef(name) => format!("d_{name}"),
        IrType::ListRef(inner) => format!("L_{}_E", mangle_type(inner)),
        IrType::MapRef(k, v) => format!("M_{}_{}_E", mangle_type(k), mangle_type(v)),
        IrType::ClosureRef {
            param_types,
            return_type,
        } => {
            let mut s = format!("C{}", param_types.len());
            for p in param_types {
                s.push('_');
                s.push_str(&mangle_type(p));
            }
            s.push('_');
            s.push_str(&mangle_type(return_type));
            s.push_str("_E");
            s
        }
        IrType::TypeVar(name) => format!("T_{name}"),
    }
}

/// Mangle a specialized function name: `orig__i64` for a one-arg
/// specialization, `orig__i64__str` for two args, and so on. The
/// delimiter `__` cannot appear in a Phoenix identifier, so specialized
/// names are collision-free with user-defined function names. See
/// [`mangle_type`] for the per-argument encoding grammar.
pub(super) fn mangle(orig_name: &str, targs: &[IrType]) -> String {
    let mut s = orig_name.to_string();
    for t in targs {
        s.push_str("__");
        s.push_str(&mangle_type(t));
    }
    s
}

/// Run the monomorphization pass on `module`. After return, no function
/// body contains `IrType::TypeVar`, every generic call site has been
/// rewritten to its specialized target (and its `type_args` cleared), and
/// generic templates remain in `module.functions` as inert stubs.
pub(crate) fn monomorphize(module: &mut IrModule) {
    let seed = function_mono::collect_seed(module);
    if seed.is_empty() {
        // No generic calls to specialize, but non-generic functions may
        // still contain orphan `IrType::TypeVar` from unresolved sema
        // inference (e.g., `let x = []` with no list-element annotation).
        erase_type_vars_in_non_templates(module);
        // Generic structs can still need specialization even in programs
        // with no generic functions (`struct Box<T> { T v }` + plain
        // instantiations).  Run struct-mono unconditionally.
        struct_mono::monomorphize_structs(module);
        debug_assert_no_unresolved_placeholder_ops(module);
        return;
    }

    let (specialized, order) = function_mono::assign_specialization_ids(module, seed);
    function_mono::clone_and_substitute_bodies(module, &specialized, &order);
    function_mono::rewrite_root_call_sites(module, &specialized);
    erase_type_vars_in_non_templates(module);

    // Struct-monomorphization runs after function-monomorphization so the
    // concrete struct args appear in their final form in each
    // non-template function's types before we begin reifying layouts.
    struct_mono::monomorphize_structs(module);
    debug_assert_no_unresolved_placeholder_ops(module);
}

/// Post-mono invariant: every placeholder op emitted by IR lowering for
/// generic-context receivers / sources
/// ([`Op::UnresolvedTraitMethod`] for trait-bound method calls and
/// [`Op::UnresolvedDynAlloc`] for `dyn Trait` coercion from a generic
/// parameter) must have been rewritten by
/// [`function_mono::clone_and_substitute_bodies`].  Template bodies are
/// allowed to retain the placeholders — they exist only as inert stubs.
fn debug_assert_no_unresolved_placeholder_ops(module: &IrModule) {
    if !cfg!(debug_assertions) {
        return;
    }
    for func in module.concrete_functions() {
        for block in &func.blocks {
            for instr in &block.instructions {
                match &instr.op {
                    Op::UnresolvedTraitMethod(method, _, _) => panic!(
                        "post-mono invariant violated in `{}`: Op::UnresolvedTraitMethod \
                         `.{method}` survived monomorphization — \
                         resolve_trait_bound_method_calls failed to rewrite it",
                        func.name,
                    ),
                    Op::UnresolvedDynAlloc(trait_name, _) => panic!(
                        "post-mono invariant violated in `{}`: Op::UnresolvedDynAlloc \
                         `@{trait_name}` survived monomorphization — \
                         resolve_unresolved_dyn_allocs failed to rewrite it",
                        func.name,
                    ),
                    _ => {}
                }
            }
        }
    }
}

/// Pass D. Erase any residual `IrType::TypeVar` in non-template functions
/// (from unresolved sema inference) to the `GENERIC_PLACEHOLDER` sentinel.
/// Template bodies are left untouched — they remain as inert stubs.
fn erase_type_vars_in_non_templates(module: &mut IrModule) {
    for func in module.concrete_functions_mut() {
        erase_type_vars_in_fn(func);
    }
}

/// Walk `func` and erase every `IrType::TypeVar` (via
/// [`IrType::erase_type_vars`]).
fn erase_type_vars_in_fn(func: &mut IrFunction) {
    for pt in &mut func.param_types {
        *pt = pt.erase_type_vars();
    }
    func.return_type = func.return_type.erase_type_vars();
    for block in &mut func.blocks {
        for instr in &mut block.instructions {
            instr.result_type = instr.result_type.erase_type_vars();
        }
        for bp in &mut block.params {
            bp.1 = bp.1.erase_type_vars();
        }
    }
}

/// Returns `true` if `ty` contains an `IrType::TypeVar` at any depth.
pub(super) fn contains_type_var(ty: &IrType) -> bool {
    match ty {
        IrType::TypeVar(_) => true,
        IrType::ListRef(inner) => contains_type_var(inner),
        IrType::MapRef(k, v) => contains_type_var(k) || contains_type_var(v),
        IrType::ClosureRef {
            param_types,
            return_type,
        } => param_types.iter().any(contains_type_var) || contains_type_var(return_type),
        IrType::EnumRef(_, args) | IrType::StructRef(_, args) => args.iter().any(contains_type_var),
        IrType::I64
        | IrType::F64
        | IrType::Bool
        | IrType::Void
        | IrType::StringRef
        | IrType::DynRef(_) => false,
    }
}

/// Returns `true` if `ty` contains an `IrType::DynRef` at any depth.  Used
/// to enforce the MVP-scope rule that generic specialization never happens
/// at `dyn Trait`: sema is supposed to reject `foo<dyn Drawable>(...)` and
/// this guard catches any regression that lets it through to the IR.
pub(super) fn contains_dyn_ref(ty: &IrType) -> bool {
    match ty {
        IrType::DynRef(_) => true,
        IrType::ListRef(inner) => contains_dyn_ref(inner),
        IrType::MapRef(k, v) => contains_dyn_ref(k) || contains_dyn_ref(v),
        IrType::ClosureRef {
            param_types,
            return_type,
        } => param_types.iter().any(contains_dyn_ref) || contains_dyn_ref(return_type),
        IrType::EnumRef(_, args) | IrType::StructRef(_, args) => args.iter().any(contains_dyn_ref),
        IrType::I64
        | IrType::F64
        | IrType::Bool
        | IrType::Void
        | IrType::StringRef
        | IrType::TypeVar(_) => false,
    }
}

#[cfg(test)]
mod tests;
