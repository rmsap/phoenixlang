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

use crate::instruction::{FuncId, Op};
use crate::module::{IrFunction, IrModule};
use crate::types::IrType;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};

/// A `(template_FuncId, concrete_type_args)` key, keyed by the template's
/// original `FuncId` and the concrete type-argument vector substituted in.
type SpecKey = (FuncId, Vec<IrType>);

/// Map from `SpecKey` to the fresh `FuncId` assigned to that specialization.
type SpecMap = HashMap<SpecKey, FuncId>;

/// Ordered record of `(template, targs, new_id)` triples in insertion
/// order, used by Pass B to clone template bodies in the same sequence.
type SpecOrder = Vec<(FuncId, Vec<IrType>, FuncId)>;

/// Substitute every `IrType::TypeVar(name)` in `ty` using `subst`. Types
/// not referenced by the substitution map pass through unchanged.
fn substitute(ty: &IrType, subst: &HashMap<String, IrType>) -> IrType {
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
        // Value types, strings, struct refs, trait objects: no inner types
        // that can contain TypeVar. Pass through. (DynRef carries only a
        // trait name, and trait-object generics on the trait itself are out
        // of scope — see docs/design-decisions.md multi-bound section.)
        IrType::I64
        | IrType::F64
        | IrType::Bool
        | IrType::Void
        | IrType::StringRef
        | IrType::StructRef(_)
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
        IrType::StructRef(name) => format!("s_{name}"),
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
        return;
    }

    let (specialized, order) = assign_specialization_ids(module, seed);
    clone_and_substitute_bodies(module, &specialized, &order);
    rewrite_root_call_sites(module, &specialized);
    erase_type_vars_in_non_templates(module);
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
        IrType::EnumRef(_, args) => args.iter().any(contains_type_var),
        IrType::I64
        | IrType::F64
        | IrType::Bool
        | IrType::Void
        | IrType::StringRef
        | IrType::StructRef(_)
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
        IrType::EnumRef(_, args) => args.iter().any(contains_dyn_ref),
        IrType::I64
        | IrType::F64
        | IrType::Bool
        | IrType::Void
        | IrType::StringRef
        | IrType::StructRef(_)
        | IrType::TypeVar(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BasicBlock, BlockId};
    use crate::instruction::{Instruction, ValueId};
    use crate::terminator::Terminator;

    // ── Test builders ──────────────────────────────────────────────
    //
    // These builders exist so each test shows its *intent* — a function
    // with these type params, these params, this return type, these
    // instructions — not seven lines of struct construction per call.

    /// Shorthand for `IrType::TypeVar(name.into())`.
    fn tv(name: &str) -> IrType {
        IrType::TypeVar(name.into())
    }

    /// A generic `Op::Call` with no value arguments (sufficient for
    /// monomorphization tests, which only care about the callee and
    /// embedded type_args).
    fn gcall(callee: u32, type_args: Vec<IrType>) -> Op {
        Op::Call(FuncId(callee), type_args, vec![])
    }

    /// Fluent builder for an `IrFunction` with a single entry block.
    /// Non-empty `type_params` flip `is_generic_template` automatically.
    struct FnBuilder {
        func: IrFunction,
        instrs: Vec<Instruction>,
    }

    impl FnBuilder {
        fn new(id: u32, name: &str) -> Self {
            Self {
                func: IrFunction::new(
                    FuncId(id),
                    name.to_string(),
                    Vec::new(),
                    Vec::new(),
                    IrType::Void,
                    None,
                ),
                instrs: Vec::new(),
            }
        }

        fn generic(mut self, names: &[&str]) -> Self {
            self.func.type_param_names = names.iter().map(|s| (*s).to_string()).collect();
            self.func.is_generic_template = !names.is_empty();
            self
        }

        fn params(mut self, types: Vec<IrType>) -> Self {
            self.func.param_types = types;
            self
        }

        fn ret(mut self, ty: IrType) -> Self {
            self.func.return_type = ty;
            self
        }

        fn instr(mut self, op: Op, result_type: IrType) -> Self {
            self.instrs.push(Instruction {
                result: Some(ValueId(0)),
                op,
                result_type,
                span: None,
            });
            self
        }

        fn build(mut self) -> IrFunction {
            self.func.blocks.push(BasicBlock {
                id: BlockId(0),
                params: vec![],
                instructions: self.instrs,
                terminator: Terminator::Return(None),
            });
            self.func
        }
    }

    /// Build a module from a list of functions, registering each in
    /// `function_index` automatically.
    fn module_of(funcs: Vec<IrFunction>) -> IrModule {
        let mut m = IrModule::new();
        for f in funcs {
            m.function_index.insert(f.name.clone(), f.id);
            m.functions.push(f);
        }
        m
    }

    /// Look up a function by name and return a reference.
    fn lookup<'a>(m: &'a IrModule, name: &str) -> &'a IrFunction {
        &m.functions[m.function_index[name].0 as usize]
    }

    /// Destructure the `Op::Call` at `(block, instr)` within `func`,
    /// asserting it is in fact a direct call.
    fn call_at(func: &IrFunction, block: usize, instr: usize) -> (FuncId, &[IrType]) {
        match &func.blocks[block].instructions[instr].op {
            Op::Call(callee, targs, _) => (*callee, targs.as_slice()),
            other => panic!("expected Op::Call at [{block}][{instr}], got {other:?}"),
        }
    }

    // ── Tests ──────────────────────────────────────────────────────

    #[test]
    fn specializes_identity_at_int_and_string() {
        // identity<T>(x: T) -> T, called at Int and String from main.
        let mut module = module_of(vec![
            FnBuilder::new(0, "identity")
                .generic(&["T"])
                .params(vec![tv("T")])
                .ret(tv("T"))
                .build(),
            FnBuilder::new(1, "main")
                .instr(gcall(0, vec![IrType::I64]), IrType::I64)
                .instr(gcall(0, vec![IrType::StringRef]), IrType::StringRef)
                .build(),
        ]);

        monomorphize(&mut module);

        let int_spec = lookup(&module, "identity__i64");
        assert!(!int_spec.is_generic_template);
        assert_eq!(int_spec.param_types, vec![IrType::I64]);
        assert_eq!(int_spec.return_type, IrType::I64);

        let str_spec = lookup(&module, "identity__str");
        assert_eq!(str_spec.param_types, vec![IrType::StringRef]);

        // Template preserved as inert stub at FuncId(0).
        assert!(module.functions[0].is_generic_template);

        // Call sites rewritten: targets point at specializations,
        // type_args are cleared.
        let main = lookup(&module, "main");
        for (i, expected_name) in ["identity__i64", "identity__str"].iter().enumerate() {
            let (callee, targs) = call_at(main, 0, i);
            assert_eq!(callee, module.function_index[*expected_name]);
            assert!(targs.is_empty(), "specialized call kept residual type_args");
        }
    }

    #[test]
    fn specializes_multi_param_function() {
        // first<A, B>(a: A, b: B) -> A
        let mut module = module_of(vec![
            FnBuilder::new(0, "first")
                .generic(&["A", "B"])
                .params(vec![tv("A"), tv("B")])
                .ret(tv("A"))
                .build(),
            FnBuilder::new(1, "main")
                .instr(gcall(0, vec![IrType::I64, IrType::StringRef]), IrType::I64)
                .build(),
        ]);

        monomorphize(&mut module);

        let spec = lookup(&module, "first__i64__str");
        assert_eq!(spec.param_types, vec![IrType::I64, IrType::StringRef]);
        assert_eq!(spec.return_type, IrType::I64);
    }

    #[test]
    fn recursion_through_generics_preserves_specialization() {
        // count<T>(x: T) -> Void { count(x) }  (self-call must stay specialized)
        let mut module = module_of(vec![
            FnBuilder::new(0, "count")
                .generic(&["T"])
                .params(vec![tv("T")])
                .instr(gcall(0, vec![tv("T")]), IrType::Void)
                .build(),
            FnBuilder::new(1, "main")
                .instr(gcall(0, vec![IrType::I64]), IrType::Void)
                .build(),
        ]);

        monomorphize(&mut module);

        let count_int_id = module.function_index["count__i64"];
        let (inner_callee, inner_targs) = call_at(lookup(&module, "count__i64"), 0, 0);
        assert_eq!(inner_callee, count_int_id);
        assert!(inner_targs.is_empty());
    }

    #[test]
    fn uninstantiated_template_leaves_module_unchanged_up_to_erasure() {
        let mut module = module_of(vec![
            FnBuilder::new(0, "unused")
                .generic(&["T"])
                .params(vec![tv("T")])
                .ret(tv("T"))
                .build(),
        ]);

        monomorphize(&mut module);

        assert_eq!(module.functions.len(), 1);
        assert!(module.functions[0].is_generic_template);
        assert!(module.concrete_functions().next().is_none());
    }

    #[test]
    fn mangling_is_symbol_safe_for_reference_types() {
        // Mangled names must match [A-Za-z0-9_]: no angle brackets,
        // commas, parens, spaces, or arrows — even when the type arg is
        // a compound reference type like List / Map / closure.
        let cases: Vec<IrType> = vec![
            IrType::ListRef(Box::new(IrType::I64)),
            IrType::MapRef(Box::new(IrType::StringRef), Box::new(IrType::I64)),
            IrType::ClosureRef {
                param_types: vec![IrType::I64, IrType::Bool],
                return_type: Box::new(IrType::StringRef),
            },
            IrType::StructRef("Point".into()),
            IrType::EnumRef("Option".into(), Vec::new()),
            IrType::EnumRef("Option".into(), vec![IrType::I64]),
            IrType::EnumRef("Result".into(), vec![IrType::StringRef, IrType::I64]),
        ];
        for ty in cases {
            let name = mangle("fn", std::slice::from_ref(&ty));
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "mangled name `{name}` (from {ty:?}) contains non-symbol-safe chars"
            );
        }
    }

    /// Lock in the exact mangle grammar for `EnumRef` with args. Injectivity
    /// is covered separately; this test guards against silent reformatting
    /// (e.g. changing the `_E` terminator or the per-arg separator) that
    /// would change the Cranelift symbol names Phoenix binaries are linked
    /// against.
    #[test]
    fn mangles_enum_ref_with_args_verbatim() {
        assert_eq!(
            mangle_type(&IrType::EnumRef("Option".into(), Vec::new())),
            "e_Option"
        );
        assert_eq!(
            mangle_type(&IrType::EnumRef("Option".into(), vec![IrType::I64])),
            "e_Option__i64_E"
        );
        assert_eq!(
            mangle_type(&IrType::EnumRef(
                "Result".into(),
                vec![IrType::StringRef, IrType::I64]
            )),
            "e_Result__str__i64_E"
        );
        // Nesting is unambiguous thanks to the `_E` terminator.
        assert_eq!(
            mangle_type(&IrType::ListRef(Box::new(IrType::EnumRef(
                "Option".into(),
                vec![IrType::I64]
            )))),
            "L_e_Option__i64_E_E"
        );
    }

    /// Regression guard for the name/arg delimiter ambiguity: with a single-
    /// underscore separator, `EnumRef("Opt", [StructRef("foo_i64")])` and
    /// `EnumRef("Opt", [StructRef("foo"), I64])` would both mangle to
    /// `e_Opt_s_foo_i64_E`. The `__` delimiter splits these cleanly because
    /// Phoenix identifiers forbid `__`, so the boundary between name and
    /// first arg (and between adjacent args) is unambiguous.
    #[test]
    fn enum_ref_mangle_is_injective_under_underscore_in_arg_names() {
        let a = IrType::EnumRef("Opt".into(), vec![IrType::StructRef("foo_i64".into())]);
        let b = IrType::EnumRef(
            "Opt".into(),
            vec![IrType::StructRef("foo".into()), IrType::I64],
        );
        assert_ne!(mangle_type(&a), mangle_type(&b));
        assert_eq!(mangle_type(&a), "e_Opt__s_foo_i64_E");
        assert_eq!(mangle_type(&b), "e_Opt__s_foo__i64_E");
    }

    #[test]
    fn specializes_at_reference_type_list_of_int() {
        let list_i64 = IrType::ListRef(Box::new(IrType::I64));
        let mut module = module_of(vec![
            FnBuilder::new(0, "wrap")
                .generic(&["T"])
                .params(vec![tv("T")])
                .ret(tv("T"))
                .build(),
            FnBuilder::new(1, "main")
                .instr(gcall(0, vec![list_i64.clone()]), list_i64.clone())
                .build(),
        ]);

        monomorphize(&mut module);

        let spec = lookup(&module, "wrap__L_i64_E");
        assert_eq!(spec.param_types, vec![list_i64.clone()]);
        assert_eq!(spec.return_type, list_i64);
    }

    #[test]
    fn empty_template_body_does_not_panic() {
        // Pass A must handle zero-instruction blocks without panicking.
        let mut module = module_of(vec![
            FnBuilder::new(0, "noop")
                .generic(&["T"])
                .params(vec![tv("T")])
                .build(),
            FnBuilder::new(1, "main")
                .instr(gcall(0, vec![IrType::I64]), IrType::Void)
                .build(),
        ]);

        monomorphize(&mut module);
        assert!(module.function_index.contains_key("noop__i64"));
    }

    /// `substitute` must recurse into `EnumRef.args` so a `TypeVar` inside
    /// an `Option<T>` or `Result<T, E>` position is replaced when the
    /// template is specialized. Without this the backend would see an
    /// unsubstituted `TypeVar` at a reference-type use site, which has no
    /// Cranelift lowering.
    #[test]
    fn substitute_recurses_into_enum_ref_args() {
        let mut subst = HashMap::new();
        subst.insert("T".to_string(), IrType::I64);
        subst.insert("E".to_string(), IrType::StringRef);

        let ty = IrType::EnumRef("Result".into(), vec![tv("T"), tv("E")]);
        assert_eq!(
            substitute(&ty, &subst),
            IrType::EnumRef("Result".into(), vec![IrType::I64, IrType::StringRef])
        );

        // Nested: Option<List<T>>
        let nested = IrType::EnumRef("Option".into(), vec![IrType::ListRef(Box::new(tv("T")))]);
        assert_eq!(
            substitute(&nested, &subst),
            IrType::EnumRef(
                "Option".into(),
                vec![IrType::ListRef(Box::new(IrType::I64))]
            )
        );

        // Empty-args EnumRef is untouched (no TypeVars to substitute).
        let bare = IrType::EnumRef("Color".into(), Vec::new());
        assert_eq!(substitute(&bare, &subst), bare);
    }

    /// `contains_type_var` must recurse into every compound type constructor
    /// — a missing arm would let an orphan `TypeVar` slip past the Pass A
    /// `debug_assert` guard. Regression test for a prior miss on `EnumRef`.
    #[test]
    fn contains_type_var_recurses_into_every_compound() {
        assert!(contains_type_var(&tv("T")));
        assert!(contains_type_var(&IrType::ListRef(Box::new(tv("T")))));
        assert!(contains_type_var(&IrType::MapRef(
            Box::new(IrType::I64),
            Box::new(tv("V"))
        )));
        assert!(contains_type_var(&IrType::ClosureRef {
            param_types: vec![IrType::I64, tv("P")],
            return_type: Box::new(IrType::Void),
        }));
        assert!(contains_type_var(&IrType::EnumRef(
            "Option".into(),
            vec![tv("T")]
        )));
        // Deeply nested: Option<List<T>>.
        assert!(contains_type_var(&IrType::EnumRef(
            "Option".into(),
            vec![IrType::ListRef(Box::new(tv("T")))]
        )));

        // Atomic and concrete compound types report false.
        assert!(!contains_type_var(&IrType::I64));
        assert!(!contains_type_var(&IrType::StructRef("Point".into())));
        assert!(!contains_type_var(&IrType::EnumRef(
            "Result".into(),
            vec![IrType::I64, IrType::StringRef]
        )));
        assert!(!contains_type_var(&IrType::EnumRef("Color".into(), vec![])));
    }

    #[test]
    fn residual_type_var_erased_to_placeholder_when_no_specializations() {
        // Non-template function with an orphan TypeVar (e.g., empty list
        // literal with unresolved element type) has it erased to
        // GENERIC_PLACEHOLDER even when no monomorphization is needed.
        let mut module = module_of(vec![
            FnBuilder::new(0, "main")
                .instr(Op::ListAlloc(vec![]), IrType::ListRef(Box::new(tv("U"))))
                .build(),
        ]);

        monomorphize(&mut module);

        let instr = &module.functions[0].blocks[0].instructions[0];
        assert_eq!(
            instr.result_type,
            IrType::ListRef(Box::new(IrType::StructRef(
                crate::types::GENERIC_PLACEHOLDER.to_string()
            )))
        );
    }

    /// An orphan `TypeVar` inside an `EnumRef` arg (e.g. `Option<T>` where
    /// `T` was never bound) must be erased to `GENERIC_PLACEHOLDER` by
    /// Pass D, matching the treatment of orphan TypeVars in `ListRef` /
    /// `MapRef` positions. Without this, the backend would hit an
    /// unsubstituted `TypeVar` at a reference-type use site.
    #[test]
    fn residual_type_var_in_enum_ref_args_erased_by_pass_d() {
        let mut module = module_of(vec![
            FnBuilder::new(0, "main")
                .instr(
                    Op::ListAlloc(vec![]),
                    IrType::EnumRef("Option".into(), vec![tv("T")]),
                )
                .build(),
        ]);

        monomorphize(&mut module);

        let instr = &module.functions[0].blocks[0].instructions[0];
        assert_eq!(
            instr.result_type,
            IrType::EnumRef(
                "Option".into(),
                vec![IrType::StructRef(
                    crate::types::GENERIC_PLACEHOLDER.to_string()
                )]
            )
        );
    }
}
