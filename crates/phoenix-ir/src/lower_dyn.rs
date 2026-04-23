//! Dyn-coercion lowering: `Op::DynAlloc` wrapping at assignment
//! boundaries (function-call args, returns, `let` annotations, struct
//! field inits) and on-demand vtable registration, plus the
//! receiver-is-`dyn` method-call lowering to [`Op::DynCall`].
//!
//! The sema side records compatibility (`dyn Trait` accepts concrete `T`
//! if `T: Trait`); the IR side has to materialize the `(data_ptr,
//! vtable_ptr)` pair and ensure the vtable for `(T, Trait)` is in the
//! module before the Cranelift backend tries to emit it.

use crate::instruction::{FuncId, Op, ValueId};
use crate::lower::LoweringContext;
use crate::types::IrType;
use phoenix_common::span::Span;

impl<'a> LoweringContext<'a> {
    /// Coerce `value` (current IR type `actual`) to the expected IR type,
    /// inserting `Op::DynAlloc` when a concrete value flows into a
    /// `dyn Trait` slot. Returns `value` unchanged for non-dyn targets
    /// or when the value is already `DynRef`.
    pub(crate) fn coerce_to_expected(
        &mut self,
        value: ValueId,
        actual: &IrType,
        expected: &IrType,
        span: Span,
    ) -> ValueId {
        if actual == expected {
            return value;
        }
        let IrType::DynRef(trait_name) = expected else {
            return value;
        };
        if matches!(actual, IrType::DynRef(_)) {
            return value;
        }
        let concrete_type = match actual {
            IrType::StructRef(name) => name.clone(),
            IrType::EnumRef(name, _) => name.clone(),
            // Sema's `Checker::concrete_type_impls_trait`
            // (phoenix-sema/src/check_types.rs) rejects non-struct/enum
            // coercions into `dyn Trait` before lowering runs.
            other => unreachable!(
                "coerce_to_expected: unexpected IR type {other} flowing into \
                 dyn Trait position — `Checker::concrete_type_impls_trait` \
                 (phoenix-sema/src/check_types.rs) must reject before lowering"
            ),
        };
        self.register_dyn_vtable(trait_name, &concrete_type);
        // `Op::DynAlloc` and the result `IrType::DynRef` both take owned
        // `String`s for the trait name, so we clone the borrowed
        // `trait_name` at each consumption site. The `concrete_type`
        // above was moved out of the match by value and is consumed by
        // `Op::DynAlloc` directly.
        self.emit(
            Op::DynAlloc(trait_name.clone(), concrete_type, value),
            IrType::DynRef(trait_name.clone()),
            Some(span),
        )
    }

    /// Coerce `value` to `expected` using the IR-recorded result type of
    /// `value` as the `actual` type. Convenience wrapper over
    /// [`Self::coerce_to_expected`] for single-value assignment boundaries
    /// where the caller only has a [`ValueId`] (no source span to look up
    /// via sema's `expr_types`).
    ///
    /// If the IR type is unavailable (parallel `value_types` index out of
    /// sync with `value_count` — see the fragility note in
    /// `docs/known-issues.md`) the original value is returned without
    /// coercion. A `debug_assert!` flags this in debug builds so the
    /// underlying invariant break surfaces in tests rather than silently
    /// dropping a `dyn` wrap in release.
    pub(crate) fn coerce_value_to_expected(
        &mut self,
        value: ValueId,
        expected: &IrType,
        span: Span,
    ) -> ValueId {
        let Some(actual) = self.current_func().instruction_result_type(value).cloned() else {
            debug_assert!(
                false,
                "coerce_value_to_expected: no recorded IR type for ValueId {value:?} \
                 (expected `{expected}`); `IrFunction::value_types` is out of sync with \
                 `value_count` — see docs/known-issues.md, parallel-index fragility"
            );
            return value;
        };
        self.coerce_to_expected(value, &actual, expected, span)
    }

