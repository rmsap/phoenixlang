use crate::checker::Checker;
use crate::types::Type;
use phoenix_parser::ast::TypeExpr;
use std::collections::HashMap;

/// Failure modes surfaced by [`Checker::unify`] during generic call
/// inference. Each variant is turned into a user-facing diagnostic by the
/// caller; see [`Checker::infer_type_args`] for which variants propagate.
#[derive(Debug, Clone)]
pub(crate) enum UnifyError {
    /// A type variable was bound to two incompatible concrete types across
    /// different argument positions (e.g., `identity(1, "s")` against
    /// `identity<T>(a: T, b: T)` — `T` seen as both `Int` and `String`).
    Conflict {
        /// The name of the type parameter that received conflicting bindings.
        param: String,
        /// The binding established by an earlier argument position.
        existing: Type,
        /// The incoming binding from the current argument position.
        incoming: Type,
    },
    /// A type variable would be bound to a type that mentions itself
    /// (e.g., `T := List<T>`), which would make substitution diverge.
    ///
    /// Reserved for future use: Phoenix does not currently alpha-rename
    /// type parameters, so a scope-oblivious occurs-check would
    /// false-positive on template-body shadowing (`function outer<T> {
    /// inner(x) }` binding `inner.T := outer.T`). This variant is not
    /// emitted by the current inference; it is kept so callers can be
    /// written forward-compatibly.
    #[allow(dead_code)]
    OccursCheck {
        /// The name of the type parameter.
        param: String,
        /// The cyclic candidate type the caller tried to bind.
        incoming: Type,
    },
    /// A non-variable mismatch (e.g., pattern `Int` vs concrete `String`).
    /// Reported by `unify` but not surfaced by `infer_type_args`, since the
    /// per-argument type check produces a clearer diagnostic at the call site.
    Mismatch,
}

impl Checker {
    /// Checks whether two types are compatible, accounting for type variables
    /// in generic contexts.
    ///
    /// Compatibility rules:
    /// - Equal types are always compatible.
    /// - A [`Type::TypeVar`] is compatible with any type (it acts as a
    ///   wildcard during generic inference).
    /// - Two [`Type::Generic`] types are compatible when they share the same
    ///   base name, the same number of type arguments, and each pair of
    ///   arguments is recursively compatible.
    pub(crate) fn types_compatible(&self, declared: &Type, actual: &Type) -> bool {
        if declared == actual {
            return true;
        }
        // Type variables act as wildcards — they match any type. This is needed
        // for unresolved generic parameters (e.g. the E in Ok(42) which has type
        // Result<Int, TypeVar("E")>) and for active type parameters in generic
        // function bodies.
        if declared.is_type_var() || actual.is_type_var() {
            return true;
        }
        // Compare generic types structurally — recurse into type args
        if let (Type::Generic(name1, args1), Type::Generic(name2, args2)) = (declared, actual) {
            return name1 == name2
                && args1.len() == args2.len()
                && args1
                    .iter()
                    .zip(args2.iter())
                    .all(|(a, b)| self.types_compatible(a, b));
        }
        false
    }

    /// Resolves a parser-level [`TypeExpr`] into a semantic [`Type`], expanding
    /// type aliases and validating that named types exist.
    pub(crate) fn resolve_type_expr(&mut self, type_expr: &TypeExpr) -> Type {
        match type_expr {
            TypeExpr::Named(named) => {
                // Check if it's a type parameter currently in scope
                if self.current_type_params.contains(&named.name) {
                    return Type::TypeVar(named.name.clone());
                }
                // Check if it's a type alias
                if let Some(alias_info) = self.type_aliases.get(&named.name).cloned() {
                    if alias_info.type_params.is_empty() {
                        return alias_info.target;
                    }
                    // Generic alias used without type args — this is an error
                    self.error(
                        format!(
                            "generic type alias `{}` requires type arguments",
                            named.name
                        ),
                        named.span,
                    );
                    return Type::Error;
                }
                let ty = Type::from_name(&named.name);
                if let Type::Named(ref name) = ty {
                    if self.structs.contains_key(name) || self.enums.contains_key(name) {
                        return ty;
                    }
                    if name == "Self" {
                        return ty;
                    }
                    self.error(format!("unknown type `{}`", name), named.span);
                    Type::Error
                } else {
                    ty
                }
            }
            TypeExpr::Function(ft) => {
                let params: Vec<Type> = ft
                    .param_types
                    .iter()
                    .map(|t| self.resolve_type_expr(t))
                    .collect();
                let ret = self.resolve_type_expr(&ft.return_type);
                Type::Function(params, Box::new(ret))
            }
            TypeExpr::Generic(gt) => {
                let type_args: Vec<Type> = gt
                    .type_args
                    .iter()
                    .map(|t| self.resolve_type_expr(t))
                    .collect();
                // Check if it's a generic type alias (e.g. `StringResult<Int>`)
                if let Some(alias_info) = self.type_aliases.get(&gt.name)
                    && !alias_info.type_params.is_empty()
                {
                    let mut bindings = HashMap::new();
                    for (i, param) in alias_info.type_params.iter().enumerate() {
                        if i < type_args.len() {
                            bindings.insert(param.clone(), type_args[i].clone());
                        }
                    }
                    return Self::substitute(&alias_info.target, &bindings);
                }
                // Built-in generic types
                if gt.name == "List" || gt.name == "Map" {
                    return Type::Generic(gt.name.clone(), type_args);
                }
                // Verify the base type exists and type argument count matches
                if let Some(si) = self.structs.get(&gt.name) {
                    let expected = si.type_params.len();
                    if type_args.len() != expected {
                        self.error(
                            format!(
                                "type `{}` expects {} type argument(s), got {}",
                                gt.name,
                                expected,
                                type_args.len()
                            ),
                            gt.span,
                        );
                    }
                } else if let Some(ei) = self.enums.get(&gt.name) {
                    let expected = ei.type_params.len();
                    if type_args.len() != expected {
                        self.error(
                            format!(
                                "type `{}` expects {} type argument(s), got {}",
                                gt.name,
                                expected,
                                type_args.len()
                            ),
                            gt.span,
                        );
                    }
                } else {
                    self.error(format!("unknown type `{}`", gt.name), gt.span);
                    return Type::Error;
                }
                Type::Generic(gt.name.clone(), type_args)
            }
        }
    }

