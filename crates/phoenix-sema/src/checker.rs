use crate::resolved::Analysis;
use crate::scope::{ScopeStack, VarInfo};
use crate::types::Type;
use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::ids::FuncId;
use phoenix_common::span::Span;
use phoenix_parser::ast::{
    Block, CaptureInfo, Declaration, ElseBranch, Expr, FunctionDecl, IfExpr, ImplBlock,
    InlineTraitImpl, MethodCallExpr, Param, Program, Statement,
};
use std::collections::{HashMap, HashSet};

/// A reference from a use-site to a symbol definition.
///
/// Stored in [`ResolvedModule::symbol_references`] for each identifier,
/// field access, or type reference that resolves to a known declaration.
#[derive(Debug, Clone)]
pub struct SymbolRef {
    /// What kind of symbol is being referenced.
    pub kind: SymbolKind,
    /// The name of the referenced symbol.
    pub name: String,
}

/// The kind of symbol a reference points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    /// A function declared with `function`.
    Function,
    /// A struct declared with `struct`.
    Struct,
    /// An enum declared with `enum`.
    Enum,
    /// A local variable or parameter.
    Variable,
    /// A field on a struct (`struct_name.field_name`).
    Field {
        /// The struct type that owns this field.
        struct_name: String,
    },
    /// A method on a type.
    Method {
        /// The type that owns this method.
        type_name: String,
    },
    /// An enum variant (e.g., `Some`, `None`, `Ok`, `Err`).
    EnumVariant {
        /// The enum type that owns this variant.
        enum_name: String,
    },
}

/// Information about a registered function signature, captured during the
/// first pass so that call sites can be type-checked in the second pass.
#[derive(Debug, Clone)]
pub struct FunctionInfo {
    /// Stable id for this function within the resolved module.  Allocated
    /// by [`Checker`] during the registration pass in AST-declaration
    /// order; IR lowering adopts the same id.
    pub func_id: FuncId,
    /// Source span of the function name identifier (for go-to-definition).
    pub definition_span: Span,
    /// Generic type parameters declared on this function.
    pub type_params: Vec<String>,
    /// Trait bounds for type parameters (e.g. `[("T", ["Display"])]`).
    pub type_param_bounds: Vec<(String, Vec<String>)>,
    /// The resolved types of the function parameters (excludes `self`).
    pub params: Vec<Type>,
    /// Parameter names, parallel to `params` (excludes `self`).
    pub param_names: Vec<String>,
    /// Default-value expressions keyed by parameter index (relative to
    /// the `params` / `param_names` vectors, i.e. excluding `self`).
    ///
    /// Cloned from the parsed AST at function registration so IR
    /// lowering can materialize the default at each call site that
    /// omits the slot, and so call-site validation can see which
    /// parameters have defaults without re-walking the AST.  Callers
    /// querying "does parameter i have a default?" use
    /// `default_param_exprs.contains_key(&i)`; callers iterating
    /// defaults use `default_param_exprs.keys()`.
    pub default_param_exprs: HashMap<usize, Expr>,
    /// The resolved return type.
    pub return_type: Type,
}

/// Information about a registered trait.
#[derive(Debug, Clone)]
pub struct TraitInfo {
    /// Source span of the trait name identifier (for go-to-definition).
    pub definition_span: Span,
    /// Generic type parameters declared on this trait.
    pub type_params: Vec<String>,
    /// The method signatures required by this trait.  Iteration order is the
    /// declaration order and is the ordering contract that the `dyn Trait`
    /// vtable relies on — each method's index in this `Vec` IS its slot
    /// index in the emitted vtable.  Do not reorder or sort.
    pub methods: Vec<TraitMethodInfo>,
    /// `None` if the trait is object-safe (can be used as `dyn TraitName`);
    /// `Some(human_readable_reason)` if not.  Populated once at trait
    /// registration time by `object_safety::validate` and read at every
    /// `dyn TraitName` type-expression resolution site.  A non-object-safe
    /// trait is still usable as a generic bound (`<T: TraitName>`).
    ///
    /// A trait with zero methods is trivially object-safe (`None`) — a
    /// `dyn EmptyTrait` pair would simply never invoke anything through
    /// its vtable, which is vacuously correct.
    pub object_safety_error: Option<String>,
}

