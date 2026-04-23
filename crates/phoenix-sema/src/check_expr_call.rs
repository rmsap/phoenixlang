use crate::check_types::UnifyError;
use crate::checker::{Checker, FunctionInfo};
use crate::types::Type;
use phoenix_common::span::Span;
use phoenix_parser::ast::{CallExpr, Expr, MethodCallExpr, StructLiteralExpr};
use std::collections::{HashMap, HashSet};

/// Output of [`Checker::infer_and_check_call_generics`]:
/// `(bindings, arg_types, errors)`.
/// See that method's docstring for field semantics.
type CallGenericsInference = (HashMap<String, Type>, Vec<Type>, Vec<(usize, UnifyError)>);

impl Checker {
    /// Type-checks a method call (`obj.method(args)`), dispatching to built-in
    /// methods for `List`, `String`, `Map`, `Option`, and `Result`, or looking
    /// up user-defined methods and trait-bounded methods.
    pub(crate) fn check_method_call(&mut self, mc: &MethodCallExpr) -> Type {
        let obj_type = self.check_expr(&mc.object);
        if obj_type.is_error() {
            return Type::Error;
        }
        if obj_type == Type::Void {
            self.error(format!("cannot call method on {}", obj_type), mc.span);
            return Type::Error;
        }
        let (base_name, bindings) = self.extract_type_name_and_bindings(&obj_type);
        let type_name = base_name.unwrap_or_else(|| obj_type.to_string());

        // Dispatch to built-in type helpers
        let builtin_result = match type_name.as_str() {
            "List" => {
                let elem_type = bindings
                    .get("T")
                    .cloned()
                    .or_else(|| {
                        if let Type::Generic(_, ref args) = obj_type {
                            args.first().cloned()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(Type::TypeVar("T".to_string()));
                self.check_list_method(mc, elem_type)
            }
            "String" => self.check_string_method(mc),
            "Map" => {
                let key_type = bindings
                    .get("K")
                    .cloned()
                    .unwrap_or(Type::TypeVar("K".to_string()));
                let val_type = bindings
                    .get("V")
                    .cloned()
                    .unwrap_or(Type::TypeVar("V".to_string()));
                self.check_map_method(mc, key_type, val_type)
            }
            "Option" => {
                let inner_type = bindings
                    .get("T")
                    .cloned()
                    .unwrap_or(Type::TypeVar("T".to_string()));
                self.check_option_method(mc, inner_type)
            }
            "Result" => {
                let ok_type = bindings
                    .get("T")
                    .cloned()
                    .unwrap_or(Type::TypeVar("T".to_string()));
                let err_type = bindings
                    .get("E")
                    .cloned()
                    .unwrap_or(Type::TypeVar("E".to_string()));
                self.check_result_method(mc, ok_type, err_type)
            }
            _ => None,
        };
        if let Some(ty) = builtin_result {
            return ty;
        }

        // Trait-object method dispatch: single-bound `dyn Trait` only.
        if let Type::Dyn(trait_name) = &obj_type {
            // Clone just the method signature so we don't hold a borrow of
            // `self.traits` across `check_method_args`. Sema rejects
            // `dyn UnknownTrait` upstream in `resolve_type_expr`, so the
            // trait must be present here; only the *method name* may be
            // wrong at this site.
            let Some(trait_info) = self.traits.get(trait_name) else {
                unreachable!(
                    "compiler bug: receiver typed `dyn {trait_name}` but trait is missing \
                     from sema metadata — `Checker::resolve_type_expr` must reject \
                     `dyn UnknownTrait` before checker reaches a method call on it"
                );
            };
            let method_sig = trait_info
                .methods
                .iter()
                .find(|m| m.name == mc.method)
                .map(|m| (m.params.clone(), m.return_type.clone()));
            return match method_sig {
                Some((params, ret)) => {
                    self.check_method_args(mc, &params, &HashMap::new());
                    ret
                }
                None => {
                    self.error(
                        format!("trait `{}` has no method `{}`", trait_name, mc.method),
                        mc.span,
                    );
                    Type::Error
                }
            };
        }

        // User-defined methods
        if let Some(type_methods) = self.methods.get(&type_name).cloned()
            && let Some(method_info) = type_methods.get(&mc.method)
        {
            // Merge parent-type bindings (from the receiver) with bindings
            // inferred for the method's own type parameters (from the
            // argument types), then record the method's concrete type
            // args for IR monomorphization.
            let mut all_bindings = bindings.clone();
            if !method_info.type_params.is_empty() {
                // Pre-check arg types so inference has something to unify.
                let arg_types: Vec<Type> = mc.args.iter().map(|a| self.check_expr(a)).collect();
                let (method_bindings, errors) =
                    self.infer_type_args(&method_info.params, &arg_types);
                for (k, v) in method_bindings.iter() {
                    all_bindings.entry(k.clone()).or_insert_with(|| v.clone());
                }
                self.record_inferred_type_args(
                    &format!("{}.{}", type_name, mc.method),
                    &method_info.type_params,
                    &all_bindings,
                    &errors,
                    &arg_types,
                    mc.span,
                );
            }
            self.check_method_args(mc, &method_info.params, &all_bindings);
            return Self::substitute(&method_info.return_type, &all_bindings);
        }
        // Check trait bounds for type variables
        if let Some(ty) = self.resolve_trait_bound_method(&obj_type, mc) {
            return ty;
        }
        self.error(
            format!("no method `{}` on type `{}`", mc.method, type_name),
            mc.span,
        );
        Type::Error
    }

    /// Validates argument count and types for a method call against expected
    /// parameter types, applying generic substitutions from `bindings`.
    pub(crate) fn check_method_args(
        &mut self,
        mc: &MethodCallExpr,
        params: &[Type],
        bindings: &HashMap<String, Type>,
    ) {
        if mc.args.len() != params.len() {
            self.error(
                format!(
                    "method `{}` takes {} argument(s), got {}",
                    mc.method,
                    params.len(),
                    mc.args.len()
                ),
                mc.span,
            );
            return;
        }
        for (i, arg) in mc.args.iter().enumerate() {
            let arg_type = self.check_expr(arg);
            let expected = Self::substitute(&params[i], bindings);
            if !arg_type.is_error()
                && !expected.is_error()
                && !self.types_compatible(&expected, &arg_type)
            {
                self.error(
                    format!(
                        "argument {} of `{}`: expected `{}` but got `{}`",
                        i + 1,
                        mc.method,
                        expected,
                        arg_type
                    ),
                    arg.span(),
                );
            }
        }
    }

    /// Looks up a method on a type variable via its trait bounds. Returns
    /// `Some(return_type)` if a matching trait method is found, `None` otherwise.
    fn resolve_trait_bound_method(&mut self, obj_type: &Type, mc: &MethodCallExpr) -> Option<Type> {
        let tv_name = match obj_type {
            Type::TypeVar(name) => name,
            _ => return None,
        };
        for (param_name, bound_traits) in &self.current_type_param_bounds.clone() {
            if param_name != tv_name {
                continue;
            }
            for bound_trait in bound_traits {
                let trait_info = match self.traits.get(bound_trait).cloned() {
                    Some(info) => info,
                    None => continue,
                };
                let trait_method = match trait_info.methods.iter().find(|m| m.name == mc.method) {
                    Some(m) => m,
                    None => continue,
                };
                let empty_bindings = HashMap::new();
                self.check_method_args(mc, &trait_method.params, &empty_bindings);
                return Some(trait_method.return_type.clone());
            }
        }
        None
    }

    /// Type-checks a struct constructor or enum variant constructor expression,
    /// validating field count, field types, and inferring generic type arguments.
    pub(crate) fn check_struct_literal(&mut self, sl: &StructLiteralExpr) -> Type {
        // Check if it's a struct constructor
        if let Some(struct_info) = self.structs.get(&sl.name).cloned() {
            if sl.args.len() != struct_info.fields.len() {
                self.error(
                    format!(
                        "struct `{}` has {} field(s), got {}",
                        sl.name,
                        struct_info.fields.len(),
                        sl.args.len()
                    ),
                    sl.span,
                );
            } else if !struct_info.type_params.is_empty() {
                let mut arg_types = Vec::new();
                for arg in &sl.args {
                    arg_types.push(self.check_expr(arg));
                }
                let field_types: Vec<Type> =
                    struct_info.fields.iter().map(|f| f.ty.clone()).collect();
                let (bindings, _) = self.infer_type_args(&field_types, &arg_types);
                for (i, arg) in sl.args.iter().enumerate() {
                    let expected = Self::substitute(&struct_info.fields[i].ty, &bindings);
                    // `types_compatible` so dyn-typed fields on a generic
                    // struct still get the concrete-to-dyn coercion.
                    if !expected.has_type_vars() && !self.types_compatible(&expected, &arg_types[i])
                    {
                        self.error(
                            format!(
                                "field `{}`: expected `{}` but got `{}`",
                                struct_info.fields[i].name, expected, arg_types[i]
                            ),
                            arg.span(),
                        );
                    }
                }
                let result_args: Vec<Type> = struct_info
                    .type_params
                    .iter()
                    .map(|p| bindings.get(p).cloned().unwrap_or(Type::TypeVar(p.clone())))
                    .collect();
                return Type::Generic(sl.name.clone(), result_args);
            } else {
                for (i, arg) in sl.args.iter().enumerate() {
                    let arg_type = self.check_expr(arg);
                    let field = &struct_info.fields[i];
                    if !arg_type.is_error()
                        && !field.ty.is_error()
                        && !self.types_compatible(&field.ty, &arg_type)
                    {
                        self.error(
                            format!(
                                "field `{}`: expected `{}` but got `{}`",
                                field.name, field.ty, arg_type
                            ),
                            arg.span(),
                        );
                    }
                }
            }
            return Type::Named(sl.name.clone());
        }

        // Check if it's an enum variant constructor
        self.check_enum_variant_constructor(sl)
    }

    /// Type-checks an enum variant constructor expression, validating field
    /// count, field types, and inferring generic type arguments for the
    /// parent enum.
    fn check_enum_variant_constructor(&mut self, sl: &StructLiteralExpr) -> Type {
        let variant_match = self.enums.iter().find_map(|(enum_name, enum_info)| {
            enum_info
                .variants
                .iter()
                .find(|(n, _)| n == &sl.name)
                .map(|(_, types)| {
                    (
                        enum_name.clone(),
                        enum_info.type_params.clone(),
                        types.clone(),
                    )
                })
        });

        if let Some((enum_name, type_params, variant_types)) = variant_match {
            if sl.args.len() != variant_types.len() {
                self.error(
                    format!(
                        "variant `{}` takes {} field(s), got {}",
                        sl.name,
                        variant_types.len(),
                        sl.args.len()
                    ),
                    sl.span,
                );
            } else if !type_params.is_empty() {
                let mut arg_types = Vec::new();
                for arg in &sl.args {
                    arg_types.push(self.check_expr(arg));
                }
                let (bindings, _) = self.infer_type_args(&variant_types, &arg_types);
                for (i, arg) in sl.args.iter().enumerate() {
                    let expected = Self::substitute(&variant_types[i], &bindings);
                    // `types_compatible` so dyn-typed variant fields on a
                    // generic enum still get the concrete-to-dyn coercion.
                    if !expected.has_type_vars() && !self.types_compatible(&expected, &arg_types[i])
                    {
                        self.error(
                            format!(
                                "variant `{}` field {}: expected `{}` but got `{}`",
                                sl.name,
                                i + 1,
                                expected,
                                arg_types[i]
                            ),
                            arg.span(),
                        );
                    }
                }
                let result_args: Vec<Type> = type_params
                    .iter()
                    .map(|p| bindings.get(p).cloned().unwrap_or(Type::TypeVar(p.clone())))
                    .collect();
                return Type::Generic(enum_name, result_args);
            } else {
                for (i, arg) in sl.args.iter().enumerate() {
                    let arg_type = self.check_expr(arg);
                    if !arg_type.is_error()
                        && !variant_types[i].is_error()
                        && !self.types_compatible(&variant_types[i], &arg_type)
                    {
                        self.error(
                            format!(
                                "variant `{}` field {}: expected `{}` but got `{}`",
                                sl.name,
                                i + 1,
                                variant_types[i],
                                arg_type
                            ),
                            arg.span(),
                        );
                    }
                }
            }
            return Type::Named(enum_name);
        }

        self.error(format!("undefined type or variant `{}`", sl.name), sl.span);
        Type::Error
    }

    /// Type-checks a function call expression, resolving the callee and
    /// validating argument count and types.  Handles named arguments and
    /// default parameter values.
    pub(crate) fn check_call(&mut self, call: &CallExpr) -> Type {
        if let Expr::Ident(ident) = &call.callee {
            // Built-in: print
            if ident.name == "print" {
                if !call.named_args.is_empty() {
                    self.error(
                        "built-in function `print` does not accept named arguments".to_string(),
                        call.span,
                    );
                }
                if call.args.len() != 1 {
                    self.error(
                        format!("print() takes 1 argument, got {}", call.args.len()),
                        call.span,
                    );
                } else {
                    self.check_expr(&call.args[0]);
                }
                return Type::Void;
            }
            // Built-in: toString
            if ident.name == "toString" {
                if !call.named_args.is_empty() {
                    self.error(
                        "built-in function `toString` does not accept named arguments".to_string(),
                        call.span,
                    );
                }
                if call.args.len() != 1 {
                    self.error(
                        format!("toString() takes 1 argument, got {}", call.args.len()),
                        call.span,
                    );
                } else {
                    self.check_expr(&call.args[0]);
                }
                return Type::String;
            }

            // User-defined function
            if let Some(func_info) = self.functions.get(&ident.name).cloned() {
                self.record_reference(
                    ident.span,
                    crate::checker::SymbolKind::Function,
                    ident.name.clone(),
                );
                return self.check_call_with_info(&ident.name, &func_info, call);
            }

            // Check if it's a variable with a function type
            if let Some(info) = self.scopes.lookup(&ident.name).cloned() {
                return self.check_call_on_type(info.ty, call);
            }

            self.error(format!("undefined function `{}`", ident.name), ident.span);
            return Type::Error;
        }

        // Non-ident callee (e.g. lambda call) — check callee type
        let callee_type = self.check_expr(&call.callee);
        self.check_call_on_type(callee_type, call)
    }

    /// Validates a call against a known `FunctionInfo`, handling positional
    /// args, named args, and default parameter values.
    fn check_call_with_info(
        &mut self,
        func_name: &str,
        func_info: &FunctionInfo,
        call: &CallExpr,
    ) -> Type {
        let total_params = func_info.params.len();
        let positional_count = call.args.len();
        let named_count = call.named_args.len();

        self.validate_named_args(func_name, func_info, call, positional_count);

        // Positional args must not exceed total params
        if positional_count > total_params {
            self.error(
                format!(
                    "function `{}` takes {} argument(s), got {}",
                    func_name, total_params, positional_count
                ),
                call.span,
            );
            return func_info.return_type.clone();
        }

        // Check that all required (non-default) params are covered
        // Count how many params are covered: positional + named + default
        let mut covered = vec![false; total_params];
        for c in covered.iter_mut().take(positional_count.min(total_params)) {
            *c = true;
        }
        for (name, _) in &call.named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name) {
                covered[idx] = true;
            }
        }
        for &idx in &func_info.default_param_indices {
            covered[idx] = true; // defaults fill in uncovered params
        }
        let missing: Vec<String> = covered
            .iter()
            .enumerate()
            .filter(|(_, c)| !**c)
            .map(|(i, _)| func_info.param_names[i].clone())
            .collect();
        if !missing.is_empty() {
            self.error(
                format!(
                    "function `{}` missing argument(s): {}",
                    func_name,
                    missing.join(", ")
                ),
                call.span,
            );
            return func_info.return_type.clone();
        }

        // Also check total supplied doesn't exceed params
        // (positional + named should not provide more args than params)
        if positional_count + named_count > total_params {
            self.error(
                format!(
                    "function `{}` takes {} argument(s), got {} (positional) + {} (named)",
                    func_name, total_params, positional_count, named_count
                ),
                call.span,
            );
            return func_info.return_type.clone();
        }

        // Now type-check all provided arguments
        if !func_info.type_params.is_empty() {
            let (bindings, arg_types, errors) =
                self.infer_and_check_call_generics(func_name, func_info, call, positional_count);
            self.record_inferred_type_args(
                func_name,
                &func_info.type_params,
                &bindings,
                &errors,
                &arg_types,
                call.span,
            );
            return Self::substitute(&func_info.return_type, &bindings);
        }

        // Non-generic: type-check positional args
        for (i, arg) in call.args.iter().enumerate() {
            let arg_type = self.check_expr(arg);
            if !arg_type.is_error()
                && !func_info.params[i].is_error()
                && !self.types_compatible(&func_info.params[i], &arg_type)
            {
                self.error(
                    format!(
                        "argument {} of `{}`: expected `{}` but got `{}`",
                        i + 1,
                        func_name,
                        func_info.params[i],
                        arg_type
                    ),
                    arg.span(),
                );
            }
        }
        // Type-check named args
        for (name, expr) in &call.named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name) {
                let arg_type = self.check_expr(expr);
                if !arg_type.is_error()
                    && !func_info.params[idx].is_error()
                    && !self.types_compatible(&func_info.params[idx], &arg_type)
                {
                    self.error(
                        format!(
                            "named argument `{}` of `{}`: expected `{}` but got `{}`",
                            name, func_name, func_info.params[idx], arg_type
                        ),
                        expr.span(),
                    );
                }
            }
        }

