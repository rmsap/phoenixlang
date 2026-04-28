use phoenix_common::span::Span;
use serde::Serialize;

/// Classifies each token produced by the lexer.
///
/// Variants are grouped into **literals**, **keywords**, **type keywords**,
/// **operators**, **delimiters**, and **special** categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TokenKind {
    // ── Literals ──────────────────────────────────────────────
    /// An integer literal such as `0`, `42`, or `1000`.
    IntLiteral,
    /// A floating-point literal such as `3.14`.
    FloatLiteral,
    /// A double-quoted string literal, including its quotes (e.g. `"hello"`).
    StringLiteral,

    // ── Keywords ─────────────────────────────────────────────
    /// The boolean literal `true`.
    True,
    /// The boolean literal `false`.
    False,
    /// The `let` keyword, introducing a variable declaration.
    Let,
    /// The `mut` keyword, marking a binding as mutable.
    Mut,
    /// The `function` keyword, introducing a function definition.
    Function,
    /// The `return` keyword.
    Return,
    /// The `if` keyword.
    If,
    /// The `else` keyword.
    Else,
    /// The `while` keyword.
    While,
    /// The `for` keyword.
    For,
    /// The `in` keyword (used in for-in loops).
    In,
    /// The `struct` keyword.
    Struct,
    /// The `impl` keyword.
    Impl,
    /// The `enum` keyword.
    Enum,
    /// The `match` keyword.
    Match,
    /// The `self` keyword (method receiver).
    SelfKw,
    /// The `break` keyword (exits a loop).
    Break,
    /// The `continue` keyword (skips to next loop iteration).
    Continue,
    /// The `trait` keyword, introducing a trait declaration.
    Trait,
    /// The `dyn` keyword, marking a trait-object type (`dyn Trait`).
    ///
    /// Phoenix distinguishes static-dispatch trait bounds (`<T: Trait>`) from
    /// runtime trait-object dispatch (`dyn Trait`) syntactically so the
    /// performance cost of the latter is visible at use sites. See the
    /// "why explicit `dyn`" rationale in `docs/design-decisions.md`.
    Dyn,
    /// The `type` keyword, introducing a type alias declaration.
    Type,
    /// The `import` keyword, introducing a module import.
    ///
    /// Syntax: `import a.b.c { Item, Other as Alias, * }`. See Phase 2.6.
    Import,
    /// The `public` keyword, marking a declaration, struct field, or method as exported.
    ///
    /// Optional prefix on `function` / `struct` / `enum` / `trait` / `type`
    /// declarations, on struct fields, and on inline methods (in struct/enum
    /// bodies and inherent `impl` blocks). Default visibility is private.
    /// Methods inside `impl Trait for Type` blocks take their visibility from
    /// the trait and reject an explicit `public` modifier. See Phase 2.6.
    Public,
    /// The `as` keyword, used in import-item aliases.
    ///
    /// Appears as `import a.b { Foo as Bar }`.
    As,

    // ── Gen keywords ────────────────────────────────────────
    /// The `endpoint` keyword, introducing an endpoint declaration.
    ///
    /// Syntax: `endpoint name: METHOD "path" { ... }`.
    Endpoint,
    /// The `body` keyword, declaring the request body type inside an endpoint.
    ///
    /// Appears as `body TypeExpr [modifiers]` within an endpoint block.
    Body,
    /// The `response` keyword, declaring the response type inside an endpoint.
    ///
    /// Appears as `response TypeExpr` within an endpoint block.
    Response,
    /// The `error` keyword, introducing an error-variant block inside an endpoint.
    ///
    /// Named `ErrorKw` to avoid collision with the [`Error`](TokenKind::Error) variant
    /// used for malformed tokens. Appears as `error { Variant(code), ... }`.
    ErrorKw,
    /// The `omit` keyword, a type derivation operator that excludes listed fields.
    ///
    /// Used in endpoint body declarations: `User omit { id, created_at }`.
    Omit,
    /// The `pick` keyword, a type derivation operator that includes only listed fields.
    ///
    /// Used in endpoint body declarations: `User pick { name, email }`.
    Pick,
    /// The `partial` keyword, a type derivation operator that makes fields optional.
    ///
    /// Can apply to all fields (`partial`) or a subset (`partial { name, email }`).
    Partial,
    /// The `query` keyword, introducing a query-parameter block inside an endpoint.
    ///
    /// Appears as `query { Type name [= default], ... }`.
    Query,
    /// The `where` keyword, introducing a constraint clause on a struct field.
    ///
    /// Appears as `Type name where <expr>` inside a struct body. The constraint
    /// expression must evaluate to `Bool` and uses `self` to refer to the field value.
    Where,
    /// The `schema` keyword, introducing a database schema declaration.
    ///
    /// Appears as `schema name { table ... }`. Parsed for forward compatibility
    /// with Phase 4 (typed database queries and migrations).
    Schema,

    // ── HTTP methods ────────────────────────────────────────
    /// The `GET` HTTP method keyword (case-sensitive).
    Get,
    /// The `POST` HTTP method keyword (case-sensitive).
    Post,
    /// The `PUT` HTTP method keyword (case-sensitive).
    Put,
    /// The `PATCH` HTTP method keyword (case-sensitive).
    Patch,
    /// The `DELETE` HTTP method keyword (case-sensitive).
    Delete,

    // ── Type keywords ────────────────────────────────────────
    /// The `Int` type name.
    IntType,
    /// The `Float` type name.
    FloatType,
    /// The `String` type name.
    StringType,
    /// The `Bool` type name.
    BoolType,
    /// The `Void` type name (used as a return type).
    Void,

    // ── Operators ────────────────────────────────────────────
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `=` (assignment)
    Eq,
    /// `==` (equality comparison)
    EqEq,
    /// `!=`
    NotEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    LtEq,
    /// `>=`
    GtEq,
    /// `&&` (logical and)
    And,
    /// `||` (logical or)
    Or,
    /// `!` (logical not)
    Not,
    /// `->` (return-type arrow)
    Arrow,
    /// `..` (range operator, used in for loops)
    DotDot,
    /// `?` (error propagation / try operator)
    Question,
    /// `|>` (pipe operator)
    Pipe,
    /// `+=` (compound addition assignment)
    PlusEq,
    /// `-=` (compound subtraction assignment)
    MinusEq,
    /// `*=` (compound multiplication assignment)
    StarEq,
    /// `/=` (compound division assignment)
    SlashEq,
    /// `%=` (compound modulo assignment)
    PercentEq,

    // ── Delimiters ───────────────────────────────────────────
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `.`
    Dot,
    /// `[`
    LBracket,
    /// `]`
    RBracket,

    // ── Special ──────────────────────────────────────────────
    /// A doc comment `/** ... */`. The token text contains the inner content
    /// (stripped of the `/**` and `*/` delimiters).
    DocComment,
    /// A user-defined identifier (variable name, function name, etc.).
    Ident,
    /// A significant newline that acts as a statement terminator.
    Newline,
    /// End-of-file sentinel; always the last token in a `tokenize` result.
    Eof,
    /// Produced for characters the lexer does not recognise, or for
    /// malformed tokens such as unterminated strings.
    Error,
}

/// A single token produced by the Phoenix lexer.
///
/// Every token carries its [`TokenKind`], the original source text it was
/// lexed from, and a [`Span`] that records its byte-offset range in the
/// source file.
#[derive(Debug, Clone, Serialize)]
pub struct Token {
    /// The syntactic category of this token.
    pub kind: TokenKind,
    /// The exact source text that was consumed to form this token.
    pub text: String,
    /// The byte-offset range in the source file that this token covers.
    pub span: Span,
}

impl Token {
    /// Creates a new `Token`.
    ///
    /// `text` accepts anything that can be converted into a `String` (e.g.
    /// `&str` or `String`), making call sites concise.
    pub fn new(kind: TokenKind, text: impl Into<String>, span: Span) -> Self {
        Self {
            kind,
            text: text.into(),
            span,
        }
    }
}