/// Information about a method signature in a trait.
#[derive(Debug, Clone)]
pub struct TraitMethodInfo {
    /// The method name.
    pub name: String,
    /// The resolved parameter types (excludes `self`).
    pub params: Vec<Type>,
    /// The resolved return type.
    pub return_type: Type,
}

/// A single resolved field in a struct definition, including any constraint.
#[derive(Debug, Clone)]
pub struct FieldInfo {
    /// The field name.
    pub name: String,
    /// The resolved type.
    pub ty: Type,
    /// Optional constraint expression (from `where` clause on the field).
    pub constraint: Option<phoenix_parser::ast::Expr>,
    /// Source span of the field declaration.
    pub definition_span: Span,
}

/// Information about a registered struct definition (fields and generic params).
#[derive(Debug, Clone)]
pub struct StructInfo {
    /// Source span of the struct name identifier (for go-to-definition).
    pub definition_span: Span,
    /// Generic type parameters declared on this struct.
    pub type_params: Vec<String>,
    /// The resolved field definitions.
    pub fields: Vec<FieldInfo>,
}

/// Information about a registered enum definition (variants and generic params).
#[derive(Debug, Clone)]
pub struct EnumInfo {
    /// Source span of the enum name identifier (for go-to-definition).
    pub definition_span: Span,
    /// Generic type parameters declared on this enum.
    pub type_params: Vec<String>,
    /// The enum variants as `(name, field_types)` pairs.
    pub variants: Vec<(String, Vec<Type>)>,
}

/// Information about a method registered on a type (parameter types exclude `self`).
#[derive(Debug, Clone)]
pub struct MethodInfo {
    /// Stable id for user-declared methods.  `None` for built-in stdlib
    /// methods (`Option.unwrap`, `List.push`, …) — those have no IR
    /// function; the Cranelift backend inlines each one.  Allocated by
    /// [`Checker`] during the registration pass in AST-declaration order
    /// and adopted unchanged by IR lowering.
    pub func_id: Option<FuncId>,
    /// Source span of the method name identifier (for go-to-definition).
    pub definition_span: Span,
    /// Parameter types for the method, excluding the implicit `self` receiver.
    pub params: Vec<Type>,
    /// Parameter names in source order, aligned with `params` (i.e.
    /// excluding `self`).  Used for diagnostics — in particular, the
    /// "missing argument for parameter `x`" message in
    /// `check_method_args`.  For built-in methods populated via
    /// [`MethodInfo::builtin`], synthesized `arg1, arg2, …` names stand
    /// in; the invariant is `param_names.len() == params.len()`.
    pub param_names: Vec<String>,
    /// Default-value expressions keyed by parameter index (relative to
    /// the `params` vector — i.e. excluding `self`).
    ///
    /// Populated at registration time in the same shape as
    /// [`FunctionInfo::default_param_exprs`]; consumed by IR lowering's
    /// `merge_method_call_args` and by the AST interpreter's
    /// `call_method`.  `self` is never defaulted and never appears here.
    pub default_param_exprs: HashMap<usize, Expr>,
    /// The method's return type.
    pub return_type: Type,
    /// The method's own generic type parameters in source order.
    ///
    /// Does **not** include the parent `impl` block's type parameters; those
    /// are bound by the receiver's type (e.g., calling `p.swap()` on
    /// `Pair<Int, String>` supplies the `impl<T, U>`-level `T` and `U`
    /// directly, without re-inference). Only the method's own `<...>`
    /// binders appear here, and only these are inferred from argument
    /// types at each call site for IR monomorphization.
    pub type_params: Vec<String>,
    /// `true` if the method's first parameter is the implicit `self`
    /// receiver. `false` for static / associated functions declared in
    /// an `impl` block. Recorded at registration time so IR lowering
    /// can drive method registration from [`crate::ResolvedModule`]
    /// without re-inspecting the AST.
    pub has_self: bool,
}