    /// Substitutes type variables in a type according to the given bindings.
    ///
    /// Recursively walks the type tree, replacing each [`Type::TypeVar`] and
    /// matching [`Type::Named`] with the concrete type from `bindings`.
    /// Compound types ([`Type::Generic`], [`Type::Function`]) are rebuilt
    /// with their inner types substituted.
    ///
    /// For example, given bindings `{T -> Int}`, the type `Option<T>` becomes
    /// `Option<Int>`, and the bare type variable `T` becomes `Int`.
    pub(crate) fn substitute(ty: &Type, bindings: &HashMap<String, Type>) -> Type {
        match ty {
            Type::TypeVar(name) => bindings.get(name).cloned().unwrap_or_else(|| ty.clone()),
            Type::Generic(name, args) => {
                let new_args: Vec<Type> =
                    args.iter().map(|a| Self::substitute(a, bindings)).collect();
                Type::Generic(name.clone(), new_args)
            }
            Type::Function(params, ret) => {
                let new_params: Vec<Type> = params
                    .iter()
                    .map(|p| Self::substitute(p, bindings))
                    .collect();
                let new_ret = Self::substitute(ret, bindings);
                Type::Function(new_params, Box::new(new_ret))
            }
            Type::Named(name) => {
                // A Named type might also be a type var if it matches a binding
                bindings.get(name).cloned().unwrap_or_else(|| ty.clone())
            }
            _ => ty.clone(),
        }
    }

    /// Attempts to unify a pattern type with a concrete type, building up bindings.
    ///
    /// When the pattern contains a [`Type::TypeVar`], it is bound to the
    /// corresponding concrete type (or checked against an existing binding).
    /// Generic and function types are unified structurally by recursing into
    /// their type arguments.
    ///
    /// Returns `Ok(())` on success. Returns `Err(UnifyError::Conflict { .. })`
    /// when a type variable already bound to one concrete type is unified
    /// against a different concrete type (e.g., `T` seen as both `Int` and
    /// `String` across two argument positions). Returns
    /// `Err(UnifyError::OccursCheck { .. })` when binding `T` to a type that
    /// itself mentions `T` (e.g., `T := List<T>`) — this would make
    /// [`substitute`](Self::substitute) diverge.
    ///
    /// A mismatch between non-variable types (e.g., `Int` vs `String`) is
    /// reported as `Err(UnifyError::Mismatch)` and is surfaced to the user
    /// by the per-argument type check in
    /// [`infer_and_check_call_generics`](Checker::infer_and_check_call_generics);
    /// [`infer_type_args`](Self::infer_type_args) only propagates the first
    /// two variants, since mismatches already produce a clearer "argument N
    /// expected X got Y" diagnostic at the call site.
    fn unify(
        pattern: &Type,
        concrete: &Type,
        bindings: &mut HashMap<String, Type>,
    ) -> Result<(), UnifyError> {
        match (pattern, concrete) {
            (Type::TypeVar(name), _) => {
                if let Some(existing) = bindings.get(name) {
                    if existing == concrete {
                        Ok(())
                    } else {
                        Err(UnifyError::Conflict {
                            param: name.clone(),
                            existing: existing.clone(),
                            incoming: concrete.clone(),
                        })
                    }
                } else {
                    // Note: Phoenix does not alpha-rename type parameters,
                    // so nested generic templates shadow each other's
                    // binder names (`function outer<T> { inner(x) }` where
                    // both `outer<T>` and `inner<T>` use the name `T`).
                    // This means a scope-oblivious occurs-check would
                    // false-positive on every same-named shadowing, so we
                    // do not run one here. `UnifyError::OccursCheck` stays
                    // in the enum for a future alpha-renaming pass but is
                    // not emitted by the current inference.
                    bindings.insert(name.clone(), concrete.clone());
                    Ok(())
                }
            }
            (Type::Generic(pn, pa), Type::Generic(cn, ca)) if pn == cn && pa.len() == ca.len() => {
                for (p, c) in pa.iter().zip(ca.iter()) {
                    Self::unify(p, c, bindings)?;
                }
                Ok(())
            }
            (Type::Function(pp, pr), Type::Function(cp, cr)) if pp.len() == cp.len() => {
                for (p, c) in pp.iter().zip(cp.iter()) {
                    Self::unify(p, c, bindings)?;
                }
                Self::unify(pr, cr, bindings)
            }
            _ if pattern == concrete => Ok(()),
            _ => Err(UnifyError::Mismatch),
        }
    }

