//! Registration pass for the semantic checker.
//!
//! Pre-registers built-in types, user-declared types, functions, traits,
//! and impl blocks so that the checking pass can resolve references.

use crate::checker::{
    Checker, EnumInfo, FunctionInfo, MethodInfo, StructInfo, TraitInfo, TraitMethodInfo,
    TypeAliasInfo,
};
use crate::types::Type;
use phoenix_common::module_path::ModulePath;
use phoenix_common::span::Span;
use phoenix_parser::ast::{
    EnumDecl, FunctionDecl, ImplBlock, InlineTraitImpl, Param, StructDecl, TraitDecl,
    TypeAliasDecl, TypeExpr, Visibility,
};
use std::collections::HashMap;

/// True iff `existing_def_module` is `Some` and refers to a module other
/// than `current`. Used by the four `register_*` paths to early-return
/// when the slot is already owned by an earlier-registered, different
/// module — preserving first-write-wins under cross-module name
/// collisions (which are separately diagnosed by
/// [`Checker::detect_cross_module_collisions`](crate::checker::Checker::detect_cross_module_collisions)).
fn slot_owned_by_other_module(
    current: &ModulePath,
    existing_def_module: Option<&ModulePath>,
) -> bool {
    matches!(existing_def_module, Some(m) if m != current)
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

    /// Registers a top-level function declaration in the function table.
    ///
    /// Resolves parameter and return types and records them for later use
    /// during call-site checking.  Reports an error if a function with the
    /// same name has already been registered.
    pub(crate) fn register_function(&mut self, func: &FunctionDecl) {
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

        if let Some(existing) = self.functions.get(&func.name) {
            if existing.def_module == self.current_module {
                // In-module duplicate — emit the existing "already defined" diagnostic.
                self.error(
                    format!("function `{}` is already defined", func.name),
                    func.span,
                );
            }
            // Cross-module duplicates are diagnosed by
            // `detect_cross_module_collisions`; preserve first-write-wins
            // by skipping this registration so the symbol table keeps the
            // earlier definition.
        } else {
            self.record_reference(
                func.name_span,
                crate::checker::SymbolKind::Function,
                func.name.clone(),
            );
            // Adopt the pre-allocated FuncId from pre-pass A so sema
            // and IR share an id space.  The id should always be
            // present because `pre_allocate_function_ids` walks the
            // same AST nodes — but we look it up safely so a future
            // refactor that diverges the walks fails with a clear
            // diagnostic instead of an unindexed-HashMap panic.
            let func_id = *self
                .pending_function_ids
                .get(&func.name)
                .unwrap_or_else(|| {
                    panic!(
                        "internal compiler error: function `{}` has no pre-allocated FuncId — \
                         pre_allocate_function_ids and register_function disagree on AST walk order",
                        func.name
                    )
                });
            self.functions.insert(
                func.name.clone(),
                FunctionInfo {
                    func_id,
                    definition_span: func.name_span,
                    type_params: func.type_params.clone(),
                    type_param_bounds: func.type_param_bounds.clone(),
                    params,
                    param_names,
                    default_param_exprs,
                    return_type,
                    visibility: func.visibility,
                    def_module: self.current_module.clone(),
                },
            );
        }
    }

    /// Registers a struct declaration, resolving its field types and storing
    /// them in the struct table for use during constructor and field-access
    /// checking.  Also registers any inline methods and trait implementations.
    ///
    /// Cross-module collisions are diagnosed earlier by
    /// `detect_cross_module_collisions`; this function preserves
    /// first-write-wins semantics by returning early if the struct's slot
    /// is already owned by a different module.
    pub(crate) fn register_struct(&mut self, s: &StructDecl) {
        if slot_owned_by_other_module(
            &self.current_module,
            self.structs.get(&s.name).map(|i| &i.def_module),
        ) {
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
            s.name.clone(),
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
    /// Cross-module collisions are diagnosed earlier; this function
    /// preserves first-write-wins semantics by returning early if the
    /// enum's slot is already owned by a different module.
    pub(crate) fn register_enum(&mut self, e: &EnumDecl) {
        if slot_owned_by_other_module(
            &self.current_module,
            self.enums.get(&e.name).map(|i| &i.def_module),
        ) {
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
            e.name.clone(),
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
    /// Cross-module collisions are diagnosed earlier; this function
    /// preserves first-write-wins semantics by returning early if the
    /// trait's slot is already owned by a different module.
    pub(crate) fn register_trait(&mut self, t: &TraitDecl) {
        if slot_owned_by_other_module(
            &self.current_module,
            self.traits.get(&t.name).map(|i| &i.def_module),
        ) {
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
            t.name.clone(),
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
    /// Cross-module collisions are diagnosed earlier; this function
    /// preserves first-write-wins semantics by returning early if the
    /// alias's slot is already owned by a different module.
    pub(crate) fn register_type_alias(&mut self, ta: &TypeAliasDecl) {
        if slot_owned_by_other_module(
            &self.current_module,
            self.type_aliases.get(&ta.name).map(|i| &i.def_module),
        ) {
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
            ta.name.clone(),
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
            if let Some(alias_info) = self.type_aliases.get(name) {
                return self.type_alias_cycle_walk(alias_name, &alias_info.target, visited);
            }
        }
        false
    }

    /// Looks up the generic type parameters declared on a struct or enum.
    pub(crate) fn parent_type_params(&self, type_name: &str) -> Vec<String> {
        self.structs
            .get(type_name)
            .map(|s| s.type_params.clone())
            .or_else(|| self.enums.get(type_name).map(|e| e.type_params.clone()))
            .unwrap_or_default()
    }

    /// Registers an `impl` block, recording each method's signature in the
    /// method table.  For trait implementations, validates that all required
    /// methods are provided with compatible signatures.
    pub(crate) fn register_impl(&mut self, imp: &ImplBlock) {
        let parent_type_params = self.parent_type_params(&imp.type_name);

        let mut methods_to_add = Vec::new();
        for func in &imp.methods {
            let mut merged = parent_type_params.clone();
            merged.extend(func.type_params.iter().cloned());
            let (params, param_names, default_param_exprs, return_type) =
                self.with_type_params(&merged, None, |this| {
                    let non_self_params: Vec<&phoenix_parser::ast::Param> =
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
            // and IR share an id space.  The id should always be
            // present because `pre_allocate_user_method_ids` walks
            // the same AST nodes — but we look it up safely so a
            // future refactor that diverges the walks fails with a
            // clear diagnostic instead of an unindexed-HashMap panic.
            let key = (imp.type_name.clone(), func.name.clone());
            let func_id = *self.pending_user_method_ids.get(&key).unwrap_or_else(|| {
                panic!(
                    "internal compiler error: method `{}.{}` has no pre-allocated FuncId — \
                     pre_allocate_user_method_ids and register_impl disagree on AST walk order",
                    imp.type_name, func.name
                )
            });
            let has_self = func.params.first().is_some_and(|p| p.name == "self");
            methods_to_add.push((
                func.name.clone(),
                MethodInfo {
                    func_id: Some(func_id),
                    definition_span: func.name_span,
                    params,
                    param_names,
                    default_param_exprs,
                    return_type,
                    type_params: func.type_params.clone(),
                    has_self,
                },
            ));
        }
        let type_methods = self.methods.entry(imp.type_name.clone()).or_default();
        let mut duplicates: Vec<(String, Span)> = Vec::new();
        for (name, info) in methods_to_add {
            match type_methods.entry(name) {
                std::collections::hash_map::Entry::Vacant(slot) => {
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

        if let Some(ref trait_name) = imp.trait_name {
            if self
                .trait_impls
                .contains(&(imp.type_name.clone(), trait_name.clone()))
            {
                self.error(
                    format!(
                        "duplicate implementation of trait `{}` for type `{}`",
                        trait_name, imp.type_name
                    ),
                    imp.span,
                );
            }
            if let Some(trait_info) = self.traits.get(trait_name).cloned() {
                self.validate_trait_impl(imp, trait_name, &trait_info);
                self.trait_impls
                    .insert((imp.type_name.clone(), trait_name.clone()));
            } else {
                self.error(format!("unknown trait `{}`", trait_name), imp.span);
            }
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