    /// Coerce `value` to `expected` using sema's resolved type for the
    /// source expression at `expr_span` as the `actual` type. Use this at
    /// assignment boundaries where a parser expression is being bound into
    /// an annotated slot — sema's type is the authoritative post-
    /// inference answer and picks up alias expansion automatically.
    pub(crate) fn coerce_expr_to_expected(
        &mut self,
        value: ValueId,
        expr_span: Span,
        expected: &IrType,
        span: Span,
    ) -> ValueId {
        let actual = self.expr_type(&expr_span);
        self.coerce_to_expected(value, &actual, expected, span)
    }

    /// Apply [`Self::coerce_to_expected`] to every positional argument of
    /// a direct function call against the callee's declared parameter
    /// types.
    pub(crate) fn coerce_call_args(
        &mut self,
        callee: FuncId,
        args: Vec<ValueId>,
        span: Span,
    ) -> Vec<ValueId> {
        let param_types = self.module.functions[callee.0 as usize].param_types.clone();
        self.coerce_args_to_expected(args, &param_types, span)
    }

    /// Coerce each argument against the corresponding expected type.
    /// Extra args or missing expecteds pass through unchanged. If a
    /// `ValueId`'s IR type is missing from the parallel `value_types`
    /// index, a `debug_assert!` fires (see
    /// [`Self::coerce_value_to_expected`] for the fragility note) and
    /// the argument passes through uncoerced.
    pub(crate) fn coerce_args_to_expected(
        &mut self,
        args: Vec<ValueId>,
        expected_types: &[IrType],
        span: Span,
    ) -> Vec<ValueId> {
        args.into_iter()
            .enumerate()
            .map(|(i, val)| {
                let Some(expected) = expected_types.get(i) else {
                    return val;
                };
                let Some(actual) = self.current_func().instruction_result_type(val).cloned() else {
                    debug_assert!(
                        false,
                        "coerce_args_to_expected: no recorded IR type for ValueId {val:?} \
                         at arg position {i} (expected `{expected}`); \
                         `IrFunction::value_types` is out of sync with `value_count` — \
                         see docs/known-issues.md, parallel-index fragility"
                    );
                    return val;
                };
                self.coerce_to_expected(val, &actual, expected, span)
            })
            .collect()
    }

    /// Lower a trait-object method call to [`Op::DynCall`].
    ///
    /// Resolves the method's slot index (from the IR-level trait
    /// metadata in [`crate::module::IrModule::traits`], which mirrors
    /// declaration order) and coerces the positional arguments against
    /// the trait method's IR-lowered parameter types so anything
    /// flowing into a nested `dyn Trait` slot is wrapped first.
    ///
    /// # Panics
    ///
    /// Panics (via `unreachable!`) if `trait_name` has no IR metadata
    /// or if `method` is not one of its declared methods. Sema rejects
    /// both before lowering runs; reaching here means a compiler bug.
    pub(crate) fn lower_dyn_method_call(
        &mut self,
        trait_name: &str,
        receiver: ValueId,
        method: &str,
        args: Vec<ValueId>,
        span: Span,
    ) -> ValueId {
        let (method_idx, expected) = self
            .module
            .traits
            .get(trait_name)
            .and_then(|info| {
                info.methods.iter().enumerate().find_map(|(i, m)| {
                    (m.name == method).then(|| (i as u32, m.param_types.clone()))
                })
            })
            .unwrap_or_else(|| {
                unreachable!(
                    "compiler bug: IR lowering for `dyn {trait_name}.{method}` but trait \
                     metadata is missing or has no such method — \
                     `Checker::check_method_call` (phoenix-sema/src/check_expr_call.rs) \
                     must reject unknown-trait-method calls before lowering runs, and \
                     `Checker::register_trait_decl` (phoenix-sema/src/check_register.rs) \
                     must have mirrored the trait into `IrModule::traits`"
                )
            });
        let args = self.coerce_args_to_expected(args, &expected, span);
        let result_type = self.expr_type(&span);
        self.emit(
            Op::DynCall(trait_name.to_string(), method_idx, receiver, args),
            result_type,
            Some(span),
        )
    }