impl MethodInfo {
    /// Construct a built-in method descriptor with no per-method generic
    /// parameters. Use [`MethodInfo`]'s struct literal form when a method
    /// has its own `type_params`.
    ///
    /// Built-in methods are always called as `receiver.method(...)`, so
    /// `has_self` is `true`.
    pub(crate) fn builtin(params: Vec<Type>, return_type: Type) -> Self {
        let param_names = (0..params.len()).map(|i| format!("arg{}", i + 1)).collect();
        Self {
            func_id: None,
            definition_span: Span::BUILTIN,
            params,
            param_names,
            default_param_exprs: HashMap::new(),
            return_type,
            type_params: Vec::new(),
            has_self: true,
        }
    }
}

/// A resolved field in a derived endpoint body type.
///
/// Each `DerivedField` represents a single field after all `omit`/`pick`/`partial`
/// modifiers have been applied to the base struct. Fields removed by `omit` or
/// not included by `pick` are absent entirely; fields made optional by `partial`
/// have `optional` set to `true`.
#[derive(Debug, Clone)]
pub struct DerivedField {
    /// The field name.
    pub name: String,
    /// The resolved type of the field.
    pub ty: Type,
    /// Whether this field is optional (from a `partial` modifier).
    pub optional: bool,
    /// Constraint inherited from the base struct field's `where` clause.
    pub constraint: Option<phoenix_parser::ast::Expr>,
}

/// A resolved derived type for an endpoint body, with all modifiers applied.
///
/// Produced by [`Checker::check_endpoint`] after validating and applying
/// the `omit`/`pick`/`partial` modifier chain to the base struct's fields.
/// Downstream code generators use this to emit the exact field set for
/// request body types without re-evaluating the modifier chain.
#[derive(Debug, Clone)]
pub struct ResolvedDerivedType {
    /// The base struct type name.
    pub base_type: String,
    /// The fields after applying omit/pick/partial modifiers.
    pub fields: Vec<DerivedField>,
}

/// A literal default value for a query parameter, extracted from the AST.
///
/// Stored in a language-agnostic form so that code generators for different
/// target languages can each emit the correct literal syntax.
#[derive(Debug, Clone)]
pub enum DefaultValue {
    /// An integer default, e.g. `1`.
    Int(i64),
    /// A floating-point default, e.g. `3.14`.
    Float(f64),
    /// A string default, e.g. `"hello"`.
    String(String),
    /// A boolean default: `true` or `false`.
    Bool(bool),
}

/// Information about a resolved query parameter in an endpoint.
///
/// Captures the parameter name, its resolved type, and whether a default
/// value was provided. Code generators use this to determine which query
/// parameters are required vs. optional in generated client signatures,
/// and to apply default values in server router wiring.
#[derive(Debug, Clone)]
pub struct QueryParamInfo {
    /// The parameter name.
    pub name: String,
    /// The resolved type.
    pub ty: Type,
    /// Whether the parameter has a default value.
    pub has_default: bool,
    /// The literal default value, if provided. Used by server-side code
    /// generators to apply defaults when query parameters are omitted.
    pub default_value: Option<DefaultValue>,
}

/// Resolved information about an endpoint declaration, produced during
/// semantic analysis.
///
/// Contains everything a code generator needs to emit typed client methods,
/// server handler stubs, and OpenAPI-style documentation for a single HTTP
/// endpoint: the HTTP method, URL pattern (with extracted path parameters),
/// query parameters, resolved body and response types, and error variants.
#[derive(Debug, Clone)]
pub struct EndpointInfo {
    /// The endpoint name (e.g., "createUser").
    pub name: String,
    /// The HTTP method.
    pub method: phoenix_parser::ast::HttpMethod,
    /// The URL path pattern (e.g., "/api/users/{id}").
    pub path: String,
    /// Path parameters extracted from `{param}` segments in the URL.
    pub path_params: Vec<String>,
    /// Resolved query parameters.
    pub query_params: Vec<QueryParamInfo>,
    /// The resolved body type with modifiers applied, if present.
    pub body: Option<ResolvedDerivedType>,
    /// The resolved response type, if declared.
    pub response: Option<Type>,
    /// Error variants: (name, status_code).
    pub errors: Vec<(String, i64)>,
    /// Doc comment attached to this endpoint.
    pub doc_comment: Option<String>,
}

