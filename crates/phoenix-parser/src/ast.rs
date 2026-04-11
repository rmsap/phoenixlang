use phoenix_common::span::Span;
use serde::Serialize;

/// The root node of a Phoenix program AST.
///
/// Every Phoenix source file is parsed into a single `Program` containing
/// an ordered list of top-level declarations.
#[derive(Debug, Clone, Serialize)]
pub struct Program {
    /// The top-level declarations in source order.
    pub declarations: Vec<Declaration>,
    /// The span covering the entire program.
    pub span: Span,
}

/// A top-level declaration in a Phoenix program.
///
/// Phoenix programs consist of function definitions, struct and enum type
/// declarations, and `impl` blocks that attach methods to types.
#[derive(Debug, Clone, Serialize)]
pub enum Declaration {
    /// A function declaration: `fn name(params) -> ReturnType { body }`.
    Function(FunctionDecl),
    /// A struct type declaration: `struct Name { fields }`.
    Struct(StructDecl),
    /// An enum (algebraic data type) declaration: `enum Name { variants }`.
    Enum(EnumDecl),
    /// An `impl` block attaching methods to a named type.
    Impl(ImplBlock),
    /// A trait declaration: `trait Name { method signatures }`.
    Trait(TraitDecl),
    /// A type alias declaration: `type Name = TypeExpr` or `type Name<T> = TypeExpr`.
    TypeAlias(TypeAliasDecl),
    /// An endpoint declaration: `endpoint name: METHOD "path" { ... }`.
    Endpoint(EndpointDecl),
    /// A database schema declaration: `schema name { table ... }`.
    Schema(SchemaDecl),
}

/// A function declaration, including its name, parameters, optional return
/// type, and body block. May include generic type parameters.
///
/// ```text
/// function add(a: Int, b: Int) -> Int {
///     return a + b
/// }
///
/// function identity<T>(x: T) -> T {
///     return x
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct FunctionDecl {
    /// The function name.
    pub name: String,
    /// Source span covering just the function name identifier.
    pub name_span: Span,
    /// Generic type parameters (e.g. `["T", "U"]` for `function map<T, U>(...)`).
    pub type_params: Vec<String>,
    /// Trait bounds for type parameters (e.g. `T -> [Display]` for `<T: Display>`).
    pub type_param_bounds: Vec<(String, Vec<String>)>,
    /// The list of formal parameters.
    pub params: Vec<Param>,
    /// The declared return type, or `None` if the function returns `Void`.
    pub return_type: Option<TypeExpr>,
    /// The function body.
    pub body: Block,
    /// Source span covering the entire function declaration.
    pub span: Span,
}

/// A single function parameter with its type annotation, name, and optional
/// default value.
///
/// ```text
/// Int count
/// String greeting = "Hello"
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct Param {
    /// The declared type of the parameter.
    pub type_annotation: TypeExpr,
    /// The parameter name.
    pub name: String,
    /// An optional default value expression for the parameter.
    pub default_value: Option<Expr>,
    /// Source span covering the parameter.
    pub span: Span,
}

/// A brace-delimited block of statements.
///
/// Blocks appear as function bodies, loop bodies, `if`/`else` branches,
/// and match-arm bodies.
#[derive(Debug, Clone, Serialize)]
pub struct Block {
    /// The statements contained in this block, in source order.
    pub statements: Vec<Statement>,
    /// Source span covering the opening `{` through the closing `}`.
    pub span: Span,
}

/// A single statement inside a block.
///
/// Phoenix statements include variable declarations, expression statements,
/// return statements, and control-flow constructs (`if`, `while`, `for`).
#[derive(Debug, Clone, Serialize)]
pub enum Statement {
    /// A variable declaration: `[mut] Type name = expr`.
    VarDecl(VarDecl),
    /// An expression used as a statement (e.g. a function call).
    Expression(ExprStmt),
    /// A `return` statement.
    Return(ReturnStmt),
    /// An `if` / `else if` / `else` statement.
    If(IfStmt),
    /// A `while` loop.
    While(WhileStmt),
    /// A `for` loop over a range.
    For(ForStmt),
    /// A `break` statement that exits the enclosing loop.
    Break(Span),
    /// A `continue` statement that skips to the next loop iteration.
    Continue(Span),
}

/// The binding target of a variable declaration.
///
/// Supports both simple name bindings and struct destructuring patterns.
#[derive(Debug, Clone, Serialize)]
pub enum VarDeclTarget {
    /// A simple variable name: `let x = ...`
    Simple(String),
    /// A struct destructuring pattern: `let Point { x, y } = ...`
    StructDestructure {
        /// The struct type name (e.g. `Point`).
        type_name: String,
        /// The field names to bind (e.g. `["x", "y"]`).
        field_names: Vec<String>,
    },
}

