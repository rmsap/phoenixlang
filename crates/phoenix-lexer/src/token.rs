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
    /// The `type` keyword, introducing a type alias declaration.
    Type,

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
    /// The `and` keyword operator.
    And,
    /// The `or` keyword operator.
    Or,
    /// The `not` keyword operator, or `!` when followed by `=`.
    Not,
    /// `->` (return-type arrow)
    Arrow,
    /// `..` (range operator, used in for loops)
    DotDot,
    /// `?` (error propagation / try operator)
    Question,
    /// `|>` (pipe operator)
    Pipe,

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
