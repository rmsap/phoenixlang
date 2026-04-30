//! Default-expression wrapper synthesis.
//!
//! For every (callee, parameter) pair where sema flagged the default
//! expression as non-trivial (i.e. not a pure literal), this pass
//! synthesizes a zero-arg wrapper function in the callee's module that
//! evaluates the default expression once. Caller-site lowering then
//! emits `Op::Call(wrapper_id, [], [])` instead of inlining the AST
//! expression — preventing private symbols referenced by the default
//! from leaking into a foreign caller's compiled output. See the
//! "Default-expression visibility across module boundaries" bug-closure
//! entry in `docs/phases/phase-2.md` (under §2.6) for the design
//! rationale and the failure modes the inline path produced.
//!
//! Runs as Pass 1.5 in both [`crate::lower::lower_modules`] and
//! [`crate::lower::lower_program`] — **after** Pass 1 has registered
//! every function stub (so the wrapper can refer to those stubs by
//! `FuncId`) and **before** Pass 2 lowers user-function bodies (so
//! every call site can consult `default_wrapper_index` and emit a
//! wrapper call instead of inlining the AST default expression).
//! Generic callees cannot have wrapper-needing defaults because
//! sema's `default_ty.has_type_vars()` rejection prevents them; the
//! pass enforces this with an explicit assertion so a future
//! relaxation announces itself loudly rather than silently
//! miscompiling.

use crate::instruction::{FuncId, ValueId};
use crate::lower::{LoweringContext, lower_type};
use crate::module::IrFunction;
use crate::terminator::Terminator;
use phoenix_common::module_path::ModulePath;
use phoenix_parser::ast::Expr;
use phoenix_sema::types::Type;

/// Walk every flagged `(callee_id, param_idx)` and synthesize a zero-arg
/// wrapper function. Records the result in
/// [`crate::module::IrModule::default_wrapper_index`].
///
/// Idempotent on empty inputs: with no flagged defaults, the
/// `default_wrapper_index` stays empty and no IR functions are
/// appended. Single-file programs whose defaults are all pure
/// literals therefore see no IR-shape change.
pub(crate) fn synthesize_default_wrappers(ctx: &mut LoweringContext<'_>) {
    // Snapshot the work list up front so we don't try to iterate
    // `ctx.check.functions` (immutable borrow) while mutating
    // `ctx.module` / `ctx.current_module` for each wrapper synthesis.
    let work = collect_wrapper_work(ctx);

    // Two-pass synthesis. Pass A registers every wrapper's stub and
    // its `default_wrapper_index` entry up front. Pass B then lowers
    // each wrapper's body. This ordering matters: a wrapper's body
    // can itself contain a call whose missing arg is filled by
    // *another* wrapper (e.g. `function g(y: Int = f())` where `f`'s
    // own default also needs wrapping). If we registered + lowered in
    // the same step, an entry registered in this same pass might not
    // yet exist in `default_wrapper_index` when an earlier wrapper's
    // body lowers a call referencing it — regardless of where each
    // ends up in the work list — and the call site would silently fall
    // back to inlining the AST default, leaking private symbols into
    // the wrapper's compiled output. Pass A guarantees every entry
    // exists before any body is lowered.
    let wrapper_ids: Vec<FuncId> = work.iter().map(|w| register_wrapper_stub(ctx, w)).collect();
    for (w, wrapper_id) in work.iter().zip(wrapper_ids) {
        lower_wrapper_body(ctx, w, wrapper_id);
    }
}

