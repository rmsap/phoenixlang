use crate::checker::{Checker, FunctionInfo};
use crate::scope::VarInfo;
use crate::types::Type;
use phoenix_parser::ast::*;
use std::collections::{HashMap, HashSet};

impl Checker {
    /// Returns the type of an expression without emitting diagnostics.
    /// Used when the expression has already been checked and we just need its type.
    pub(crate) fn infer_expr_type(&mut self, expr: &Expr) -> Type {
        // Suppress diagnostics during inference
        let prev_len = self.diagnostics.len();
        let ty = self.check_expr(expr);
        self.diagnostics.truncate(prev_len);
        ty
    }

    /// Type-checks an expression and returns its inferred type.
    ///
    /// The resolved type is also recorded in `self.expr_types` keyed by the
    /// expression's source span, so that downstream passes can look up the
    /// type of any expression without re-running inference.
    pub(crate) fn check_expr(&mut self, expr: &Expr) -> Type {
        let ty = match expr {
            Expr::Literal(lit) => match &lit.kind {
                LiteralKind::Int(_) => Type::Int,
                LiteralKind::Float(_) => Type::Float,
                LiteralKind::String(_) => Type::String,
                LiteralKind::Bool(_) => Type::Bool,
            },
            Expr::Ident(ident) => self.check_ident(ident),
            Expr::Binary(binary) => self.check_binary(binary),
            Expr::Unary(unary) => self.check_unary(unary),
            Expr::Call(call) => self.check_call(call),
            Expr::Assignment(assign) => self.check_assignment(assign),
            Expr::FieldAssignment(fa) => self.check_field_assignment(fa),
            Expr::FieldAccess(fa) => self.check_field_access(fa),
            Expr::MethodCall(mc) => self.check_method_call(mc),
            Expr::StructLiteral(sl) => self.check_struct_literal(sl),
            Expr::Match(m) => self.check_match(m),
            Expr::ListLiteral(list) => self.check_list_literal(list),
            Expr::MapLiteral(map) => self.check_map_literal(map),
            Expr::Try(try_expr) => self.check_try(try_expr),
            Expr::StringInterpolation(interp) => self.check_string_interpolation(interp),
            Expr::Lambda(lambda) => self.check_lambda(lambda),
        };
        self.expr_types.insert(expr.span(), ty.clone());
        ty
    }

    /// Resolves an identifier to its type by looking it up in the scope stack,
    /// falling back to zero-field enum variants if not found as a variable.
    fn check_ident(&mut self, ident: &IdentExpr) -> Type {
        if let Some(info) = self.scopes.lookup(&ident.name) {
            info.ty.clone()
        } else {
            // Check if it's an enum variant with no fields
            let resolved = self
                .enums
                .iter()
                .find(|(_, info)| {
                    info.variants
                        .iter()
                        .any(|(n, fields)| n == &ident.name && fields.is_empty())
                })
                .map(|(name, info)| {
                    if info.type_params.is_empty() {
                        Type::Named(name.clone())
                    } else {
                        let args = info
                            .type_params
                            .iter()
                            .map(|p| Type::TypeVar(p.clone()))
                            .collect();
                        Type::Generic(name.clone(), args)
                    }
                });
            if let Some(ty) = resolved {
                return ty;
            }
            self.error(format!("undefined variable `{}`", ident.name), ident.span);
            Type::Error
        }
    }

    /// Type-checks a variable assignment (`x = expr`), verifying the variable
    /// is mutable and the value type is compatible with the declared type.
    fn check_assignment(&mut self, assign: &AssignmentExpr) -> Type {
        let value_type = self.check_expr(&assign.value);
        if let Some(info) = self.scopes.lookup(&assign.name).cloned() {
            if !info.is_mut {
                self.error(
                    format!("cannot assign to immutable variable `{}`", assign.name),
                    assign.span,
                );
            }
            let var_type = info.ty;
            if !var_type.is_error()
                && !value_type.is_error()
                && !self.types_compatible(&var_type, &value_type)
            {
                self.error(
                    format!(
                        "type mismatch: cannot assign `{}` to variable `{}` of type `{}`",
                        value_type, assign.name, var_type
                    ),
                    assign.span,
                );
            }

            var_type
        } else {
            self.error(format!("undefined variable `{}`", assign.name), assign.span);
            Type::Error
        }
    }