        func_info.return_type.clone()
    }

    /// Validates named arguments: checks for duplicates, unknown names, and
    /// overlap with positional arguments.
    fn validate_named_args(
        &mut self,
        func_name: &str,
        func_info: &FunctionInfo,
        call: &CallExpr,
        positional_count: usize,
    ) {
        let mut named_set = HashSet::new();
        for (name, expr) in &call.named_args {
            if !named_set.insert(name.clone()) {
                self.error(format!("duplicate named argument `{}`", name), expr.span());
            }
            if !func_info.param_names.contains(name) {
                self.error(
                    format!("function `{}` has no parameter named `{}`", func_name, name),
                    expr.span(),
                );
            }
        }
        for (name, expr) in &call.named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name)
                && idx < positional_count
            {
                self.error(
                    format!(
                        "parameter `{}` already provided as positional argument {}",
                        name,
                        idx + 1
                    ),
                    expr.span(),
                );
            }
        }
    }

    /// For a generic function call, builds the full argument type array,
    /// infers type variable bindings, type-checks each argument against its
    /// substituted parameter type, and validates trait bounds.
    ///
    /// Returns `(bindings, arg_types, errors)`:
    /// - `bindings` maps declared type parameters to their inferred concrete
    ///   types. May be incomplete if no argument constrains a parameter.
    /// - `arg_types` is the fully resolved `Vec<Type>` in declared parameter
    ///   order (defaults fill in uncovered positions). Used downstream by
    ///   [`record_inferred_type_args`](Self::record_inferred_type_args) to
    ///   suppress unresolved-param diagnostics when a cascade is already in
    ///   flight from `Type::Error` arguments.
    /// - `errors` are the binding-level failures ([`UnifyError::Conflict`]
    ///   and [`UnifyError::OccursCheck`]) discovered by
    ///   [`infer_type_args`](Self::infer_type_args).
    fn infer_and_check_call_generics(
        &mut self,
        func_name: &str,
        func_info: &FunctionInfo,
        call: &CallExpr,
        positional_count: usize,
    ) -> CallGenericsInference {
        let total_params = func_info.params.len();

        // Build the full arg_types array in param order
        let mut arg_types = vec![Type::Error; total_params];
        for (i, arg) in call.args.iter().enumerate() {
            arg_types[i] = self.check_expr(arg);
        }
        for (name, expr) in &call.named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name) {
                arg_types[idx] = self.check_expr(expr);
            }
        }
        // For params covered by defaults only, use the declared param type
        for (i, at) in arg_types.iter_mut().enumerate() {
            if *at == Type::Error
                && func_info.default_param_indices.contains(&i)
                && i >= positional_count
                && !call
                    .named_args
                    .iter()
                    .any(|(n, _)| *n == func_info.param_names[i])
            {
                *at = func_info.params[i].clone();
            }
        }
        let (bindings, errors) = self.infer_type_args(&func_info.params, &arg_types);

        // Type-check provided args against substituted param types.
        // `!has_type_vars()` gates the check until inference has resolved
        // every TypeVar in `expected` — leftover vars mean unification
        // didn't bind the parameter, and the unresolved-type-parameter
        // diagnostic in `record_inferred_type_args` is the better signal.
        // The compatibility check uses `types_compatible` (not `==`) so a
        // concrete arg flowing into a `dyn Trait` parameter coerces just
        // like in the non-generic call path.
        for (i, arg) in call.args.iter().enumerate() {
            let expected = Self::substitute(&func_info.params[i], &bindings);
            if !expected.has_type_vars() && !self.types_compatible(&expected, &arg_types[i]) {
                self.error(
                    format!(
                        "argument {} of `{}`: expected `{}` but got `{}`",
                        i + 1,
                        func_name,
                        expected,
                        arg_types[i]
                    ),
                    arg.span(),
                );
            }
        }
        for (name, expr) in &call.named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name) {
                let expected = Self::substitute(&func_info.params[idx], &bindings);
                if !expected.has_type_vars() && !self.types_compatible(&expected, &arg_types[idx]) {
                    self.error(
                        format!(
                            "named argument `{}` of `{}`: expected `{}` but got `{}`",
                            name, func_name, expected, arg_types[idx]
                        ),
                        expr.span(),
                    );
                }
            }
        }

        // Check trait bounds
        for (param_name, bound_traits) in &func_info.type_param_bounds {
            if let Some(concrete) = bindings.get(param_name) {
                let concrete_name = self.type_name_for_bounds(concrete);
                for bound_trait in bound_traits {
                    if !self
                        .trait_impls
                        .contains(&(concrete_name.clone(), bound_trait.clone()))
                    {
                        self.error(
                            format!(
                                "type `{}` does not implement trait `{}`",
                                concrete_name, bound_trait
                            ),
                            call.span,
                        );
                    }
                }
            }
        }

        (bindings, arg_types, errors)
    }

    /// Finalize a generic call: emit diagnostics for any unification errors
    /// and for unresolved type parameters, then (if everything resolved
    /// cleanly) record the concrete type arguments in
    /// [`call_type_args`](Checker::call_type_args) keyed by `call_span` for
    /// IR monomorphization to consume.
    ///
    /// The contract is:
    /// - Conflicts and occurs-check failures always produce a diagnostic.
    /// - Unresolved type parameters produce a diagnostic **unless** some
    ///   argument already has `Type::Error` (suppresses cascades from
    ///   undefined identifiers and similar upstream errors).
    /// - Nothing is inserted into `call_type_args` if any diagnostic fires
    ///   or if any resolved type is `Type::Error`. Downstream IR lowering
    ///   relies on this invariant: entries always have fully-resolved
    ///   concrete types.
    pub(crate) fn record_inferred_type_args(
        &mut self,
        callee_name: &str,
        type_params: &[String],
        bindings: &HashMap<String, Type>,
        errors: &[(usize, UnifyError)],
        arg_types: &[Type],
        call_span: Span,
    ) {
        // Surface binding-level failures first.
        let mut had_hard_error = false;
        for (i, err) in errors {
            had_hard_error = true;
            match err {
                UnifyError::Conflict {
                    param,
                    existing,
                    incoming,
                } => {
                    self.error(
                        format!(
                            "argument {} of `{}`: conflicting bindings for type parameter `{}` (was `{}`, now `{}`)",
                            i + 1,
                            callee_name,
                            param,
                            existing,
                            incoming
                        ),
                        call_span,
                    );
                }
                UnifyError::OccursCheck { param, incoming } => {
                    self.error(
                        format!(
                            "argument {} of `{}`: cannot bind type parameter `{}` to `{}` (recursive type)",
                            i + 1,
                            callee_name,
                            param,
                            incoming
                        ),
                        call_span,
                    );
                }
                UnifyError::Mismatch => {}
            }
        }

        // Surface unresolved type parameters — but only if no argument is
        // already `Type::Error`, to avoid cascading diagnostics from
        // upstream failures (undefined identifiers, type errors in args).
        let has_error_arg = arg_types.iter().any(Type::is_error);
        let unresolved: Vec<&str> = type_params
            .iter()
            .filter(|tp| !bindings.contains_key(tp.as_str()))
            .map(String::as_str)
            .collect();
        if !unresolved.is_empty() && !has_error_arg {
            let names = unresolved
                .iter()
                .map(|n| format!("`{}`", n))
                .collect::<Vec<_>>()
                .join(", ");
            let (param_word, them) = if unresolved.len() == 1 {
                ("type parameter", "it")
            } else {
                ("type parameters", "them")
            };
            self.error(
                format!(
                    "cannot infer {} {} for call to `{}`; no argument constrains {}",
                    param_word, names, callee_name, them
                ),
                call_span,
            );
            return;
        }

        if had_hard_error {
            return;
        }

        let ordered: Option<Vec<Type>> = type_params
            .iter()
            .map(|tp| bindings.get(tp).cloned())
            .collect();
        if let Some(ordered) = ordered
            && !ordered.iter().any(Type::is_error)
        {
            self.call_type_args.insert(call_span, ordered);
        }
    }

    /// Checks a call expression against a callee that has a known type.
    ///
    /// Used when the callee is a variable with a `Type::Function` type or a
    /// lambda expression.  Validates that the number and types of arguments
    /// match the function's parameter list and returns the function's return
    /// type.  Named arguments and defaults are not supported for indirect calls.
    fn check_call_on_type(&mut self, callee_type: Type, call: &CallExpr) -> Type {
        if !call.named_args.is_empty() {
            self.error(
                "named arguments are not supported for indirect function calls".to_string(),
                call.span,
            );
        }
        if let Type::Function(ref param_types, ref return_type) = callee_type {
            if call.args.len() != param_types.len() {
                self.error(
                    format!(
                        "function takes {} argument(s), got {}",
                        param_types.len(),
                        call.args.len()
                    ),
                    call.span,
                );
            } else {
                for (i, arg) in call.args.iter().enumerate() {
                    let arg_type = self.check_expr(arg);
                    if !arg_type.is_error()
                        && !param_types[i].is_error()
                        && !self.types_compatible(&param_types[i], &arg_type)
                    {
                        self.error(
                            format!(
                                "argument {}: expected {} but got {}",
                                i + 1,
                                param_types[i],
                                arg_type
                            ),
                            arg.span(),
                        );
                    }
                }
            }
            return *return_type.clone();
        }
        if !callee_type.is_error() {
            self.error(
                format!("cannot call value of type {}", callee_type),
                call.span,
            );
        }
        Type::Error
    }
}
