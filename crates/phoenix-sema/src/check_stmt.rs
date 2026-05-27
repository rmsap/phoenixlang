use crate::checker::Checker;
use crate::scope::VarInfo;
use crate::types::Type;
use phoenix_parser::ast::{
    Expr, ForSource, ForStmt, ReturnStmt, Statement, VarDecl, VarDeclTarget, WhileStmt,
};

impl Checker {
    /// Dispatches type-checking for a single statement.
    pub(crate) fn check_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::VarDecl(var) => self.check_var_decl(var),
            Statement::Expression(expr_stmt) => {
                self.check_expr(&expr_stmt.expr);
            }
            Statement::Return(ret) => self.check_return(ret),
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
            // `defer expr` — the result of `expr` is always discarded;
            // we only type-check that `expr` itself is well-formed.
            // Free variables in `expr` are resolved lazily at function
            // exit (not snapshotted at the defer point).
            //
            // The `return` / `?` rejection and the placement rule
            // (defer must be at the function/lambda body's outermost
            // statement level) are both enforced by
            // [`Self::check_defer_placement`], which runs once per
            // function/lambda body and only emits the inner
            // `return`/`?` diagnostics for top-level defers — so a
            // nested defer that *also* contains `return`/`?` produces
            // a single placement error, not two.
            Statement::Defer(d) => {
                self.check_expr(&d.expr);
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
                    // Record the resolved annotation so IR lowering can
                    // consult it for boundary coercions (e.g. `dyn Trait`
                    // wrapping). Skipped for Error types to avoid
                    // propagating partial results downstream.
                    if !declared_type.is_error() {
                        self.var_annotation_types
                            .insert(var.span, declared_type.clone());
                        // A payload-free literal initializer (empty `[]`,
                        // a zero-field generic enum variant) infers with an
                        // unconstrained type var — nothing drives inference.
                        // The variable takes the annotation, but the
                        // expression's recorded type (what IR lowering reads
                        // for the alloc's type args) keeps the type var and
                        // lowers to the `__generic` placeholder, which
                        // miscompiles on the WASM backend. Pin it to the
                        // annotation. See `pin_inferred_type_to_annotation`.
                        if !init_type.is_error() && init_type.has_type_vars() {
                            self.pin_inferred_type_to_annotation(&var.initializer, &declared_type);
                        }
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
                        definition_span: var.span,
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
                // Look up the struct definition through the module scope
                // so the right module's struct is resolved.
                let struct_info = self.lookup_struct(type_name).cloned();
                if let Some(info) = struct_info {
                    // Verify the initializer type is compatible with the struct type.
                    // Use the qualified key so cross-module destructuring matches
                    // the qualified `Type::Named` the initializer evaluates to.
                    let expected_type = Type::Named(self.qualify_in_current(type_name));
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
                        if let Some(field) = info.fields.iter().find(|f| &f.name == field_name) {
                            if !self.scopes.define(
                                field_name.clone(),
                                VarInfo {
                                    ty: field.ty.clone(),
                                    is_mut: var.is_mut,
                                    definition_span: var.span,
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

    /// Pin the recorded type of a *payload-free* literal initializer to
    /// its binding's declared type.
    ///
    /// The checker is single-pass (no bidirectional/expected-type
    /// threading), so a payload-free literal is inferred with an
    /// unconstrained type var before the annotation is seen: an empty
    /// `[]` infers as `List<T>`, a zero-field generic enum variant
    /// (`Empty` in `enum Box<T>`) infers as `Box<T>`. The *variable*
    /// takes the annotation, but the *expression*'s recorded type — what
    /// `phoenix-ir` lowering reads (`expr_types[span]`) for the
    /// `ListAlloc` / `MapAlloc` / `EnumAlloc` type args — keeps the type
    /// var. An unresolved type var lowers to the `__generic` placeholder,
    /// which the WASM backend sizes as a bare pointer (i32); that
    /// mismatches the real element/payload width when, e.g., a list
    /// method's closure ABI flattens it, and wasmparser rejects the
    /// module. Rewriting the recorded type to the annotation closes the
    /// hole.
    ///
    /// Two guards keep this from clobbering legitimate inference:
    ///
    /// - The recorded type must still carry a type var — a concretely
    ///   inferred expression is left exactly as the checker computed it.
    /// - The annotation must be a *refinement* of the inferred type: the
    ///   same generic constructor with its params pinned (`List<T>` →
    ///   `List<Int>`, `Box<T>` → `Box<Int>`), not a structurally
    ///   different coercion target. `let d: dyn Drawable = s` inside a
    ///   generic fn infers `s` as the bare type var `S` and relies on IR
    ///   lowering coercing `S` → `dyn Drawable` at the `let`; overwriting
    ///   `s`'s recorded type to `dyn Drawable` would erase the gap the
    ///   coercion needs (the `DynRef` wrap is skipped, leaving the raw
    ///   struct). The same-constructor check excludes that — `S` is a
    ///   bare `TypeVar`, not a `Generic`.
    ///
    /// Only *empty* collection literals are pinned: a non-empty
    /// collection may hold elements that themselves need coercion to the
    /// declared element type (`[circle]: List<dyn Drawable>`), and
    /// pinning the container type would skip that. Non-collection
    /// payload-free literals (zero-field generic enum variants) are leaf
    /// expressions with no sub-values to coerce, so they pin directly.
    fn pin_inferred_type_to_annotation(&mut self, expr: &Expr, declared: &Type) {
        // Refinement check: only fill type-var holes when the recorded
        // type still has a type var *and* the annotation is the same
        // generic constructor (so the rewrite pins params rather than
        // re-typing across a coercion like `S` → `dyn Drawable`).
        let same_constructor = matches!(
            (self.expr_types.get(&expr.span()), declared),
            (Some(Type::Generic(rec_name, rec_args)), Type::Generic(dec_name, dec_args))
                if rec_name == dec_name
                    && rec_args.len() == dec_args.len()
                    && rec_args.iter().any(Type::has_type_vars)
        );
        if !same_constructor {
            return;
        }
        match expr {
            // Non-empty collections may carry elements needing their own
            // coercion to the declared element type — pinning the
            // container would skip it. Leave them for lowering to handle.
            Expr::ListLiteral(list) if !list.elements.is_empty() => {}
            Expr::MapLiteral(map) if !map.entries.is_empty() => {}
            _ => {
                self.expr_types.insert(expr.span(), declared.clone());
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
                definition_span: f.span,
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
