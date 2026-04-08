use crate::scope::{ScopeStack, VarInfo};
use crate::types::Type;
use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::span::Span;
use phoenix_parser::ast::*;
use std::collections::{HashMap, HashSet};

/// The result of semantic analysis, containing diagnostics and resolved
/// type information needed by downstream passes (interpreter or compiler).
pub struct CheckResult {
    /// Semantic errors and warnings found during analysis.
    pub diagnostics: Vec<Diagnostic>,
    /// Captured variables for each lambda expression, keyed by the lambda's
    /// source span. The interpreter/compiler uses this to set up closures
    /// without needing interior mutability on the AST.
    pub lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
    /// Registered function signatures (name → info).
    pub functions: HashMap<String, FunctionInfo>,
    /// Registered struct definitions (name → info).
    pub structs: HashMap<String, StructInfo>,
    /// Registered enum definitions (name → info).
    pub enums: HashMap<String, EnumInfo>,
    /// Registered methods (type_name → method_name → info).
    pub methods: HashMap<String, HashMap<String, MethodInfo>>,
    /// Registered trait declarations (trait_name → info).
    pub traits: HashMap<String, TraitInfo>,
    /// Set of (type_name, trait_name) pairs recording which types implement
    /// which traits.
    pub trait_impls: HashSet<(String, String)>,
    /// Registered type aliases (alias_name → info).
    pub type_aliases: HashMap<String, TypeAliasInfo>,
    /// Resolved type for each expression, keyed by the expression's source
    /// span. Populated during the checking pass so that downstream passes
    /// (IR lowering, codegen) can look up the type of any expression without
    /// re-running type inference.
    pub expr_types: HashMap<Span, Type>,
}

/// Information about a registered function signature, captured during the
/// first pass so that call sites can be type-checked in the second pass.
#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub type_params: Vec<String>,
    /// Trait bounds for type parameters (e.g. `[("T", ["Display"])]`).
    pub type_param_bounds: Vec<(String, Vec<String>)>,
    pub params: Vec<Type>,
    /// Parameter names, parallel to `params` (excludes `self`).
    pub param_names: Vec<String>,
    /// Indices of parameters that have default values (relative to the
    /// `params`/`param_names` vectors, i.e. excluding `self`).
    pub default_param_indices: Vec<usize>,
    pub return_type: Type,
}

/// Information about a registered trait.
#[derive(Debug, Clone)]
pub struct TraitInfo {
    pub type_params: Vec<String>,
    pub methods: Vec<TraitMethodInfo>,
}

/// Information about a method signature in a trait.
#[derive(Debug, Clone)]
pub struct TraitMethodInfo {
    pub name: String,
    pub params: Vec<Type>,
    pub return_type: Type,
}

/// Information about a registered struct definition (fields and generic params).
#[derive(Debug, Clone)]
pub struct StructInfo {
    pub type_params: Vec<String>,
    pub fields: Vec<(String, Type)>,
}

/// Information about a registered enum definition (variants and generic params).
#[derive(Debug, Clone)]
pub struct EnumInfo {
    pub type_params: Vec<String>,
    pub variants: Vec<(String, Vec<Type>)>,
}

/// Information about a method registered on a type (parameter types exclude `self`).
#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub params: Vec<Type>, // excludes self
    pub return_type: Type,
}

/// Information about a registered type alias.
///
/// Stores the generic parameters and the resolved target type so that the
/// checker can expand aliases when they appear in type positions.
#[derive(Debug, Clone)]
pub struct TypeAliasInfo {
    /// Generic type parameters declared on the alias (e.g. `["T"]` for
    /// `type StringResult<T> = Result<T, String>`). Empty for non-generic aliases.
    pub type_params: Vec<String>,
    /// The resolved target type that this alias expands to.
    pub target: Type,
}

/// The semantic checker for a Phoenix program.
pub struct Checker {
    pub(crate) scopes: ScopeStack,
    pub(crate) functions: HashMap<String, FunctionInfo>,
    pub(crate) structs: HashMap<String, StructInfo>,
    pub(crate) enums: HashMap<String, EnumInfo>,
    pub(crate) methods: HashMap<String, HashMap<String, MethodInfo>>, // type_name -> method_name -> info
    /// Registered trait declarations.
    pub(crate) traits: HashMap<String, TraitInfo>,
    /// Set of (type_name, trait_name) pairs recording which types implement which traits.
    pub(crate) trait_impls: HashSet<(String, String)>,
    /// Registered type aliases (`type Name = TypeExpr`).
    pub(crate) type_aliases: HashMap<String, TypeAliasInfo>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// Captured variables for each lambda, keyed by the lambda's source span.
    pub(crate) lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
    pub(crate) current_return_type: Option<Type>,
    /// Tracks whether we're currently inside a loop (for break/continue validation).
    pub(crate) loop_depth: usize,
    /// Type parameters currently in scope (e.g. inside a generic function body).
    pub(crate) current_type_params: Vec<String>,
    /// Trait bounds for the current function's type parameters.
    pub(crate) current_type_param_bounds: Vec<(String, Vec<String>)>,
    /// Resolved type for each expression, keyed by source span.
    pub(crate) expr_types: HashMap<Span, Type>,
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

impl Checker {
    /// Creates a new semantic checker with empty scope and type registries.
    pub fn new() -> Self {
        Self {
            scopes: ScopeStack::new(),
            functions: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            methods: HashMap::new(),
            traits: HashMap::new(),
            trait_impls: HashSet::new(),
            type_aliases: HashMap::new(),
            diagnostics: Vec::new(),
            lambda_captures: HashMap::new(),
            current_return_type: None,
            loop_depth: 0,
            current_type_params: Vec::new(),
            current_type_param_bounds: Vec::new(),
            expr_types: HashMap::new(),
        }
    }

    /// Temporarily sets `current_type_params` (and optionally bounds) for the
    /// duration of the closure, then restores the previous values.
    pub(crate) fn with_type_params<R>(
        &mut self,
        type_params: &[String],
        bounds: Option<&[(String, Vec<String>)]>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let prev_params = std::mem::replace(&mut self.current_type_params, type_params.to_vec());
        let prev_bounds =
            bounds.map(|b| std::mem::replace(&mut self.current_type_param_bounds, b.to_vec()));
        let result = f(self);
        self.current_type_params = prev_params;
        if let Some(prev) = prev_bounds {
            self.current_type_param_bounds = prev;
        }
        result
    }

    /// Checks an entire program using two passes: first registers all type and
    /// function signatures, then checks function and method bodies.
    ///
    /// Before processing user declarations, the built-in `Option<T>` and
    /// `Result<T, E>` enums and their methods are pre-registered so they are
    /// available without explicit declaration.
    pub fn check_program(&mut self, program: &Program) {
        self.register_builtins();

        // Pass 0: pre-register all type names (struct/enum) so that
        // self-referential types (e.g. `enum IntList { Cons(Int, IntList), Nil }`)
        // can reference themselves during field/variant type resolution.
        for decl in &program.declarations {
            match decl {
                Declaration::Struct(s) => {
                    self.structs.insert(
                        s.name.clone(),
                        StructInfo {
                            type_params: s.type_params.clone(),
                            fields: Vec::new(),
                        },
                    );
                }
                Declaration::Enum(e) => {
                    self.enums.insert(
                        e.name.clone(),
                        EnumInfo {
                            type_params: e.type_params.clone(),
                            variants: Vec::new(),
                        },
                    );
                }
                _ => {}
            }
        }

        // First pass: register all type/function signatures (field types now
        // resolve correctly because type names were pre-registered above).
        for decl in &program.declarations {
            match decl {
                Declaration::Function(func) => self.register_function(func),
                Declaration::Struct(s) => self.register_struct(s),
                Declaration::Enum(e) => self.register_enum(e),
                Declaration::Impl(imp) => self.register_impl(imp),
                Declaration::Trait(t) => self.register_trait(t),
                Declaration::TypeAlias(ta) => self.register_type_alias(ta),
            }
        }

        // Second pass: check bodies
        for decl in &program.declarations {
            match decl {
                Declaration::Function(func) => self.check_function(func),
                Declaration::Impl(imp) => self.check_impl(imp),
                Declaration::Struct(s) => {
                    self.check_inline_methods(&s.name, &s.methods, &s.trait_impls, s.span);
                }
                Declaration::Enum(e) => {
                    self.check_inline_methods(&e.name, &e.methods, &e.trait_impls, e.span);
                }
                Declaration::Trait(_) | Declaration::TypeAlias(_) => {}
            }
        }
    }

    // Registration methods (register_function, register_struct, etc.) are in
    // check_register.rs to keep this file focused on the checking pass.

    /// Type-checks a function body against its declared signature.
    fn check_function(&mut self, func: &FunctionDecl) {
        self.with_type_params(&func.type_params, Some(&func.type_param_bounds), |this| {
            let return_type = func
                .return_type
                .as_ref()
                .map(|t| this.resolve_type_expr(t))
                .unwrap_or(Type::Void);
            this.current_return_type = Some(return_type.clone());
            this.scopes.push();

            for param in &func.params {
                if param.name == "self" {
                    continue;
                }
                let ty = this.resolve_type_expr(&param.type_annotation);
                // Type-check default value expression, if present
                if let Some(ref default_expr) = param.default_value {
                    let default_ty = this.check_expr(default_expr);
                    if !default_ty.is_error()
                        && !ty.is_error()
                        && !this.types_compatible(&ty, &default_ty)
                    {
                        this.error(
                            format!(
                                "default value for parameter `{}`: expected `{}` but got `{}`",
                                param.name, ty, default_ty
                            ),
                            default_expr.span(),
                        );
                    }
                }
                this.scopes
                    .define(param.name.clone(), VarInfo { ty, is_mut: false });
            }

            this.check_block(&func.body);
            this.validate_implicit_return(func, &return_type);
            this.scopes.pop();
            this.current_return_type = None;
        });
    }

