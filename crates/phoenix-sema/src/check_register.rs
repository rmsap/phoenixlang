//! Registration pass for the semantic checker.
//!
//! Pre-registers built-in types, user-declared types, functions, traits,
//! and impl blocks so that the checking pass can resolve references.

use crate::checker::{
    Checker, EnumInfo, FunctionInfo, MethodInfo, StructInfo, TraitInfo, TraitMethodInfo,
    TypeAliasInfo,
};
use crate::types::Type;
use phoenix_common::span::Span;
use phoenix_parser::ast::*;
use std::collections::HashMap;

impl Checker {
    /// Pre-registers built-in `Option<T>` and `Result<T, E>` enums along with
    /// their standard methods so they are available without explicit declaration.
    pub(crate) fn register_builtins(&mut self) {
        // Register built-in Option<T> enum
        self.enums.insert(
            "Option".to_string(),
            EnumInfo {
                type_params: vec!["T".to_string()],
                variants: vec![
                    ("Some".to_string(), vec![Type::TypeVar("T".to_string())]),
                    ("None".to_string(), vec![]),
                ],
            },
        );

        // Register built-in Result<T, E> enum
        self.enums.insert(
            "Result".to_string(),
            EnumInfo {
                type_params: vec!["T".to_string(), "E".to_string()],
                variants: vec![
                    ("Ok".to_string(), vec![Type::TypeVar("T".to_string())]),
                    ("Err".to_string(), vec![Type::TypeVar("E".to_string())]),
                ],
            },
        );

        // Register built-in Option methods
        self.methods.insert(
            "Option".to_string(),
            HashMap::from([
                (
                    "isSome".to_string(),
                    MethodInfo {
                        params: vec![],
                        return_type: Type::Bool,
                    },
                ),
                (
                    "isNone".to_string(),
                    MethodInfo {
                        params: vec![],
                        return_type: Type::Bool,
                    },
                ),
                (
                    "unwrap".to_string(),
                    MethodInfo {
                        params: vec![],
                        return_type: Type::TypeVar("T".to_string()),
                    },
                ),
                (
                    "unwrapOr".to_string(),
                    MethodInfo {
                        params: vec![Type::TypeVar("T".to_string())],
                        return_type: Type::TypeVar("T".to_string()),
                    },
                ),
            ]),
        );

        // Register built-in Result methods
        self.methods.insert(
            "Result".to_string(),
            HashMap::from([
                (
                    "isOk".to_string(),
                    MethodInfo {
                        params: vec![],
                        return_type: Type::Bool,
                    },
                ),
                (
                    "isErr".to_string(),
                    MethodInfo {
                        params: vec![],
                        return_type: Type::Bool,
                    },
                ),
                (
                    "unwrap".to_string(),
                    MethodInfo {
                        params: vec![],
                        return_type: Type::TypeVar("T".to_string()),
                    },
                ),
                (
                    "unwrapOr".to_string(),
                    MethodInfo {
                        params: vec![Type::TypeVar("T".to_string())],
                        return_type: Type::TypeVar("T".to_string()),
                    },
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
        let (params, param_names, default_param_indices, return_type) =
            self.with_type_params(&func.type_params, None, |this| {
                let non_self_params: Vec<&Param> =
                    func.params.iter().filter(|p| p.name != "self").collect();
                let params: Vec<Type> = non_self_params
                    .iter()
                    .map(|p| this.resolve_type_expr(&p.type_annotation))
                    .collect();
                let param_names: Vec<String> =
                    non_self_params.iter().map(|p| p.name.clone()).collect();
                let default_param_indices: Vec<usize> = non_self_params
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.default_value.is_some())
                    .map(|(i, _)| i)
                    .collect();
                let return_type = func
                    .return_type
                    .as_ref()
                    .map(|t| this.resolve_type_expr(t))
                    .unwrap_or(Type::Void);
                (params, param_names, default_param_indices, return_type)
            });

        if self.functions.contains_key(&func.name) {
            self.error(
                format!("function `{}` is already defined", func.name),
                func.span,
            );
        } else {
            self.functions.insert(
                func.name.clone(),
                FunctionInfo {
                    type_params: func.type_params.clone(),
                    type_param_bounds: func.type_param_bounds.clone(),
                    params,
                    param_names,
                    default_param_indices,
                    return_type,
                },
            );
        }
    }

    /// Registers a struct declaration, resolving its field types and storing
    /// them in the struct table for use during constructor and field-access
    /// checking.  Also registers any inline methods and trait implementations.
    pub(crate) fn register_struct(&mut self, s: &StructDecl) {
        let fields: Vec<(String, Type)> = self.with_type_params(&s.type_params, None, |this| {
            s.fields
                .iter()
                .map(|f| (f.name.clone(), this.resolve_type_expr(&f.type_annotation)))
                .collect()
        });
        self.structs.insert(
            s.name.clone(),
            StructInfo {
                type_params: s.type_params.clone(),
                fields,
            },
        );

        self.register_inline_methods(&s.name, &s.methods, &s.trait_impls, s.span);
    }

    /// Registers an enum declaration, resolving each variant's field types and
    /// storing them in the enum table for use during pattern matching and
    /// constructor checking.  Also registers any inline methods and trait
    /// implementations.
    pub(crate) fn register_enum(&mut self, e: &EnumDecl) {
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
        self.enums.insert(
            e.name.clone(),
            EnumInfo {
                type_params: e.type_params.clone(),
                variants,
            },
        );

        self.register_inline_methods(&e.name, &e.methods, &e.trait_impls, e.span);
    }

    /// Registers a trait declaration, storing its method signatures.
    pub(crate) fn register_trait(&mut self, t: &TraitDecl) {
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
        self.traits.insert(
            t.name.clone(),
            TraitInfo {
                type_params: t.type_params.clone(),
                methods,
            },
        );
    }

    /// Registers a type alias, resolving the target type expression.
    ///
    /// Temporarily swaps the alias's own type parameters into scope so that
    /// references like `T` in `type StringResult<T> = Result<T, String>` are
    /// resolved as type variables rather than concrete types.
    pub(crate) fn register_type_alias(&mut self, ta: &TypeAliasDecl) {
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
                type_params: ta.type_params.clone(),
                target,
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
            let (params, return_type) = self.with_type_params(&merged, None, |this| {
                let params: Vec<Type> = func
                    .params
                    .iter()
                    .filter(|p| p.name != "self")
                    .map(|p| this.resolve_type_expr(&p.type_annotation))
                    .collect();
                let return_type = func
                    .return_type
                    .as_ref()
                    .map(|t| this.resolve_type_expr(t))
                    .unwrap_or(Type::Void);
                (params, return_type)
            });
            methods_to_add.push((
                func.name.clone(),
                MethodInfo {
                    params,
                    return_type,
                },
            ));
        }
        let type_methods = self.methods.entry(imp.type_name.clone()).or_default();
        for (name, info) in methods_to_add {
            type_methods.insert(name, info);
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
