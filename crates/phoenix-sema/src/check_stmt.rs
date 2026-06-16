use crate::checker::Checker;
use crate::scope::VarInfo;
use crate::types::Type;
use phoenix_parser::ast::{
    Block, ElseBranch, Expr, ForSource, ForStmt, IfExpr, MatchBody, ReturnStmt, Statement, VarDecl,
    VarDeclTarget, WhileStmt,
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
                        //
                        // Unconditional (not gated on `has_type_vars`): a
                        // collection whose container type unified to a
                        // concrete type can still hold a phantom-typed
                        // element (`[Some(1), None]: List<Option<Int>>`),
                        // and `pin_inferred_type_to_annotation` recurses
                        // into elements to pin those. It is a no-op when
                        // nothing needs pinning.
                        if !init_type.is_error() {
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
    /// Non-empty collection literals recurse into their elements/entries
    /// (pinning each to the declared element type) *and* fall through to
    /// the leaf refinement for the container itself. The element recursion
    /// preserves coercions the container can't express — `[circle]:
    /// List<dyn Drawable>` leaves the concrete `circle` for IR lowering to
    /// coerce (the element pin is a no-op: `Circle` is a bare `Named`, not
    /// a generic to refine). The container pin only fires when the recorded
    /// container type still carries a type var (`[None, …]` records
    /// `List<Option<?>>` because `check_list_literal` takes the *first*
    /// element's type without cross-element unification), pinning it to the
    /// concrete annotation so the `ListAlloc`/`MapAlloc` result type can't
    /// reach a backend with a `__generic` enum argument. Non-collection
    /// payload-free literals (zero-field generic enum variants) are leaf
    /// expressions with no sub-values to coerce, so they pin directly.
    pub(crate) fn pin_inferred_type_to_annotation(&mut self, expr: &Expr, declared: &Type) {
        // Non-empty collections: recurse into each element/entry to pin it
        // to the declared element type, then fall through to the leaf
        // refinement so the *container* is pinned too. Both matter:
        //   - Elements: a list whose container type unified to a concrete
        //     type can still hold a phantom-typed element — e.g.
        //     `[Some(1), None, Some(3)]: List<Option<Int>>`, where `None`
        //     stayed `Option<?>`. Recursing pins that `None` to
        //     `Option<Int>` while leaving concrete elements (and
        //     concrete→`dyn` coercions) untouched, since leaf refinement is
        //     a no-op for them.
        //   - Container: `check_list_literal` records the container as
        //     `List<first_element_type>` (no cross-element unification), so
        //     a phantom-typed *first* element (`[None, Some(1)]`) leaves the
        //     container `List<Option<?>>`. `lower_list_literal` uses that
        //     recorded type as the `ListAlloc` result type, which the IR
        //     verifier rejects (a `__generic` enum arg). Falling through to
        //     leaf-refine the container to the concrete declared type closes
        //     that gap. Map containers behave identically via the first
        //     key/value.
        match expr {
            Expr::ListLiteral(list) if !list.elements.is_empty() => {
                if let Type::Generic(_, dec_args) = declared
                    && let Some(elem_ty) = dec_args.first().cloned()
                {
                    for el in &list.elements {
                        self.pin_inferred_type_to_annotation(el, &elem_ty);
                    }
                }
            }
            Expr::MapLiteral(map) if !map.entries.is_empty() => {
                if let Type::Generic(_, dec_args) = declared
                    && dec_args.len() == 2
                {
                    let (k_ty, v_ty) = (dec_args[0].clone(), dec_args[1].clone());
                    for (k, v) in &map.entries {
                        self.pin_inferred_type_to_annotation(k, &k_ty);
                        self.pin_inferred_type_to_annotation(v, &v_ty);
                    }
                }
            }
            // `if` / `match` in value position: the expected type applies
            // to each branch's trailing expression, so propagate it down.
            // A `None` (or other phantom-typed constructor) in one arm
            // would otherwise keep its unconstrained slot even though the
            // surrounding context fixes it (`function(x) -> Option<Int> {
            // if c { Some(x) } else { None } }`). Then fall through to the
            // leaf refinement so the `if` / `match` *expression itself* is
            // pinned too — its inferred type drives the lowered merge
            // block's parameter type, which must be concrete as well.
            Expr::If(if_expr) => {
                self.pin_if_branches(if_expr, declared);
            }
            Expr::Match(match_expr) => {
                for arm in &match_expr.arms {
                    match &arm.body {
                        MatchBody::Expr(e) => self.pin_inferred_type_to_annotation(e, declared),
                        MatchBody::Block(b) => {
                            if let Some(tail) = Self::block_tail_expr(b) {
                                self.pin_inferred_type_to_annotation(tail, declared);
                            }
                        }
                    }
                }
            }
            _ => {
                // Constructor-argument propagation: pin each constructor
                // argument to its field type derived from the *declared*
                // enum/struct type, resolving a nested phantom-typed
                // constructor from the outer context — `Ok(None)` against
                // `Result<Option<String>, String>` pins the inner `None`
                // to `Option<String>`; `Box(None)` against `Box<Int>`
                // (field `v: Option<T>`) pins `None` to `Option<Int>`. The
                // struct case matters most when the phantom is the *only*
                // field, so sema can't recover the param from the argument
                // alone. The declared type names the enum/struct, so the
                // variant/fields are looked up directly (no global scan /
                // ambiguity diagnostics). Then falls through to pin the
                // constructor expression itself.
                //
                // Skip the walk when the constructor's recorded type is
                // already fully concrete: that type is synthesized from the
                // arguments' types, so it carries a type var iff some nested
                // argument still needs pinning — a concrete recorded type
                // guarantees the walk would be a no-op. This keeps the
                // unconditional `pin` calls at every call/return boundary
                // cheap on the common all-concrete case.
                if self
                    .expr_types
                    .get(&expr.span())
                    .is_some_and(Type::has_type_vars)
                    && let (Some((cname, cargs)), Type::Generic(type_name, dec_args)) =
                        (Self::constructor_name_args(expr), declared)
                {
                    let field_tys = self.constructor_field_types(type_name, cname, cargs, dec_args);
                    if let Some(field_tys) = field_tys {
                        for (arg, fty) in cargs.iter().zip(field_tys.iter()) {
                            self.pin_inferred_type_to_annotation(arg, fty);
                        }
                    }
                }
            }
        }
        // Leaf refinement of the expression's own recorded type.
        self.refine_recorded_type(expr.span(), declared);
    }

    /// Fill the type-var holes in the type recorded for `span`, but only
    /// when the recorded type still has a type var, the annotation is the
    /// same generic constructor (so the rewrite pins params rather than
    /// re-typing across a coercion like `S` → `dyn Drawable`, where the
    /// recorded type is a bare `TypeVar`/`Named`, not a `Generic`), and the
    /// annotation is fully concrete — so we only refine *toward* a concrete
    /// type, never swap one type var for another in a generic context
    /// (where monomorphization does the resolution). These guards make the
    /// pinning safe to apply unconditionally at every boundary: it is a
    /// no-op unless a concrete context fills a hole.
    ///
    /// Keyed by `span` rather than `&Expr` so it can also refine the
    /// recorded type of a nested `else if`, whose own `IfExpr` is not an
    /// `Expr` we hold (see [`Self::pin_if_branches`]).
    fn refine_recorded_type(&mut self, span: phoenix_common::span::Span, declared: &Type) {
        let same_constructor = matches!(
            (self.expr_types.get(&span), declared),
            (Some(Type::Generic(rec_name, rec_args)), Type::Generic(dec_name, dec_args))
                if rec_name == dec_name
                    && rec_args.len() == dec_args.len()
                    && rec_args.iter().any(Type::has_type_vars)
                    && !dec_args.iter().any(Type::has_type_vars)
        );
        if same_constructor {
            self.expr_types.insert(span, declared.clone());
        }
    }

    /// The `(variant/struct name, positional args)` of an enum/struct
    /// constructor expression, whether written `Name(args)` (a [`Expr::Call`]
    /// on a bare identifier) or as a struct literal. `None` for any other
    /// expression. Used by the constructor-argument propagation in
    /// [`Self::pin_inferred_type_to_annotation`].
    ///
    /// This is a *syntactic* classifier: any call on a bare identifier
    /// qualifies, including ordinary function calls (`foo(x)`). That is
    /// harmless because the caller passes the name to
    /// [`Self::constructor_field_types`], which only yields field types when
    /// the name matches a variant of the declared enum or *is* the declared
    /// struct's own name, and the arities line up — a non-constructor call
    /// (or a call whose arity coincidentally matches the declared struct)
    /// falls through to `None` and the propagation is skipped.
    ///
    /// Only the *positional* args are returned: a `Call`'s `named_args` are
    /// intentionally dropped. Phoenix constructors are positional (a
    /// phantom constructor passed as a *named* argument to a function is
    /// pinned by the dedicated named-arg call sites in
    /// `check_expr_call.rs`, not by this nested-constructor walk), so a
    /// named-arg Call form is never a constructor that needs recursing
    /// into. If named-field construction is ever added, extend this to
    /// thread the named args through to `constructor_field_types`.
    fn constructor_name_args(expr: &Expr) -> Option<(&str, &[Expr])> {
        match expr {
            Expr::StructLiteral(sl) => Some((sl.name.as_str(), sl.args.as_slice())),
            Expr::Call(call) => match &call.callee {
                Expr::Ident(id) => Some((id.name.as_str(), call.args.as_slice())),
                _ => None,
            },
            _ => None,
        }
    }

    /// The field types of an enum-variant or struct constructor `cname`
    /// of the declared template `type_name<dec_args>`, with the declared
    /// type's arguments substituted for the template's generic params.
    /// Returns `None` when the name is neither a known enum nor struct,
    /// when the arities don't line up, (for an enum) when no variant
    /// matches `cname`, or (for a struct) when `cname` is not the struct's
    /// own name. Used by the constructor-argument propagation in
    /// [`Self::pin_inferred_type_to_annotation`].
    ///
    /// `type_name` comes from the declared `Type::Generic` and may be a
    /// bare user-source name or an already-qualified key, so lookups go
    /// through `lookup_enum` / `lookup_struct` (which canonicalize) rather
    /// than a raw map `get` — otherwise an aliased / unqualified enum name
    /// would silently miss, leaving the phantom slot unpinned for the IR
    /// verifier to reject.
    fn constructor_field_types(
        &self,
        type_name: &str,
        cname: &str,
        cargs: &[Expr],
        dec_args: &[Type],
    ) -> Option<Vec<Type>> {
        if let Some(info) = self.lookup_enum(type_name) {
            if info.type_params.len() != dec_args.len() {
                return None;
            }
            let (_, vtys) = info.variants.iter().find(|(n, _)| n == cname)?;
            if vtys.len() != cargs.len() {
                return None;
            }
            let bindings = Self::param_bindings(&info.type_params, dec_args);
            return Some(
                vtys.iter()
                    .map(|t| Self::substitute(t, &bindings))
                    .collect(),
            );
        }
        // Struct constructor: the constructor *is* the struct, so its
        // positional fields map directly. This is the path that resolves
        // a sole-phantom-field struct (`Box(None)`), where the param can't
        // be recovered from the argument's own type.
        //
        // The struct's name *is* the constructor name, so require `cname`
        // to resolve to the same struct as `type_name`. Without this, a
        // call on a bare identifier (`constructor_name_args` classifies
        // any such call as a constructor) that coincidentally matches the
        // declared struct's arity — `let x: Box<Int> = makeBox(None)`, or a
        // wrong-struct literal — would mis-pin its arguments to this
        // struct's field types. Compare canonical names so a qualified
        // declared name (`lib::Box`) and a bare source name (`Box`) match.
        if self.canonicalize_type_name(cname) != self.canonicalize_type_name(type_name) {
            return None;
        }
        let info = self.lookup_struct(type_name)?;
        if info.type_params.len() != dec_args.len() || info.fields.len() != cargs.len() {
            return None;
        }
        let bindings = Self::param_bindings(&info.type_params, dec_args);
        Some(
            info.fields
                .iter()
                .map(|f| Self::substitute(&f.ty, &bindings))
                .collect(),
        )
    }

    /// Zip a template's generic parameter names with the concrete type
    /// arguments from a declared type into a substitution map.
    fn param_bindings(
        type_params: &[String],
        dec_args: &[Type],
    ) -> std::collections::HashMap<String, Type> {
        type_params
            .iter()
            .cloned()
            .zip(dec_args.iter().cloned())
            .collect()
    }

    /// A block's trailing (implicit-value) expression, if its last
    /// statement is a bare expression. Shared by the `if` / `match`
    /// branch-pinning in [`Self::pin_inferred_type_to_annotation`].
    ///
    /// Returning `None` for a branch arm whose value does not arrive as a
    /// bare tail expression means that arm is conservatively *not* pinned.
    /// That is safe, not a gap: the IR verifier backstops any residual
    /// `__generic` enum arg, and a phantom in such a shape is inert under
    /// the immutability/empty-list reasoning. If a verifier rejection ever
    /// traces back to a match-block arm, this conservative miss is the
    /// first place to look.
    fn block_tail_expr(block: &Block) -> Option<&Expr> {
        match block.statements.last() {
            Some(Statement::Expression(es)) => Some(&es.expr),
            _ => None,
        }
    }

    /// Pin the trailing expression of each branch of an `if` (including
    /// `else if` chains and the `else` block) to `declared` — the
    /// expected-type propagation for `if`-in-value-position.
    fn pin_if_branches(&mut self, if_expr: &IfExpr, declared: &Type) {
        if let Some(tail) = Self::block_tail_expr(&if_expr.then_block) {
            self.pin_inferred_type_to_annotation(tail, declared);
        }
        match &if_expr.else_branch {
            Some(ElseBranch::Block(b)) => {
                if let Some(tail) = Self::block_tail_expr(b) {
                    self.pin_inferred_type_to_annotation(tail, declared);
                }
            }
            Some(ElseBranch::ElseIf(nested)) => {
                self.pin_if_branches(nested, declared);
                // The nested `else if` is itself a value expression whose
                // recorded type drives the *outer* merge block's parameter
                // (and its own inner merge block's). Refine it directly —
                // the top-level `Expr::If` arm only leaf-refines the
                // outermost `if`'s span, so without this a phantom in an
                // `else if` arm (`… else if c { None } else …`) would leave
                // the nested merge `Option<__generic>`.
                self.refine_recorded_type(nested.span, declared);
            }
            None => {}
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
                // Pin a returned constructor with unbound phantom type
                // params (`return Ok(99)` under `-> Result<Int, String>`)
                // to the declared return type. See
                // `pin_inferred_type_to_annotation`.
                self.pin_inferred_type_to_annotation(expr, &expected);
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

#[cfg(test)]
mod constructor_field_types_tests {
    //! Locks the struct-arity guard in
    //! [`super::Checker::constructor_field_types`]: a constructor call on a
    //! bare identifier whose name is *not* the declared struct must not be
    //! mis-pinned to that struct's field types, even when the arities and
    //! field shapes coincide.
    //!
    //! This guard only ever fires during *error recovery* on the live
    //! pipeline — a well-typed `let x: Box<Int> = …` forces the
    //! initializer's recorded type concrete, so the `has_type_vars` gate at
    //! the call site already skips the constructor walk, and the only way to
    //! reach the walk with a struct name that differs from the annotation is
    //! an already-ill-typed program. So it can't be exercised by a runnable
    //! matrix fixture; this direct unit test is the only thing that pins it.

    use crate::checker::Checker;
    use crate::types::Type;
    use phoenix_common::span::{SourceId, Span};
    use phoenix_lexer::lexer::tokenize;
    use phoenix_parser::ast::{Expr, IdentExpr};
    use phoenix_parser::parser;

    fn checker_with(source: &str) -> Checker {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
        let mut checker = Checker::new();
        checker.check_program(&program);
        checker
    }

    /// One dummy positional argument. `constructor_field_types` only
    /// inspects `cargs.len()` for the arity check, never the argument
    /// expressions themselves, so a placeholder identifier suffices.
    fn one_arg() -> Vec<Expr> {
        vec![Expr::Ident(IdentExpr {
            name: "x".into(),
            span: Span::new(SourceId(0), 0, 1),
        })]
    }

    #[test]
    fn struct_field_types_rejects_mismatched_constructor_name() {
        // Two same-shape generic structs. `Other` has `Box`'s arity and
        // field shape, so without the name guard `constructor_field_types`
        // for declared `Box<Int>` against a call named `Other` would happily
        // return `Box`'s substituted field types.
        let checker = checker_with(
            "struct Box<T> { v: Option<T> }\n\
             struct Other<T> { v: Option<T> }\n",
        );
        let args = one_arg();
        let int = vec![Type::Named("Int".into())];

        // Mismatched name: `Other` is not `Box` → `None`, even though the
        // arities line up.
        assert!(
            checker
                .constructor_field_types("Box", "Other", &args, &int)
                .is_none(),
            "a constructor name that isn't the declared struct must not \
             resolve to its field types"
        );

        // Positive control: the struct's own name resolves, substituting
        // `T = Int` into the field type `Option<T>` so the result carries no
        // residual type var. (Asserting concreteness rather than an exact
        // `Type` keeps this robust to how `Option`/`Int` are canonicalized.)
        let fields = checker
            .constructor_field_types("Box", "Box", &args, &int)
            .expect("the struct's own name must resolve to its field types");
        assert_eq!(fields.len(), 1, "Box has exactly one field");
        assert!(
            !fields[0].has_type_vars(),
            "substituting T = Int must leave no residual type var, got {:?}",
            fields[0]
        );
    }
}