    /// Validates that a function with a non-Void return type produces a value.
    ///
    /// Checks for implicit return (trailing expression), if/else producing a
    /// value, or explicit `return` statements.
    fn validate_implicit_return(&mut self, func: &FunctionDecl, return_type: &Type) {
        if *return_type == Type::Void || return_type.is_error() {
            return;
        }
        match func.body.statements.last() {
            Some(Statement::Expression(expr_stmt)) => {
                let implicit_type = self.infer_expr_type(&expr_stmt.expr);
                if !implicit_type.is_error() && !self.types_compatible(return_type, &implicit_type)
                {
                    self.error(
                        format!(
                            "implicit return type mismatch: expected {} but got {}",
                            return_type, implicit_type
                        ),
                        expr_stmt.span,
                    );
                }
            }
            Some(Statement::If(if_stmt)) => {
                let implicit_type = self.infer_if_implicit_type(if_stmt);
                if implicit_type != Type::Void
                    && !implicit_type.is_error()
                    && !self.types_compatible(return_type, &implicit_type)
                {
                    self.error(
                        format!(
                            "implicit return type mismatch: expected {} but got {}",
                            return_type, implicit_type
                        ),
                        if_stmt.span,
                    );
                } else if implicit_type == Type::Void
                    && !func.body.statements.iter().any(Self::contains_return)
                {
                    self.error(
                        format!(
                            "function `{}` has return type `{}` but body does not return a value",
                            func.name, return_type
                        ),
                        func.span,
                    );
                }
            }
            _ => {
                if !func.body.statements.iter().any(Self::contains_return) {
                    self.error(
                        format!(
                            "function `{}` has return type `{}` but body does not return a value",
                            func.name, return_type
                        ),
                        func.span,
                    );
                }
            }
        }
    }

    /// Type-checks all methods in an `impl` block.
    fn check_impl(&mut self, imp: &ImplBlock) {
        let parent_type_params = self.parent_type_params(&imp.type_name);

        for func in &imp.methods {
            // Merge parent type params with the method's own type params
            let mut merged = parent_type_params.clone();
            merged.extend(func.type_params.iter().cloned());
            self.with_type_params(&merged, Some(&func.type_param_bounds), |this| {
                let return_type = func
                    .return_type
                    .as_ref()
                    .map(|t| this.resolve_type_expr(t))
                    .unwrap_or(Type::Void);
                this.current_return_type = Some(return_type);
                this.scopes.push();

                // Build proper self type with TypeVar args for generic types
                let self_type = if parent_type_params.is_empty() {
                    Type::from_name(&imp.type_name)
                } else {
                    let args: Vec<Type> = parent_type_params
                        .iter()
                        .map(|p| Type::TypeVar(p.clone()))
                        .collect();
                    Type::Generic(imp.type_name.clone(), args)
                };
                this.scopes.define(
                    "self".to_string(),
                    VarInfo {
                        ty: self_type,
                        is_mut: false,
                    },
                );

                for param in &func.params {
                    if param.name == "self" {
                        continue;
                    }
                    let ty = this.resolve_type_expr(&param.type_annotation);
                    this.scopes
                        .define(param.name.clone(), VarInfo { ty, is_mut: false });
                }

                this.check_block(&func.body);
                this.scopes.pop();
                this.current_return_type = None;
            });
        }
    }

    /// Checks all statements in a block without inferring a return type.
    pub(crate) fn check_block(&mut self, block: &Block) {
        for stmt in &block.statements {
            self.check_statement(stmt);
        }
    }

    /// Checks a block and returns its type.
    ///
    /// The type is determined by:
    /// 1. An explicit `return` statement, if present.
    /// 2. The last statement if it is an expression (implicit return).
    /// 3. `Void` otherwise.
    pub(crate) fn check_block_type(&mut self, block: &Block) -> Type {
        let mut return_type = Type::Void;
        for (i, stmt) in block.statements.iter().enumerate() {
            self.check_statement(stmt);
            if let Statement::Return(ret) = stmt {
                return_type = match &ret.value {
                    Some(expr) => self.infer_expr_type(expr),
                    None => Type::Void,
                };
            }
            // Last expression in block is an implicit return
            if i == block.statements.len() - 1
                && let Statement::Expression(expr_stmt) = stmt
            {
                return_type = self.infer_expr_type(&expr_stmt.expr);
            }
        }
        return_type
    }

    // Statement checking methods are in check_stmt.rs.
    // Expression checking methods are in check_expr.rs.
    // Type resolution and unification methods are in check_types.rs.

    /// Infers the implicit return type of an `if`/`else` chain.
    ///
    /// Returns the common type of the last expression in every branch when all
    /// branches (including `else`) end with a bare expression. Returns
    /// [`Type::Void`] when the chain is missing an `else` branch or any branch
    /// does not end with an expression.
    fn infer_if_implicit_type(&mut self, if_stmt: &IfStmt) -> Type {
        let then_type = self.infer_block_implicit_type(&if_stmt.then_block);
        if then_type == Type::Void {
            return Type::Void;
        }
        let else_type = match &if_stmt.else_branch {
            Some(ElseBranch::Block(b)) => self.infer_block_implicit_type(b),
            Some(ElseBranch::ElseIf(elif)) => self.infer_if_implicit_type(elif),
            None => return Type::Void, // no else → cannot guarantee a value
        };
        if else_type == Type::Void {
            return Type::Void;
        }
        // Both branches produce a value — check they're compatible.
        if self.types_compatible(&then_type, &else_type) {
            then_type
        } else if self.types_compatible(&else_type, &then_type) {
            else_type
        } else {
            self.error(
                format!(
                    "if/else branches have incompatible types: {} and {}",
                    then_type, else_type
                ),
                if_stmt.span,
            );
            Type::Error
        }
    }

    /// Returns the implicit return type of a block — the type of its last
    /// statement when that statement is a bare expression or an if/else chain
    /// whose branches all produce values. Returns [`Type::Void`] otherwise.
    fn infer_block_implicit_type(&mut self, block: &Block) -> Type {
        match block.statements.last() {
            Some(Statement::Expression(expr_stmt)) => self.infer_expr_type(&expr_stmt.expr),
            Some(Statement::If(if_stmt)) => self.infer_if_implicit_type(if_stmt),
            _ => Type::Void,
        }
    }

    /// Returns `true` if a block's last statement is a control-flow statement
    /// that diverges (break, continue, or return).  Diverging blocks are
    /// compatible with any expected type in match arm type checking.
    pub(crate) fn block_diverges(block: &Block) -> bool {
        matches!(
            block.statements.last(),
            Some(Statement::Break(_) | Statement::Continue(_) | Statement::Return(_))
        )
    }

    /// Returns `true` if a statement contains (or is) an explicit `return`.
    ///
    /// Recurses into `if`/`else` and loop bodies so that returns nested inside
    /// control flow are detected.  This is a conservative check -- it does not
    /// prove that *all* paths return, only that *at least one* return exists.
    fn contains_return(stmt: &Statement) -> bool {
        match stmt {
            Statement::Return(_) => true,
            Statement::If(if_stmt) => Self::if_contains_return(if_stmt),
            Statement::While(w) => w.body.statements.iter().any(Self::contains_return),
            Statement::For(f) => f.body.statements.iter().any(Self::contains_return),
            _ => false,
        }
    }

    /// Recursive helper for [`Self::contains_return`] -- checks an `if`/`else`
    /// chain for at least one explicit `return` statement.
    fn if_contains_return(if_stmt: &IfStmt) -> bool {
        if if_stmt
            .then_block
            .statements
            .iter()
            .any(Self::contains_return)
        {
            return true;
        }
        match &if_stmt.else_branch {
            Some(ElseBranch::Block(b)) => b.statements.iter().any(Self::contains_return),
            Some(ElseBranch::ElseIf(nested)) => Self::if_contains_return(nested),
            None => false,
        }
    }

    /// Validates that a method call has exactly `expected` positional arguments.
    /// Emits a diagnostic and returns `false` on mismatch.
    pub(crate) fn expect_arg_count(&mut self, mc: &MethodCallExpr, expected: usize) -> bool {
        if mc.args.len() != expected {
            self.error(
                format!(
                    "method `{}` takes {} argument(s), got {}",
                    mc.method,
                    expected,
                    mc.args.len()
                ),
                mc.span,
            );
            false
        } else {
            true
        }
    }

    /// Type-checks a single method argument at `index` against `expected` type.
    /// Emits a diagnostic on mismatch. Returns the actual type.
    pub(crate) fn check_method_arg(
        &mut self,
        mc: &MethodCallExpr,
        index: usize,
        expected: &Type,
    ) -> Type {
        let arg_type = self.check_expr(&mc.args[index]);
        if !arg_type.is_error()
            && !expected.is_error()
            && !self.types_compatible(expected, &arg_type)
        {
            self.error(
                format!(
                    "argument {} of `{}`: expected {} but got {}",
                    index + 1,
                    mc.method,
                    expected,
                    arg_type
                ),
                mc.args[index].span(),
            );
        }
        arg_type
    }