/// A variable declaration with an optional mutability qualifier.
///
/// ```text
/// let x: Int = 10
/// let mut name: String = "hello"
/// let inferred = 42
/// let Point { x, y } = some_point
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct VarDecl {
    /// Whether the variable is declared as mutable (`mut`).
    pub is_mut: bool,
    /// The declared type of the variable, or `None` if the type is inferred
    /// from the initializer expression.
    pub type_annotation: Option<TypeExpr>,
    /// The binding target (simple name or destructuring pattern).
    pub target: VarDeclTarget,
    /// The initializer expression.
    pub initializer: Expr,
    /// Source span covering the entire declaration.
    pub span: Span,
}

impl VarDecl {
    /// Returns the variable name for simple bindings, or `None` for
    /// destructuring patterns.  Use `target` directly when handling both
    /// variants.
    pub fn simple_name(&self) -> Option<&str> {
        match &self.target {
            VarDeclTarget::Simple(name) => Some(name),
            VarDeclTarget::StructDestructure { .. } => None,
        }
    }
}

/// An expression used as a statement.
///
/// This wraps any expression that appears at the statement level, such as
/// a bare function call.
#[derive(Debug, Clone, Serialize)]
pub struct ExprStmt {
    /// The expression being evaluated for its side effects.
    pub expr: Expr,
    /// Source span covering the expression statement.
    pub span: Span,
}

/// A `return` statement with an optional value expression.
///
/// ```text
/// return x + 1
/// return
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct ReturnStmt {
    /// The value to return, or `None` for a bare `return`.
    pub value: Option<Expr>,
    /// Source span covering the return statement.
    pub span: Span,
}

/// An `if`/`else if`/`else` statement.
///
/// Chained `else if` branches are represented recursively via
/// [`ElseBranch::ElseIf`].
///
/// ```text
/// if x > 0 {
///     print("positive")
/// } else if x == 0 {
///     print("zero")
/// } else {
///     print("negative")
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct IfStmt {
    /// The boolean condition guarding the `then` block.
    pub condition: Expr,
    /// The block executed when `condition` is true.
    pub then_block: Block,
    /// An optional `else` or `else if` branch.
    pub else_branch: Option<ElseBranch>,
    /// Source span covering the entire `if` statement.
    pub span: Span,
}

/// The else branch of an `if` statement.
#[derive(Debug, Clone, Serialize)]
pub enum ElseBranch {
    /// A plain `else { ... }` block.
    Block(Block),
    /// An `else if ...` chain, represented as a nested [`IfStmt`].
    ElseIf(Box<IfStmt>),
}

/// A `while` loop that runs its body as long as the condition is true.
///
/// ```text
/// while n > 0 {
///     n = n - 1
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct WhileStmt {
    /// The loop condition, evaluated before each iteration.
    pub condition: Expr,
    /// The loop body.
    pub body: Block,
    /// Optional else block, executed when the loop completes without `break`.
    pub else_block: Option<Block>,
    /// Source span covering the entire `while` statement.
    pub span: Span,
}

/// A `for` loop over an integer range.
///
/// ```text
/// for i in 0..10 {
///     print(i)
/// }
/// ```
/// The source of iteration for a `for` loop.
#[derive(Debug, Clone, Serialize)]
pub enum ForSource {
    /// Range-based: `for i in 0..10`
    Range {
        /// The start of the range (inclusive).
        start: Expr,
        /// The end of the range (exclusive).
        end: Expr,
    },
    /// Collection-based: `for item in expr`
    Iterable(Expr),
}

/// A `for` loop over a range or collection.
///
/// ```text
/// for i in 0..10 { print(i) }
/// for item in list { print(item) }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct ForStmt {
    /// The loop variable name.
    pub var_name: String,
    /// The declared type of the loop variable.
    pub var_type: Option<TypeExpr>,
    /// The iteration source (range or collection).
    pub source: ForSource,
    /// The loop body.
    pub body: Block,
    /// Optional else block, executed when the loop completes without `break`.
    pub else_block: Option<Block>,
    /// Source span covering the entire `for` statement.
    pub span: Span,
}

/// A struct type declaration. May include generic type parameters.
///
/// ```text
/// struct Point {
///     Int x
///     Int y
/// }
///
/// struct Pair<A, B> {
///     A first
///     B second
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct StructDecl {
    /// The struct name.
    pub name: String,
    /// Source span covering just the struct name identifier.
    pub name_span: Span,
    /// Generic type parameters (e.g. `["A", "B"]` for `struct Pair<A, B>`).
    pub type_params: Vec<String>,
    /// The fields declared in this struct.
    pub fields: Vec<FieldDecl>,
    /// Inline methods defined directly inside the struct body.
    pub methods: Vec<FunctionDecl>,
    /// Inline trait implementations defined inside the struct body.
    pub trait_impls: Vec<InlineTraitImpl>,
    /// Doc comment attached to this struct, if any.
    pub doc_comment: Option<String>,
    /// Source span covering the entire struct declaration.
    pub span: Span,
}

