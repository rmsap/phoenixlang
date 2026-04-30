//! Registration pass for the semantic checker.
//!
//! Pre-registers built-in types, user-declared types, functions, traits,
//! and impl blocks so that the checking pass can resolve references.

use crate::checker::{
    Checker, EnumInfo, FunctionInfo, MethodInfo, StructInfo, TraitInfo, TraitMethodInfo,
    TypeAliasInfo,
};
use crate::impl_classify::ImplTarget;
use crate::types::Type;
use phoenix_common::module_path::{ModulePath, module_qualify};
use phoenix_common::span::Span;
use phoenix_parser::ast::{
    EnumDecl, Expr, FunctionDecl, ImplBlock, InlineTraitImpl, LiteralKind, Param, StructDecl,
    TraitDecl, TypeAliasDecl, TypeExpr, UnaryOp, Visibility,
};
use std::collections::{HashMap, HashSet};

/// True for default-argument expressions that can be safely inlined at
/// every call site without privacy concerns: any `Literal(...)` (Int /
/// Float / Bool — and `String`, whose contents are user-controlled but
/// reference no symbols, so it's still privacy-safe) plus
/// `Unary(Neg, Literal(Int|Float))`, which is how the parser models
/// negative numeric literals (`-1`, `-3.14`) so they don't trigger
/// needless wrapper synthesis.
///
/// Anything else (a call, an identifier, a struct literal, a binary
/// op, an interpolated string, …) is conservatively treated as
/// scope-dependent: it may reference symbols whose visibility differs
/// between the callee's module and a foreign caller's module, so it
/// must be lowered as a synthesized wrapper in the callee's scope. See
/// the "Default-expression visibility across module boundaries"
/// bug-closure entry in `docs/phases/phase-2.md` (§2.6) for the design.
///
/// Over-approximates: `Int = 1 + 2` would still be wrapped today even
/// though the expression is closed. The wrapper-call cost is one
/// indirection vs. an inlined two-instruction add — negligible at IR
/// scale, and the simplicity of the rule prevents subtle leaks.
///
/// TODO(perf): once a const-eval / closed-form-expression pass exists,
/// expand this to recognize any expression that evaluates to a constant
/// without referencing user symbols (e.g. `1 + 2`, string concat of
/// literals). Until then the conservative literal-only rule keeps the
/// privacy invariant trivially auditable.
fn is_pure_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Unary(u) if matches!(u.op, UnaryOp::Neg) => matches!(
            &u.operand,
            Expr::Literal(lit)
                if matches!(lit.kind, LiteralKind::Int(_) | LiteralKind::Float(_))
        ),
        _ => false,
    }
}

/// Compute the `default_needs_wrapper` slot set for a callee, gated on
/// `is_generic`. Generic callees fall back to the inline-default path
/// (returns an empty set) because wrapping their defaults would
/// require per-specialization wrapper cloning — a deferred follow-on.
/// Non-generic callees flag every slot whose default is not a pure
/// literal. Shared by free-function and method registration so the
/// two paths can't drift on the gate or the per-slot rule.
fn compute_default_needs_wrapper(
    default_param_exprs: &HashMap<usize, Expr>,
    is_generic: bool,
) -> HashSet<usize> {
    if is_generic {
        HashSet::new()
    } else {
        default_param_exprs
            .iter()
            .filter(|(_, e)| !is_pure_literal(e))
            .map(|(i, _)| *i)
            .collect()
    }
}