    /// Validates a closure argument: checks it's a Function type with the expected
    /// parameter count, optional parameter type checks, and optional return type check.
    /// Returns `Some((params, ret))` if it's a function type, `None` otherwise.
    pub(crate) fn check_closure_arg(
        &mut self,
        mc: &MethodCallExpr,
        arg_index: usize,
        expected_params: usize,
        expected_param_types: Option<&[&Type]>,
        expected_return: Option<&Type>,
    ) -> Option<(Vec<Type>, Type)> {
        let arg_type = self.check_expr(&mc.args[arg_index]);
        if let Type::Function(params, ret) = &arg_type {
            if params.len() != expected_params {
                self.error(
                    format!(
                        "{} callback must take {} parameter(s), got {}",
                        mc.method,
                        expected_params,
                        params.len()
                    ),
                    mc.args[arg_index].span(),
                );
            } else if let Some(expected) = expected_param_types {
                for (i, exp) in expected.iter().enumerate() {
                    if !exp.is_error()
                        && !params[i].is_error()
                        && !self.types_compatible(exp, &params[i])
                    {
                        self.error(
                            format!(
                                "{} callback parameter {}: expected {} but got {}",
                                mc.method,
                                i + 1,
                                exp,
                                params[i]
                            ),
                            mc.args[arg_index].span(),
                        );
                    }
                }
            }
            if let Some(exp_ret) = expected_return
                && !ret.is_error()
                && !exp_ret.is_error()
                && !self.types_compatible(exp_ret, ret)
            {
                self.error(
                    format!(
                        "{} callback must return {}, got {}",
                        mc.method, exp_ret, ret
                    ),
                    mc.args[arg_index].span(),
                );
            }
            return Some((params.clone(), (**ret).clone()));
        }
        None
    }

    /// Checks inline method and trait impl bodies from a type declaration.
    fn check_inline_methods(
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
            self.check_impl(&synthetic_impl);
        }
        for ti in trait_impls {
            let synthetic_impl = ImplBlock {
                type_name: type_name.to_string(),
                trait_name: Some(ti.trait_name.clone()),
                methods: ti.methods.clone(),
                span: ti.span,
            };
            self.check_impl(&synthetic_impl);
        }
    }

    /// Records a semantic error diagnostic at the given source span.
    pub(crate) fn error(&mut self, message: String, span: Span) {
        self.diagnostics.push(Diagnostic::error(message, span));
    }
}