/// A field in a struct declaration.
///
/// ```text
/// Int x
/// String name where self.length > 0 and self.length <= 100
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct FieldDecl {
    /// The declared type of the field.
    pub type_annotation: TypeExpr,
    /// The field name.
    pub name: String,
    /// Optional constraint expression introduced by `where`. The expression
    /// must evaluate to `Bool` and uses `self` to refer to the field value.
    pub constraint: Option<Expr>,
    /// Doc comment attached to this field, if any.
    pub doc_comment: Option<String>,
    /// Source span covering the field declaration.
    pub span: Span,
}

/// An enum (algebraic data type) declaration. May include generic type
/// parameters that are referenced by variant fields.
///
/// ```text
/// enum Shape {
///     Circle(Float)
///     Rectangle(Float, Float)
///     Unit
/// }
///
/// enum Option<T> {
///     Some(T)
///     None
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct EnumDecl {
    /// The enum name.
    pub name: String,
    /// Source span covering just the enum name identifier.
    pub name_span: Span,
    /// Generic type parameters (e.g. `["T"]` for `enum Option<T>`).
    pub type_params: Vec<String>,
    /// The variants of this enum.
    pub variants: Vec<EnumVariant>,
    /// Inline methods defined directly inside the enum body.
    pub methods: Vec<FunctionDecl>,
    /// Inline trait implementations defined inside the enum body.
    pub trait_impls: Vec<InlineTraitImpl>,
    /// Doc comment attached to this enum, if any.
    pub doc_comment: Option<String>,
    /// Source span covering the entire enum declaration.
    pub span: Span,
}

/// A variant of an enum, optionally carrying typed fields.
///
/// ```text
/// Circle(Float)
/// None
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct EnumVariant {
    /// The variant name.
    pub name: String,
    /// The types of the positional fields, empty for unit variants.
    pub fields: Vec<TypeExpr>,
    /// Source span covering the variant declaration.
    pub span: Span,
}

/// An inline trait implementation defined inside a struct or enum body.
///
/// ```text
/// struct Point {
///     Int x
///     Int y
///
///     impl Display {
///         function to_string(self) -> String { "({self.x}, {self.y})" }
///     }
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct InlineTraitImpl {
    /// The name of the trait being implemented.
    pub trait_name: String,
    /// The methods defined in this inline trait impl.
    pub methods: Vec<FunctionDecl>,
    /// Source span covering the entire inline `impl TraitName { ... }` block.
    pub span: Span,
}

/// An `impl` block that attaches methods to a named type.
///
/// ```text
/// impl Point {
///     fn distance(Point self) -> Float { ... }
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct ImplBlock {
    /// The name of the type being implemented.
    pub type_name: String,
    /// If this is a trait impl, the trait name (e.g. `Display` in `impl Display for Point`).
    pub trait_name: Option<String>,
    /// The methods defined in this `impl` block.
    pub methods: Vec<FunctionDecl>,
    /// Source span covering the entire `impl` block.
    pub span: Span,
}

/// A trait declaration that defines a set of method signatures.
///
/// ```text
/// trait Display {
///     function to_string(self) -> String
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct TraitDecl {
    /// The trait name.
    pub name: String,
    /// Source span covering just the trait name identifier.
    pub name_span: Span,
    /// Generic type parameters for the trait.
    pub type_params: Vec<String>,
    /// The method signatures declared in this trait.
    pub methods: Vec<TraitMethodSig>,
    /// Source span covering the entire trait declaration.
    pub span: Span,
}

/// A method signature inside a trait declaration (no body).
///
/// ```text
/// function to_string(self) -> String
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct TraitMethodSig {
    /// The method name.
    pub name: String,
    /// The parameters of the method signature.
    pub params: Vec<Param>,
    /// The return type, or `None` for `Void`.
    pub return_type: Option<TypeExpr>,
    /// Source span covering the method signature.
    pub span: Span,
}

/// A type alias declaration that creates a named shorthand for a type.
///
/// Type aliases may include generic type parameters for partially applied
/// generic types.
///
/// ```text
/// type UserId = Int
/// type Handler = (Request) -> Response
/// type StringResult<T> = Result<T, String>
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct TypeAliasDecl {
    /// The alias name.
    pub name: String,
    /// Source span covering just the alias name identifier.
    pub name_span: Span,
    /// Generic type parameters (e.g. `["T"]` for `type StringResult<T> = ...`).
    pub type_params: Vec<String>,
    /// The type expression that this alias expands to.
    pub target: TypeExpr,
    /// Source span covering the entire type alias declaration.
    pub span: Span,
}