/// One wrapper to synthesize: which callee, which slot, and what to
/// lower into the wrapper's body.
struct WrapperJob {
    /// The callee function whose default is being wrapped.
    callee_id: FuncId,
    /// The parameter slot inside the callee whose default is wrapped
    /// (0-based, excluding `self` for methods — same convention as
    /// `default_param_exprs`).
    param_idx: usize,
    /// The callee's module — the wrapper is lowered in this module's
    /// scope so private symbols referenced by the default resolve
    /// the same way they do at the callee's declaration site.
    callee_module: ModulePath,
    /// The AST default expression cloned out of sema's table.
    default_expr: Expr,
    /// The default's resolved sema type, used to compute the
    /// wrapper's return type. We re-resolve from the param type
    /// rather than re-checking the default expression because sema
    /// already validated compatibility between them.
    return_sema_type: Type,
    /// Human-readable name for the wrapper, embedded into
    /// [`IrFunction::name`] for debug output and IR snapshots.
    /// Format for free functions:
    /// `__default_fn<FID>_<callee_name>_<param_idx>`. Format for
    /// methods: `__default_m<FID>_<Type>__<method>_<param_idx>`. The
    /// `fn{FID}`/`m{FID}` prefix disambiguates between forms and
    /// removes any chance of collision (e.g. between a function
    /// `Foo_bar` and a method `Foo.bar`).
    wrapper_name: String,
}

/// Generic callees cannot have wrapper-needing defaults: sema's
/// `default_ty.has_type_vars()` rejection prevents that shape from
/// reaching IR. If a future change relaxes that rejection, this
/// assertion will fire and the synthesis pass needs to be reordered
/// to run after monomorphization (see Phase 2.6 plan §4). Shared by
/// the function and method walks so both sites can't drift on the
/// message.
///
/// The boolean argument must mirror sema's full gate: a method is
/// generic if either its own `type_params` or its receiver's
/// `type_params` are non-empty (see
/// [`compute_default_needs_wrapper`][cdw] in sema). Passing only
/// `info.type_params.is_empty()` would leave methods on generic
/// receivers as a silent gap if sema's gate ever drifts.
///
/// [cdw]: phoenix_sema::check_register
fn assert_callee_non_generic(non_generic: bool, kind: &str, name: &str) {
    assert!(
        non_generic,
        "default-wrapper synthesis: generic {kind} `{name}` has wrapper-needing \
         defaults — sema's `default_ty.has_type_vars()` rejection should prevent this. \
         If a future change relaxes that rejection, reorder wrapper synthesis to run \
         after monomorphization (see Phase 2.6 plan §4)."
    );
}