    /// Type-checks a field assignment (`obj.field = expr`), verifying the root
    /// variable is mutable, the field exists on the struct, and the value type
    /// is compatible with the field's declared type.
    fn check_field_assignment(&mut self, fa: &FieldAssignmentExpr) -> Type {
        let value_type = self.check_expr(&fa.value);
        let root_name = self.extract_root_ident(&fa.object);
        if let Some(ref name) = root_name
            && let Some(info) = self.scopes.lookup(name)
            && !info.is_mut
        {
            self.error(
                format!("cannot assign to field of immutable variable `{}`", name),
                fa.span,
            );
        }
        let obj_type = self.check_expr(&fa.object);
        let (type_name, bindings) = self.extract_type_name_and_bindings(&obj_type);
        if let Some(ref tn) = type_name
            && let Some(struct_info) = self.structs.get(tn).cloned()
        {
            if let Some((_, field_type)) = struct_info.fields.iter().find(|(n, _)| n == &fa.field) {
                let resolved = Self::substitute(field_type, &bindings);
                if !value_type.is_error()
                    && !resolved.is_error()
                    && !self.types_compatible(&resolved, &value_type)
                {
                    self.error(
                        format!(
                            "type mismatch: field `{}` is `{}` but assigned `{}`",
                            fa.field, resolved, value_type
                        ),
                        fa.span,
                    );
                }
                return resolved;
            }
            self.error(
                format!("struct `{}` has no field `{}`", tn, fa.field),
                fa.span,
            );
            return Type::Error;
        }
        if !obj_type.is_error() {
            self.error(
                format!("cannot assign to field on non-struct type `{}`", obj_type),
                fa.span,
            );
        }
        Type::Error
    }

    /// Type-checks a field access (`obj.field`), verifying the field exists on
    /// the struct and returning the field's type (with generic substitution).
    fn check_field_access(&mut self, fa: &FieldAccessExpr) -> Type {
        let obj_type = self.check_expr(&fa.object);
        let (type_name, bindings) = self.extract_type_name_and_bindings(&obj_type);
        if let Some(ref tn) = type_name
            && let Some(struct_info) = self.structs.get(tn).cloned()
        {
            if let Some((_, field_type)) = struct_info.fields.iter().find(|(n, _)| n == &fa.field) {
                return Self::substitute(field_type, &bindings);
            }
            self.error(
                format!("struct `{}` has no field `{}`", tn, fa.field),
                fa.span,
            );
        }
        if obj_type.is_error() {
            return Type::Error;
        }
        Type::Error
    }