/// An HTTP method for an endpoint declaration.
///
/// Each variant maps directly to the corresponding uppercase keyword token
/// produced by the lexer (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`).
///
/// ```text
/// endpoint getUser: GET "/api/users/{id}" { ... }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HttpMethod {
    /// The `GET` HTTP method — used for read-only retrieval.
    Get,
    /// The `POST` HTTP method — used for creating resources.
    Post,
    /// The `PUT` HTTP method — used for full replacement of a resource.
    Put,
    /// The `PATCH` HTTP method — used for partial updates to a resource.
    Patch,
    /// The `DELETE` HTTP method — used for removing a resource.
    Delete,
}

impl HttpMethod {
    /// Returns the uppercase string representation (e.g. `"GET"`, `"POST"`).
    pub fn as_upper_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    /// Returns the lowercase string representation (e.g. `"get"`, `"post"`).
    pub fn as_lower_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
            Self::Put => "put",
            Self::Patch => "patch",
            Self::Delete => "delete",
        }
    }
}

/// A type derivation modifier applied to a base type in endpoint body declarations.
///
/// Modifiers chain left-to-right: `User omit { id } partial` means
/// "start with User, remove the `id` field, then make all remaining fields optional."
///
/// These modifiers are inspired by TypeScript's utility types (`Omit`, `Pick`,
/// `Partial`) and allow endpoint declarations to re-use existing struct
/// definitions without repeating field lists.
#[derive(Debug, Clone, Serialize)]
pub enum TypeModifier {
    /// `omit { field1, field2 }` -- exclude the listed fields from the base type.
    Omit {
        /// The field names to exclude.
        fields: Vec<String>,
        /// Source span covering the `omit { ... }` expression.
        span: Span,
    },
    /// `pick { field1, field2 }` -- include only the listed fields from the base type.
    Pick {
        /// The field names to include.
        fields: Vec<String>,
        /// Source span covering the `pick { ... }` expression.
        span: Span,
    },
    /// `partial` -- make fields optional.
    ///
    /// If `fields` is `None`, **all** fields become optional.
    /// If `fields` is `Some(vec)`, only the listed fields become optional.
    Partial {
        /// The field names to make optional, or `None` for all fields.
        fields: Option<Vec<String>>,
        /// Source span covering the `partial` or `partial { ... }` expression.
        span: Span,
    },
}

/// A derived type reference: a base type with zero or more chained modifiers.
///
/// Derived types appear in endpoint `body` declarations and allow re-using an
/// existing struct definition with field-level transformations.
///
/// ```text
/// User omit { id } partial          // all User fields except id, all optional
/// User pick { name, email }          // only name and email from User
/// User partial { email }             // User with email made optional
/// ```
///
/// When `modifiers` is empty the derived type is equivalent to a plain type
/// reference.
#[derive(Debug, Clone, Serialize)]
pub struct DerivedType {
    /// The base struct type that modifiers are applied to (e.g. `User`).
    pub base_type: TypeExpr,
    /// Chained type modifiers (`omit`, `pick`, `partial`), applied left-to-right.
    /// May be empty when the body type is used without derivation.
    pub modifiers: Vec<TypeModifier>,
    /// Source span covering the entire derived type expression, from the base
    /// type name through the last modifier.
    pub span: Span,
}

/// An error variant in an endpoint `error` block.
///
/// Each variant pairs a descriptive name with an HTTP status code. These are
/// used by code generators to produce typed error responses.
///
/// ```text
/// error {
///     NotFound(404)
///     Conflict(409)
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct EndpointErrorVariant {
    /// The error variant name (e.g. `"NotFound"`, `"Conflict"`).
    pub name: String,
    /// The HTTP status code associated with this error (e.g. `404`, `409`).
    pub status_code: i64,
    /// Source span covering the entire `Name(code)` variant.
    pub span: Span,
}

/// A query parameter declared in an endpoint `query` block.
///
/// Query parameters define the typed query-string inputs for an endpoint.
/// Each parameter has a type, a name, and an optional default value.
///
/// ```text
/// query {
///     Int page = 1
///     Option<String> search
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct QueryParam {
    /// The declared type of the query parameter (e.g. `Int`, `Option<String>`).
    pub type_annotation: TypeExpr,
    /// The parameter name as it appears in the query string (e.g. `"page"`).
    pub name: String,
    /// An optional default value expression, used when the parameter is omitted
    /// from the request.
    pub default_value: Option<Expr>,
    /// Source span covering the entire parameter declaration.
    pub span: Span,
}

