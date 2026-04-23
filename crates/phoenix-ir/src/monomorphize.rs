//! Monomorphization pass for generic functions.
//!
//! Collects every concrete instantiation of every generic function from
//! call sites embedded in the IR (the middle `Vec<IrType>` of
//! [`Op::Call`]), emits one specialized [`IrFunction`] per
//! `(template, concrete_type_args)` pair, and rewrites the call sites to
//! point at the specialized functions.
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
//!    occurrences with concrete types, and rewrite internal generic
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
//! Generic templates are kept in `module.functions` as inert stubs
//! (`is_generic_template = true`) to preserve the `FuncId`-as-vector-index
//! invariant relied on across the codebase. Every downstream pass should
//! iterate via [`crate::module::IrModule::concrete_functions`] to skip them.
//!
//! ## Integration with earlier passes
//!
//! The concrete type arguments this pass consumes are populated by sema's
//! `Checker::record_inferred_type_args` (in `phoenix-sema/src/check_expr_call.rs`)
//! into `CheckResult.call_type_args`, keyed by each call expression's
//! source span. IR lowering (`crate::lower_expr`) looks them up at call
//! sites via `LowerContext::resolve_call_type_args` and embeds them into
//! the middle slot of `Op::Call`. This pass reads that middle slot, rewrites
//! the call to target a specialized `FuncId`, and clears the slot.
//!
//! ## Determinism
//!
//! FuncId assignment is stable across builds. The seed set is sorted by
//! `(caller_FuncId, block_idx, instr_idx)` before being drained as a
//! VecDeque BFS (see `collect_seed`), and new FuncIds are assigned via
//! `FuncId(base_id + specialized.len() as u32)` at insertion time — so
//! the only thing that can perturb the assignment is the insertion order,
//! which is deterministic. The `SpecMap` `HashMap` is only ever consulted
//! via direct lookup (never iterated for output), so its non-deterministic
//! iteration order does not leak into the generated IR.
//!
//! ## MVP scope (deliberate omissions)
//!
//! - Only direct calls (`Op::Call`) are specialized; closures over
//!   generic parameters and first-class generic function values are not.
//! - Cross-module instantiation is not handled (single-module compilation).
//! - Trait-bounded method-call specialization is not implemented.
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
//!    body would carry `TypeVar` in its `value_types` index — a
//!    verifier violation.
//!
//! 2. **At struct-mono time**, inside a *template* method body. The
//!    template's `self` parameter is `StructRef("Container", [TypeVar(T)])`
//!    per `register_method`, and sema types `self.value` as just
//!    `Named("Container")` (no concrete args — `self` refers to the
//!    parametric form). `resolve_field_type` takes the no-op path at
//!    lowering time, and the `StructGetField` result type stays as
//!    `TypeVar("T")`. [`specialize_layouts_and_methods`] clones the
//!    template at each concrete instantiation and runs
//!    [`substitute_types_in_fn`] over the clone's
//!    `param_types` / `return_type` / `value_types` / block params —
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
//! at runtime in debug builds by [`debug_assert_no_pending_generic_calls`].

use crate::instruction::{FuncId, Op};
use crate::module::{IrFunction, IrModule};
use crate::types::IrType;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

/// A `(template_FuncId, concrete_type_args)` key, keyed by the template's
/// original `FuncId` and the concrete type-argument vector substituted in.
type SpecKey = (FuncId, Vec<IrType>);

/// Map from `SpecKey` to the fresh `FuncId` assigned to that specialization.
type SpecMap = HashMap<SpecKey, FuncId>;

/// Ordered record of `(template, targs, new_id)` triples in insertion
/// order, used by Pass B to clone template bodies in the same sequence.
type SpecOrder = Vec<(FuncId, Vec<IrType>, FuncId)>;

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
fn substitute_types_in_fn(func: &mut IrFunction, subst: &HashMap<String, IrType>) {
    func.for_each_type_mut(|ty| *ty = substitute(ty, subst));
}

/// Symbol-safe encoding of an [`IrType`] for name mangling. The output
/// matches `[A-Za-z0-9_]`, suitable for use directly as a Cranelift
/// symbol name fragment.
///
/// Grammar (recursive):
/// - `i64`, `f64`, `bool`, `void`, `string` → `i64`, `f64`, `bool`, `void`, `str`
/// - `StructRef(name)` → `s_{name}` (names are Phoenix identifiers ⊂ `[A-Za-z0-9_]`)
/// - `EnumRef(name, args)` → `e_{name}` when args is empty, else
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
fn mangle(orig_name: &str, targs: &[IrType]) -> String {
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
    let seed = collect_seed(module);
    if seed.is_empty() {
        // No generic calls to specialize, but non-generic functions may
        // still contain orphan `IrType::TypeVar` from unresolved sema
        // inference (e.g., `let x = []` with no list-element annotation).
        erase_type_vars_in_non_templates(module);
        // Generic structs can still need specialization even in programs
        // with no generic functions (`struct Box<T> { T v }` + plain
        // instantiations).  Run struct-mono unconditionally.
        monomorphize_structs(module);
        return;
    }

    let (specialized, order) = assign_specialization_ids(module, seed);
    clone_and_substitute_bodies(module, &specialized, &order);
    rewrite_root_call_sites(module, &specialized);
    erase_type_vars_in_non_templates(module);

    // Struct-monomorphization runs after function-monomorphization so the
    // concrete struct args appear in their final form in each
    // non-template function's types before we begin reifying layouts.
    monomorphize_structs(module);
}

