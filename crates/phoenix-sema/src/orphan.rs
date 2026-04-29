//! Pre-allocated `FuncId` consumption for rejected method-bearing decls.
//!
//! Pre-pass B (`pre_allocate_user_method_ids`) allocates a `FuncId` for
//! every method on every struct/enum/impl AST node before the
//! registration pass runs. When registration *rejects* a parent decl —
//! a within-module duplicate type, or a coherence-violating
//! `impl` block on an imported type — those allocated ids would
//! otherwise sit unfilled, tripping the "FuncId was pre-allocated but
//! never registered" panic in
//! [`crate::resolved::build_user_and_builtin_methods`].
//!
//! The fix is to consume each rejected method's id under a separate
//! `Checker.orphan_methods` table, keyed by `(qualified_receiver_type,
//! parent_span)`. The resolved-module builder drains both `methods`
//! and `orphan_methods`; only `methods` contributes to the public
//! method-name index, so orphan entries fill `user_methods` slots
//! without becoming reachable through any name lookup.

use crate::checker::{Checker, MethodInfo};
use crate::types::Type;
use phoenix_common::ids::FuncId;
use phoenix_common::module_path::module_qualify;
use phoenix_common::span::Span;
use phoenix_parser::ast::{FunctionDecl, InlineTraitImpl, Param};
use std::collections::HashMap;

impl Checker {
    /// Consume the pre-allocated `FuncId`s for a rejected type's inline
    /// methods + trait-impl methods. Called by `register_struct` /
    /// `register_enum` when a within-module duplicate is diagnosed:
    /// the parent decl is dropped but pre-pass B already allocated
    /// `FuncId`s for every method on it, so the resolved-module
    /// builder will panic if those slots stay empty.
    ///
    /// Walks both `methods` (inline `methods { … }`) and
    /// `trait_impls.*.methods` so the consumption mirrors
    /// `register_inline_methods`. Each method's `MethodInfo` lands
    /// in `Checker.orphan_methods` keyed by `(qualified_type,
    /// parent_span)` — distinct from `Checker.methods`, so the
    /// surviving same-named decl's methods table is unaffected.
    pub(crate) fn consume_orphan_inline_methods(
        &mut self,
        type_name: &str,
        methods: &[FunctionDecl],
        trait_impls: &[InlineTraitImpl],
        parent_span: Span,
    ) {
        self.consume_orphan_methods_with(type_name, methods, parent_span);
        for ti in trait_impls {
            self.consume_orphan_methods_with(type_name, &ti.methods, ti.span);
        }
    }

    /// Consume the pre-allocated `FuncId`s for one batch of methods
    /// belonging to a rejected parent (duplicate type or coherence-
    /// violating impl). Stores each method's `MethodInfo` in
    /// `Checker.orphan_methods` so it never collides with the surviving
    /// methods table; skips any method whose `FuncId` is already filled
    /// by a different registration (e.g. a duplicate type whose first
    /// decl has the same method name) to avoid the "FuncId populated
    /// twice" assertion in `build_user_and_builtin_methods`.
    ///
    /// Resolves the method's actual signature (params, return type,
    /// defaults) so the slot we file into `user_methods` is shape-
    /// equivalent to a normal registration. IR lowering reads this
    /// slot through [`crate::resolved::ResolvedModule::user_methods`]
    /// and would otherwise see `Type::Error`/empty placeholders flow
    /// into the Cranelift signature. Any diagnostics produced during
    /// orphan resolution are discarded — the user has already seen
    /// the parent rejection ("is already defined" or coherence error)
    /// and shouldn't be told again about types referenced inside a
    /// dropped decl.
    pub(crate) fn consume_orphan_methods(
        &mut self,
        type_name: &str,
        methods: &[FunctionDecl],
        parent_span: Span,
    ) {
        self.consume_orphan_methods_with(type_name, methods, parent_span);
    }

    /// Inner helper for [`Self::consume_orphan_methods`] /
    /// [`Self::consume_orphan_inline_methods`]. The `filled` guard is
    /// served by `Checker.filled_method_func_ids`, which is maintained
    /// incrementally by every `methods`/`orphan_methods` insert site
    /// — so each rejected method only pays one `HashSet` lookup, not
    /// an O(N·M) scan over `self.methods`.
    fn consume_orphan_methods_with(
        &mut self,
        type_name: &str,
        methods: &[FunctionDecl],
        parent_span: Span,
    ) {
        let qualified_type = module_qualify(&self.current_module, type_name);
        let parent_type_params = self.parent_type_params(type_name);
        for func in methods {
            let key = (qualified_type.clone(), func.name.clone());
            let Some(&func_id) = self.pending_user_method_ids.get(&key) else {
                continue;
            };
            if !self.filled_method_func_ids.insert(func_id) {
                continue;
            }
            let info = self.resolve_orphan_method_info(func, func_id, &parent_type_params);
            self.orphan_methods
                .entry((qualified_type.clone(), parent_span))
                .or_default()
                .entry(func.name.clone())
                .or_insert(info);
        }
    }

    /// Resolve a single rejected method's full `MethodInfo` (params,
    /// names, defaults, return type) under the given parent type-param
    /// scope, discarding any diagnostics emitted during resolution.
    /// See [`Self::consume_orphan_methods`] for why diagnostics are
    /// suppressed here.
    fn resolve_orphan_method_info(
        &mut self,
        func: &FunctionDecl,
        func_id: FuncId,
        parent_type_params: &[String],
    ) -> MethodInfo {
        let mut merged = parent_type_params.to_vec();
        merged.extend(func.type_params.iter().cloned());
        let diag_snapshot = self.diagnostics.len();
        let (params, param_names, default_param_exprs, return_type) =
            self.with_type_params(&merged, None, |this| {
                let non_self: Vec<&Param> =
                    func.params.iter().filter(|p| p.name != "self").collect();
                let params: Vec<Type> = non_self
                    .iter()
                    .map(|p| this.resolve_type_expr(&p.type_annotation))
                    .collect();
                let param_names: Vec<String> = non_self.iter().map(|p| p.name.clone()).collect();
                let default_param_exprs: HashMap<usize, _> = non_self
                    .iter()
                    .enumerate()
                    .filter_map(|(i, p)| p.default_value.as_ref().map(|e| (i, e.clone())))
                    .collect();
                let return_type = func
                    .return_type
                    .as_ref()
                    .map(|t| this.resolve_type_expr(t))
                    .unwrap_or(Type::Void);
                (params, param_names, default_param_exprs, return_type)
            });
        self.diagnostics.truncate(diag_snapshot);
        let has_self = func.params.first().is_some_and(|p| p.name == "self");
        MethodInfo {
            func_id: Some(func_id),
            definition_span: func.name_span,
            params,
            param_names,
            default_param_exprs,
            return_type,
            type_params: func.type_params.clone(),
            has_self,
        }
    }
}