/// An endpoint declaration describing a single HTTP API endpoint.
///
/// Endpoint declarations are a Phoenix Gen feature that enables code generation
/// of typed API clients and server stubs. Each endpoint specifies an HTTP
/// method, a URL path, and optional sections for query parameters, request
/// body, response type, and error variants.
///
/// ```text
/// /** Creates a new user account. */
/// endpoint createUser: POST "/api/users" {
///     body User omit { id }
///     response User
///     query {
///         Bool notify = true
///     }
///     error {
///         Conflict(409)
///     }
/// }
/// ```
///
/// All inner sections (`body`, `response`, `query`, `error`) are optional and
/// may appear in any order within the endpoint block.
#[derive(Debug, Clone, Serialize)]
pub struct EndpointDecl {
    /// The endpoint name, used as an identifier in generated code
    /// (e.g. `"createUser"`).
    pub name: String,
    /// Source span covering just the endpoint name identifier.
    pub name_span: Span,
    /// The HTTP method for this endpoint (e.g. `POST`).
    pub method: HttpMethod,
    /// The URL path pattern, potentially containing path parameters
    /// (e.g. `"/api/users/{id}"`).
    pub path: String,
    /// Query parameters declared in an optional `query { ... }` block.
    /// Empty if no query block is present.
    pub query_params: Vec<QueryParam>,
    /// The request body type with optional derivation modifiers, parsed from
    /// the `body` section. `None` if the endpoint has no body (e.g. `GET`).
    pub body: Option<DerivedType>,
    /// The response type, parsed from the `response` section.
    /// `None` if the endpoint does not declare a response type.
    pub response: Option<TypeExpr>,
    /// Error variants with HTTP status codes, parsed from the `error { ... }`
    /// block. Empty if no error block is present.
    pub errors: Vec<EndpointErrorVariant>,
    /// An optional doc comment (`/** ... */`) attached to this endpoint.
    /// Carries the trimmed inner text for use by code generators.
    pub doc_comment: Option<String>,
    /// Source span covering the entire endpoint declaration, from the
    /// `endpoint` keyword through the closing `}`.
    pub span: Span,
}

// ── Schema declarations (forward compatibility for Phase 4) ─────────

/// A database schema declaration containing table definitions.
///
/// Parsed for forward compatibility with Phase 4 (typed database queries
/// and migrations). Not type-checked or code-generated in the current phase.
///
/// ```text
/// schema db {
///     table users from User {
///         primary key id
///         unique email
///     }
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct SchemaDecl {
    /// The schema name (e.g., `"db"`).
    pub name: String,
    /// The table declarations within this schema.
    pub tables: Vec<SchemaTable>,
    /// Source span covering the entire schema declaration.
    pub span: Span,
}

/// A table declaration within a schema block.
///
/// May reference a Phoenix struct via `from TypeName` or declare columns
/// inline. Constraints (primary key, unique, index, foreign key, exclude)
/// are stored as opaque token sequences for forward compatibility.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaTable {
    /// The table name (e.g., `"users"`).
    pub name: String,
    /// Optional struct name from `from TypeName` clause.
    pub source_type: Option<String>,
    /// Raw constraint/column lines stored as token text sequences.
    pub body_tokens: Vec<Vec<String>>,
    /// Source span covering this table declaration.
    pub span: Span,
}

/// An expression node in the AST.
///
/// Expressions produce values and can appear on the right-hand side of
/// assignments, as function arguments, in conditions, and so on.
#[derive(Debug, Clone, Serialize)]
pub enum Expr {
    /// A literal value such as `42`, `3.14`, `"hello"`, or `true`.
    Literal(Literal),
    /// An identifier reference, e.g. a variable name.
    Ident(IdentExpr),
    /// A binary operation such as `a + b` or `x == y`.
    Binary(Box<BinaryExpr>),
    /// A unary operation such as `-x` or `not flag`.
    Unary(Box<UnaryExpr>),
    /// A function call such as `print(42)`.
    Call(Box<CallExpr>),
    /// An assignment such as `x = 42`.
    Assignment(Box<AssignmentExpr>),
    /// A field assignment such as `point.x = 10`.
    FieldAssignment(Box<FieldAssignmentExpr>),
    /// A field access such as `point.x`.
    FieldAccess(Box<FieldAccessExpr>),
    /// A method call such as `obj.method(args)`.
    MethodCall(Box<MethodCallExpr>),
    /// A struct literal constructor such as `Point(1, 2)`.
    StructLiteral(Box<StructLiteralExpr>),
    /// A `match` expression.
    Match(Box<MatchExpr>),
    /// A lambda (anonymous function) expression.
    ///
    /// ```text
    /// function(x: Int, y: Int) -> Int { return x + y }
    /// ```
    Lambda(Box<LambdaExpr>),
    /// A list literal expression such as `[1, 2, 3]` or `[]`.
    ListLiteral(ListLiteralExpr),
    /// A map literal expression such as `{"key": value}` or `{:}`.
    MapLiteral(MapLiteralExpr),
    /// A string interpolation expression such as `"hello {name}"`.
    StringInterpolation(StringInterpolationExpr),
    /// The `?` (try/propagation) operator applied to a `Result` or `Option` value.
    ///
    /// If the value is `Ok(v)` or `Some(v)`, evaluates to `v`.
    /// If it is `Err(e)` or `None`, immediately returns the error/none from
    /// the enclosing function.
    Try(Box<TryExpr>),
}