    /// Register a `(concrete_type, trait_name)` entry in
    /// [`crate::module::IrModule::dyn_vtables`] with the trait's methods
    /// in declaration order. Idempotent.
    ///
    /// # Ordering constraint
    ///
    /// Must run *during* IR lowering, before monomorphization — the
    /// `FuncId`s cached in each vtable entry have to be those of the
    /// bodies that mono will specialize. If a `DynAlloc` ever materializes
    /// after mono (not possible today, since `dyn GenericTrait` is
    /// rejected at the sema level in `check_types.rs`), mono would have
    /// to re-register vtables for newly materialized
    /// `(concrete, trait)` pairs.
    ///
    /// FIXME(phase-2.6/3): the vtables this registers are stored as
    /// `FuncId`s pointing into `module.functions`, which after monomorphize
    /// is a mix of concrete functions and inert generic-template stubs
    /// (flagged via `is_generic_template`). The stubs are harmless here —
    /// `register_dyn_vtable` is invoked before mono runs, so the entries
    /// always point at real bodies — but the pattern of "consumers filter
    /// via `concrete_functions()`" is proliferating and is tracked as
    /// tech debt in docs/known-issues.md
    /// ("Generic-template stubs tracked by a `bool` flag"). When that
    /// flag is replaced with a typed split, this cache should be re-keyed
    /// onto the concrete-functions newtype rather than raw `FuncId`.
    ///
    /// # Panics
    ///
    /// Via `unreachable!` if a trait method has no registered impl on
    /// `concrete_type`. Sema rejects incomplete impls, so reaching here
    /// is a compiler bug.
    pub(crate) fn register_dyn_vtable(&mut self, trait_name: &str, concrete_type: &str) {
        let key = (concrete_type.to_owned(), trait_name.to_owned());
        if self.module.dyn_vtables.contains_key(&key) {
            return;
        }
        // Borrow `trait_info` rather than clone — the sema `TraitInfo`
        // carries `Vec<TraitMethodInfo>` plus type-param metadata, and
        // we only read `.methods[i].name` here.
        let method_names: Vec<String> = self
            .check
            .traits
            .get(trait_name)
            .unwrap_or_else(|| {
                unreachable!(
                    "compiler bug: register_dyn_vtable for `{concrete_type}` as \
                     dyn `{trait_name}` but trait is missing from sema metadata — \
                     `Checker::register_trait_decl` (phoenix-sema/src/check_register.rs) \
                     must have populated `Checker::traits` before lowering runs, and \
                     `Checker::check_type_expr` must reject `dyn UnknownTrait`"
                )
            })
            .methods
            .iter()
            .map(|m| m.name.clone())
            .collect();
        let entries: Vec<(String, FuncId)> = method_names
            .iter()
            .map(|name| {
                let fid = self
                    .module
                    .method_index
                    .get(&(concrete_type.to_owned(), name.clone()))
                    .copied()
                    .unwrap_or_else(|| {
                        unreachable!(
                            "vtable for `{concrete_type}` as dyn `{trait_name}`: method `{name}` \
                             not found in method_index — `Checker::check_impl_block` \
                             (phoenix-sema/src/check_register.rs) must have rejected an \
                             incomplete impl before lowering runs"
                        )
                    });
                (name.clone(), fid)
            })
            .collect();
        // Slot-index contract: `entries[i]` must correspond to
        // `trait_info.methods[i]`. The iteration above preserves that by
        // construction — this assertion guards the invariant so a future
        // refactor that filters or reorders the mapped iterator fails
        // loudly (every `Op::DynCall` carries a pre-resolved slot index
        // that indexes this vector; out-of-order entries silently
        // miscompile). Promoted to a hard `assert!` so release builds
        // catch the cross-file ABI drift before the wrong function
        // pointer ships to the Cranelift backend.
        assert!(
            entries
                .iter()
                .zip(method_names.iter())
                .all(|((entry_name, _), expected)| entry_name == expected),
            "vtable slot order drifted from trait method declaration order for \
             `{concrete_type}` as dyn `{trait_name}`"
        );
        self.module.dyn_vtables.insert(key, entries);
    }
}
