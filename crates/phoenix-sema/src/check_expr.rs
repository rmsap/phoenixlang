use crate::checker::Checker;
use crate::scope::VarInfo;
use crate::types::Type;
use phoenix_parser::ast::{
    AssignmentExpr, BinaryExpr, BinaryOp, CaptureInfo, ElseBranch, Expr, FieldAccessExpr,
    FieldAssignmentExpr, IdentExpr, IfExpr, LambdaExpr, ListLiteralExpr, LiteralKind,
    MapLiteralExpr, MatchBody, MatchExpr, Pattern, Statement, StringInterpolationExpr,
    StringSegment, TryExpr, UnaryExpr, UnaryOp,
};

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
            Expr::If(if_expr) => self.check_if_expr(if_expr),
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
        if let Some(info) = self.scopes.lookup(&ident.name).cloned() {
            self.record_reference(
                ident.span,
                crate::checker::SymbolKind::Variable,
                ident.name.clone(),
            );
            info.ty
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
            if let Some(field) = struct_info.fields.iter().find(|f| f.name == fa.field) {
                let resolved = Self::substitute(&field.ty, &bindings);
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
            if let Some(field) = struct_info.fields.iter().find(|f| f.name == fa.field) {
                let result = Self::substitute(&field.ty, &bindings);
                self.record_reference(
                    fa.span,
                    crate::checker::SymbolKind::Field {
                        struct_name: tn.clone(),
                    },
                    fa.field.clone(),
                );
                return result;
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
                    definition_span: param.span,
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
                Pattern::Binding(name, span) => {
                    self.scopes.define(
                        name.clone(),
                        VarInfo {
                            ty: subject_type.clone(),
                            is_mut: false,
                            definition_span: *span,
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
                                self.scopes.define(
                                    binding.clone(),
                                    VarInfo {
                                        ty,
                                        is_mut: false,
                                        definition_span: arm.span,
                                    },
                                );
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

    /// Type-checks an `if`/`else if`/`else` expression.
    ///
    /// Mirrors [`Self::check_match`]: validates the condition is `Bool`, checks
    /// each branch in a fresh scope, and unifies the branch types.  A branch
    /// whose block diverges (ends in `return`/`break`/`continue`) does not
    /// contribute to unification.  If the `else` branch is missing, the
    /// expression has type `Void` (branches are still checked for diagnostics).
    fn check_if_expr(&mut self, if_expr: &IfExpr) -> Type {
        let cond_type = self.check_expr(&if_expr.condition);
        if !cond_type.is_error() && cond_type != Type::Bool {
            self.error(
                format!("if condition must be Bool, got {}", cond_type),
                if_expr.condition.span(),
            );
        }

        // Check the then-branch and record its type unless it diverges.
        self.scopes.push();
        let then_type = self.check_block_type(&if_expr.then_block);
        self.scopes.pop();
        let then_diverges = Self::block_diverges(&if_expr.then_block);

        // No else branch → Void (but we already checked the then block for diagnostics).
        let Some(else_branch) = &if_expr.else_branch else {
            return Type::Void;
        };

        let (else_type, else_diverges) = match else_branch {
            ElseBranch::Block(block) => {
                self.scopes.push();
                let ty = self.check_block_type(block);
                self.scopes.pop();
                (ty, Self::block_diverges(block))
            }
            ElseBranch::ElseIf(nested) => {
                let ty = self.check_if_expr(nested);
                (ty, Self::if_expr_diverges(nested))
            }
        };

        // Unify non-diverging branch types.
        let ty = match (then_diverges, else_diverges) {
            (true, true) => Type::Void,
            (true, false) => else_type,
            (false, true) => then_type,
            (false, false) => {
                if then_type.is_error() || else_type.is_error() {
                    Type::Error
                } else if self.types_compatible(&then_type, &else_type) {
                    then_type
                } else if self.types_compatible(&else_type, &then_type) {
                    else_type
                } else {
                    self.error(
                        format!(
                            "if/else branches have incompatible types: {} and {}",
                            then_type, else_type
                        ),
                        if_expr.span,
                    );
                    Type::Error
                }
            }
        };

        // Record the type so IR lowering can look it up by span.  Needed in
        // particular for nested `else if` chains, which recurse through
        // `check_if_expr` directly without passing through `check_expr`.
        self.expr_types.insert(if_expr.span, ty.clone());
        ty
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
                    self.error(format!("cannot apply `!` to {}", operand), unary.span);
                    Type::Error
                }
            }
        }
    }

    /// Walks a chain of field accesses to find the root variable name.
    ///
    /// For `a.b.c`, this returns `Some("a")`. For non-identifier roots (e.g.
    /// a function call result), returns `None`.
    pub(crate) fn extract_root_ident(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Ident(ident) => Some(ident.name.clone()),
            Expr::FieldAccess(fa) => self.extract_root_ident(&fa.object),
            _ => None,
        }
    }
}