impl Expr {
    /// Returns the source [`Span`] of this expression, regardless of variant.
    ///
    /// This is a convenience method that dispatches to the inner span field
    /// for whichever expression variant is present.
    pub fn span(&self) -> Span {
        match self {
            Expr::Literal(l) => l.span,
            Expr::Ident(i) => i.span,
            Expr::Binary(b) => b.span,
            Expr::Unary(u) => u.span,
            Expr::Call(c) => c.span,
            Expr::Assignment(a) => a.span,
            Expr::FieldAssignment(fa) => fa.span,
            Expr::FieldAccess(f) => f.span,
            Expr::MethodCall(m) => m.span,
            Expr::StructLiteral(s) => s.span,
            Expr::Match(m) => m.span,
            Expr::Lambda(l) => l.span,
            Expr::ListLiteral(l) => l.span,
            Expr::MapLiteral(m) => m.span,
            Expr::StringInterpolation(s) => s.span,
            Expr::Try(t) => t.span,
        }
    }
}

/// A literal value expression.
#[derive(Debug, Clone, Serialize)]
pub struct Literal {
    /// The kind and value of the literal.
    pub kind: LiteralKind,
    /// Source span covering the literal token.
    pub span: Span,
}

/// The concrete kind and value of a literal expression.
#[derive(Debug, Clone, Serialize)]
pub enum LiteralKind {
    /// An integer literal, e.g. `42`.
    Int(i64),
    /// A floating-point literal, e.g. `3.14`.
    Float(f64),
    /// A string literal, e.g. `"hello"`.
    String(String),
    /// A boolean literal: `true` or `false`.
    Bool(bool),
}

/// An identifier used as an expression (a variable or parameter reference).
#[derive(Debug, Clone, Serialize)]
pub struct IdentExpr {
    /// The identifier name.
    pub name: String,
    /// Source span covering the identifier token.
    pub span: Span,
}

/// A binary operator.
///
/// Covers arithmetic, comparison, and logical operators available in Phoenix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BinaryOp {
    /// Addition (`+`).
    Add,
    /// Subtraction (`-`).
    Sub,
    /// Multiplication (`*`).
    Mul,
    /// Division (`/`).
    Div,
    /// Modulo (`%`).
    Mod,
    /// Equality comparison (`==`).
    Eq,
    /// Inequality comparison (`!=`).
    NotEq,
    /// Less-than comparison (`<`).
    Lt,
    /// Greater-than comparison (`>`).
    Gt,
    /// Less-than-or-equal comparison (`<=`).
    LtEq,
    /// Greater-than-or-equal comparison (`>=`).
    GtEq,
    /// Logical AND (`and`).
    And,
    /// Logical OR (`or`).
    Or,
}

impl std::fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryOp::Add => write!(f, "+"),
            BinaryOp::Sub => write!(f, "-"),
            BinaryOp::Mul => write!(f, "*"),
            BinaryOp::Div => write!(f, "/"),
            BinaryOp::Mod => write!(f, "%"),
            BinaryOp::Eq => write!(f, "=="),
            BinaryOp::NotEq => write!(f, "!="),
            BinaryOp::Lt => write!(f, "<"),
            BinaryOp::Gt => write!(f, ">"),
            BinaryOp::LtEq => write!(f, "<="),
            BinaryOp::GtEq => write!(f, ">="),
            BinaryOp::And => write!(f, "&&"),
            BinaryOp::Or => write!(f, "||"),
        }
    }
}

/// A binary expression: two operands joined by an operator.
///
/// ```text
/// a + b
/// x == y
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct BinaryExpr {
    /// The left-hand operand.
    pub left: Expr,
    /// The binary operator.
    pub op: BinaryOp,
    /// The right-hand operand.
    pub right: Expr,
    /// Source span covering the entire binary expression.
    pub span: Span,
}