/// Type-checks a Phoenix program and returns diagnostics and resolved type
/// information.
///
/// This is the main entry point for semantic analysis. It performs two passes:
/// 1. **Registration pass:** collects all type, function, and trait declarations.
/// 2. **Checking pass:** validates types, resolves names, and checks constraints.
///
/// The returned [`CheckResult`] contains:
/// - `diagnostics`: empty if the program is semantically valid.
/// - `lambda_captures`: captured variables for each lambda, keyed by span.
///
/// # Examples
///
/// A valid program produces no diagnostics:
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_common::span::SourceId;
/// use phoenix_parser::parser;
/// use phoenix_sema::checker::check;
///
/// let tokens = tokenize("function main() { print(42) }", SourceId(0));
/// let (program, parse_errors) = parser::parse(&tokens);
/// assert!(parse_errors.is_empty());
///
/// let result = check(&program);
/// assert!(result.diagnostics.is_empty());
/// ```
///
/// A program with a type error produces diagnostics:
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_common::span::SourceId;
/// use phoenix_parser::parser;
/// use phoenix_sema::checker::check;
///
/// let tokens = tokenize(r#"function main() { let x: Int = "hello" }"#, SourceId(0));
/// let (program, parse_errors) = parser::parse(&tokens);
/// assert!(parse_errors.is_empty());
///
/// let result = check(&program);
/// assert!(!result.diagnostics.is_empty());
/// assert!(result.diagnostics[0].message.contains("type mismatch"));
/// ```
#[must_use]
pub fn check(program: &Program) -> CheckResult {
    let mut checker = Checker::new();
    checker.check_program(program);
    CheckResult {
        diagnostics: checker.diagnostics,
        lambda_captures: checker.lambda_captures,
        functions: checker.functions,
        structs: checker.structs,
        enums: checker.enums,
        methods: checker.methods,
        traits: checker.traits,
        trait_impls: checker.trait_impls,
        type_aliases: checker.type_aliases,
        expr_types: checker.expr_types,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_common::span::SourceId;
    use phoenix_lexer::lexer::tokenize;
    use phoenix_parser::parser;

    fn check_source(source: &str) -> Vec<Diagnostic> {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        check(&program).diagnostics
    }

    fn assert_no_errors(source: &str) {
        let errors = check_source(source);
        assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
    }

    fn assert_has_error(source: &str, expected_msg: &str) {
        let errors = check_source(source);
        assert!(
            errors.iter().any(|e| e.message.contains(expected_msg)),
            "expected error containing '{}', got: {:?}",
            expected_msg,
            errors
        );
    }

    #[test]
    fn valid_simple_program() {
        assert_no_errors("function main() { let x: Int = 42\n print(x) }");
    }

    #[test]
    fn valid_function_call() {
        assert_no_errors(
            "function add(a: Int, b: Int) -> Int { return a + b }\nfunction main() { let result: Int = add(1, 2)\n print(result) }",
        );
    }

    #[test]
    fn type_mismatch_var_decl() {
        assert_has_error(
            "function main() { let x: Int = \"hello\" }",
            "type mismatch",
        );
    }

    #[test]
    fn undefined_variable() {
        assert_has_error("function main() { print(x) }", "undefined variable `x`");
    }

    #[test]
    fn duplicate_variable() {
        assert_has_error(
            "function main() { let x: Int = 1\n let x: Int = 2 }",
            "already defined",
        );
    }

    #[test]
    fn assignment_to_immutable() {
        assert_has_error(
            "function main() { let x: Int = 1\n x = 2 }",
            "cannot assign to immutable",
        );
    }

    #[test]
    fn assignment_to_mutable() {
        assert_no_errors("function main() { let mut x: Int = 1\n x = 2 }");
    }

    #[test]
    fn return_type_mismatch() {
        assert_has_error(
            "function foo() -> Int { return \"hello\" }",
            "return type mismatch",
        );
    }

    #[test]
    fn if_condition_not_bool() {
        assert_has_error(
            "function main() { if 42 { print(1) } }",
            "if condition must be Bool",
        );
    }

    #[test]
    fn wrong_argument_count() {
        assert_has_error(
            "function foo(a: Int) -> Int { return a }\nfunction main() { foo(1, 2) }",
            "takes 1 argument",
        );
    }

    #[test]
    fn wrong_argument_type() {
        assert_has_error(
            "function foo(a: Int) -> Int { return a }\nfunction main() { foo(\"hello\") }",
            "expected `Int` but got `String`",
        );
    }

    #[test]
    fn while_loop_valid() {
        assert_no_errors("function main() { let mut x: Int = 0\n while x < 10 { x = x + 1 } }");
    }

    #[test]
    fn while_condition_not_bool() {
        assert_has_error(
            "function main() { while 42 { print(1) } }",
            "while condition must be Bool",
        );
    }

    #[test]
    fn for_loop_valid() {
        assert_no_errors("function main() { for i in 0..10 { print(i) } }");
    }

    #[test]
    fn struct_valid() {
        assert_no_errors(
            "struct Point {\n  Int x\n  Int y\n}\nfunction main() { let p: Point = Point(1, 2)\n print(p.x) }",
        );
    }

    #[test]
    fn struct_wrong_field_count() {
        assert_has_error(
            "struct Point {\n  Int x\n  Int y\n}\nfunction main() { let p: Point = Point(1) }",
            "has 2 field(s), got 1",
        );
    }

    #[test]
    fn enum_and_match() {
        assert_no_errors(
            "enum Color {\n  Red\n  Green\n  Blue\n}\nfunction main() {\n  let c: Color = Red\n  match c {\n    Red -> print(\"red\")\n    Green -> print(\"green\")\n    Blue -> print(\"blue\")\n  }\n}",
        );
    }

    #[test]
    fn for_loop_non_int_range() {
        assert_has_error(
            "function main() { for i: Float in 0..10 { print(i) } }",
            "for loop variable must be Int",
        );
    }

    #[test]
    fn while_loop_with_return() {
        assert_no_errors(
            "function foo() -> Int { let mut x: Int = 0\n while x < 10 { x = x + 1\n return x } return 0 }",
        );
    }

    #[test]
    fn struct_field_access_valid() {
        assert_no_errors(
            "struct Point {\n  Int x\n  Int y\n}\nfunction main() { let p: Point = Point(1, 2)\n print(p.x) }",
        );
    }

    #[test]
    fn struct_field_access_invalid() {
        assert_has_error(
            "struct Point {\n  Int x\n  Int y\n}\nfunction main() { let p: Point = Point(1, 2)\n print(p.z) }",
            "has no field `z`",
        );
    }

    #[test]
    fn struct_field_type_check() {
        assert_no_errors(
            "struct Point {\n  Int x\n  Int y\n}\nfunction main() { let p: Point = Point(1, 2)\n let val: Int = p.x }",
        );
    }

    #[test]
    fn method_call_valid() {
        assert_no_errors(
            "struct Counter {\n  Int value\n}\nimpl Counter {\n  function get(self) -> Int { return self.value }\n}\nfunction main() { let c: Counter = Counter(0)\n let v: Int = c.get() }",
        );
    }

    #[test]
    fn method_call_undefined() {
        assert_has_error(
            "struct Counter {\n  Int value\n}\nfunction main() { let c: Counter = Counter(0)\n c.reset() }",
            "no method `reset`",
        );
    }

    #[test]
    fn method_wrong_args() {
        assert_has_error(
            "struct Counter {\n  Int value\n}\nimpl Counter {\n  function add(self, n: Int) -> Int { return self.value + n }\n}\nfunction main() { let c: Counter = Counter(0)\n c.add(1, 2) }",
            "takes 1 argument",
        );
    }

    #[test]
    fn enum_variant_with_wrong_field_count() {
        assert_has_error(
            "enum Shape {\n  Circle(Float)\n  Square(Float)\n}\nfunction main() { let s: Shape = Circle(1.0, 2.0) }",
            "takes 1 field(s), got 2",
        );
    }

    #[test]
    fn enum_variant_with_wrong_field_type() {
        assert_has_error(
            "enum Shape {\n  Circle(Float)\n  Square(Float)\n}\nfunction main() { let s: Shape = Circle(\"hello\") }",
            "expected `Float` but got `String`",
        );
    }

    #[test]
    fn match_on_enum_with_bindings() {
        assert_no_errors(
            "enum Shape {\n  Circle(Float)\n  Square(Float)\n}\nfunction main() {\n  let s: Shape = Circle(3.14)\n  match s {\n    Circle(r) -> print(r)\n    Square(side) -> print(side)\n  }\n}",
        );
    }

    #[test]
    fn impl_on_enum_valid() {
        assert_no_errors(
            "enum Color {\n  Red\n  Green\n  Blue\n}\nimpl Color {\n  function describe(self) -> String { return \"a color\" }\n}\nfunction main() {\n  let c: Color = Red\n  let desc: String = c.describe()\n}",
        );
    }

    #[test]
    fn else_if_chain_type_check() {
        assert_has_error(
            "function main() { let x: Int = 1\n if x == 1 { print(1) } else if 42 { print(2) } }",
            "if condition must be Bool",
        );
    }

    #[test]
    fn break_inside_while_valid() {
        assert_no_errors("function main() { while true { break } }");
    }

    #[test]
    fn continue_inside_for_valid() {
        assert_no_errors("function main() { for i in 0..10 { continue } }");
    }

    #[test]
    fn break_outside_loop_error() {
        assert_has_error("function main() { break }", "`break` outside of loop");
    }

    #[test]
    fn continue_outside_loop_error() {
        assert_has_error("function main() { continue }", "`continue` outside of loop");
    }

    #[test]
    fn break_in_nested_if_inside_loop() {
        assert_no_errors(
            "function main() { let mut x: Int = 0\n while true { x = x + 1\n if x == 5 { break } } }",
        );
    }

    #[test]
    fn struct_type_as_param() {
        assert_no_errors(
            "struct Point {\n  Int x\n  Int y\n}\nfunction show(p: Point) { print(p.x) }\nfunction main() { let p: Point = Point(1, 2)\n show(p) }",
        );
    }

    /// A variable with a function type assigned a compatible lambda passes type checking.
    #[test]
    fn lambda_type_check_valid() {
        assert_no_errors(
            "function main() {\n  let double: (Int) -> Int = function(x: Int) -> Int { return x * 2 }\n  print(double(5))\n}",
        );
    }

    /// Assigning a lambda with mismatched parameter types to a function-typed variable
    /// produces a type mismatch error.
    #[test]
    fn lambda_type_mismatch() {
        assert_has_error(
            "function main() {\n  let f: (Int) -> Int = function(x: String) -> Int { return 0 }\n}",
            "type mismatch",
        );
    }

    /// Calling a variable that holds a function value type-checks correctly,
    /// verifying argument types and returning the correct result type.
    #[test]
    fn call_function_variable() {
        assert_no_errors(
            "function main() {\n  let add: (Int, Int) -> Int = function(a: Int, b: Int) -> Int { return a + b }\n  let result: Int = add(1, 2)\n}",
        );
    }

    /// A function that takes another function as a parameter type-checks
    /// when the argument is a compatible lambda.
    #[test]
    fn higher_order_function() {
        assert_no_errors(
            "function apply(f: (Int) -> Int, x: Int) -> Int {\n  return f(x)\n}\nfunction main() {\n  let double: (Int) -> Int = function(x: Int) -> Int { return x * 2 }\n  let result: Int = apply(double, 5)\n}",
        );
    }

    /// A lambda that references a variable from an outer scope type-checks
    /// successfully (the outer variable is visible inside the lambda body).
    #[test]
    fn closure_captures_outer_variable() {
        assert_no_errors(
            "function main() {\n  let offset: Int = 10\n  let addOffset: (Int) -> Int = function(x: Int) -> Int { return x + offset }\n  let result: Int = addOffset(5)\n}",
        );
    }

    /// A generic identity function infers T from its argument and type-checks.
    #[test]
    fn generic_function_identity() {
        assert_no_errors(
            "function identity<T>(x: T) -> T { return x }\nfunction main() { let result: Int = identity(42) }",
        );
    }

    /// A generic struct with two type parameters type-checks when constructed
    /// with matching concrete types.
    #[test]
    fn generic_struct_valid() {
        assert_no_errors(
            "struct Pair<A, B> {\n  A first\n  B second\n}\nfunction main() { let p: Pair<Int, String> = Pair(1, \"hi\") }",
        );
    }

    /// A generic enum with a value-carrying variant type-checks correctly.
    #[test]
    fn generic_enum_option() {
        assert_no_errors(
            "enum Option<T> {\n  Some(T)\n  None\n}\nfunction main() { let x: Option<Int> = Some(42) }",
        );
    }

    /// The `None` variant of a generic enum is compatible with any concrete
    /// instantiation because its type arguments remain as type variables.
    #[test]
    fn generic_enum_none_compatible() {
        assert_no_errors(
            "enum Option<T> {\n  Some(T)\n  None\n}\nfunction main() { let x: Option<Int> = None }",
        );
    }

    /// Assigning the result of a generic function to the wrong concrete type
    /// produces a type mismatch error (T is inferred as Int, not String).
    #[test]
    fn generic_function_type_mismatch() {
        assert_has_error(
            "function identity<T>(x: T) -> T { return x }\nfunction main() { let s: String = identity(42) }",
            "type mismatch",
        );
    }

    #[test]
    fn list_literal_valid() {
        assert_no_errors("function main() { let nums: List<Int> = [1, 2, 3] }");
    }

    #[test]
    fn list_literal_empty() {
        assert_no_errors("function main() { let nums: List<Int> = [] }");
    }

    #[test]
    fn list_element_type_mismatch() {
        assert_has_error(
            "function main() { let nums: List<Int> = [1, \"hello\", 3] }",
            "list element type mismatch",
        );
    }

    #[test]
    fn list_length_method() {
        assert_no_errors(
            "function main() { let nums: List<Int> = [1, 2, 3]\n let len: Int = nums.length() }",
        );
    }

    #[test]
    fn list_get_method() {
        assert_no_errors(
            "function main() { let nums: List<Int> = [1, 2, 3]\n let first: Int = nums.get(0) }",
        );
    }

    #[test]
    fn list_get_wrong_arg_type() {
        assert_has_error(
            "function main() { let nums: List<Int> = [1, 2, 3]\n let first: Int = nums.get(\"zero\") }",
            "expected Int but got String",
        );
    }

    #[test]
    fn list_push_method() {
        assert_no_errors(
            "function main() { let nums: List<Int> = [1, 2]\n let nums2: List<Int> = nums.push(3) }",
        );
    }

    #[test]
    fn list_push_wrong_type() {
        assert_has_error(
            "function main() { let nums: List<Int> = [1, 2]\n let nums2: List<Int> = nums.push(\"hello\") }",
            "expected Int but got String",
        );
    }

    #[test]
    fn list_unknown_method() {
        assert_has_error(
            "function main() { let nums: List<Int> = [1]\n nums.foo() }",
            "no method `foo` on type `List`",
        );
    }

    #[test]
    fn list_type_annotation() {
        assert_no_errors("function main() { let names: List<String> = [\"alice\", \"bob\"] }");
    }

    #[test]
    fn list_var_decl_type_mismatch() {
        assert_has_error(
            "function main() { let nums: List<Int> = [\"hello\"] }",
            "type mismatch",
        );
    }

    // --- Built-in Option and Result type tests ---

    /// `Option<Int> x = Some(42)` passes type checking using the built-in Option enum.
    #[test]
    fn option_some_valid() {
        assert_no_errors("function main() { let x: Option<Int> = Some(42) }");
    }

    /// `Option<Int> x = None` passes because None is compatible with any Option instantiation.
    #[test]
    fn option_none_valid() {
        assert_no_errors("function main() { let x: Option<Int> = None }");
    }

    /// `Option<String> x = Some(42)` produces a type mismatch (Int vs String).
    #[test]
    fn option_type_mismatch() {
        assert_has_error(
            "function main() { let x: Option<String> = Some(42) }",
            "type mismatch",
        );
    }

    /// The return type of `unwrap()` on `Option<Int>` is `Int`.
    #[test]
    fn option_unwrap_type() {
        assert_no_errors(
            "function main() { let x: Option<Int> = Some(42)\n let v: Int = x.unwrap() }",
        );
    }

    /// `Result<Int, String> x = Ok(42)` passes type checking using the built-in Result enum.
    #[test]
    fn result_ok_valid() {
        assert_no_errors(r#"function main() { let x: Result<Int, String> = Ok(42) }"#);
    }

    /// `Result<Int, String> x = Err("oops")` passes type checking.
    #[test]
    fn result_err_valid() {
        assert_no_errors(r#"function main() { let x: Result<Int, String> = Err("oops") }"#);
    }

    /// `isOk()` on a Result returns Bool.
    #[test]
    fn result_is_ok_returns_bool() {
        assert_no_errors(
            r#"function main() { let r: Result<Int, String> = Ok(1)
let b: Bool = r.isOk() }"#,
        );
    }

    /// Providing the wrong number of type arguments to a generic struct
    /// produces a type mismatch error (Pair needs 2, but only 1 is given).
    #[test]
    fn generic_wrong_type_arg_count() {
        assert_has_error(
            "struct Pair<A, B> {\n  A first\n  B second\n}\nfunction main() { let p: Pair<Int> = Pair(1, \"hi\") }",
            "type mismatch",
        );
    }

    /// A generic higher-order function `unwrapOr` that takes an `Option<T>`
    /// and a default `T` value type-checks correctly.
    #[test]
    fn generic_unwrap_or() {
        assert_no_errors(
            "enum Option<T> {\n  Some(T)\n  None\n}\nfunction unwrapOr<T>(opt: Option<T>, defaultVal: T) -> T {\n  return match opt {\n    Some(v) -> v\n    None -> defaultVal\n  }\n}\nfunction main() {\n  let x: Option<Int> = Some(42)\n  let result: Int = unwrapOr(x, 0)\n}",
        );
    }

    /// Calling a closure with the wrong number of arguments produces an error.
    #[test]
    fn closure_wrong_arg_count() {
        assert_has_error(
            "function main() {\n  let f: (Int) -> Int = function(x: Int) -> Int { return x * 2 }\n  f(1, 2)\n}",
            "takes 1 argument(s), got 2",
        );
    }

    /// A lambda whose body returns the wrong type produces a return type mismatch error.
    #[test]
    fn closure_return_type_mismatch() {
        assert_has_error(
            "function main() {\n  let f: (Int) -> Int = function(x: Int) -> Int { return \"hello\" }\n}",
            "return type mismatch",
        );
    }

    /// `Option<List<Int>>` with a nested generic type passes type checking.
    #[test]
    fn generic_nested_type() {
        assert_no_errors("function main() { let x: Option<List<Int>> = Some([1, 2, 3]) }");
    }

    /// Passing a function with the wrong type signature to a higher-order
    /// function produces a type mismatch error.
    #[test]
    fn function_type_param_mismatch() {
        assert_has_error(
            "function apply(f: (Int) -> Int, x: Int) -> Int {\n  return f(x)\n}\nfunction main() {\n  let g: (String) -> String = function(s: String) -> String { return s }\n  apply(g, 5)\n}",
            "expected `(Int) -> Int` but got `(String) -> String`",
        );
    }

    /// A lambda that returns another lambda type-checks correctly with
    /// nested function types.
    #[test]
    fn nested_closures_valid() {
        assert_no_errors(
            "function main() {\n  let makeAdder: (Int) -> (Int) -> Int = function(n: Int) -> (Int) -> Int {\n    return function(x: Int) -> Int { return x + n }\n  }\n  let add5: (Int) -> Int = makeAdder(5)\n  let result: Int = add5(10)\n}",
        );
    }

    /// A full trait decl + impl + method call passes type checking.
    #[test]
    fn trait_impl_valid() {
        assert_no_errors(
            r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  Int x
  Int y

  impl Display {
    function toString(self) -> String { return "Point" }
  }
}
function main() {
  let p: Point = Point(1, 2)
  let s: String = p.toString()
}
"#,
        );
    }

    /// An impl that is missing a required trait method produces an error.
    #[test]
    fn trait_impl_missing_method() {
        assert_has_error(
            r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  Int x
  Int y

  impl Display {
  }
}
function main() { }
"#,
            "missing method `toString`",
        );
    }

    /// A generic function with a trait bound, called with a type that implements
    /// the trait, passes type checking.
    #[test]
    fn trait_bound_satisfied() {
        assert_no_errors(
            r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  Int x
  Int y

  impl Display {
    function toString(self) -> String { return "Point" }
  }
}
function show<T: Display>(item: T) -> String {
  return item.toString()
}
function main() {
  let p: Point = Point(1, 2)
  let s: String = show(p)
}
"#,
        );
    }

    /// A generic function with a trait bound, called with a type that does NOT
    /// implement the trait, produces an error.
    #[test]
    fn trait_bound_not_satisfied() {
        assert_has_error(
            r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  Int x
  Int y
}
function show<T: Display>(item: T) -> String {
  return item.toString()
}
function main() {
  let p: Point = Point(1, 2)
  let s: String = show(p)
}
"#,
            "does not implement trait `Display`",
        );
    }

    /// `impl FakeTrait for X` where FakeTrait is not defined produces an error.
    #[test]
    fn unknown_trait_in_impl() {
        assert_has_error(
            r#"
struct Point {
  Int x
  Int y

  impl FakeTrait {
    function foo(self) -> Int { return 0 }
  }
}
function main() { }
"#,
            "unknown trait `FakeTrait`",
        );
    }

    /// A trait with two methods, both implemented, passes type checking.
    #[test]
    fn trait_multiple_methods_valid() {
        assert_no_errors(
            r#"
trait Shape {
  function area(self) -> Float
  function name(self) -> String
}
struct Circle {
  Float radius

  impl Shape {
    function area(self) -> Float { return 3.14 }
    function name(self) -> String { return "Circle" }
  }
}
function main() {
  let c: Circle = Circle(1.0)
  let a: Float = c.area()
  let n: String = c.name()
}
"#,
        );
    }

    /// A trait with two methods where impl only provides one should error.
    #[test]
    fn trait_partial_impl() {
        assert_has_error(
            r#"
trait Shape {
  function area(self) -> Float
  function name(self) -> String
}
struct Circle {
  Float radius

  impl Shape {
    function area(self) -> Float { return 3.14 }
  }
}
function main() { }
"#,
            "missing method `name`",
        );
    }

    // --- Type inference tests ---

    /// Type inference works for literals.
    #[test]
    fn type_inference_literal() {
        assert_no_errors("function main() { let x = 42\n print(x) }");
    }

    /// Type inference works for struct constructors.
    #[test]
    fn type_inference_struct() {
        assert_no_errors(
            r#"
struct Point { Int x  Int y }
function main() {
  let p = Point(1, 2)
  print(p.x)
}
"#,
        );
    }

    /// Type inference rejects Void initializer.
    #[test]
    fn type_inference_rejects_void() {
        assert_has_error(
            "function foo() { }\nfunction main() { let x = foo() }",
            "cannot infer type for `x`: initializer has type Void",
        );
    }

    /// Type inference rejects ambiguous generic types (e.g. `None`).
    #[test]
    fn type_inference_rejects_ambiguous_generic() {
        assert_has_error(
            "function main() { let x = None }",
            "cannot infer type for `x`: initializer has ambiguous type",
        );
    }

    /// Explicit annotation with `None` is fine — the annotation resolves the type.
    #[test]
    fn type_annotation_resolves_none() {
        assert_no_errors("function main() { let x: Option<Int> = None }");
    }

    /// Type inference works for mutable variables.
    #[test]
    fn type_inference_mut() {
        assert_no_errors(
            r#"
function main() {
  let mut x = 42
  x = x + 1
  print(x)
}
"#,
        );
    }

    /// Type inference works for string values with mutability.
    #[test]
    fn type_inference_mut_string() {
        assert_no_errors(
            r#"
function main() {
  let mut s = "hello"
  s = "world"
  print(s)
}
"#,
        );
    }

    /// Type inference catches type mismatches on reassignment.
    #[test]
    fn type_inference_mut_mismatch() {
        assert_has_error(
            r#"
function main() {
  let mut x = 42
  x = "hello"
}
"#,
            "type mismatch",
        );
    }

    /// For-loop with inferred type (no annotation) works.
    #[test]
    fn for_loop_inferred_type() {
        assert_no_errors("function main() { for i in 0..10 { print(i) } }");
    }

    /// For-loop with explicit Int type annotation works.
    #[test]
    fn for_loop_explicit_int_type() {
        assert_no_errors("function main() { for i: Int in 0..10 { print(i) } }");
    }

    // --- GC memory model tests ---
    // Phoenix uses garbage collection.  All values — including structs, enums,
    // lists, and closures — can be freely shared and reused after assignment or
    // being passed to functions.

    /// A struct assigned to another variable remains usable.
    #[test]
    fn struct_reusable_after_assignment() {
        assert_no_errors(
            r#"
struct Point {
  Int x
  Int y
}
function main() {
  let p: Point = Point(1, 2)
  let q: Point = p
  print(p.x)
  print(q.x)
}
"#,
        );
    }

    /// A list assigned to another variable remains usable.
    #[test]
    fn list_reusable_after_assignment() {
        assert_no_errors(
            r#"
function main() {
  let a: List<Int> = [1, 2, 3]
  let b: List<Int> = a
  print(a.length())
  print(b.length())
}
"#,
        );
    }

    /// An enum (Option) assigned to another variable remains usable.
    #[test]
    fn enum_reusable_after_assignment() {
        assert_no_errors(
            r#"
function main() {
  let a: Option<Int> = Some(42)
  let b: Option<Int> = a
  print(a.isSome())
}
"#,
        );
    }

    /// A struct passed to a function can still be used by the caller.
    #[test]
    fn function_arg_does_not_consume() {
        assert_no_errors(
            r#"
struct Point {
  Int x
  Int y
}
function take(p: Point) { print(p.x) }
function main() {
  let p: Point = Point(1, 2)
  take(p)
  print(p.x)
}
"#,
        );
    }

    /// A struct referenced inside a closure can still be used outside.
    #[test]
    fn closure_capture_does_not_consume() {
        assert_no_errors(
            r#"
struct Point {
  Int x
  Int y
}
function main() {
  let p: Point = Point(1, 2)
  let q: Point = p
  let f: (Int) -> Int = function(x: Int) -> Int { return p.x }
  print(p.x)
}
"#,
        );
    }

    /// The same variable can be passed as multiple arguments to a function.
    #[test]
    fn same_var_passed_twice() {
        assert_no_errors(
            r#"
struct Point {
  Int x
  Int y
}
function both(a: Point, b: Point) { print(a.x) }
function main() {
  let p: Point = Point(1, 2)
  both(p, p)
}
"#,
        );
    }

    /// A variable used inside an if branch can still be used after the branch.
    #[test]
    fn use_after_assign_in_if_branch() {
        assert_no_errors(
            r#"
struct Data { Int value }
function take(d: Data) { print(d.value) }
function main() {
  let d: Data = Data(42)
  if true {
    take(d)
  }
  print(d.value)
}
"#,
        );
    }

    // --- Match exhaustiveness tests ---

    /// A match on an enum missing a variant (without wildcard) should error.
    #[test]
    fn match_non_exhaustive_error() {
        assert_has_error(
            r#"
enum Color {
  Red
  Green
  Blue
}
function main() {
  let c: Color = Red
  match c {
    Red -> print("red")
    Green -> print("green")
  }
}
"#,
            "non-exhaustive match",
        );
    }

    /// A match with a wildcard is always exhaustive.
    #[test]
    fn match_exhaustive_with_wildcard() {
        assert_no_errors(
            r#"
enum Color {
  Red
  Green
  Blue
}
function main() {
  let c: Color = Red
  match c {
    Red -> print("red")
    _ -> print("other")
  }
}
"#,
        );
    }

    /// A match with a binding catch-all is always exhaustive.
    #[test]
    fn match_exhaustive_with_binding() {
        assert_no_errors(
            r#"
enum Color {
  Red
  Green
  Blue
}
function main() {
  let c: Color = Red
  match c {
    Red -> print("red")
    other -> print("other")
  }
}
"#,
        );
    }

    /// A match covering all enum variants is exhaustive.
    #[test]
    fn match_exhaustive_all_variants() {
        assert_no_errors(
            r#"
enum Color {
  Red
  Green
  Blue
}
function main() {
  let c: Color = Red
  match c {
    Red -> print("red")
    Green -> print("green")
    Blue -> print("blue")
  }
}
"#,
        );
    }

    // --- Comparison operator error type tests ---

    /// Comparing incompatible types returns an error (not Bool).
    #[test]
    fn comparison_incompatible_types_error() {
        assert_has_error(
            "function main() { let b: Bool = 42 < \"hello\" }",
            "cannot compare",
        );
    }

    /// Equality between incompatible types returns an error.
    #[test]
    fn equality_incompatible_types_error() {
        assert_has_error(
            "function main() { let b: Bool = 42 == \"hello\" }",
            "cannot compare",
        );
    }

    // --- Additional missing tests ---

    /// Duplicate function definition produces an error.
    #[test]
    fn duplicate_function_error() {
        assert_has_error(
            "function foo() { }\nfunction foo() { }\nfunction main() { }",
            "already defined",
        );
    }

    /// A match block body with a return statement has the correct type.
    #[test]
    fn match_block_body_with_return() {
        assert_no_errors(
            r#"
enum Shape {
  Circle(Float)
  Rect(Float, Float)
}
impl Shape {
  function describe(self) -> String {
    return match self {
      Circle(_) -> "circle"
      Rect(w, h) -> {
        if w == h { return "square" }
        return "rectangle"
      }
    }
  }
}
function main() {
  let s: Shape = Rect(3.0, 3.0)
  let desc: String = s.describe()
}
"#,
        );
    }

    /// Empty match on an enum without arms should error for exhaustiveness.
    #[test]
    fn match_empty_arms_error() {
        assert_has_error(
            r#"
enum Color {
  Red
  Green
}
function main() {
  let c: Color = Red
  match c {
  }
}
"#,
            "non-exhaustive match",
        );
    }

    /// A generic function with a closure parameter infers type arguments correctly.
    #[test]
    fn generic_function_with_closure() {
        assert_no_errors(
            r#"
function map<T, U>(value: T, f: (T) -> U) -> U {
  return f(value)
}
function main() {
  let result: String = map(42, function(n: Int) -> String { return toString(n) })
}
"#,
        );
    }

    /// Multiple errors are accumulated and all reported.
    #[test]
    fn multiple_errors_accumulated() {
        let errors = check_source(
            r#"
function main() {
  let x: Int = "hello"
  let y: Bool = 42
  let z: Float = true
}
"#,
        );
        assert!(
            errors.len() >= 3,
            "expected at least 3 errors, got: {:?}",
            errors
        );
    }

    /// Match on Option missing Some variant (without wildcard) errors.
    #[test]
    fn match_option_non_exhaustive() {
        assert_has_error(
            r#"
function main() {
  let x: Option<Int> = Some(42)
  match x {
    Some(v) -> print(v)
  }
}
"#,
            "non-exhaustive match",
        );
    }

    /// A deeply nested generic type passes type checking.
    #[test]
    fn deeply_nested_generic_type() {
        assert_no_errors(
            r#"
function main() {
  let items: List<Option<Int>> = [Some(1), None, Some(3)]
  let opt: Option<List<Int>> = Some([1, 2, 3])
}
"#,
        );
    }

    /// Closures at 3 levels of nesting type-check correctly.
    #[test]
    fn triple_nested_closures() {
        assert_no_errors(
            r#"
function main() {
  let a: Int = 1
  let f: (Int) -> (Int) -> (Int) -> Int = function(b: Int) -> (Int) -> (Int) -> Int {
    return function(c: Int) -> (Int) -> Int {
      return function(d: Int) -> Int {
        return a + b + c + d
      }
    }
  }
  let g: (Int) -> (Int) -> Int = f(2)
  let h: (Int) -> Int = g(3)
  let result: Int = h(4)
}
"#,
        );
    }

    /// Using the for-loop variable after the loop is fine (it's scoped).
    #[test]
    fn for_loop_variable_scoped() {
        assert_has_error(
            "function main() { for i in 0..10 { print(i) }\n print(i) }",
            "undefined variable `i`",
        );
    }

    /// Calling a method on a void expression errors.
    #[test]
    fn method_on_void_error() {
        assert_has_error(
            "function foo() { }\nfunction main() { foo().bar() }",
            "cannot call method on Void",
        );
    }

    // --- Phase 1.8 feature tests ---

    /// Field assignment to an immutable variable is an error.
    #[test]
    fn field_assignment_immutable_error() {
        assert_has_error(
            r#"
struct Point { Int x  Int y }
function main() {
  let p: Point = Point(1, 2)
  p.x = 10
}
"#,
            "immutable",
        );
    }

    /// Field assignment with wrong type is an error.
    #[test]
    fn field_assignment_wrong_type_error() {
        assert_has_error(
            r#"
struct Point { Int x  Int y }
function main() {
  let mut p: Point = Point(1, 2)
  p.x = "hello"
}
"#,
            "type mismatch",
        );
    }

    /// The `?` operator on a non-Result/non-Option type is an error.
    #[test]
    fn try_operator_on_non_result_error() {
        assert_has_error(
            r#"
function foo() -> Result<Int, String> {
  let x: Int = 42
  let y: Int = x?
  return Ok(y)
}
function main() { }
"#,
            "?",
        );
    }

    /// The `?` operator in a function not returning Result/Option is an error.
    #[test]
    fn try_operator_wrong_return_type_error() {
        assert_has_error(
            r#"
function helper() -> Result<Int, String> { return Ok(1) }
function main() {
  let x: Int = helper()?
}
"#,
            "?",
        );
    }

    /// Type aliases resolve correctly so `type Id = Int; Id x = 42` passes.
    #[test]
    fn type_alias_resolves() {
        assert_no_errors(
            r#"
type Id = Int
function main() {
  let x: Id = 42
}
"#,
        );
    }

    /// String interpolation type-checks to String.
    #[test]
    fn string_interpolation_type_checks() {
        assert_no_errors(
            r#"
function main() {
  let name: String = "world"
  let greeting: String = "hello {name}"
}
"#,
        );
    }

    #[test]
    fn lambda_implicit_return_type_mismatch() {
        assert_has_error(
            "function main() {\n  let f: (Int) -> String = function(x: Int) -> String { x }\n}",
            "lambda return type mismatch",
        );
    }

    #[test]
    fn generic_type_alias_missing_args() {
        assert_has_error(
            "type StringResult<T> = Result<T, String>\nfunction main() {\n  let x: StringResult = Ok(42)\n}",
            "generic type alias `StringResult` requires type arguments",
        );
    }

    #[test]
    fn field_assignment_type_mismatch() {
        assert_has_error(
            "struct Point {\n  Int x\n  Int y\n}\nfunction main() {\n  let mut p: Point = Point(1, 2)\n  p.x = \"hello\"\n}",
            "type mismatch",
        );
    }

    // ── Low-priority edge case tests ───────────────────────────────

    #[test]
    fn circular_type_alias_produces_error() {
        // type A refers to B which doesn't exist yet at registration time
        assert_has_error(
            "type A = B\ntype B = A\nfunction main() { let x: A = 42 }",
            "unknown type `B`",
        );
    }

    #[test]
    fn trait_bound_only_valid_on_type_params() {
        // Trait bounds on concrete (non-generic) parameter types should still work
        // when the type actually implements the trait
        assert_no_errors(
            r#"
trait Display {
  function toString(self) -> String
}
struct Point {
  Int x
  Int y

  impl Display {
    function toString(self) -> String { return "point" }
  }
}
function show<T: Display>(item: T) -> String {
  return item.toString()
}
function main() {
  let p: Point = Point(1, 2)
  print(show(p))
}
"#,
        );
    }

    #[test]
    fn method_arg_type_compat_with_generics_regression() {
        // Regression test: method argument checking should use types_compatible()
        // not strict equality, so type variables work correctly
        assert_no_errors(
            r#"
function main() {
  let x: Option<Int> = Some(42)
  let val: Int = x.unwrapOr(0)
  print(val)
}
"#,
        );
    }

    #[test]
    fn empty_match_exhaustiveness_error() {
        assert_has_error(
            "enum Color {\n  Red\n  Green\n}\nfunction main() {\n  let c: Color = Red\n  match c { }\n}",
            "non-exhaustive match",
        );
    }

    #[test]
    fn unknown_escape_sequence_passthrough() {
        // Unknown escape sequences like \x should pass through as literal characters
        assert_no_errors(
            r#"function main() { let s: String = "hello\x41"
  print(s) }"#,
        );
    }

    #[test]
    fn and_or_with_error_operand_no_cascade() {
        // When one operand has a prior error, And/Or should not report
        // an additional "must be Bool" error about the error type
        let errors = check_source("function main() { let b: Bool = undefinedVar and true }");
        // Should have "undefined variable" but NOT "must be Bool"
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("undefined variable"))
        );
        assert!(!errors.iter().any(|e| e.message.contains("must be Bool")));
    }

    #[test]
    fn trait_impl_wrong_param_count() {
        assert_has_error(
            r#"
trait Greet {
  function hello(self) -> String
}
struct Person {
  String name

  impl Greet {
    function hello(self, extra: Int) -> String { return "hi" }
  }
}
"#,
            "parameter(s) but trait",
        );
    }

    #[test]
    fn trait_impl_wrong_return_type() {
        assert_has_error(
            r#"
trait Greet {
  function hello(self) -> String
}
struct Person {
  String name

  impl Greet {
    function hello(self) -> Int { return 42 }
  }
}
"#,
            "returns `Int` but trait",
        );
    }

    #[test]
    fn trait_impl_wrong_parameter_type() {
        assert_has_error(
            r#"
trait Adder {
  function add(self, x: Int) -> Int
}
struct Foo {
  Int val

  impl Adder {
    function add(self, x: String) -> Int { return 42 }
  }
}
"#,
            "parameter `x` has type `String` but trait `Adder` expects `Int`",
        );
    }

    #[test]
    fn named_arguments_duplicate() {
        assert_has_error(
            r#"
function foo(a: Int, b: Int) -> Int { return a + b }
function main() { print(foo(a: 1, a: 2)) }
"#,
            "duplicate",
        );
    }

    #[test]
    fn named_arguments_unknown_parameter() {
        let diags = check_source(
            r#"
function foo(a: Int) -> Int { return a }
function main() { print(foo(z: 1)) }
"#,
        );
        let has_relevant_error = diags.iter().any(|d| {
            let msg = d.message.to_lowercase();
            msg.contains("unknown") || msg.contains("no parameter")
        });
        assert!(
            has_relevant_error,
            "expected error about unknown/no parameter, got: {:?}",
            diags
        );
    }

    #[test]
    fn default_parameters_valid() {
        assert_no_errors(
            r#"
function greet(name: String, prefix: String = "Hello") -> String {
  return prefix + " " + name
}
function main() { print(greet("Alice")) }
"#,
        );
    }

    #[test]
    fn struct_destructuring_valid() {
        assert_no_errors(
            r#"
struct Point {
  Int x
  Int y
}
function main() {
  let p: Point = Point(3, 4)
  let Point { x, y } = p
  print(x)
}
"#,
        );
    }

    #[test]
    fn struct_destructuring_unknown_field() {
        let diags = check_source(
            r#"
struct Point {
  Int x
  Int y
}
function main() {
  let p: Point = Point(3, 4)
  let Point { x, z } = p
}
"#,
        );
        let has_relevant_error = diags.iter().any(|d| {
            let msg = d.message.to_lowercase();
            msg.contains("z")
                && (msg.contains("not found")
                    || msg.contains("no field")
                    || msg.contains("unknown"))
        });
        assert!(
            has_relevant_error,
            "expected error about unknown field `z`, got: {:?}",
            diags
        );
    }

    #[test]
    fn closure_captures_outer_mutable_variable() {
        assert_no_errors(
            r#"
function main() {
  let mut count: Int = 0
  let inc: () -> Void = function() { count = count + 1 }
  inc()
  print(count)
}
"#,
        );
    }

    #[test]
    fn generic_function_conflicting_types() {
        assert_has_error(
            r#"
function same<T>(a: T, b: T) -> T { return a }
function main() { same(1, "hello") }
"#,
            "expected `Int` but got `String`",
        );
    }

    #[test]
    fn match_on_int_literal_patterns() {
        assert_no_errors(
            r#"
function main() {
  let x: Int = 42
  match x {
    1 -> print("one")
    _ -> print("other")
  }
}
"#,
        );
    }

    #[test]
    fn trait_impl_correct_parameter_types() {
        assert_no_errors(
            r#"
trait Converter {
  function convert(self, x: Int) -> String
}
struct MyConv {
  impl Converter {
    function convert(self, x: Int) -> String { return toString(x) }
  }
}
function main() { }
"#,
        );
    }

    // ── Bug fix: match arm type mismatch with break/continue/return ──

    /// A match arm with `break` should not cause a type mismatch error when
    /// another arm evaluates to a non-Void type.
    #[test]
    fn match_arm_break_error() {
        assert_has_error(
            r#"
enum Action { Go  Stop }
function main() {
  let actions: List<Action> = [Go, Stop]
  let mut count: Int = 0
  for a in actions {
    match a {
      Go -> { count = count + 1 }
      Stop -> { break }
    }
  }
}
"#,
            "`break` is not allowed inside match arms",
        );
    }

    #[test]
    fn match_arm_continue_error() {
        assert_has_error(
            r#"
function main() {
  let mut sum: Int = 0
  for i in 0..10 {
    match i % 2 {
      0 -> { sum = sum + i }
      _ -> { continue }
    }
  }
}
"#,
            "`continue` is not allowed inside match arms",
        );
    }

    /// `break` in a match arm outside of any loop still produces the match-arm error,
    /// not the "break outside of loop" error.
    #[test]
    fn match_arm_break_outside_loop_error() {
        assert_has_error(
            r#"
function main() {
  let x: Int = 1
  match x {
    1 -> { break }
    _ -> {}
  }
}
"#,
            "`break` is not allowed inside match arms",
        );
    }

    /// `continue` in a match arm outside of any loop still produces the match-arm error.
    #[test]
    fn match_arm_continue_outside_loop_error() {
        assert_has_error(
            r#"
function main() {
  let x: Int = 1
  match x {
    1 -> { continue }
    _ -> {}
  }
}
"#,
            "`continue` is not allowed inside match arms",
        );
    }

    /// `return` in a match arm is still allowed (it exits the enclosing function).
    #[test]
    fn match_arm_return_still_allowed() {
        assert_no_errors(
            r#"
function foo(x: Int) -> Int {
  match x {
    1 -> { return 42 }
    _ -> { return 0 }
  }
  return -1
}
function main() { print(foo(1)) }
"#,
        );
    }

    /// A match arm with `return` should not cause a type mismatch error.
    #[test]
    fn match_arm_return_no_type_mismatch() {
        assert_no_errors(
            r#"
function find(nums: List<Int>) -> Int {
  for n in nums {
    match n % 2 {
      0 -> { return n }
      _ -> { let x: Int = 0 }
    }
  }
  return -1
}
function main() { print(find([1, 3, 4])) }
"#,
        );
    }

    // ── 1.13.1: CheckResult exposes type registries ─────────────────

    fn check_full(source: &str) -> CheckResult {
        let tokens = tokenize(source, SourceId(0));
        let (program, parse_errors) = parser::parse(&tokens);
        assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
        let result = check(&program);
        assert!(
            result.diagnostics.is_empty(),
            "type errors: {:?}",
            result.diagnostics
        );
        result
    }

    #[test]
    fn check_result_contains_functions() {
        let result = check_full(
            r#"
function add(a: Int, b: Int) -> Int { return a + b }
function greet(name: String) -> String { return name }
function main() { }
"#,
        );
        assert!(result.functions.contains_key("add"));
        assert!(result.functions.contains_key("greet"));
        assert!(result.functions.contains_key("main"));
        let add_info = &result.functions["add"];
        assert_eq!(add_info.params.len(), 2);
        assert_eq!(add_info.params[0], Type::Int);
        assert_eq!(add_info.params[1], Type::Int);
        assert_eq!(add_info.return_type, Type::Int);
        let greet_info = &result.functions["greet"];
        assert_eq!(greet_info.params, vec![Type::String]);
        assert_eq!(greet_info.return_type, Type::String);
    }

    #[test]
    fn check_result_contains_structs() {
        let result = check_full(
            r#"
struct Point { Int x  Int y }
struct Named { String name }
function main() { }
"#,
        );
        assert!(result.structs.contains_key("Point"));
        let point = &result.structs["Point"];
        assert_eq!(point.fields.len(), 2);
        assert_eq!(point.fields[0], ("x".to_string(), Type::Int));
        assert_eq!(point.fields[1], ("y".to_string(), Type::Int));
        assert!(point.type_params.is_empty());

        assert!(result.structs.contains_key("Named"));
        assert_eq!(result.structs["Named"].fields[0].1, Type::String);
    }

    #[test]
    fn check_result_contains_generic_struct() {
        let result = check_full(
            r#"
struct Wrapper<T> { T value }
function main() { }
"#,
        );
        let wrapper = &result.structs["Wrapper"];
        assert_eq!(wrapper.type_params, vec!["T".to_string()]);
        assert_eq!(wrapper.fields.len(), 1);
    }

    #[test]
    fn check_result_contains_enums() {
        let result = check_full(
            r#"
enum Color { Red  Green  Blue }
enum Shape { Circle(Float)  Rect(Float, Float) }
function main() { }
"#,
        );
        assert!(result.enums.contains_key("Color"));
        let color = &result.enums["Color"];
        assert_eq!(color.variants.len(), 3);
        assert_eq!(color.variants[0].0, "Red");
        assert!(color.variants[0].1.is_empty()); // unit variant

        assert!(result.enums.contains_key("Shape"));
        let shape = &result.enums["Shape"];
        assert_eq!(shape.variants.len(), 2);
        assert_eq!(shape.variants[0].0, "Circle");
        assert_eq!(shape.variants[0].1.len(), 1); // one Float field
        assert_eq!(shape.variants[1].0, "Rect");
        assert_eq!(shape.variants[1].1.len(), 2); // two Float fields
    }

    #[test]
    fn check_result_contains_builtin_enums() {
        let result = check_full("function main() { }");
        // Option and Result are pre-registered builtins
        assert!(result.enums.contains_key("Option"));
        assert!(result.enums.contains_key("Result"));
        let option = &result.enums["Option"];
        assert_eq!(option.type_params, vec!["T".to_string()]);
        assert_eq!(option.variants.len(), 2); // Some, None
    }

    #[test]
    fn check_result_contains_methods() {
        let result = check_full(
            r#"
struct Counter { Int val }
impl Counter {
    function get(self) -> Int { return self.val }
    function inc(self) -> Counter { return Counter(self.val + 1) }
}
function main() { }
"#,
        );
        assert!(result.methods.contains_key("Counter"));
        let counter_methods = &result.methods["Counter"];
        assert!(counter_methods.contains_key("get"));
        assert!(counter_methods.contains_key("inc"));
        assert_eq!(counter_methods["get"].return_type, Type::Int);
        assert!(counter_methods["get"].params.is_empty()); // excludes self
    }

    #[test]
    fn check_result_contains_traits_and_impls() {
        let result = check_full(
            r#"
trait Display {
    function toString(self) -> String
}
struct Point {
    Int x
    Int y

    impl Display {
        function toString(self) -> String { return "point" }
    }
}
function main() { }
"#,
        );
        assert!(result.traits.contains_key("Display"));
        let display = &result.traits["Display"];
        assert_eq!(display.methods.len(), 1);
        assert_eq!(display.methods[0].name, "toString");
        assert_eq!(display.methods[0].return_type, Type::String);

        assert!(
            result
                .trait_impls
                .contains(&("Point".to_string(), "Display".to_string()))
        );
    }

    #[test]
    fn check_result_contains_type_aliases() {
        let result = check_full(
            r#"
type UserId = Int
type StringResult<T> = Result<T, String>
function main() { }
"#,
        );
        assert!(result.type_aliases.contains_key("UserId"));
        assert_eq!(result.type_aliases["UserId"].target, Type::Int);
        assert!(result.type_aliases["UserId"].type_params.is_empty());

        assert!(result.type_aliases.contains_key("StringResult"));
        assert_eq!(
            result.type_aliases["StringResult"].type_params,
            vec!["T".to_string()]
        );
    }

    #[test]
    fn check_result_function_with_defaults() {
        let result = check_full(
            r#"
function greet(name: String, greeting: String = "Hello") -> String {
    return greeting + " " + name
}
function main() { }
"#,
        );
        let info = &result.functions["greet"];
        assert_eq!(info.params.len(), 2);
        assert_eq!(info.param_names, vec!["name", "greeting"]);
        assert_eq!(info.default_param_indices, vec![1]);
    }

    // ── 1.13.2: Expression-level type annotations ───────────────────

    #[test]
    fn expr_types_populated_for_literals() {
        let result = check_full(
            r#"
function main() {
    let x: Int = 42
    let y: Float = 3.14
    let s: String = "hello"
    let b: Bool = true
}
"#,
        );
        // expr_types should be non-empty — every expression gets recorded
        assert!(
            !result.expr_types.is_empty(),
            "expr_types should be populated"
        );
        // Check that all basic types appear in the values
        let types: Vec<&Type> = result.expr_types.values().collect();
        assert!(types.contains(&&Type::Int));
        assert!(types.contains(&&Type::Float));
        assert!(types.contains(&&Type::String));
        assert!(types.contains(&&Type::Bool));
    }

    #[test]
    fn expr_types_populated_for_binary_ops() {
        let result = check_full(
            r#"
function main() {
    let x: Int = 1 + 2
    let y: Bool = 1 < 2
    let z: Float = 1.0 + 2.0
}
"#,
        );
        let types: Vec<&Type> = result.expr_types.values().collect();
        assert!(types.contains(&&Type::Int));
        assert!(types.contains(&&Type::Bool));
        assert!(types.contains(&&Type::Float));
    }

    #[test]
    fn expr_types_populated_for_function_calls() {
        let result = check_full(
            r#"
function add(a: Int, b: Int) -> Int { return a + b }
function main() {
    let x: Int = add(1, 2)
}
"#,
        );
        // The call expression `add(1, 2)` should be recorded as Type::Int
        let has_int_call = result.expr_types.values().any(|t| *t == Type::Int);
        assert!(has_int_call, "call to add() should produce Type::Int");
    }

    #[test]
    fn expr_types_populated_for_method_calls() {
        let result = check_full(
            r#"
struct Counter { Int val }
impl Counter {
    function get(self) -> Int { return self.val }
}
function main() {
    let c: Counter = Counter(5)
    let v: Int = c.get()
}
"#,
        );
        let has_int = result.expr_types.values().any(|t| *t == Type::Int);
        assert!(has_int, "method call should produce Type::Int");
    }

    #[test]
    fn expr_types_populated_for_string_interpolation() {
        let result = check_full(
            r#"
function main() {
    let name: String = "world"
    let msg: String = "hello {name}"
}
"#,
        );
        let string_count = result
            .expr_types
            .values()
            .filter(|t| **t == Type::String)
            .count();
        // At least: the "world" literal, the "hello {name}" interpolation, and the `name` ident
        assert!(
            string_count >= 3,
            "should have at least 3 String-typed expressions, got {}",
            string_count
        );
    }

    // ── Snapshot tests for error messages ──────────────────────────────

    #[test]
    fn snapshot_error_type_mismatch() {
        let diags = check_source(r#"function main() { let x: Int = "hello" }"#);
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }

    #[test]
    fn snapshot_error_undefined_variable() {
        let diags = check_source("function main() { print(x) }");
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }

    #[test]
    fn snapshot_error_immutable_assignment() {
        let diags = check_source("function main() { let x: Int = 1\n x = 2 }");
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }

    #[test]
    fn snapshot_error_wrong_arg_count() {
        let diags = check_source(
            "function add(a: Int, b: Int) -> Int { return a + b }\nfunction main() { add(1) }",
        );
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }

    #[test]
    fn snapshot_error_trait_not_implemented() {
        let diags = check_source(
            "trait Display {\n  function toString(self) -> String\n}\nstruct Point { Int x  Int y }\nfunction show<T: Display>(item: T) -> String { return item.toString() }\nfunction main() { show(Point(1, 2)) }",
        );
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }
}