/// Information about a registered type alias.
///
/// Stores the generic parameters and the resolved target type so that the
/// checker can expand aliases when they appear in type positions.
#[derive(Debug, Clone)]
pub struct TypeAliasInfo {
    /// Source span of the alias name identifier (for go-to-definition).
    pub definition_span: Span,
    /// Generic type parameters declared on the alias (e.g. `["T"]` for
    /// `type StringResult<T> = Result<T, String>`). Empty for non-generic aliases.
    pub type_params: Vec<String>,
    /// The resolved target type that this alias expands to.
    pub target: Type,
}

/// The semantic checker for a Phoenix program.
///
/// During checking the [`Checker`] maintains:
/// - **Name-keyed registries** (`functions`, `structs`, `enums`,
///   `methods`, `traits`, `type_aliases`) used for in-pass lookup.
///   These are flattened into id-indexed [`Vec`]s on
///   [`ResolvedModule`] at the end of [`check`].
/// - **`FuncId` allocation state** (`next_func_id`,
///   `pending_user_method_ids`).  Free-function ids are assigned in
///   AST-declaration order at [`Self::register_function`]; user-method
///   ids are pre-allocated by [`Self::pre_allocate_user_method_ids`]
///   in the AST-declaration order that
///   [`crate::resolved::ResolvedModule`] commits to (struct/enum
///   inline methods at the type's declaration site, inherent then
///   trait impls, with standalone `impl` blocks visited at their own
///   declaration site), so that IR lowering can adopt the same ids
///   verbatim by walking the AST in identical order.
/// - **Per-span output maps** (`expr_types`, `call_type_args`,
///   `var_annotation_types`, `symbol_references`, `lambda_captures`)
///   that pass through to [`ResolvedModule`] unchanged.
/// - **Transient checking state** (`scopes`, `current_return_type`,
///   `loop_depth`, `current_type_params*`) that is dropped when
///   [`check`] returns.
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
    /// Resolved endpoint declarations.
    pub(crate) endpoints: Vec<EndpointInfo>,
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
    /// Symbol references collected during checking.
    pub(crate) symbol_references: HashMap<Span, SymbolRef>,
    /// Concrete type arguments inferred at each generic function call site,
    /// keyed by the call expression's source span.
    pub(crate) call_type_args: HashMap<Span, Vec<Type>>,
    /// Resolved annotation type for each `let`-with-annotation, keyed by
    /// the `VarDecl`'s source span. See
    /// [`ResolvedModule::var_annotation_types`].
    pub(crate) var_annotation_types: HashMap<Span, Type>,
    /// Next `FuncId` to allocate.  Free-function ids occupy `0..N` in
    /// AST-declaration order, then user-method ids occupy `N..N+M` in
    /// AST-declaration order; the boundary is recorded as
    /// [`Self::user_method_offset`].
    pub(crate) next_func_id: u32,
    /// `FuncId` at which the user-method range begins.  Captured
    /// after free-function id allocation completes and reflected onto
    /// [`ResolvedModule::user_method_offset`].
    pub(crate) user_method_offset: u32,
    /// Pre-allocated FuncIds for free functions, keyed by name.
    /// Populated by [`Self::pre_allocate_function_ids`] before
    /// registration so [`crate::check_register::Checker::register_function`]
    /// can stamp each `FunctionInfo` with the id IR will adopt.
    pub(crate) pending_function_ids: HashMap<String, FuncId>,
    /// Pre-allocated FuncIds for user methods, keyed by
    /// `(type_name, method_name)`.  Populated by
    /// [`Self::pre_allocate_user_method_ids`] before registration so
    /// that [`crate::check_register::Checker::register_impl`] can
    /// stamp each `MethodInfo` with the id IR will see.
    pub(crate) pending_user_method_ids: HashMap<(String, String), FuncId>,
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
            endpoints: Vec::new(),
            diagnostics: Vec::new(),
            lambda_captures: HashMap::new(),
            current_return_type: None,
            loop_depth: 0,
            current_type_params: Vec::new(),
            current_type_param_bounds: Vec::new(),
            expr_types: HashMap::new(),
            symbol_references: HashMap::new(),
            call_type_args: HashMap::new(),
            var_annotation_types: HashMap::new(),
            next_func_id: 0,
            user_method_offset: 0,
            pending_function_ids: HashMap::new(),
            pending_user_method_ids: HashMap::new(),
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
                            definition_span: s.name_span,
                            type_params: s.type_params.clone(),
                            fields: Vec::new(),
                        },
                    );
                }
                Declaration::Enum(e) => {
                    self.enums.insert(
                        e.name.clone(),
                        EnumInfo {
                            definition_span: e.name_span,
                            type_params: e.type_params.clone(),
                            variants: Vec::new(),
                        },
                    );
                }
                _ => {}
            }
        }

        // Pre-pass A: allocate `FuncId`s for free functions in AST
        // declaration order.  Free functions occupy `0..N`.  This runs
        // before the registration pass so `register_function` can stamp
        // each `FunctionInfo` with the id IR lowering will adopt.
        self.pre_allocate_function_ids(program);

        // Pre-pass B: allocate `FuncId`s for user-declared methods,
        // matching IR lowering's registration order — inline methods at
        // the struct/enum declaration site (inherent first, then trait
        // impls in source order), and standalone `impl` blocks at their
        // own declaration site.  User methods occupy `N..N+M`.
        self.user_method_offset = self.next_func_id;
        self.pre_allocate_user_method_ids(program);

        // Pre-pass invariant: pre-pass A allocated exactly
        // `user_method_offset` ids (one per unique free-function
        // name), and pre-pass B's count plus that boundary equals
        // the next available id.  If a future refactor reorders
        // these passes, this assertion fires with a clear
        // diagnostic — much better than a HashMap-index panic deep
        // inside `register_*`.
        debug_assert_eq!(
            self.user_method_offset as usize,
            self.pending_function_ids.len(),
            "user_method_offset must equal the number of pre-allocated free-function ids"
        );
        debug_assert_eq!(
            self.pending_user_method_ids.len() as u32,
            self.next_func_id - self.user_method_offset,
            "pre-pass B id count disagrees with the user-method id range"
        );

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
                Declaration::Endpoint(ep) => self.check_endpoint(ep),
                Declaration::Schema(_) => {} // Parse-only, no type checking
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
                Declaration::Trait(_)
                | Declaration::TypeAlias(_)
                | Declaration::Endpoint(_)
                | Declaration::Schema(_) => {} // Parse-only, no checking
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

            // Parameter-type resolution happens under `with_type_params`
            // so `<T>`-style names in annotations resolve; params
            // themselves are not bound into any scope yet.
            let non_self: Vec<&Param> = func.params.iter().filter(|p| p.name != "self").collect();
            let param_types: Vec<Type> = non_self
                .iter()
                .map(|p| this.resolve_type_expr(&p.type_annotation))
                .collect();

            this.check_param_defaults(&non_self, &param_types);

            // Pass 2: fresh scope, bind params, check body.
            this.scopes.push();
            for (param, ty) in non_self.into_iter().zip(param_types.into_iter()) {
                this.scopes.define(
                    param.name.clone(),
                    VarInfo {
                        ty,
                        is_mut: false,
                        definition_span: param.span,
                    },
                );
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
                // Tail `if` expression: three outcomes depending on branch shape.
                //   1. All branches diverge (e.g. each ends in `return`)  → OK.
                //   2. Implicit type is Void (e.g. missing `else`)        → "does not return" error.
                //   3. Otherwise → standard implicit-return type check.
                if let Expr::If(if_expr) = &expr_stmt.expr
                    && Self::if_expr_diverges(if_expr)
                {
                    return;
                }
                let is_tail_if = matches!(&expr_stmt.expr, Expr::If(_));
                let implicit_type = self.infer_expr_type(&expr_stmt.expr);
                if is_tail_if && implicit_type == Type::Void {
                    if !func.body.statements.iter().any(Self::contains_return) {
                        self.error(
                            format!(
                                "function `{}` has return type `{}` but body does not return a value",
                                func.name, return_type
                            ),
                            func.span,
                        );
                    }
                    return;
                }
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
                this.current_return_type = Some(return_type.clone());

                // Resolve non-self parameter types under `with_type_params`
                // scope; no params / `self` bound yet.
                let non_self: Vec<&Param> =
                    func.params.iter().filter(|p| p.name != "self").collect();
                let param_types: Vec<Type> = non_self
                    .iter()
                    .map(|p| this.resolve_type_expr(&p.type_annotation))
                    .collect();

                this.check_param_defaults(&non_self, &param_types);

                // Pass 2: fresh scope, bind `self` and params, check body.
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
                        definition_span: func.span,
                    },
                );

                for (param, ty) in non_self.into_iter().zip(param_types.into_iter()) {
                    this.scopes.define(
                        param.name.clone(),
                        VarInfo {
                            ty,
                            is_mut: false,
                            definition_span: param.span,
                        },
                    );
                }

                this.check_block(&func.body);
                this.validate_implicit_return(func, &return_type);
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

    /// Pass-1 validation of parameter default expressions.
    ///
    /// Shared by [`Self::check_function`] and [`Self::check_impl`]'s
    /// method-body pass.  Defaults are materialized at the caller's
    /// call site (see [design-decisions.md: *Default-argument lowering
    /// strategy*]), so at this point neither `self` nor sibling params
    /// are legal references — a default referencing either would fail
    /// at lowering/runtime.
    ///
    /// The scope push/pop here is load-bearing: pass 1 must run with
    /// an empty (param-free) scope layer so any accidental
    /// `scopes.define()` that creeps into a default expression fails
    /// loudly rather than silently masking unbound-identifier
    /// diagnostics.  Callers must *not* rely on any definitions
    /// surviving this call.
    ///
    /// `params` and `param_types` must be aligned and in source order.
    pub(crate) fn check_param_defaults(&mut self, params: &[&Param], param_types: &[Type]) {
        debug_assert_eq!(
            params.len(),
            param_types.len(),
            "check_param_defaults: params/param_types length mismatch",
        );
        self.scopes.push();
        for (param, ty) in params.iter().zip(param_types.iter()) {
            let Some(ref default_expr) = param.default_value else {
                continue;
            };
            let default_ty = self.check_expr(default_expr);
            if !default_ty.is_error() && !ty.is_error() && !self.types_compatible(ty, &default_ty) {
                self.error(
                    format!(
                        "default value for parameter `{}`: expected `{}` but got `{}`",
                        param.name, ty, default_ty
                    ),
                    default_expr.span(),
                );
            }
            // Free type vars in the default's inferred type would trip
            // `contains_type_var` in mono — caller-site substitution
            // binds the caller's type params, not the callee's.
            if !default_ty.is_error() && default_ty.has_type_vars() {
                self.error(
                    format!(
                        "default value for parameter `{}`: type `{}` references the \
                         function's own generic parameters — default expressions are \
                         evaluated at the caller's call site where those parameters \
                         are not bound",
                        param.name, default_ty
                    ),
                    default_expr.span(),
                );
            }
        }
        self.scopes.pop();
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

    /// Returns `true` if a block's last statement diverges (cannot produce
    /// a value).  Diverging blocks are compatible with any expected type in
    /// match arm / if-branch type checking.
    ///
    /// Diverges when the last statement is `break`, `continue`, `return`,
    /// or an `if`-expression whose every branch diverges.
    pub(crate) fn block_diverges(block: &Block) -> bool {
        match block.statements.last() {
            Some(Statement::Break(_) | Statement::Continue(_) | Statement::Return(_)) => true,
            Some(Statement::Expression(expr_stmt)) => match &expr_stmt.expr {
                Expr::If(if_expr) => Self::if_expr_diverges(if_expr),
                _ => false,
            },
            _ => false,
        }
    }

    /// Returns `true` if every branch of an `if`/`else if`/`else` chain
    /// diverges.  Used for tail-position `if` expressions like
    /// `if c { return a } else { return b }`.
    pub(crate) fn if_expr_diverges(if_expr: &IfExpr) -> bool {
        if !Self::block_diverges(&if_expr.then_block) {
            return false;
        }
        match &if_expr.else_branch {
            Some(ElseBranch::Block(b)) => Self::block_diverges(b),
            Some(ElseBranch::ElseIf(nested)) => Self::if_expr_diverges(nested),
            None => false, // missing else → non-diverging (value is Void)
        }
    }

    /// Returns `true` if a statement contains (or is) an explicit `return`.
    ///
    /// Recurses into `if`/`else` and loop bodies so that returns nested inside
    /// control flow are detected.  This is a conservative check -- it does not
    /// prove that *all* paths return, only that *at least one* return exists.
    fn contains_return(stmt: &Statement) -> bool {
        match stmt {
            Statement::Return(_) => true,
            Statement::Expression(es) => Self::expr_contains_return(&es.expr),
            Statement::VarDecl(vd) => Self::expr_contains_return(&vd.initializer),
            Statement::While(w) => {
                Self::expr_contains_return(&w.condition)
                    || w.body.statements.iter().any(Self::contains_return)
                    || w.else_block
                        .as_ref()
                        .is_some_and(|b| b.statements.iter().any(Self::contains_return))
            }
            Statement::For(f) => {
                f.body.statements.iter().any(Self::contains_return)
                    || f.else_block
                        .as_ref()
                        .is_some_and(|b| b.statements.iter().any(Self::contains_return))
            }
            Statement::Break(_) | Statement::Continue(_) => false,
        }
    }

    /// Returns `true` if an expression contains an explicit `return`.
    ///
    /// Only constructs that can embed statements (currently `if` expressions)
    /// need to recurse; other expressions cannot contain a `return` statement.
    fn expr_contains_return(expr: &Expr) -> bool {
        match expr {
            Expr::If(if_expr) => Self::if_expr_contains_return(if_expr),
            _ => false,
        }
    }

    /// Returns `true` if any branch of an `if`/`else if`/`else` chain
    /// contains an explicit `return`.
    fn if_expr_contains_return(if_expr: &IfExpr) -> bool {
        if if_expr
            .then_block
            .statements
            .iter()
            .any(Self::contains_return)
        {
            return true;
        }
        match &if_expr.else_branch {
            Some(ElseBranch::Block(b)) => b.statements.iter().any(Self::contains_return),
            Some(ElseBranch::ElseIf(nested)) => Self::if_expr_contains_return(nested),
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

    /// Records a symbol reference at the given use-site span.
    ///
    /// This is called during type checking whenever an identifier, field access,
    /// or type name resolves to a known declaration. The LSP uses this data for
    /// go-to-definition, find-references, and rename.
    pub(crate) fn record_reference(&mut self, use_span: Span, kind: SymbolKind, name: String) {
        self.symbol_references
            .insert(use_span, SymbolRef { kind, name });
    }
}

/// Type-checks a Phoenix program and returns the [`Analysis`] that
/// downstream stages consume.
///
/// Performs three phases:
/// 1. **Pre-allocation:** assigns stable [`FuncId`]s for every free
///    function and user-declared method in AST-declaration order, so
///    sema and IR lowering share a single id space.
/// 2. **Registration pass:** collects all type, function, and trait
///    signatures into name-keyed registries on the [`Checker`].
/// 3. **Checking pass:** validates types, resolves names, and checks
///    constraints; populates per-span output maps.
///
/// The returned [`Analysis`] contains:
/// - `module: ResolvedModule` — the IR-facing schema (callables,
///   types, per-span maps).  IR lowering and the IR interpreter
///   take `&ResolvedModule` directly.
/// - `diagnostics` — empty iff the program is semantically valid.
/// - `endpoints` — for `phoenix-codegen`.
/// - `symbol_references` — for the LSP.
/// - `trait_impls` and `type_aliases` — sema-internal outputs kept
///   for tooling consumers.
///
/// At the end, the [`Checker`]'s registries are flattened and
/// ownership of all collected state is transferred (no clones — see
/// [`crate::resolved`] for the layout contract).
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
pub fn check(program: &Program) -> Analysis {
    let mut checker = Checker::new();
    checker.check_program(program);
    crate::resolved::build_from_checker(program, checker)
}