/// A unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum UnaryOp {
    /// Arithmetic negation (`-`).
    Neg,
    /// Logical negation (`!`).
    Not,
}

/// A unary expression: an operator applied to a single operand.
///
/// ```text
/// -x
/// !flag
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct UnaryExpr {
    /// The unary operator.
    pub op: UnaryOp,
    /// The operand expression.
    pub operand: Expr,
    /// Source span covering the entire unary expression.
    pub span: Span,
}

/// A function call expression.
///
/// ```text
/// print(42)
/// add(a, b)
/// listen(port: 3000)
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct CallExpr {
    /// The expression being called (typically an identifier).
    pub callee: Expr,
    /// The positional argument expressions.
    pub args: Vec<Expr>,
    /// Named arguments: `(name, value)` pairs like `port: 3000`.
    pub named_args: Vec<(String, Expr)>,
    /// Source span covering the entire call expression.
    pub span: Span,
}

/// An assignment expression that sets a variable to a new value.
///
/// ```text
/// x = 42
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct AssignmentExpr {
    /// The name of the variable being assigned.
    pub name: String,
    /// The value expression.
    pub value: Expr,
    /// Source span covering the entire assignment.
    pub span: Span,
}

/// A field assignment expression that sets a struct field to a new value.
///
/// Supports direct and nested field assignment on mutable struct variables.
///
/// ```text
/// point.x = 10
/// user.address.city = "NYC"
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct FieldAssignmentExpr {
    /// The object expression whose field is being assigned.
    pub object: Expr,
    /// The name of the field being assigned.
    pub field: String,
    /// The value expression being assigned to the field.
    pub value: Expr,
    /// Source span covering the entire field assignment.
    pub span: Span,
}

/// A field access expression using dot notation.
///
/// ```text
/// point.x
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct FieldAccessExpr {
    /// The object expression whose field is being accessed.
    pub object: Expr,
    /// The name of the field.
    pub field: String,
    /// Source span covering the entire field access.
    pub span: Span,
}

/// A method call expression using dot notation.
///
/// ```text
/// point.distance(other)
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct MethodCallExpr {
    /// The object on which the method is called.
    pub object: Expr,
    /// The method name.
    pub method: String,
    /// The argument expressions (not including the receiver).
    pub args: Vec<Expr>,
    /// Source span covering the entire method call.
    pub span: Span,
}

/// A struct literal (positional constructor call).
///
/// ```text
/// Point(1, 2)
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct StructLiteralExpr {
    /// The struct type name.
    pub name: String,
    /// Explicit generic type arguments (e.g. `<Int, String>` in `Pair<Int, String>(1, "hi")`).
    pub type_args: Vec<TypeExpr>,
    /// The positional argument expressions for each field.
    pub args: Vec<Expr>,
    /// Source span covering the entire struct literal.
    pub span: Span,
}

/// A `match` expression that dispatches on a subject value.
///
/// ```text
/// match shape {
///     Circle(r) => 3.14 * r * r
///     Rectangle(w, h) => w * h
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct MatchExpr {
    /// The expression being matched on.
    pub subject: Expr,
    /// The match arms, evaluated in order.
    pub arms: Vec<MatchArm>,
    /// Source span covering the entire `match` expression.
    pub span: Span,
}

/// A single arm in a match expression, consisting of a pattern and a body.
#[derive(Debug, Clone, Serialize)]
pub struct MatchArm {
    /// The pattern to match against.
    pub pattern: Pattern,
    /// The body to evaluate if the pattern matches.
    pub body: MatchBody,
    /// Source span covering the entire match arm.
    pub span: Span,
}

/// The body of a match arm: either a single expression or a block.
#[derive(Debug, Clone, Serialize)]
pub enum MatchBody {
    /// A single expression body: `pattern => expr`.
    Expr(Expr),
    /// A block body: `pattern => { statements }`.
    Block(Block),
}

/// A pattern in a match arm.
#[derive(Debug, Clone, Serialize)]
pub enum Pattern {
    /// A wildcard `_` pattern that matches anything.
    Wildcard(Span),
    /// A literal pattern that matches a specific value: `42`, `"hello"`, `true`.
    Literal(Literal),
    /// A variant pattern: `VariantName(binding1, binding2)` or just `VariantName`.
    Variant(VariantPattern),
    /// A binding pattern that captures the matched value into a named variable.
    Binding(String, Span),
}

/// A variant pattern used to destructure enum variants in `match` arms.
///
/// ```text
/// Circle(r)
/// Rectangle(w, h)
/// None
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct VariantPattern {
    /// The variant name to match against.
    pub variant: String,
    /// Names for the bindings that capture the variant's fields. Empty for
    /// unit variants.
    pub bindings: Vec<String>,
    /// Source span covering the variant pattern.
    pub span: Span,
}