impl Checker {
    /// Pre-registers built-in `Option<T>` and `Result<T, E>` enums along with
    /// their standard methods so they are available without explicit declaration.
    pub(crate) fn register_builtins(&mut self) {
        // Register built-in Option<T> enum
        self.enums.insert(
            "Option".to_string(),
            EnumInfo {
                definition_span: Span::BUILTIN,
                type_params: vec!["T".to_string()],
                variants: vec![
                    ("Some".to_string(), vec![Type::TypeVar("T".to_string())]),
                    ("None".to_string(), vec![]),
                ],
                visibility: Visibility::Public,
                def_module: ModulePath::builtin(),
            },
        );

        // Register built-in Result<T, E> enum
        self.enums.insert(
            "Result".to_string(),
            EnumInfo {
                definition_span: Span::BUILTIN,
                type_params: vec!["T".to_string(), "E".to_string()],
                variants: vec![
                    ("Ok".to_string(), vec![Type::TypeVar("T".to_string())]),
                    ("Err".to_string(), vec![Type::TypeVar("E".to_string())]),
                ],
                visibility: Visibility::Public,
                def_module: ModulePath::builtin(),
            },
        );

        // Register built-in Option methods
        self.methods.insert(
            "Option".to_string(),
            HashMap::from([
                (
                    "isSome".to_string(),
                    MethodInfo::builtin(vec![], Type::Bool),
                ),
                (
                    "isNone".to_string(),
                    MethodInfo::builtin(vec![], Type::Bool),
                ),
                (
                    "unwrap".to_string(),
                    MethodInfo::builtin(vec![], Type::TypeVar("T".to_string())),
                ),
                (
                    "unwrapOr".to_string(),
                    MethodInfo::builtin(
                        vec![Type::TypeVar("T".to_string())],
                        Type::TypeVar("T".to_string()),
                    ),
                ),
            ]),
        );

        // Register built-in Result methods
        self.methods.insert(
            "Result".to_string(),
            HashMap::from([
                ("isOk".to_string(), MethodInfo::builtin(vec![], Type::Bool)),
                ("isErr".to_string(), MethodInfo::builtin(vec![], Type::Bool)),
                (
                    "unwrap".to_string(),
                    MethodInfo::builtin(vec![], Type::TypeVar("T".to_string())),
                ),
                (
                    "unwrapOr".to_string(),
                    MethodInfo::builtin(
                        vec![Type::TypeVar("T".to_string())],
                        Type::TypeVar("T".to_string()),
                    ),
                ),
            ]),
        );
    }

    /// Emit a diagnostic and return `true` if `name` shadows a builtin —
    /// callers in the five name-shadow `register_*` paths use this to
    /// short-circuit before any registration work. `kind_phrase` is the
    /// full noun phrase (e.g. `"a function name"`, `"an enum name"`)
    /// embedded into the message; centralising the wording here keeps
    /// the five callers from drifting.
    ///
    /// `register_impl` does *not* go through this helper — `impl Foo`
    /// where `Foo` is a builtin gets its own coherence-themed
    /// diagnostic ("cannot implement methods on builtin type") rather
    /// than the name-reservation wording, and routes the impl's
    /// methods through the orphan path.
    fn reject_builtin_name_shadow(&mut self, name: &str, kind_phrase: &str, span: Span) -> bool {
        if !self.is_builtin_name(name) {
            return false;
        }
        self.error(
            format!("`{name}` is a reserved builtin name and cannot be used as {kind_phrase}"),
            span,
        );
        true
    }

