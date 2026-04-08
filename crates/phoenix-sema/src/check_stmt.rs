use crate::checker::Checker;
use crate::scope::VarInfo;
use crate::types::Type;
use phoenix_parser::ast::*;

impl Checker {
    /// Dispatches type-checking for a single statement.
    pub(crate) fn check_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::VarDecl(var) => self.check_var_decl(var),
            Statement::Expression(expr_stmt) => {
                self.check_expr(&expr_stmt.expr);
            }
            Statement::Return(ret) => self.check_return(ret),
            Statement::If(if_stmt) => self.check_if(if_stmt),
            Statement::While(w) => self.check_while(w),
            Statement::For(f) => self.check_for(f),
            // `break` is only valid inside a loop body. The checker tracks
            // nesting depth via `loop_depth`, incremented when entering a
            // `while` or `for` and decremented on exit.
            Statement::Break(span) => {
                if self.loop_depth == 0 {
                    self.error("`break` outside of loop".to_string(), *span);
                }
            }
            // `continue` follows the same nesting rules as `break`.
            Statement::Continue(span) => {
                if self.loop_depth == 0 {
                    self.error("`continue` outside of loop".to_string(), *span);
                }
            }
        }
    }

    /// Type-checks a variable declaration.
    ///
    /// When a type annotation is present, the initializer is checked against
    /// the declared type.  When the annotation is omitted, the variable's type
    /// is inferred from the initializer expression.
    fn check_var_decl(&mut self, var: &VarDecl) {
        let init_type = self.check_expr(&var.initializer);

        match &var.target {
            VarDeclTarget::Simple(name) => {
                let var_type = if let Some(ref type_ann) = var.type_annotation {
                    let declared_type = self.resolve_type_expr(type_ann);
                    if !declared_type.is_error()
                        && !init_type.is_error()
                        && !self.types_compatible(&declared_type, &init_type)
                    {
                        self.error(
                            format!(
                                "type mismatch: variable `{}` declared as `{}` but initialized with `{}`",
                                name, declared_type, init_type
                            ),
                            var.span,
                        );
                    }
                    declared_type
                } else {
                    // Type inference: use the initializer's type.
                    if !init_type.is_error() && init_type == Type::Void {
                        self.error(
                            format!("cannot infer type for `{}`: initializer has type Void (add a type annotation)", name),
                            var.span,
                        );
                    }
                    if !init_type.is_error() && init_type.has_type_vars() {
                        self.error(
                            format!("cannot infer type for `{}`: initializer has ambiguous type `{}` (add a type annotation)", name, init_type),
                            var.span,
                        );
                    }
                    init_type
                };

                if !self.scopes.define(
                    name.clone(),
                    VarInfo {
                        ty: var_type,
                        is_mut: var.is_mut,
                    },
                ) {
                    self.error(
                        format!("variable `{}` is already defined in this scope", name),
                        var.span,
                    );
                }
            }
            VarDeclTarget::StructDestructure {
                type_name,
                field_names,
            } => {
                // Look up the struct definition
                let struct_info = self.structs.get(type_name).cloned();
                if let Some(info) = struct_info {
                    // Verify the initializer type is compatible with the struct type
                    let expected_type = Type::Named(type_name.clone());
                    if !init_type.is_error() && !self.types_compatible(&expected_type, &init_type) {
                        self.error(
                            format!(
                                "type mismatch: destructuring expects `{}` but got `{}`",
                                type_name, init_type
                            ),
                            var.span,
                        );
                    }

                    // For each field name in the destructuring, verify it exists and bind it
                    for field_name in field_names {
                        if let Some((_fname, ftype)) =
                            info.fields.iter().find(|(n, _)| n == field_name)
                        {
                            if !self.scopes.define(
                                field_name.clone(),
                                VarInfo {
                                    ty: ftype.clone(),
                                    is_mut: var.is_mut,
                                },
                            ) {
                                self.error(
                                    format!(
                                        "variable `{}` is already defined in this scope",
                                        field_name
                                    ),
                                    var.span,
                                );
                            }
                        } else {
                            self.error(
                                format!("struct `{}` has no field `{}`", type_name, field_name),
                                var.span,
                            );
                        }
                    }
                } else {
                    self.error(format!("unknown struct type `{}`", type_name), var.span);
                }
            }
        }
    }

    /// Type-checks a `return` statement against the enclosing function's
    /// declared return type.
    fn check_return(&mut self, ret: &ReturnStmt) {
        let expected = self.current_return_type.clone().unwrap_or(Type::Void);
        match &ret.value {
            Some(expr) => {
                let actual = self.check_expr(expr);
                if !expected.is_error()
                    && !actual.is_error()
                    && !self.types_compatible(&expected, &actual)
                {
                    self.error(
                        format!(
                            "return type mismatch: expected {} but got {}",
                            expected, actual
                        ),
                        ret.span,
                    );
                }
            }
            None => {
                if expected != Type::Void && !expected.is_error() {
                    self.error(
                        format!("expected return value of type {}", expected),
                        ret.span,
                    );
                }
            }
        }
    }

    /// Type-checks an `if` statement, ensuring the condition is `Bool` and
    /// recursively checking then/else branches.
    fn check_if(&mut self, if_stmt: &IfStmt) {
        let cond_type = self.check_expr(&if_stmt.condition);
        if !cond_type.is_error() && cond_type != Type::Bool {
            self.error(
                format!("if condition must be Bool, got {}", cond_type),
                if_stmt.condition.span(),
            );
        }

        self.scopes.push();
        self.check_block(&if_stmt.then_block);
        self.scopes.pop();

        if let Some(ref else_branch) = if_stmt.else_branch {
            match else_branch {
                ElseBranch::Block(block) => {
                    self.scopes.push();
                    self.check_block(block);
                    self.scopes.pop();
                }
                ElseBranch::ElseIf(elif) => {
                    self.check_if(elif);
                }
            }
        }
    }

    /// Type-checks a `while` loop, ensuring the condition is `Bool`.
    fn check_while(&mut self, w: &WhileStmt) {
        let cond_type = self.check_expr(&w.condition);
        if !cond_type.is_error() && cond_type != Type::Bool {
            self.error(
                format!("while condition must be Bool, got {}", cond_type),
                w.condition.span(),
            );
        }
        self.loop_depth += 1;
        self.scopes.push();
        self.check_block(&w.body);
        self.scopes.pop();
        self.loop_depth -= 1;
        if let Some(ref else_block) = w.else_block {
            self.scopes.push();
            self.check_block(else_block);
            self.scopes.pop();
        }
    }

    /// Type-checks a `for` loop (range-based or collection-based).
    fn check_for(&mut self, f: &ForStmt) {
        let var_type = match &f.source {
            ForSource::Range { start, end } => {
                let start_type = self.check_expr(start);
                let end_type = self.check_expr(end);
                if !start_type.is_error() && start_type != Type::Int {
                    self.error(
                        format!("for range start must be Int, got {}", start_type),
                        start.span(),
                    );
                }
                if !end_type.is_error() && end_type != Type::Int {
                    self.error(
                        format!("for range end must be Int, got {}", end_type),
                        end.span(),
                    );
                }
                if let Some(ref type_ann) = f.var_type {
                    let resolved = self.resolve_type_expr(type_ann);
                    if !resolved.is_error() && resolved != Type::Int {
                        self.error(
                            format!("for loop variable must be Int, got {}", resolved),
                            f.span,
                        );
                    }
                    resolved
                } else {
                    Type::Int
                }
            }
            ForSource::Iterable(iter_expr) => {
                let iter_type = self.check_expr(iter_expr);
                match &iter_type {
                    Type::Generic(name, args) if name == "List" => args
                        .first()
                        .cloned()
                        .unwrap_or(Type::TypeVar("T".to_string())),
                    _ if iter_type.is_error() => Type::Error,
                    _ => {
                        self.error(
                            format!("for...in requires a List, got {}", iter_type),
                            iter_expr.span(),
                        );
                        Type::Error
                    }
                }
            }
        };

        self.loop_depth += 1;
        self.scopes.push();
        self.scopes.define(
            f.var_name.clone(),
            VarInfo {
                ty: var_type,
                is_mut: false,
            },
        );
        self.check_block(&f.body);
        self.scopes.pop();
        self.loop_depth -= 1;
        if let Some(ref else_block) = f.else_block {
            self.scopes.push();
            self.check_block(else_block);
            self.scopes.pop();
        }
    }
}