/// Information about a single captured variable in a closure.
///
/// Populated by the semantic checker during free-variable analysis so the
/// interpreter knows exactly which variables to capture by reference.
#[derive(Debug, Clone)]
pub struct CaptureInfo {
    /// The name of the captured variable.
    pub name: String,
    /// Whether the captured variable was declared `let mut`.
    pub is_mut: bool,
}

/// A lambda (anonymous function) expression.
///
/// Lambdas are first-class values that can be stored in variables,
/// passed as arguments, and returned from functions.  At runtime they
/// capture referenced variables from the enclosing environment by
/// reference to form closures.
///
/// ```text
/// function(x: Int, y: Int) -> Int { return x + y }
/// function() { print("side-effect") }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct LambdaExpr {
    /// The parameters of the lambda, each with a type annotation and name.
    pub params: Vec<Param>,
    /// The return type annotation, or `None` when the lambda returns `Void`.
    pub return_type: Option<TypeExpr>,
    /// The block of statements forming the lambda body.
    pub body: Block,
    /// Source span covering the entire lambda expression.
    pub span: Span,
}

/// A list literal expression such as `[1, 2, 3]` or `[]`.
///
/// ```text
/// [1, 2, 3]
/// []
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct ListLiteralExpr {
    /// The element expressions in the list literal.
    pub elements: Vec<Expr>,
    /// Source span covering the entire list literal (from `[` to `]`).
    pub span: Span,
}

/// A map literal expression such as `{"key": value}` or `{:}`.
#[derive(Debug, Clone, Serialize)]
pub struct MapLiteralExpr {
    /// The key-value pairs in the map literal.
    pub entries: Vec<(Expr, Expr)>,
    /// Source span covering the entire map literal.
    pub span: Span,
}

/// A string interpolation expression containing literal segments and
/// embedded expressions.
///
/// ```text
/// "hello {name}, you are {age} years old"
/// ```
///
/// The above produces segments:
/// `[Lit("hello "), Expr(name), Lit(", you are "), Expr(age), Lit(" years old")]`
#[derive(Debug, Clone, Serialize)]
pub struct StringInterpolationExpr {
    /// The segments of the interpolated string, alternating between literal
    /// text and embedded expressions.
    pub segments: Vec<StringSegment>,
    /// Source span covering the entire interpolated string literal.
    pub span: Span,
}

/// A segment in a string interpolation expression.
#[derive(Debug, Clone, Serialize)]
pub enum StringSegment {
    /// A literal text segment.
    Literal(String),
    /// An embedded expression segment (the part inside `{}`).
    Expr(Expr),
}

/// The `?` operator applied to a `Result<T, E>` or `Option<T>` expression.
///
/// ```text
/// Connection conn = db.connect()?
/// User user = conn.query("...")?
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct TryExpr {
    /// The expression being tried (must evaluate to `Result` or `Option`).
    pub operand: Expr,
    /// Source span covering the expression and the trailing `?`.
    pub span: Span,
}

/// A type expression used in annotations (parameter types, return types,
/// variable declarations).
#[derive(Debug, Clone, Serialize)]
pub enum TypeExpr {
    /// A named type reference such as `Int`, `String`, or `Point`.
    Named(NamedType),
    /// A function type: `(Int, Int) -> Bool`.
    Function(FunctionType),
    /// A generic type application: `Option<Int>`, `Pair<Int, String>`.
    Generic(GenericType),
}

/// A function type annotation: `(ParamType, ParamType) -> ReturnType`.
///
/// Function types are used both in variable declarations and function
/// parameter lists to describe first-class function values (closures).
///
/// ```text
/// (Int, Int) -> Bool
/// (String) -> Void
/// () -> Int
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct FunctionType {
    /// The types of each parameter. An empty vec represents a nullary function.
    pub param_types: Vec<TypeExpr>,
    /// The return type of the function.
    pub return_type: Box<TypeExpr>,
    /// Source span covering the entire function type.
    pub span: Span,
}

/// A generic type application such as `Option<Int>` or `Pair<Int, String>`.
///
/// ```text
/// Option<Int>
/// Pair<Int, String>
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct GenericType {
    /// The base type name (e.g. `"Option"`, `"Pair"`).
    pub name: String,
    /// The type arguments (e.g. `[Int]` or `[Int, String]`).
    pub type_args: Vec<TypeExpr>,
    /// Source span covering the entire generic type.
    pub span: Span,
}

/// A named type reference.
///
/// ```text
/// Int
/// String
/// Point
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct NamedType {
    /// The type name.
    pub name: String,
    /// Source span covering the type name token.
    pub span: Span,
}