/// Build the [`WrapperJob`] list by walking sema's function /
/// user-method tables.
fn collect_wrapper_work(ctx: &LoweringContext<'_>) -> Vec<WrapperJob> {
    let mut jobs: Vec<WrapperJob> = Vec::new();

    for (name, fid, info) in ctx.check.functions_with_names() {
        if info.default_needs_wrapper.is_empty() {
            continue;
        }
        assert_callee_non_generic(info.type_params.is_empty(), "function", name);
        for (slot_idx, return_sema_type) in info.params.iter().enumerate() {
            if !info.default_needs_wrapper.contains(&slot_idx) {
                continue;
            }
            let expr = info
                .default_param_exprs
                .get(&slot_idx)
                .cloned()
                .unwrap_or_else(|| {
                    panic!(
                        "default-wrapper synthesis: function `{name}` flagged param {slot_idx} \
                         as needing a wrapper but no default expression is recorded — \
                         sema's `default_needs_wrapper` and `default_param_exprs` are out of sync"
                    )
                });
            // Wrapper-name format: `__default_fn{FID}_{module-safe-callee}_{slot}`.
            // The leading `fn{FID}` segment guarantees uniqueness against the
            // method form below (which uses `m{FID}`) — without it a function
            // named `Foo_bar` and a method `Foo.bar` could collide on slot 0.
            // The `::`→`__` substitution is only for debug-readability of
            // the wrapper's `IrFunction.name` field; uniqueness is carried
            // by the `fn{FID}` prefix above, so any future name shape that
            // contains characters other than `::` (e.g. monomorphized
            // generics) is still collision-proof.
            let safe_name = name.replace("::", "__");
            jobs.push(WrapperJob {
                callee_id: fid,
                param_idx: slot_idx,
                callee_module: info.def_module.clone(),
                default_expr: expr,
                return_sema_type: return_sema_type.clone(),
                wrapper_name: format!("__default_fn{}_{safe_name}_{slot_idx}", fid.0),
            });
        }
    }

    for ((type_name, method_name), fid, info) in ctx.check.user_methods_with_names() {
        if info.default_needs_wrapper.is_empty() {
            continue;
        }
        let qualified = format!("{type_name}.{method_name}");
        // Methods don't expose `def_module` (or the receiver's
        // `type_params`) directly on `MethodInfo`, so derive both
        // from the receiver type. The receiver is registered in
        // either the struct or enum table under the qualified type
        // name; built-in receivers (`Option`, `Result`, ...) live in
        // `builtin_methods` and never reach `user_methods_with_names`,
        // so a missing entry here means a broken sema↔IR invariant —
        // panic loudly.
        let (callee_module, receiver_type_params) = receiver_metadata(ctx, type_name)
            .unwrap_or_else(|| {
                panic!(
                    "default-wrapper synthesis: method `{type_name}.{method_name}` has no \
                     registered receiver type — `user_methods_with_names` returned a method \
                     whose receiver is in neither `struct_by_name` nor `enum_by_name`. \
                     This breaks the sema invariant that every user method's parent type \
                     is registered."
                )
            });
        // Mirror sema's full generic gate: own type-params *and*
        // receiver type-params must both be empty. Checking only
        // `info.type_params` would let a method on a generic receiver
        // through if sema's gate ever drifts.
        assert_callee_non_generic(
            info.type_params.is_empty() && receiver_type_params.is_empty(),
            "method",
            &qualified,
        );
        for (slot_idx, return_sema_type) in info.params.iter().enumerate() {
            if !info.default_needs_wrapper.contains(&slot_idx) {
                continue;
            }
            let expr = info
                .default_param_exprs
                .get(&slot_idx)
                .cloned()
                .unwrap_or_else(|| {
                    panic!(
                        "default-wrapper synthesis: method `{type_name}.{method_name}` \
                         flagged param {slot_idx} as needing a wrapper but no default \
                         expression is recorded"
                    )
                });
            // Wrapper-name format: `__default_m{FID}_{module-safe-type}__{method}_{slot}`.
            // The leading `m{FID}` segment disambiguates from the function form
            // above (`fn{FID}`) — see the matching comment for the rationale.
            // The `::`→`__` substitution is debug-readability only; the
            // `m{FID}` prefix is what guarantees uniqueness even for
            // type names that contain characters other than `::`.
            let safe_type = type_name.replace("::", "__");
            jobs.push(WrapperJob {
                callee_id: fid,
                param_idx: slot_idx,
                callee_module: callee_module.clone(),
                default_expr: expr,
                return_sema_type: return_sema_type.clone(),
                wrapper_name: format!("__default_m{}_{safe_type}__{method_name}_{slot_idx}", fid.0),
            });
        }
    }

    jobs
}

/// Resolve the qualified type-name to its declaring module *and* its
/// own generic type-parameters by probing sema's struct and enum
/// tables. Both are needed for method wrapper synthesis: the module
/// determines where the wrapper lowers, and the type-params feed the
/// generic-callee assertion (a method on a generic receiver must not
/// reach the wrapper path even if its own `type_params` is empty).
///
/// Returns `None` for built-in receivers — but those can't reach this
/// path because user methods on built-in receivers (`Option.unwrap`,
/// etc.) live in `builtin_methods`, which carries no `FuncId` and is
/// never enrolled in `pending_user_method_ids`.
fn receiver_metadata(
    ctx: &LoweringContext<'_>,
    qualified_type: &str,
) -> Option<(ModulePath, Vec<String>)> {
    ctx.check
        .struct_info_by_name(qualified_type)
        .map(|s| (s.def_module.clone(), s.type_params.clone()))
        .or_else(|| {
            ctx.check
                .enum_info_by_name(qualified_type)
                .map(|e| (e.def_module.clone(), e.type_params.clone()))
        })
}