    /// Registers a top-level function declaration in the function table.
    ///
    /// Resolves parameter and return types and records them for later use
    /// during call-site checking.  Reports an error if a function with the
    /// same name has already been registered **in the same module**. With
    /// per-module name mangling (`module_qualify`), two modules can both
    /// declare `foo` and the registrations land under distinct keys
    /// (`foo` for the entry, `models.user::foo` for `models.user`) — no
    /// cross-module collision arises.
    pub(crate) fn register_function(&mut self, func: &FunctionDecl) {
        if self.reject_builtin_name_shadow(&func.name, "a function name", func.span) {
            return;
        }
        // Reject within-module duplicates *before* type-resolving the
        // signature so the duplicate's params don't emit unrelated
        // type-resolution diagnostics on top of the "already defined"
        // error. The qualified key collides only within the current
        // module because `current_module` is fixed for the in-flight
        // registration.
        let qualified = module_qualify(&self.current_module, &func.name);
        if self.functions.contains_key(&qualified) {
            self.error(
                format!("function `{}` is already defined", func.name),
                func.span,
            );
            return;
        }
        let (params, param_names, default_param_exprs, return_type) =
            self.with_type_params(&func.type_params, None, |this| {
                let non_self_params: Vec<&Param> =
                    func.params.iter().filter(|p| p.name != "self").collect();
                let params: Vec<Type> = non_self_params
                    .iter()
                    .map(|p| this.resolve_type_expr(&p.type_annotation))
                    .collect();
                let param_names: Vec<String> =
                    non_self_params.iter().map(|p| p.name.clone()).collect();
                let default_param_exprs: std::collections::HashMap<usize, _> = non_self_params
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
        self.record_reference(
            func.name_span,
            crate::checker::SymbolKind::Function,
            func.name.clone(),
        );
        // Adopt the pre-allocated FuncId from pre-pass A so sema
        // and IR share an id space.  Pre-pass A keys by the same
        // qualified name, so the lookup is symmetric.
        let func_id = *self
            .pending_function_ids
            .get(&qualified)
            .unwrap_or_else(|| {
                panic!(
                    "internal compiler error: function `{}` (qualified `{}`) has no pre-allocated FuncId — \
                     pre_allocate_function_ids and register_function disagree on AST walk order",
                    func.name, qualified
                )
            });
        // Wrapper synthesis is gated on non-generic callees today —
        // sema accepts non-trivial defaults on generic functions when
        // the default's lowered type has no free type variables, but
        // wrapping them would require a per-specialization clone
        // (analogous to how struct-monomorphization clones methods on
        // generic structs). Defer that to a follow-on. In the
        // meantime, generic callees fall back to the inline-default
        // path, which is privacy-safe within a module — the
        // `<T: Trait>` bound style is the typical generic-default
        // pattern and rarely references private helpers.
        let default_needs_wrapper =
            compute_default_needs_wrapper(&default_param_exprs, !func.type_params.is_empty());
        self.functions.insert(
            qualified,
            FunctionInfo {
                func_id,
                definition_span: func.name_span,
                type_params: func.type_params.clone(),
                type_param_bounds: func.type_param_bounds.clone(),
                params,
                param_names,
                default_param_exprs,
                default_needs_wrapper,
                return_type,
                visibility: func.visibility,
                def_module: self.current_module.clone(),
            },
        );
    }

    /// Registers a struct declaration, resolving its field types and storing
    /// them in the struct table for use during constructor and field-access
    /// checking.  Also registers any inline methods and trait implementations.
    ///
    /// Keyed by the module-qualified name (`module_qualify`), so two
    /// modules can both declare `struct User` without collision.
    /// Within-module duplicates are diagnosed and dropped so the first
    /// declaration survives intact (see `is_within_module_duplicate`).
    /// The duplicate's inline-method `FuncId`s are still consumed via
    /// `consume_orphan_inline_methods` so the resolved-module assembly
    /// invariant ("every pre-allocated FuncId is filled") holds.
    pub(crate) fn register_struct(&mut self, s: &StructDecl) {
        if self.reject_builtin_name_shadow(&s.name, "a struct name", s.span) {
            return;
        }
        let qualified = module_qualify(&self.current_module, &s.name);
        let existing = self
            .structs
            .get(&qualified)
            .map(|i| (i.definition_span, &i.def_module));
        if self.is_within_module_duplicate(existing, s.name_span) {
            self.error(format!("struct `{}` is already defined", s.name), s.span);
            self.consume_orphan_inline_methods(&s.name, &s.methods, &s.trait_impls, s.span);
            return;
        }
        let fields: Vec<crate::checker::FieldInfo> =
            self.with_type_params(&s.type_params, None, |this| {
                s.fields
                    .iter()
                    .map(|f| {
                        let ty = this.resolve_type_expr(&f.type_annotation);

                        // Validate constraint expression if present: bind `self` to
                        // the field type, type-check the expression, verify it is Bool.
                        if let Some(ref constraint) = f.constraint {
                            this.scopes.push();
                            this.scopes.define(
                                "self".to_string(),
                                crate::scope::VarInfo {
                                    ty: ty.clone(),
                                    is_mut: false,
                                    definition_span: f.span,
                                },
                            );
                            let constraint_ty = this.check_expr(constraint);
                            this.scopes.pop();
                            if constraint_ty != crate::types::Type::Bool
                                && !constraint_ty.is_error()
                            {
                                this.error(
                                    format!(
                                        "constraint on field `{}` must evaluate to Bool, got `{}`",
                                        f.name, constraint_ty
                                    ),
                                    f.span,
                                );
                            }
                        }

                        crate::checker::FieldInfo {
                            name: f.name.clone(),
                            ty,
                            constraint: f.constraint.clone(),
                            definition_span: f.span,
                            visibility: f.visibility,
                        }
                    })
                    .collect()
            });
        self.record_reference(
            s.name_span,
            crate::checker::SymbolKind::Struct,
            s.name.clone(),
        );
        self.structs.insert(
            qualified,
            StructInfo {
                definition_span: s.name_span,
                type_params: s.type_params.clone(),
                fields,
                visibility: s.visibility,
                def_module: self.current_module.clone(),
            },
        );

        self.register_inline_methods(&s.name, &s.methods, &s.trait_impls, s.span);
    }

    /// Registers an enum declaration, resolving each variant's field types and
    /// storing them in the enum table for use during pattern matching and
    /// constructor checking.  Also registers any inline methods and trait
    /// implementations.
    ///
    /// Keyed by the module-qualified name (`module_qualify`).
    /// Within-module duplicates are diagnosed and dropped.
    pub(crate) fn register_enum(&mut self, e: &EnumDecl) {
        if self.reject_builtin_name_shadow(&e.name, "an enum name", e.span) {
            return;
        }
        let qualified = module_qualify(&self.current_module, &e.name);
        let existing = self
            .enums
            .get(&qualified)
            .map(|i| (i.definition_span, &i.def_module));
        if self.is_within_module_duplicate(existing, e.name_span) {
            self.error(format!("enum `{}` is already defined", e.name), e.span);
            self.consume_orphan_inline_methods(&e.name, &e.methods, &e.trait_impls, e.span);
            return;
        }
        let variants: Vec<(String, Vec<Type>)> =
            self.with_type_params(&e.type_params, None, |this| {
                e.variants
                    .iter()
                    .map(|v| {
                        let types: Vec<Type> =
                            v.fields.iter().map(|t| this.resolve_type_expr(t)).collect();
                        (v.name.clone(), types)
                    })
                    .collect()
            });
        self.record_reference(
            e.name_span,
            crate::checker::SymbolKind::Enum,
            e.name.clone(),
        );
        self.enums.insert(
            qualified,
            EnumInfo {
                definition_span: e.name_span,
                type_params: e.type_params.clone(),
                variants,
                visibility: e.visibility,
                def_module: self.current_module.clone(),
            },
        );

        self.register_inline_methods(&e.name, &e.methods, &e.trait_impls, e.span);
    }

    /// Registers a trait declaration, storing its method signatures.
    ///
    /// Keyed by the module-qualified name (`module_qualify`).
    /// Within-module duplicates are diagnosed and dropped.
    pub(crate) fn register_trait(&mut self, t: &TraitDecl) {
        if self.reject_builtin_name_shadow(&t.name, "a trait name", t.span) {
            return;
        }
        let qualified = module_qualify(&self.current_module, &t.name);
        let existing = self
            .traits
            .get(&qualified)
            .map(|i| (i.definition_span, &i.def_module));
        if self.is_within_module_duplicate(existing, t.name_span) {
            self.error(format!("trait `{}` is already defined", t.name), t.span);
            return;
        }
        let methods: Vec<TraitMethodInfo> = self.with_type_params(&t.type_params, None, |this| {
            t.methods
                .iter()
                .map(|m| {
                    let params: Vec<Type> = m
                        .params
                        .iter()
                        .filter(|p| p.name != "self")
                        .map(|p| this.resolve_type_expr(&p.type_annotation))
                        .collect();
                    let return_type = m
                        .return_type
                        .as_ref()
                        .map(|rt| this.resolve_type_expr(rt))
                        .unwrap_or(Type::Void);
                    TraitMethodInfo {
                        name: m.name.clone(),
                        params,
                        return_type,
                    }
                })
                .collect()
        });
        let object_safety_error = crate::object_safety::validate(&methods);
        self.traits.insert(
            qualified,
            TraitInfo {
                definition_span: t.name_span,
                type_params: t.type_params.clone(),
                methods,
                object_safety_error,
                visibility: t.visibility,
                def_module: self.current_module.clone(),
            },
        );
    }

    /// Registers a type alias, resolving the target type expression.
    ///
    /// Temporarily swaps the alias's own type parameters into scope so that
    /// references like `T` in `type StringResult<T> = Result<T, String>` are
    /// resolved as type variables rather than concrete types.
    ///
    /// Keyed by the module-qualified name (`module_qualify`).
    /// Within-module duplicates are diagnosed and dropped.
    pub(crate) fn register_type_alias(&mut self, ta: &TypeAliasDecl) {
        if self.reject_builtin_name_shadow(&ta.name, "a type-alias name", ta.span) {
            return;
        }
        let qualified = module_qualify(&self.current_module, &ta.name);
        let existing = self
            .type_aliases
            .get(&qualified)
            .map(|i| (i.definition_span, &i.def_module));
        if self.is_within_module_duplicate(existing, ta.name_span) {
            self.error(
                format!("type alias `{}` is already defined", ta.name),
                ta.span,
            );
            return;
        }
        // Detect direct self-reference: `type A = A`
        if let TypeExpr::Named(named) = &ta.target
            && named.name == ta.name
        {
            self.error(
                format!("type alias `{}` refers to itself", ta.name),
                ta.span,
            );
            return;
        }
        let target = self.with_type_params(&ta.type_params, None, |this| {
            this.resolve_type_expr(&ta.target)
        });

        // Detect indirect cycles: walk the resolved type transitively through
        // known aliases and check if any step mentions the alias being defined.
        if self.type_alias_creates_cycle(&ta.name, &target) {
            self.error(
                format!("type alias cycle detected involving `{}`", ta.name),
                ta.span,
            );
            return;
        }

        self.type_aliases.insert(
            qualified,
            TypeAliasInfo {
                definition_span: ta.name_span,
                type_params: ta.type_params.clone(),
                target,
                visibility: ta.visibility,
                def_module: self.current_module.clone(),
            },
        );
    }

    /// Checks whether a [`Type`] mentions a specific type name (for cycle detection).
    fn type_mentions(ty: &Type, name: &str) -> bool {
        match ty {
            Type::Named(n) => n == name,
            Type::Generic(n, args) => {
                n == name || args.iter().any(|a| Self::type_mentions(a, name))
            }
            Type::Function(params, ret) => {
                params.iter().any(|p| Self::type_mentions(p, name))
                    || Self::type_mentions(ret, name)
            }
            Type::TypeVar(n) => n == name,
            _ => false,
        }
    }

    /// Transitively walks the alias chain to detect cycles.
    fn type_alias_creates_cycle(&self, alias_name: &str, target: &Type) -> bool {
        let mut visited = std::collections::HashSet::new();
        self.type_alias_cycle_walk(alias_name, target, &mut visited)
    }

    /// Recursive helper for [`Self::type_alias_creates_cycle`].
    ///
    /// Resolves alias chain links through `lookup_type_alias` so that
    /// aliases declared in non-entry modules — which live under their
    /// module-qualified key (e.g. `lib::A`) rather than the bare name —
    /// are still followed and cycles like `type A = B; type B = A` in
    /// `lib` are detected.
    fn type_alias_cycle_walk(
        &self,
        alias_name: &str,
        ty: &Type,
        visited: &mut std::collections::HashSet<String>,
    ) -> bool {
        if Self::type_mentions(ty, alias_name) {
            return true;
        }
        if let Type::Named(name) = ty {
            if visited.contains(name) {
                return false;
            }
            visited.insert(name.clone());
            if let Some(alias_info) = self.lookup_type_alias(name) {
                return self.type_alias_cycle_walk(alias_name, &alias_info.target, visited);
            }
        }
        false
    }

    /// True iff a slot in a registration table is already filled by a
    /// *different* AST node — i.e. a real within-module duplicate.
    ///
    /// For structs and enums, [`Checker::pre_register_type_names`] inserts
    /// a placeholder whose `definition_span` matches the AST node about to
    /// be registered, so the first call has `existing.span == new_span`
    /// (returns `false`, registration proceeds and overwrites the
    /// placeholder); a second AST node with the same name has
    /// `existing.span = first.name_span != new_span` (returns `true`).
    ///
    /// Traits and type aliases have no placeholder pass, so the first
    /// registration sees `existing = None` (returns `false`); a second
    /// sees `existing = Some((first.name_span, …)) != new_span`
    /// (returns `true`). Same logic, the `None` arm short-circuits.
    ///
    /// Builtin slots never reach here: each `register_*` checks
    /// [`Self::is_builtin_name`] up front and rejects, so the only
    /// `existing` values this sees come from prior user registrations
    /// (or the placeholder pass) within the current module.
    fn is_within_module_duplicate(
        &self,
        existing: Option<(Span, &ModulePath)>,
        new_span: Span,
    ) -> bool {
        let Some((existing_span, _existing_def_module)) = existing else {
            return false;
        };
        existing_span != new_span
    }

    /// Looks up the generic type parameters declared on a struct or enum
    /// in the current module. Used by `register_impl` to derive the
    /// receiver's parent type parameters. Phoenix's coherence rule —
    /// enforced by [`Self::classify_impl_target`] — requires that an
    /// `impl` block target a type declared in the same module, so
    /// qualification against `current_module` is correct here.
    pub(crate) fn parent_type_params(&self, type_name: &str) -> Vec<String> {
        let qualified = module_qualify(&self.current_module, type_name);
        self.structs
            .get(&qualified)
            .map(|s| s.type_params.clone())
            .or_else(|| self.enums.get(&qualified).map(|e| e.type_params.clone()))
            .unwrap_or_default()
    }

    /// Registers an `impl` block, recording each method's signature in the
    /// method table.  For trait implementations, validates that all required
    /// methods are provided with compatible signatures.
    ///
    /// Up-front, the receiver type is classified by
    /// [`Self::classify_impl_target`]:
    ///
    /// - **Local** — type is declared in the current module; proceed.
    /// - **ForeignModule** — type is declared in another module
    ///   (Phoenix coherence: `impl` blocks must live in the same
    ///   module as the type they target). Diagnose and short-circuit.
    /// - **ForeignAmbiguous** — type's bare name is declared in more
    ///   than one foreign module; diagnostic lists every candidate.
    /// - **Unknown** — type isn't declared anywhere reachable.
    ///   Diagnose with "unknown type" and short-circuit.
    ///
    /// A separate up-front [`Self::is_builtin_name`] guard rejects
    /// `impl <Builtin>` regardless of module, since `module_qualify`
    /// treats the entry and builtin paths as the same key — letting
    /// an entry-module `impl Option` slip through as `Local` would
    /// pollute the builtin's methods table.
    ///
    /// On any rejection path, registration routes through
    /// [`Self::consume_orphan_methods`] so the pre-allocated `FuncId`s
    /// for the impl's methods are still consumed (the post-
    /// registration invariant in `resolved::build_*` requires every
    /// allocated id to be filled). The orphan path stores its
    /// `MethodInfo`s under a synthesized key that nothing looks up,
    /// leaving the surviving methods table uncorrupted.
    pub(crate) fn register_impl(&mut self, imp: &ImplBlock) {
        if self.is_builtin_name(&imp.type_name) {
            self.error(
                format!(
                    "cannot implement methods on builtin type `{}`: \
                     builtin types are reserved",
                    imp.type_name
                ),
                imp.span,
            );
            self.consume_orphan_methods(&imp.type_name, &imp.methods, imp.span);
            return;
        }
        match self.classify_impl_target(imp) {
            ImplTarget::Local => {}
            ImplTarget::ForeignModule(def_module) => {
                self.error(
                    format!(
                        "cannot implement methods on type `{}` from module `{}`: \
                         `impl` blocks must live in the same module as the type they target",
                        imp.type_name, def_module
                    ),
                    imp.span,
                );
                self.consume_orphan_methods(&imp.type_name, &imp.methods, imp.span);
                return;
            }
            ImplTarget::ForeignAmbiguous(modules) => {
                let candidates = modules
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.error(
                    format!(
                        "cannot implement methods on type `{}`: it is declared in modules {} — \
                         `impl` blocks must live in the same module as the type they target",
                        imp.type_name, candidates
                    ),
                    imp.span,
                );
                self.consume_orphan_methods(&imp.type_name, &imp.methods, imp.span);
                return;
            }
            ImplTarget::Unknown => {
                self.error(format!("unknown type `{}`", imp.type_name), imp.span);
                self.consume_orphan_methods(&imp.type_name, &imp.methods, imp.span);
                return;
            }
        }

        // Trait-existence check happens *before* the methods loop so
        // an unknown trait routes the impl's methods through the
        // orphan path instead of inserting them into `self.methods`
        // as inherent methods. Trait resolution goes through the
        // current module's scope (Phase B has run before
        // registration), so `impl ImportedTrait for LocalType`
        // resolves via the importer's `local_name → qualified_key`
        // mapping. The receiver type is keyed under
        // `module_qualify(current_module, ...)` because coherence
        // forces it to live in the current module.
        let qualified_type = module_qualify(&self.current_module, &imp.type_name);
        let qualified_trait_info = match self.resolve_impl_trait(imp) {
            Ok(resolved) => resolved,
            Err(()) => {
                self.consume_orphan_methods(&imp.type_name, &imp.methods, imp.span);
                return;
            }
        };

        let methods_to_add = self.build_impl_method_infos(imp, &qualified_type);
        self.insert_inherent_methods(imp, &qualified_type, methods_to_add);

        if let Some((trait_name, qualified_trait, trait_info)) = qualified_trait_info {
            if self
                .trait_impls
                .contains(&(qualified_type.clone(), qualified_trait.clone()))
            {
                self.error(
                    format!(
                        "duplicate implementation of trait `{}` for type `{}`",
                        trait_name, imp.type_name
                    ),
                    imp.span,
                );
            }
            self.validate_trait_impl(imp, &trait_name, &trait_info);
            self.trait_impls.insert((qualified_type, qualified_trait));
        }
    }

    /// Resolve every method on `imp` into a `MethodInfo` carrying its
    /// pre-allocated `FuncId`. Each method is resolved under the
    /// receiver's parent type-params merged with the method's own
    /// type-params. Caller is responsible for inserting the result into
    /// the methods table — this helper just builds the list.
    fn build_impl_method_infos(
        &mut self,
        imp: &ImplBlock,
        qualified_type: &str,
    ) -> Vec<(String, MethodInfo)> {
        let parent_type_params = self.parent_type_params(&imp.type_name);
        let mut methods_to_add = Vec::with_capacity(imp.methods.len());
        for func in &imp.methods {
            let mut merged = parent_type_params.clone();
            merged.extend(func.type_params.iter().cloned());
            let (params, param_names, default_param_exprs, return_type) =
                self.with_type_params(&merged, None, |this| {
                    let non_self_params: Vec<&Param> =
                        func.params.iter().filter(|p| p.name != "self").collect();
                    let params: Vec<Type> = non_self_params
                        .iter()
                        .map(|p| this.resolve_type_expr(&p.type_annotation))
                        .collect();
                    let param_names: Vec<String> =
                        non_self_params.iter().map(|p| p.name.clone()).collect();
                    let default_param_exprs: HashMap<usize, _> = non_self_params
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
            // Adopt the pre-allocated FuncId from pre-pass B so sema
            // and IR share an id space. The id should always be
            // present because `pre_allocate_user_method_ids` walks
            // the same AST nodes — but we look it up safely so a
            // future refactor that diverges the walks fails with a
            // clear diagnostic instead of an unindexed-HashMap panic.
            let key = (qualified_type.to_string(), func.name.clone());
            let func_id = *self.pending_user_method_ids.get(&key).unwrap_or_else(|| {
                panic!(
                    "internal compiler error: method `{}.{}` (qualified `{}.{}`) has no pre-allocated FuncId — \
                     pre_allocate_user_method_ids and register_impl disagree on AST walk order",
                    imp.type_name, func.name, qualified_type, func.name
                )
            });
            let has_self = func.params.first().is_some_and(|p| p.name == "self");
            // Wrapper synthesis is non-generic-only today (see the
            // matching gate in `register_function`). A method is
            // generic if either its own type-params are non-empty or
            // its receiver type is generic — both cases need
            // per-specialization wrapper cloning, which is a
            // follow-on. Until then, generic methods stay on the
            // inline-default path.
            let is_generic = !parent_type_params.is_empty() || !func.type_params.is_empty();
            let default_needs_wrapper =
                compute_default_needs_wrapper(&default_param_exprs, is_generic);
            methods_to_add.push((
                func.name.clone(),
                MethodInfo {
                    func_id: Some(func_id),
                    definition_span: func.name_span,
                    params,
                    param_names,
                    default_param_exprs,
                    default_needs_wrapper,
                    return_type,
                    type_params: func.type_params.clone(),
                    has_self,
                },
            ));
        }
        methods_to_add
    }

    /// Insert each `(name, MethodInfo)` into the type's method table,
    /// recording each successful insert's FuncId in
    /// [`Checker::filled_method_func_ids`] so the orphan path can
    /// detect already-filled slots in O(1). Duplicate method names
    /// (within or across impl blocks for the same type) emit a
    /// "method `M` is already defined for type `T`" diagnostic
    /// anchored at the duplicate's `definition_span`.
    ///
    /// The methods table is keyed by the type's module-qualified
    /// name. Phoenix's coherence rule (enforced by
    /// [`Self::classify_impl_target`]) requires the receiver type
    /// to live in `current_module`, so qualifying `imp.type_name`
    /// against `current_module` always yields the right key when
    /// coherence holds. Coherence violations land their methods
    /// under an orphan key that nothing looks up — see the
    /// `register_impl` doc comment.
    fn insert_inherent_methods(
        &mut self,
        imp: &ImplBlock,
        qualified_type: &str,
        methods_to_add: Vec<(String, MethodInfo)>,
    ) {
        let mut duplicates: Vec<(String, Span)> = Vec::new();
        // Split disjoint borrows of `self.methods` and
        // `self.filled_method_func_ids` so each successful method insert
        // can record its FuncId in one pass — the borrow checker sees
        // these as distinct fields when accessed through let-bindings.
        let type_methods = self.methods.entry(qualified_type.to_string()).or_default();
        let filled_method_func_ids = &mut self.filled_method_func_ids;
        for (name, info) in methods_to_add {
            match type_methods.entry(name) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    if let Some(fid) = info.func_id {
                        filled_method_func_ids.insert(fid);
                    }
                    slot.insert(info);
                }
                std::collections::hash_map::Entry::Occupied(slot) => {
                    duplicates.push((slot.key().clone(), info.definition_span));
                }
            }
        }
        for (name, span) in duplicates {
            self.error(
                format!(
                    "method `{}` is already defined for type `{}`",
                    name, imp.type_name
                ),
                span,
            );
        }
    }

    /// Validates that a trait implementation provides all required methods with
    /// compatible parameter counts, parameter types, and return types.
    pub(crate) fn validate_trait_impl(
        &mut self,
        imp: &ImplBlock,
        trait_name: &str,
        trait_info: &TraitInfo,
    ) {
        let impl_method_names: Vec<&str> = imp.methods.iter().map(|m| m.name.as_str()).collect();
        for trait_method in &trait_info.methods {
            if !impl_method_names.contains(&trait_method.name.as_str()) {
                self.error(
                    format!(
                        "impl of trait `{}` for `{}` is missing method `{}`",
                        trait_name, imp.type_name, trait_method.name
                    ),
                    imp.span,
                );
            } else if let Some(impl_func) = imp.methods.iter().find(|m| m.name == trait_method.name)
            {
                let impl_params: Vec<&Param> = impl_func
                    .params
                    .iter()
                    .filter(|p| p.name != "self")
                    .collect();
                let impl_param_count = impl_params.len();
                let trait_param_count = trait_method.params.len();
                if impl_param_count != trait_param_count {
                    self.error(
                        format!(
                            "method `{}` has {} parameter(s) but trait `{}` expects {}",
                            trait_method.name, impl_param_count, trait_name, trait_param_count
                        ),
                        imp.span,
                    );
                } else {
                    for (impl_param, trait_param_type) in
                        impl_params.iter().zip(&trait_method.params)
                    {
                        let impl_type = self.resolve_type_expr(&impl_param.type_annotation);
                        if !impl_type.is_error()
                            && !trait_param_type.is_error()
                            && !impl_type.is_type_var()
                            && !trait_param_type.is_type_var()
                            && impl_type != *trait_param_type
                        {
                            self.error(
                                format!(
                                    "method `{}` parameter `{}` has type `{}` but trait `{}` expects `{}`",
                                    trait_method.name,
                                    impl_param.name,
                                    impl_type,
                                    trait_name,
                                    trait_param_type
                                ),
                                imp.span,
                            );
                        }
                    }
                }
                let impl_return = impl_func
                    .return_type
                    .as_ref()
                    .map(|t| self.resolve_type_expr(t))
                    .unwrap_or(Type::Void);
                if !impl_return.is_error()
                    && !trait_method.return_type.is_error()
                    && impl_return != trait_method.return_type
                {
                    self.error(
                        format!(
                            "method `{}` returns `{}` but trait `{}` expects `{}`",
                            trait_method.name, impl_return, trait_name, trait_method.return_type
                        ),
                        imp.span,
                    );
                }
            }
        }
    }

    /// Registers inline methods and trait implementations from a type body.
    pub(crate) fn register_inline_methods(
        &mut self,
        type_name: &str,
        methods: &[FunctionDecl],
        trait_impls: &[InlineTraitImpl],
        span: Span,
    ) {
        if !methods.is_empty() {
            let synthetic_impl = ImplBlock {
                type_name: type_name.to_string(),
                trait_name: None,
                methods: methods.to_vec(),
                span,
            };
            self.register_impl(&synthetic_impl);
        }
        for ti in trait_impls {
            let synthetic_impl = ImplBlock {
                type_name: type_name.to_string(),
                trait_name: Some(ti.trait_name.clone()),
                methods: ti.methods.clone(),
                span: ti.span,
            };
            self.register_impl(&synthetic_impl);
        }
    }
}