/// Second-stage monomorphization: walks every concrete function's IR
/// types looking for `StructRef(name, non_empty_args)` where `name`
/// identifies a generic-struct template, registers a per-instantiation
/// struct layout under a mangled name, clones every method on the
/// template into a parallel specialized method, and rewrites every
/// reference (StructRef, StructAlloc, DynAlloc, method-dispatch Call) to
/// the mangled form.  Post-mono, no concrete function contains a
/// `StructRef` with non-empty args, and the Cranelift backend continues
/// to look up layouts and methods by bare string.
///
/// Runs as a fixed-point worklist so recursive generic types
/// (`Node<T> { T val, Option<Node<T>> next }`) and
/// nested instantiations (`Container<List<Int>>`) both converge —
/// each newly-specialized layout / method body is re-scanned for
/// further generic struct uses and enqueued.
///
/// Unlike the generic-function pass above, struct-mono does not need a
/// separate FuncId-assignment phase: the `rename_map` built during the
/// worklist tells the rewrite pass exactly which bare-name StructRef
/// maps to which mangled name, and specialized method FuncIds are
/// inserted into `method_index` as they're created.
fn monomorphize_structs(module: &mut IrModule) {
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
        let mut new_fn = module.functions[template_fid.0 as usize].clone();
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
                    let callee_name = &module.functions[callee_fid.0 as usize].name;
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

/// Collect the BFS seed: every `(caller, block, instr, callee, type_args)`
/// for generic calls in non-template functions. Sorted for determinism so
/// that FuncId assignment order is reproducible across builds.
fn collect_seed(module: &IrModule) -> Vec<SpecKey> {
    /// `(caller, block, instr)` position key for deterministic ordering.
    type Pos = (FuncId, u32, u32);
    let mut seed: Vec<(Pos, SpecKey)> = Vec::new();
    for caller in &module.functions {
        if caller.is_generic_template {
            continue;
        }
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
fn assign_specialization_ids(module: &IrModule, seed: Vec<SpecKey>) -> (SpecMap, SpecOrder) {
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
        let orig = &module.functions[orig_id.0 as usize];
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
fn clone_and_substitute_bodies(module: &mut IrModule, specialized: &SpecMap, order: &SpecOrder) {
    // Collect specialized functions before pushing to preserve `orig_id`
    // indexing into `module.functions` (which must not grow during the loop).
    let mut new_funcs: Vec<IrFunction> = Vec::with_capacity(order.len());

    for (orig_id, targs, new_id) in order {
        let orig = &module.functions[orig_id.0 as usize];
        let subst: HashMap<String, IrType> = orig
            .type_param_names
            .iter()
            .cloned()
            .zip(targs.iter().cloned())
            .collect();

        let mut spec_fn = orig.clone();
        spec_fn.id = *new_id;
        spec_fn.name = mangle(&orig.name, targs);
        spec_fn.type_param_names = Vec::new();
        spec_fn.is_generic_template = false;
        substitute_types_in_fn(&mut spec_fn, &subst);

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
        module.function_index.insert(spec.name.clone(), spec.id);
        module.functions.push(spec);
    }
}

/// Pass C. Rewrite every generic `Op::Call` in non-template callers to
/// point at the specialized `FuncId` and clear its `type_args`.
fn rewrite_root_call_sites(module: &mut IrModule, specialized: &SpecMap) {
    for func in module.functions.iter_mut() {
        if func.is_generic_template {
            continue;
        }
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

/// Pass D. Erase any residual `IrType::TypeVar` in non-template functions
/// (from unresolved sema inference) to the `GENERIC_PLACEHOLDER` sentinel.
/// Template bodies are left untouched — they remain as inert stubs.
fn erase_type_vars_in_non_templates(module: &mut IrModule) {
    for func in &mut module.functions {
        if !func.is_generic_template {
            erase_type_vars_in_fn(func);
        }
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
fn contains_type_var(ty: &IrType) -> bool {
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
fn contains_dyn_ref(ty: &IrType) -> bool {
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