    /// Type-checks a method call (`obj.method(args)`), dispatching to built-in
    /// methods for `List`, `String`, `Map`, `Option`, and `Result`, or looking
    /// up user-defined methods and trait-bounded methods.
    fn check_method_call(&mut self, mc: &MethodCallExpr) -> Type {
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

        // User-defined methods
        if let Some(type_methods) = self.methods.get(&type_name).cloned()
            && let Some(method_info) = type_methods.get(&mc.method)
        {
            self.check_method_args(mc, &method_info.params, &bindings);
            return Self::substitute(&method_info.return_type, &bindings);
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
    fn check_method_args(
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
    fn check_struct_literal(&mut self, sl: &StructLiteralExpr) -> Type {
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
                    struct_info.fields.iter().map(|(_, t)| t.clone()).collect();
                let bindings = self.infer_type_args(&field_types, &arg_types);
                for (i, arg) in sl.args.iter().enumerate() {
                    let expected = Self::substitute(&struct_info.fields[i].1, &bindings);
                    if !arg_types[i].is_error()
                        && !expected.is_error()
                        && !expected.has_type_vars()
                        && arg_types[i] != expected
                    {
                        self.error(
                            format!(
                                "field `{}`: expected `{}` but got `{}`",
                                struct_info.fields[i].0, expected, arg_types[i]
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
                    let (ref field_name, ref field_type) = struct_info.fields[i];
                    if !arg_type.is_error()
                        && !field_type.is_error()
                        && !self.types_compatible(field_type, &arg_type)
                    {
                        self.error(
                            format!(
                                "field `{}`: expected `{}` but got `{}`",
                                field_name, field_type, arg_type
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
                let bindings = self.infer_type_args(&variant_types, &arg_types);
                for (i, arg) in sl.args.iter().enumerate() {
                    let expected = Self::substitute(&variant_types[i], &bindings);
                    if !arg_types[i].is_error()
                        && !expected.is_error()
                        && !expected.has_type_vars()
                        && arg_types[i] != expected
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

    /// Type-checks a list literal (`[expr, ...]`), verifying all elements have
    /// compatible types and returning `List<T>` where `T` is the element type.
    fn check_list_literal(&mut self, list: &ListLiteralExpr) -> Type {
        if list.elements.is_empty() {
            return crate::types::list_of(Type::TypeVar("T".to_string()));
        }
        let first_type = self.check_expr(&list.elements[0]);
        for elem in list.elements.iter().skip(1) {
            let elem_type = self.check_expr(elem);
            if !first_type.is_error()
                && !elem_type.is_error()
                && !self.types_compatible(&first_type, &elem_type)
            {
                self.error(
                    format!(
                        "list element type mismatch: expected {} but got {}",
                        first_type, elem_type
                    ),
                    elem.span(),
                );
            }
        }
        crate::types::list_of(first_type)
    }

    /// Type-checks a map literal (`{key: value, ...}`), verifying all keys
    /// share one type and all values share one type, returning `Map<K, V>`.
    fn check_map_literal(&mut self, map: &MapLiteralExpr) -> Type {
        if map.entries.is_empty() {
            return crate::types::map_of(
                Type::TypeVar("K".to_string()),
                Type::TypeVar("V".to_string()),
            );
        }
        let (first_key, first_val) = &map.entries[0];
        let key_type = self.check_expr(first_key);
        let val_type = self.check_expr(first_val);
        for (k, v) in map.entries.iter().skip(1) {
            let kt = self.check_expr(k);
            let vt = self.check_expr(v);
            if !key_type.is_error() && !kt.is_error() && !self.types_compatible(&key_type, &kt) {
                self.error(
                    format!(
                        "map key type mismatch: expected {} but got {}",
                        key_type, kt
                    ),
                    k.span(),
                );
            }
            if !val_type.is_error() && !vt.is_error() && !self.types_compatible(&val_type, &vt) {
                self.error(
                    format!(
                        "map value type mismatch: expected {} but got {}",
                        val_type, vt
                    ),
                    v.span(),
                );
            }
        }
        crate::types::map_of(key_type, val_type)
    }

    /// Type-checks the `?` (try) operator, verifying the operand is
    /// `Result<T, E>` or `Option<T>` and that the enclosing function's return
    /// type is compatible for early return. Evaluates to the inner type `T`.
    fn check_try(&mut self, try_expr: &TryExpr) -> Type {
        let operand_type = self.check_expr(&try_expr.operand);
        if operand_type.is_error() {
            return Type::Error;
        }
        let (base_name, inner_type) = match &operand_type {
            Type::Generic(name, args) if name == "Result" && args.len() == 2 => {
                ("Result", args[0].clone())
            }
            Type::Generic(name, args) if name == "Option" && args.len() == 1 => {
                ("Option", args[0].clone())
            }
            _ => {
                self.error(
                    format!(
                        "the `?` operator can only be applied to Result<T, E> or Option<T>, got {}",
                        operand_type
                    ),
                    try_expr.span,
                );
                return Type::Error;
            }
        };
        if let Some(ref return_type) = self.current_return_type {
            match return_type {
                Type::Generic(name, _) if name == base_name => {}
                Type::TypeVar(_) => {}
                _ => {
                    self.error(
                        format!(
                            "the `?` operator requires the enclosing function to return {}, but it returns {}",
                            base_name, return_type
                        ),
                        try_expr.span,
                    );
                }
            }
        }
        inner_type
    }

    /// Type-checks a string interpolation expression, visiting each embedded
    /// expression segment. Always returns `String`.
    fn check_string_interpolation(&mut self, interp: &StringInterpolationExpr) -> Type {
        for segment in &interp.segments {
            if let StringSegment::Expr(expr) = segment {
                self.check_expr(expr);
            }
        }
        Type::String
    }

    /// Type-checks a lambda (anonymous function) expression, performing
    /// free-variable analysis for captures, validating the body against
    /// the declared return type, and returning the function type.
    fn check_lambda(&mut self, lambda: &LambdaExpr) -> Type {
        let param_types: Vec<Type> = lambda
            .params
            .iter()
            .map(|p| self.resolve_type_expr(&p.type_annotation))
            .collect();
        let return_type = lambda
            .return_type
            .as_ref()
            .map(|t| self.resolve_type_expr(t))
            .unwrap_or(Type::Void);

        let param_names: Vec<String> = lambda.params.iter().map(|p| p.name.clone()).collect();
        let free_vars =
            phoenix_parser::free_vars::collect_free_variables(&lambda.body, &param_names);
        let mut captures = Vec::new();
        for name in &free_vars {
            if let Some(info) = self.scopes.lookup(name) {
                captures.push(CaptureInfo {
                    name: name.clone(),
                    is_mut: info.is_mut,
                });
            }
        }
        self.lambda_captures.insert(lambda.span, captures);

        let prev_return = self.current_return_type.take();
        self.current_return_type = Some(return_type.clone());
        self.scopes.push();
        for (param, ty) in lambda.params.iter().zip(param_types.iter()) {
            self.scopes.define(
                param.name.clone(),
                VarInfo {
                    ty: ty.clone(),
                    is_mut: false,
                },
            );
        }
        let block_type = self.check_block_type(&lambda.body);
        if !return_type.is_error()
            && !block_type.is_error()
            && return_type != Type::Void
            && !self.types_compatible(&return_type, &block_type)
        {
            self.error(
                format!(
                    "lambda return type mismatch: expected `{}` but body evaluates to `{}`",
                    return_type, block_type
                ),
                lambda.span,
            );
        }
        self.scopes.pop();
        self.current_return_type = prev_return;

        Type::Function(param_types, Box::new(return_type))
    }

    fn check_match(&mut self, m: &MatchExpr) -> Type {
        let subject_type = self.check_expr(&m.subject);
        let mut result_type: Option<Type> = None;
        let (base_name, bindings) = self.extract_type_name_and_bindings(&subject_type);

        for arm in &m.arms {
            self.scopes.push();

            // Bind pattern variables
            match &arm.pattern {
                Pattern::Wildcard(_) | Pattern::Literal(_) => {}
                Pattern::Binding(name, _) => {
                    self.scopes.define(
                        name.clone(),
                        VarInfo {
                            ty: subject_type.clone(),
                            is_mut: false,
                        },
                    );
                }
                Pattern::Variant(vp) => {
                    // Find the enum and variant to get field types
                    if let Some(ref enum_name) = base_name
                        && let Some(enum_info) = self.enums.get(enum_name).cloned()
                        && let Some((_, variant_types)) =
                            enum_info.variants.iter().find(|(n, _)| n == &vp.variant)
                    {
                        // Validate binding count matches variant field count
                        if vp.bindings.len() != variant_types.len() {
                            self.error(
                                format!(
                                    "variant `{}` has {} field(s) but pattern has {} binding(s)",
                                    vp.variant,
                                    variant_types.len(),
                                    vp.bindings.len()
                                ),
                                vp.span,
                            );
                        }
                        for (i, binding) in vp.bindings.iter().enumerate() {
                            if binding != "_" && i < variant_types.len() {
                                let ty = Self::substitute(&variant_types[i], &bindings);
                                self.scopes
                                    .define(binding.clone(), VarInfo { ty, is_mut: false });
                            }
                        }
                    }
                }
            }

            // Reject break/continue inside match arms — they cannot propagate
            // to an enclosing loop through the expression evaluator.
            if let MatchBody::Block(b) = &arm.body {
                for stmt in &b.statements {
                    if let Statement::Break(span) = stmt {
                        self.error(
                            "`break` is not allowed inside match arms".to_string(),
                            *span,
                        );
                    }
                    if let Statement::Continue(span) = stmt {
                        self.error(
                            "`continue` is not allowed inside match arms".to_string(),
                            *span,
                        );
                    }
                }
            }

            let arm_type = match &arm.body {
                MatchBody::Expr(e) => self.check_expr(e),
                MatchBody::Block(b) => self.check_block_type(b),
            };

            // Arms that end with return diverge and are compatible with any
            // type — skip them for type unification.  (break/continue are now
            // rejected above, so only return causes divergence here.)
            let diverges = match &arm.body {
                MatchBody::Block(b) => Self::block_diverges(b),
                MatchBody::Expr(_) => false,
            };

            if !diverges {
                if let Some(ref expected) = result_type {
                    if !arm_type.is_error()
                        && !expected.is_error()
                        && !self.types_compatible(expected, &arm_type)
                    {
                        let span = match &arm.pattern {
                            Pattern::Wildcard(s) | Pattern::Binding(_, s) => *s,
                            Pattern::Literal(lit) => lit.span,
                            Pattern::Variant(vp) => vp.span,
                        };
                        self.error(
                            format!(
                                "match arm type mismatch: expected `{}` but got `{}`",
                                expected, arm_type
                            ),
                            span,
                        );
                    }
                } else {
                    result_type = Some(arm_type);
                }
            }

            self.scopes.pop();
        }

        // Check exhaustiveness for enum match expressions
        self.check_match_exhaustiveness(m, &base_name);

        result_type.unwrap_or(Type::Void)
    }

    /// Warns if a match on an enum type does not cover all variants and has no
    /// wildcard or binding catch-all pattern.
    /// Validates that a `match` expression covers all variants of the matched
    /// enum.  A wildcard (`_`) or binding pattern makes the match exhaustive.
    fn check_match_exhaustiveness(&mut self, m: &MatchExpr, base_name: &Option<String>) {
        let enum_name = match base_name {
            Some(name) => name,
            None => return,
        };
        let enum_info = match self.enums.get(enum_name).cloned() {
            Some(info) => info,
            None => return,
        };

        // If there's a wildcard or binding catch-all, the match is exhaustive
        for arm in &m.arms {
            match &arm.pattern {
                Pattern::Wildcard(_) | Pattern::Binding(_, _) => return,
                _ => {}
            }
        }

        // Collect covered variant names
        let covered: Vec<&str> = m
            .arms
            .iter()
            .filter_map(|arm| {
                if let Pattern::Variant(vp) = &arm.pattern {
                    Some(vp.variant.as_str())
                } else {
                    None
                }
            })
            .collect();

        let missing: Vec<&str> = enum_info
            .variants
            .iter()
            .filter(|(name, _)| !covered.contains(&name.as_str()))
            .map(|(name, _)| name.as_str())
            .collect();

        if !missing.is_empty() {
            self.error(
                format!(
                    "non-exhaustive match: missing variant(s) {}",
                    missing.join(", ")
                ),
                m.span,
            );
        }
    }

    /// Type-checks a binary expression (arithmetic, comparison, logical) and
    /// returns the result type.
    fn check_binary(&mut self, binary: &BinaryExpr) -> Type {
        let left = self.check_expr(&binary.left);
        let right = self.check_expr(&binary.right);

        if left.is_error() || right.is_error() {
            return Type::Error;
        }

        match binary.op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                if left == right && left.is_numeric() {
                    left
                } else if left == Type::String
                    && right == Type::String
                    && binary.op == BinaryOp::Add
                {
                    Type::String
                } else {
                    self.error(
                        format!("cannot apply `{}` to {} and {}", binary.op, left, right),
                        binary.span,
                    );
                    Type::Error
                }
            }
            BinaryOp::Eq | BinaryOp::NotEq => {
                if !self.types_compatible(&left, &right) {
                    self.error(
                        format!("cannot compare {} and {}", left, right),
                        binary.span,
                    );
                    Type::Error
                } else {
                    Type::Bool
                }
            }
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                if self.types_compatible(&left, &right)
                    && (left.is_numeric() || left == Type::String)
                {
                    Type::Bool
                } else {
                    self.error(
                        format!("cannot compare {} and {} with `{}`", left, right, binary.op),
                        binary.span,
                    );
                    Type::Error
                }
            }
            BinaryOp::And | BinaryOp::Or => {
                if left != Type::Bool {
                    self.error(
                        format!("left operand of `{}` must be Bool, got {}", binary.op, left),
                        binary.span,
                    );
                }
                if right != Type::Bool {
                    self.error(
                        format!(
                            "right operand of `{}` must be Bool, got {}",
                            binary.op, right
                        ),
                        binary.span,
                    );
                }
                Type::Bool
            }
        }
    }

    /// Type-checks a unary expression (negation `-` or logical `not`).
    fn check_unary(&mut self, unary: &UnaryExpr) -> Type {
        let operand = self.check_expr(&unary.operand);
        if operand.is_error() {
            return Type::Error;
        }

        match unary.op {
            UnaryOp::Neg => {
                if operand.is_numeric() {
                    operand
                } else {
                    self.error(format!("cannot negate {}", operand), unary.span);
                    Type::Error
                }
            }
            UnaryOp::Not => {
                if operand == Type::Bool {
                    Type::Bool
                } else {
                    self.error(format!("cannot apply `not` to {}", operand), unary.span);
                    Type::Error
                }
            }
        }
    }

    /// Type-checks a function call expression, resolving the callee and
    /// validating argument count and types.  Handles named arguments and
    /// default parameter values.
    fn check_call(&mut self, call: &CallExpr) -> Type {
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
            let bindings =
                self.infer_and_check_call_generics(func_name, func_info, call, positional_count);
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
    /// substituted parameter type, and validates trait bounds. Returns the
    /// inferred type bindings map.
    fn infer_and_check_call_generics(
        &mut self,
        func_name: &str,
        func_info: &FunctionInfo,
        call: &CallExpr,
        positional_count: usize,
    ) -> HashMap<String, Type> {
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
        let bindings = self.infer_type_args(&func_info.params, &arg_types);

        // Type-check provided args against substituted param types
        for (i, arg) in call.args.iter().enumerate() {
            let expected = Self::substitute(&func_info.params[i], &bindings);
            if !arg_types[i].is_error()
                && !expected.is_error()
                && !expected.has_type_vars()
                && arg_types[i] != expected
            {
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
                if !arg_types[idx].is_error()
                    && !expected.is_error()
                    && !expected.has_type_vars()
                    && arg_types[idx] != expected
                {
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

        bindings
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

    /// Walks a chain of field accesses to find the root variable name.
    ///
    /// For `a.b.c`, this returns `Some("a")`. For non-identifier roots (e.g.
    /// a function call result), returns `None`.
    fn extract_root_ident(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Ident(ident) => Some(ident.name.clone()),
            Expr::FieldAccess(fa) => self.extract_root_ident(&fa.object),
            _ => None,
        }
    }
}