/// Pass A: append the wrapper's `IrFunction` stub and record the
/// `(callee_id, param_idx) → wrapper_id` mapping in
/// `default_wrapper_index`. Body lowering happens later in
/// [`lower_wrapper_body`] so every wrapper is index-visible before any
/// body lowers.
fn register_wrapper_stub(ctx: &mut LoweringContext<'_>, job: &WrapperJob) -> FuncId {
    // Resolve the return type under the callee's module — types
    // resolve the same way they do in any of that module's own
    // functions.
    let return_ir_type = lower_type(&job.return_sema_type, ctx.check);

    // Build the wrapper IrFunction stub. Zero parameters; return type
    // matches the param's declared type. Span points at the default
    // expression so any downstream verifier / codegen error has a
    // user-meaningful source location instead of `None`.
    let stub = IrFunction::new(
        FuncId(u32::MAX), // Filled in by `push_concrete`.
        job.wrapper_name.clone(),
        Vec::new(),
        Vec::new(),
        return_ir_type,
        Some(job.default_expr.span()),
    );
    let wrapper_id = ctx.module.push_concrete(stub);
    // Duplicate `(callee_id, param_idx)` insertion would leak the first
    // wrapper as an unreachable function in `module.functions`. Sema's
    // `default_needs_wrapper` set is keyed by slot, so a duplicate here
    // means sema produced two entries for the same callee — surface
    // that as a hard panic, not a silent overwrite.
    use std::collections::hash_map::Entry;
    match ctx
        .module
        .default_wrapper_index
        .entry((job.callee_id, job.param_idx))
    {
        Entry::Vacant(slot) => {
            slot.insert(wrapper_id);
        }
        Entry::Occupied(_) => panic!(
            "default-wrapper synthesis: duplicate entry for (callee {:?}, slot {}) — \
             sema's `default_needs_wrapper` flagged the same slot twice",
            job.callee_id, job.param_idx,
        ),
    }
    wrapper_id
}

/// Pass B: lower the default expression into the wrapper's body and
/// terminate with `return val`. Assumes [`register_wrapper_stub`] has
/// already appended the stub at `wrapper_id` and recorded the index
/// entry.
fn lower_wrapper_body(ctx: &mut LoweringContext<'_>, job: &WrapperJob, wrapper_id: FuncId) {
    // Reuse the return type already lowered by `register_wrapper_stub`
    // and stored on the wrapper's `IrFunction` — keeps one canonical
    // source for "what type the wrapper returns" and removes the
    // chance of the stub and body disagreeing if `lower_type`'s output
    // ever becomes context-sensitive.
    let return_ir_type = ctx.module.functions[wrapper_id.index()]
        .func()
        .return_type
        .clone();

    with_synthetic_function(ctx, wrapper_id, job.callee_module.clone(), |ctx| {
        let entry = ctx.create_block();
        ctx.switch_to_block(entry);

        // Lower the default expression into a single value, then return.
        let val: ValueId = ctx.lower_expr(&job.default_expr);
        let val = ctx.coerce_value_to_expected(val, &return_ir_type, job.default_expr.span());
        ctx.terminate(Terminator::Return(Some(val)));
    });
}

/// Run `body` with `LoweringContext` switched into the lowering state
/// for a synthetic function (`func_id` in `module`'s scope), restoring
/// the prior state on return.
///
/// Synthesize-default runs between Pass 1 (registration) and Pass 2
/// (user body lowering), so per-function lowering state may already
/// hold values from prior setup. Centralizing the save / mutate /
/// restore dance here means adding a new per-function field to
/// `LoweringContext` only requires updating this one site, not every
/// synthesis caller.
///
/// `closure_counter` is deliberately *not* part of the snapshot: it
/// only needs to be globally unique within the module, so any closures
/// created inside `body` simply consume counter values that the user-
/// body pass then continues from. Resetting it would cause name
/// collisions.
fn with_synthetic_function<R>(
    ctx: &mut LoweringContext<'_>,
    func_id: FuncId,
    module: ModulePath,
    body: impl FnOnce(&mut LoweringContext<'_>) -> R,
) -> R {
    let saved_func_id = ctx.current_func_id;
    let saved_block = ctx.current_block;
    let saved_scopes = std::mem::take(&mut ctx.var_scopes);
    let saved_loops = std::mem::take(&mut ctx.loop_stack);
    let saved_module = std::mem::replace(&mut ctx.current_module, module);

    ctx.current_func_id = Some(func_id);
    ctx.push_scope();

    let result = body(ctx);

    ctx.pop_scope();
    ctx.var_scopes = saved_scopes;
    ctx.loop_stack = saved_loops;
    ctx.current_func_id = saved_func_id;
    ctx.current_block = saved_block;
    ctx.current_module = saved_module;

    result
}