    /// Infers type arguments for a generic call by unifying declared parameter
    /// types against the actual argument types.
    ///
    /// Each [`Type::TypeVar`] inside `param_types` is matched against the
    /// corresponding entry in `arg_types` via [`unify`](Self::unify), building
    /// a map from type-parameter names to their concrete types.
    ///
    /// Returns the bindings plus a list of `(arg_index, UnifyError)` pairs
    /// describing any binding-level failures: conflicts (same type variable
    /// forced to two different types) and occurs-check failures (binding
    /// `T := f(T)`). Pure type mismatches between non-variable types are
    /// not returned because the call-site per-argument check produces a
    /// clearer diagnostic; see [`unify`](Self::unify).
    pub(crate) fn infer_type_args(
        &self,
        param_types: &[Type],
        arg_types: &[Type],
    ) -> (HashMap<String, Type>, Vec<(usize, UnifyError)>) {
        let mut bindings = HashMap::new();
        let mut errors = Vec::new();
        for (i, (param, arg)) in param_types.iter().zip(arg_types.iter()).enumerate() {
            if arg.is_error() {
                continue;
            }
            if let Err(e) = Self::unify(param, arg, &mut bindings) {
                match e {
                    UnifyError::Conflict { .. } | UnifyError::OccursCheck { .. } => {
                        errors.push((i, e));
                    }
                    UnifyError::Mismatch => {}
                }
            }
        }
        (bindings, errors)
    }

    /// Extracts the base type name and type parameter bindings from a [`Type`].
    ///
    /// For `Type::Named("Foo")` returns `(Some("Foo"), {})`.
    /// For `Type::Generic("Foo", [Int, String])` returns
    /// `(Some("Foo"), {T: Int, U: String})`, building a substitution map from
    /// the struct/enum's declared type params to the concrete args.
    pub(crate) fn extract_type_name_and_bindings(
        &self,
        ty: &Type,
    ) -> (Option<String>, HashMap<String, Type>) {
        match ty {
            Type::Named(name) => (Some(name.clone()), HashMap::new()),
            Type::Generic(name, args) => {
                let mut bindings = HashMap::new();
                // Built-in generic types
                if name == "List" {
                    if !args.is_empty() {
                        bindings.insert("T".to_string(), args[0].clone());
                    }
                } else if name == "Map" {
                    if !args.is_empty() {
                        bindings.insert("K".to_string(), args[0].clone());
                    }
                    if args.len() >= 2 {
                        bindings.insert("V".to_string(), args[1].clone());
                    }
                } else if let Some(si) = self.structs.get(name) {
                    // Look up the type params from the struct or enum
                    for (i, param) in si.type_params.iter().enumerate() {
                        if i < args.len() {
                            bindings.insert(param.clone(), args[i].clone());
                        }
                    }
                } else if let Some(ei) = self.enums.get(name) {
                    for (i, param) in ei.type_params.iter().enumerate() {
                        if i < args.len() {
                            bindings.insert(param.clone(), args[i].clone());
                        }
                    }
                }
                (Some(name.clone()), bindings)
            }
            _ => (None, HashMap::new()),
        }
    }

    /// Returns the type name string to use for trait bound checking.
    /// Returns the canonical type name used for looking up trait implementations
    /// when checking trait bounds on generic type parameters.
    pub(crate) fn type_name_for_bounds(&self, ty: &Type) -> String {
        match ty {
            Type::Named(n) => n.clone(),
            Type::Int => "Int".to_string(),
            Type::Float => "Float".to_string(),
            Type::String => "String".to_string(),
            Type::Bool => "Bool".to_string(),
            Type::Generic(name, _) => name.clone(),
            _ => ty.to_string(),
        }
    }
}
