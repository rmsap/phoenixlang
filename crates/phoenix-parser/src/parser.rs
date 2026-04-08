use crate::ast::*;
use phoenix_common::diagnostics::Diagnostic;
use phoenix_lexer::token::{Token, TokenKind};

/// Returns a human-readable name for a [`TokenKind`], suitable for use in
/// user-facing error messages.
fn token_kind_display(kind: &TokenKind) -> &'static str {
    match kind {
        TokenKind::IntLiteral => "integer literal",
        TokenKind::FloatLiteral => "float literal",
        TokenKind::StringLiteral => "string literal",
        TokenKind::True => "'true'",
        TokenKind::False => "'false'",
        TokenKind::Ident => "identifier",
        TokenKind::Let => "'let'",
        TokenKind::Mut => "'mut'",
        TokenKind::Function => "'function'",
        TokenKind::Return => "'return'",
        TokenKind::If => "'if'",
        TokenKind::Else => "'else'",
        TokenKind::While => "'while'",
        TokenKind::For => "'for'",
        TokenKind::In => "'in'",
        TokenKind::Struct => "'struct'",
        TokenKind::Impl => "'impl'",
        TokenKind::Enum => "'enum'",
        TokenKind::Match => "'match'",
        TokenKind::SelfKw => "'self'",
        TokenKind::Break => "'break'",
        TokenKind::Continue => "'continue'",
        TokenKind::Trait => "'trait'",
        TokenKind::Type => "'type'",
        TokenKind::IntType => "Int",
        TokenKind::FloatType => "Float",
        TokenKind::StringType => "String",
        TokenKind::BoolType => "Bool",
        TokenKind::Void => "Void",
        TokenKind::Plus => "'+'",
        TokenKind::Minus => "'-'",
        TokenKind::Star => "'*'",
        TokenKind::Slash => "'/'",
        TokenKind::Percent => "'%'",
        TokenKind::Eq => "'='",
        TokenKind::EqEq => "'=='",
        TokenKind::NotEq => "'!='",
        TokenKind::Lt => "'<'",
        TokenKind::Gt => "'>'",
        TokenKind::LtEq => "'<='",
        TokenKind::GtEq => "'>='",
        TokenKind::And => "'and'",
        TokenKind::Or => "'or'",
        TokenKind::Not => "'not'",
        TokenKind::Arrow => "'->'",
        TokenKind::LParen => "'('",
        TokenKind::RParen => "')'",
        TokenKind::LBrace => "'{'",
        TokenKind::RBrace => "'}'",
        TokenKind::LBracket => "'['",
        TokenKind::RBracket => "']'",
        TokenKind::Comma => "','",
        TokenKind::Colon => "':'",
        TokenKind::Dot => "'.'",
        TokenKind::DotDot => "'..'",
        TokenKind::Question => "'?'",
        TokenKind::Pipe => "'|>'",
        TokenKind::Newline => "newline",
        TokenKind::Eof => "end of file",
        TokenKind::Error => "error token",
    }
}

/// A recursive-descent parser for Phoenix source code.
///
/// The parser consumes a slice of [`Token`]s produced by the lexer and
/// builds an [`Program`] AST.  Parse errors are collected in the
/// [`diagnostics`](Parser::diagnostics) vector so that the parser can
/// attempt error recovery and report multiple issues in a single pass.
pub struct Parser<'src> {
    tokens: &'src [Token],
    pub(crate) pos: usize,
    pub diagnostics: Vec<Diagnostic>,
}

impl<'src> Parser<'src> {
    /// Creates a new parser over the given token slice.
    ///
    /// The token slice must end with a [`TokenKind::Eof`] token (the lexer
    /// guarantees this).
    pub fn new(tokens: &'src [Token]) -> Self {
        Self {
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
        }
    }

    /// Parses the full token stream into a [`Program`] AST node.
    ///
    /// Top-level declarations are parsed in order.  When a parse error is
    /// encountered the parser attempts to synchronize by skipping tokens
    /// until the next `function` keyword or EOF, so that subsequent
    /// declarations can still be parsed.
    pub fn parse_program(&mut self) -> Program {
        let start_span = self.peek().span;
        let mut declarations = Vec::new();

        self.skip_newlines();

        while self.peek().kind != TokenKind::Eof {
            if let Some(decl) = self.parse_declaration() {
                declarations.push(decl);
            } else {
                // Error recovery: skip to the next function or EOF
                self.synchronize();
            }
            self.skip_newlines();
        }

        let end_span = self.peek().span;
        Program {
            declarations,
            span: start_span.merge(end_span),
        }
    }

    /// Parses a top-level declaration (function, struct, enum, impl, trait, or type alias).
    fn parse_declaration(&mut self) -> Option<Declaration> {
        match self.peek().kind {
            TokenKind::Function => self.parse_function_decl().map(Declaration::Function),
            TokenKind::Struct => self.parse_struct_decl().map(Declaration::Struct),
            TokenKind::Enum => self.parse_enum_decl().map(Declaration::Enum),
            TokenKind::Impl => self.parse_impl_block().map(Declaration::Impl),
            TokenKind::Trait => self.parse_trait_decl().map(Declaration::Trait),
            TokenKind::Type => self.parse_type_alias_decl().map(Declaration::TypeAlias),
            _ => {
                self.error_at_current("expected a declaration (e.g. `function`, `struct`, `enum`, `impl`, `trait`, `type`)");
                None
            }
        }
    }

    /// Parses a function declaration including optional type parameters, params, return type, and body.
    fn parse_function_decl(&mut self) -> Option<FunctionDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Function)?;

        let name_token = self.expect(TokenKind::Ident)?;
        let name = name_token.text.clone();

        let (type_params, type_param_bounds) = self.parse_type_params_with_bounds();

        self.expect(TokenKind::LParen)?;
        let params = self.parse_params();
        self.expect(TokenKind::RParen)?;

        let return_type = if self.eat(TokenKind::Arrow) {
            self.parse_type_expr()
        } else {
            None
        };

        let body = self.parse_block()?;
        let span = start.merge(body.span);

        Some(FunctionDecl {
            name,
            type_params,
            type_param_bounds,
            params,
            return_type,
            body,
            span,
        })
    }

    /// Parses optional type parameters: `<T, U>`.
    /// Returns empty vec if no `<` is present.
    ///
    /// A missing closing `>` emits a diagnostic but does not abort; the
    /// partially-parsed parameter list is still returned so the caller can
    /// continue error recovery.
    pub(crate) fn parse_type_params(&mut self) -> Vec<String> {
        if self.peek().kind != TokenKind::Lt {
            return vec![];
        }
        self.advance(); // consume '<'
        let params = self.parse_comma_separated(TokenKind::Gt, |p| {
            p.expect(TokenKind::Ident).map(|tok| tok.text.clone())
        });
        self.expect(TokenKind::Gt); // diagnostic on mismatch; recovery continues
        params
    }

    /// Parses a comma-separated list of function parameters (including `self`).
    ///
    /// Supports optional default values: `greeting: String = "Hello"`.
    /// Parameters with defaults must appear after all non-default parameters.
    pub(crate) fn parse_params(&mut self) -> Vec<Param> {
        let mut params = Vec::new();
        let mut seen_default = false;

        if self.peek().kind == TokenKind::RParen {
            return params;
        }

        loop {
            let start = self.peek().span;
            // Handle `self` as a special parameter (no type annotation)
            if self.peek().kind == TokenKind::SelfKw {
                let tok = self.advance();
                params.push(Param {
                    type_annotation: TypeExpr::Named(NamedType {
                        name: "Self".to_string(),
                        span: tok.span,
                    }),
                    name: "self".to_string(),
                    default_value: None,
                    span: tok.span,
                });
            } else if let Some(name_token) = self.expect(TokenKind::Ident)
                && self.expect(TokenKind::Colon).is_some()
                && let Some(type_expr) = self.parse_type_expr()
            {
                // Check for default value: `= expr`
                let default_value = if self.eat(TokenKind::Eq) {
                    seen_default = true;
                    self.parse_expr()
                } else {
                    if seen_default {
                        self.error_at_current(
                            "non-default parameter cannot follow a default parameter",
                        );
                    }
                    None
                };
                let end_span = if let Some(ref dv) = default_value {
                    dv.span()
                } else {
                    self.tokens
                        .get(self.pos.saturating_sub(1))
                        .map(|t| t.span)
                        .unwrap_or(name_token.span)
                };
                let span = start.merge(end_span);
                params.push(Param {
                    type_annotation: type_expr,
                    name: name_token.text.clone(),
                    default_value,
                    span,
                });
            }

            if !self.eat(TokenKind::Comma) {
                break;
            }
        }

        params
    }

    /// Parses a brace-delimited block of statements (`{ stmt; ... }`).
    pub(crate) fn parse_block(&mut self) -> Option<Block> {
        let start = self.expect(TokenKind::LBrace)?.span;
        self.skip_newlines();

        let mut statements = Vec::new();

        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            if let Some(stmt) = self.parse_statement() {
                statements.push(stmt);
            } else {
                self.synchronize_stmt();
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(Block {
            statements,
            span: start.merge(end),
        })
    }

    // ── Generic helpers ──────────────────────────────────────────────

    /// Parses a comma-separated list of items until `end` is reached.
    ///
    /// Calls `parse_one` repeatedly, separating items with commas.
    /// Stops when `parse_one` returns `None` or a comma is not found.
    /// Does NOT consume the `end` token.
    pub(crate) fn parse_comma_separated<T>(
        &mut self,
        end: TokenKind,
        mut parse_one: impl FnMut(&mut Self) -> Option<T>,
    ) -> Vec<T> {
        let mut items = Vec::new();
        if self.peek().kind == end {
            return items;
        }
        loop {
            if let Some(item) = parse_one(self) {
                items.push(item);
            } else {
                break;
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        items
    }

    /// Parses a braced block of method declarations, consuming `{` through `}`.
    ///
    /// Used by `parse_impl_block`, `parse_inline_trait_impl`, and similar
    /// constructs that contain only method definitions.
    fn parse_methods_block(&mut self) -> Option<Vec<FunctionDecl>> {
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let mut methods = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            if let Some(func) = self.parse_function_decl() {
                methods.push(func);
            } else {
                self.synchronize_stmt();
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace)?;
        Some(methods)
    }

    // ── Token helpers ───────────────────────────────────────────────

    /// Returns a reference to the current token without advancing.
    /// Returns the `Eof` token if the position is past the end.
    pub(crate) fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .unwrap_or_else(|| self.tokens.last().expect("token stream must end with Eof"))
    }

    /// Returns a reference to the token `offset` positions ahead of the current
    /// position without consuming any tokens. Returns the `Eof` token if the
    /// offset is past the end of the token stream.
    pub(crate) fn peek_at(&self, offset: usize) -> &Token {
        self.tokens
            .get(self.pos + offset)
            .unwrap_or_else(|| self.tokens.last().expect("token stream must end with Eof"))
    }

    /// Consumes the current token and returns a clone of it.
    /// Does not advance past `Eof`.
    pub(crate) fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        if token.kind != TokenKind::Eof {
            self.pos += 1;
        }
        token
    }

    /// Consumes the current token if it matches `kind`, returning it.
    /// Records a diagnostic and returns `None` on mismatch.
    pub(crate) fn expect(&mut self, kind: TokenKind) -> Option<Token> {
        if self.peek().kind == kind {
            Some(self.advance())
        } else {
            self.error_at_current(&format!("expected {}", token_kind_display(&kind)));
            None
        }
    }

    /// Consumes the current token if it matches `kind`, returning `true`.
    /// Does nothing and returns `false` on mismatch.
    pub(crate) fn eat(&mut self, kind: TokenKind) -> bool {
        if self.peek().kind == kind {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consumes consecutive newline tokens. Used between statements in blocks.
    pub(crate) fn skip_newlines(&mut self) {
        while self.peek().kind == TokenKind::Newline {
            self.advance();
        }
    }

    // ── Error handling ──────────────────────────────────────────────

    pub(crate) fn error_at_current(&mut self, message: &str) {
        let span = self.peek().span;
        self.diagnostics.push(Diagnostic::error(message, span));
    }

    /// Parses a struct declaration: `struct Name { fields, methods, impl blocks }`.
    pub(crate) fn parse_struct_decl(&mut self) -> Option<StructDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Struct)?;
        let name_token = self.expect(TokenKind::Ident)?;
        let type_params = self.parse_type_params();
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut trait_impls = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            if self.peek().kind == TokenKind::Function {
                // Inline method
                if let Some(func) = self.parse_function_decl() {
                    methods.push(func);
                } else {
                    self.synchronize_stmt();
                }
            } else if self.peek().kind == TokenKind::Impl {
                // Inline trait impl
                if let Some(ti) = self.parse_inline_trait_impl() {
                    trait_impls.push(ti);
                } else {
                    self.synchronize_stmt();
                }
            } else {
                // Field
                let fstart = self.peek().span;
                if let Some(type_expr) = self.parse_type_expr()
                    && let Some(name_tok) = self.expect(TokenKind::Ident)
                {
                    let span = fstart.merge(name_tok.span);
                    fields.push(FieldDecl {
                        type_annotation: type_expr,
                        name: name_tok.text.clone(),
                        span,
                    });
                }
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(StructDecl {
            name: name_token.text.clone(),
            type_params,
            fields,
            methods,
            trait_impls,
            span: start.merge(end),
        })
    }

    /// Parses an enum declaration: `enum Name { Variant, Variant(Type), methods, impl blocks }`.
    pub(crate) fn parse_enum_decl(&mut self) -> Option<EnumDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Enum)?;
        let name_token = self.expect(TokenKind::Ident)?;
        let type_params = self.parse_type_params();
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut variants = Vec::new();
        let mut methods = Vec::new();
        let mut trait_impls = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            if self.peek().kind == TokenKind::Function {
                // Inline method
                if let Some(func) = self.parse_function_decl() {
                    methods.push(func);
                } else {
                    self.synchronize_stmt();
                }
            } else if self.peek().kind == TokenKind::Impl {
                // Inline trait impl
                if let Some(ti) = self.parse_inline_trait_impl() {
                    trait_impls.push(ti);
                } else {
                    self.synchronize_stmt();
                }
            } else {
                // Variant
                let vstart = self.peek().span;
                if let Some(vname) = self.expect(TokenKind::Ident) {
                    let mut fields = Vec::new();
                    let mut rparen_span = None;
                    if self.eat(TokenKind::LParen) {
                        fields =
                            self.parse_comma_separated(TokenKind::RParen, |p| p.parse_type_expr());
                        rparen_span = Some(self.expect(TokenKind::RParen)?.span);
                    }
                    let vend = rparen_span.unwrap_or(vname.span);
                    variants.push(EnumVariant {
                        name: vname.text.clone(),
                        fields,
                        span: vstart.merge(vend),
                    });
                }
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(EnumDecl {
            name: name_token.text.clone(),
            type_params,
            variants,
            methods,
            trait_impls,
            span: start.merge(end),
        })
    }

    /// Parses an impl block: `impl Type { methods }` or `impl Trait for Type { methods }`.
    pub(crate) fn parse_impl_block(&mut self) -> Option<ImplBlock> {
        let start = self.peek().span;
        self.expect(TokenKind::Impl)?;
        let first_ident = self.expect(TokenKind::Ident)?;

        // Check for `impl Trait for Type` syntax
        let (trait_name, type_name) = if self.peek().kind == TokenKind::For {
            self.advance(); // consume `for`
            let type_ident = self.expect(TokenKind::Ident)?;
            (Some(first_ident.text.clone()), type_ident.text.clone())
        } else {
            (None, first_ident.text.clone())
        };

        let methods = self.parse_methods_block()?;
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(start);
        Some(ImplBlock {
            type_name,
            trait_name,
            methods,
            span: start.merge(end),
        })
    }

    /// Parses an inline trait impl inside a struct/enum body: `impl TraitName { methods }`.
    fn parse_inline_trait_impl(&mut self) -> Option<InlineTraitImpl> {
        let start = self.peek().span;
        self.expect(TokenKind::Impl)?;
        let trait_name_token = self.expect(TokenKind::Ident)?;
        let methods = self.parse_methods_block()?;
        let end = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|t| t.span)
            .unwrap_or(start);
        Some(InlineTraitImpl {
            trait_name: trait_name_token.text.clone(),
            methods,
            span: start.merge(end),
        })
    }

    /// Parses optional type parameters with optional trait bounds: `<T, U: Display>`.
    /// Returns (names, bounds) where bounds is a list of (param_name, bound_names).
    pub(crate) fn parse_type_params_with_bounds(
        &mut self,
    ) -> (Vec<String>, Vec<(String, Vec<String>)>) {
        if self.peek().kind != TokenKind::Lt {
            return (vec![], vec![]);
        }
        self.advance(); // consume '<'
        let mut params = Vec::new();
        let mut bounds = Vec::new();
        loop {
            if let Some(tok) = self.expect(TokenKind::Ident) {
                let param_name = tok.text.clone();
                if self.eat(TokenKind::Colon) {
                    // Parse bound(s) — for MVP just one bound ident
                    let mut param_bounds = Vec::new();
                    if let Some(bound_tok) = self.expect(TokenKind::Ident) {
                        param_bounds.push(bound_tok.text.clone());
                    }
                    bounds.push((param_name.clone(), param_bounds));
                }
                params.push(param_name);
            }
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::Gt);
        (params, bounds)
    }

    /// Parses a trait declaration: `trait Name { method_signatures }`.
    pub(crate) fn parse_trait_decl(&mut self) -> Option<TraitDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Trait)?;
        let name_token = self.expect(TokenKind::Ident)?;
        let type_params = self.parse_type_params();
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut methods = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            if let Some(sig) = self.parse_trait_method_sig() {
                methods.push(sig);
            } else {
                self.synchronize_stmt();
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(TraitDecl {
            name: name_token.text.clone(),
            type_params,
            methods,
            span: start.merge(end),
        })
    }

    /// Parses a method signature inside a trait (no body).
    /// `function name(param: Type) -> ReturnType`
    fn parse_trait_method_sig(&mut self) -> Option<TraitMethodSig> {
        let start = self.peek().span;
        self.expect(TokenKind::Function)?;
        let name_token = self.expect(TokenKind::Ident)?;
        self.expect(TokenKind::LParen)?;
        let params = self.parse_params();
        let rparen = self.expect(TokenKind::RParen)?;

        let return_type = if self.eat(TokenKind::Arrow) {
            self.parse_type_expr()
        } else {
            None
        };

        // Use the RParen span as fallback, or the last token consumed by parse_type_expr
        let end = if return_type.is_some() {
            self.tokens
                .get(self.pos.saturating_sub(1))
                .map(|t| t.span)
                .unwrap_or(rparen.span)
        } else {
            rparen.span
        };
        // Eat trailing newline
        self.eat(TokenKind::Newline);

        Some(TraitMethodSig {
            name: name_token.text.clone(),
            params,
            return_type,
            span: start.merge(end),
        })
    }

    /// Parses a type alias declaration: `type Name = TypeExpr` or
    /// `type Name<T> = TypeExpr`.
    pub(crate) fn parse_type_alias_decl(&mut self) -> Option<TypeAliasDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Type)?;
        let name_token = self.expect(TokenKind::Ident)?;
        let type_params = self.parse_type_params();
        self.expect(TokenKind::Eq)?;
        let target = self.parse_type_expr()?;
        let end = match &target {
            TypeExpr::Named(n) => n.span,
            TypeExpr::Function(f) => f.span,
            TypeExpr::Generic(g) => g.span,
        };
        self.eat(TokenKind::Newline);
        Some(TypeAliasDecl {
            name: name_token.text.clone(),
            type_params,
            target,
            span: start.merge(end),
        })
    }

    /// Top-level error recovery: skips tokens until a declaration-starting
    /// keyword (`function`, `struct`, `enum`, `impl`, `trait`, `type`) or EOF.
    /// Used when `parse_declaration` fails at the program level.
    fn synchronize(&mut self) {
        loop {
            match self.peek().kind {
                TokenKind::Eof
                | TokenKind::Function
                | TokenKind::Struct
                | TokenKind::Enum
                | TokenKind::Impl
                | TokenKind::Trait
                | TokenKind::Type => break,
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// Statement-level error recovery: skips tokens until a statement boundary
    /// (`}`, `function`, newline) or EOF.  Used when `parse_statement` fails
    /// inside a block body, so we stop at `RBrace` and `Newline` to avoid
    /// consuming the rest of the enclosing block.
    fn synchronize_stmt(&mut self) {
        loop {
            match self.peek().kind {
                TokenKind::Eof | TokenKind::RBrace | TokenKind::Function | TokenKind::Newline => {
                    break;
                }
                _ => {
                    self.advance();
                }
            }
        }
    }
}

/// Convenience function that creates a [`Parser`], parses the token stream
/// into a [`Program`], and returns the AST together with any diagnostics
/// that were collected during parsing.
///
/// # Examples
///
/// Parse a simple function declaration:
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_common::span::SourceId;
/// use phoenix_parser::parser::parse;
/// use phoenix_parser::ast::Declaration;
///
/// let tokens = tokenize("function add(a: Int, b: Int) -> Int { return a + b }", SourceId(0));
/// let (program, diagnostics) = parse(&tokens);
/// assert!(diagnostics.is_empty());
/// assert_eq!(program.declarations.len(), 1);
/// match &program.declarations[0] {
///     Declaration::Function(f) => {
///         assert_eq!(f.name, "add");
///         assert_eq!(f.params.len(), 2);
///     }
///     _ => panic!("expected Function"),
/// }
/// ```
///
/// Invalid source produces diagnostics without panicking:
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_common::span::SourceId;
/// use phoenix_parser::parser::parse;
///
/// let tokens = tokenize("function { }", SourceId(0));
/// let (_program, diagnostics) = parse(&tokens);
/// assert!(!diagnostics.is_empty());
/// ```
#[must_use]
pub fn parse(tokens: &[Token]) -> (Program, Vec<Diagnostic>) {
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program();
    (program, parser.diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_common::span::SourceId;
    use phoenix_lexer::lexer::tokenize;

    fn parse_source(source: &str) -> (Program, Vec<Diagnostic>) {
        let tokens = tokenize(source, SourceId(0));
        parse(&tokens)
    }

    #[test]
    fn parse_empty_function() {
        let (program, diagnostics) = parse_source("function main() { }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.name, "main");
                assert!(f.params.is_empty());
                assert!(f.return_type.is_none());
                assert!(f.body.statements.is_empty());
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_function_with_return_type() {
        let (program, diagnostics) = parse_source("function add(a: Int, b: Int) -> Int { return a }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.name, "add");
                assert_eq!(f.params.len(), 2);
                assert!(f.return_type.is_some());
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_var_decl() {
        let (program, diagnostics) = parse_source("function main() { let x: Int = 42 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.body.statements.len(), 1);
                match &f.body.statements[0] {
                    Statement::VarDecl(v) => {
                        assert!(!v.is_mut);
                        assert_eq!(v.simple_name().unwrap(), "x");
                    }
                    other => panic!("expected VarDecl, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_mut_var_decl() {
        let (program, diagnostics) = parse_source("function main() { let mut x: Int = 42 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => {
                    assert!(v.is_mut);
                    assert_eq!(v.simple_name().unwrap(), "x");
                }
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_if_else() {
        let source = "function main() {\n  if x == 1 {\n    return true\n  } else {\n    return false\n  }\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.body.statements.len(), 1);
                match &f.body.statements[0] {
                    Statement::If(if_stmt) => {
                        assert!(if_stmt.else_branch.is_some());
                    }
                    other => panic!("expected If, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_multiple_functions() {
        let source = "function foo() { }\nfunction bar() { }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 2);
    }

    #[test]
    fn parse_expression_precedence() {
        let source = "function main() { let x: Int = 1 + 2 * 3 }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                match &f.body.statements[0] {
                    Statement::VarDecl(v) => {
                        // Should be Add(1, Mul(2, 3))
                        match &v.initializer {
                            Expr::Binary(b) => {
                                assert_eq!(b.op, BinaryOp::Add);
                                match &b.right {
                                    Expr::Binary(inner) => {
                                        assert_eq!(inner.op, BinaryOp::Mul);
                                    }
                                    other => panic!("expected Binary for rhs, got {:?}", other),
                                }
                            }
                            other => panic!("expected Binary, got {:?}", other),
                        }
                    }
                    other => panic!("expected VarDecl, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_function_call() {
        let source = "function main() { print(42) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::Expression(e) => match &e.expr {
                    Expr::Call(c) => {
                        assert_eq!(c.args.len(), 1);
                    }
                    other => panic!("expected Call, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_bare_return() {
        let (program, diagnostics) = parse_source("function foo() { return }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.body.statements.len(), 1);
                match &f.body.statements[0] {
                    Statement::Return(r) => {
                        assert!(r.value.is_none(), "bare return should have no value");
                    }
                    other => panic!("expected Return, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_return_with_expr() {
        let (program, diagnostics) = parse_source("function foo() -> Int { return 1 + 2 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::Return(r) => {
                    let value = r.value.as_ref().expect("return should have a value");
                    match value {
                        Expr::Binary(b) => assert_eq!(b.op, BinaryOp::Add),
                        other => panic!("expected Binary Add, got {:?}", other),
                    }
                }
                other => panic!("expected Return, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_if_without_else() {
        let source = "function main() { if x == 1 { print(x) } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::If(if_stmt) => {
                    assert!(if_stmt.else_branch.is_none(), "should have no else block");
                    assert_eq!(if_stmt.then_block.statements.len(), 1);
                }
                other => panic!("expected If, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_nested_if() {
        let source =
            "function main() {\n  if x == 1 {\n    if y == 2 {\n      print(y)\n    }\n  }\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::If(outer) => {
                    assert_eq!(outer.then_block.statements.len(), 1);
                    match &outer.then_block.statements[0] {
                        Statement::If(inner) => {
                            assert_eq!(inner.then_block.statements.len(), 1);
                        }
                        other => panic!("expected inner If, got {:?}", other),
                    }
                }
                other => panic!("expected If, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_unary_neg() {
        let (program, diagnostics) = parse_source("function main() { let x: Int = -42 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Unary(u) => {
                        assert_eq!(u.op, UnaryOp::Neg);
                        match &u.operand {
                            Expr::Literal(Literal {
                                kind: LiteralKind::Int(42),
                                ..
                            }) => {}
                            other => panic!("expected Literal(42), got {:?}", other),
                        }
                    }
                    other => panic!("expected Unary, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_unary_not() {
        let (program, diagnostics) = parse_source("function main() { let b: Bool = not true }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Unary(u) => {
                        assert_eq!(u.op, UnaryOp::Not);
                        match &u.operand {
                            Expr::Literal(Literal {
                                kind: LiteralKind::Bool(true),
                                ..
                            }) => {}
                            other => panic!("expected Literal(true), got {:?}", other),
                        }
                    }
                    other => panic!("expected Unary, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_grouped_expr() {
        // (1 + 2) * 3 should parse as Mul(Add(1, 2), 3)
        let (program, diagnostics) = parse_source("function main() { let x: Int = (1 + 2) * 3 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Binary(b) => {
                        assert_eq!(b.op, BinaryOp::Mul, "top-level op should be Mul");
                        match &b.left {
                            Expr::Binary(inner) => {
                                assert_eq!(inner.op, BinaryOp::Add, "grouped left should be Add");
                            }
                            other => panic!("expected Binary Add inside group, got {:?}", other),
                        }
                    }
                    other => panic!("expected Binary Mul, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_string_literal() {
        let (program, diagnostics) = parse_source(r#"function main() { let s: String = "hello" }"#);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Literal(Literal {
                        kind: LiteralKind::String(s),
                        ..
                    }) => {
                        assert_eq!(s, "hello");
                    }
                    other => panic!("expected String literal, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_bool_literal() {
        let (prog_t, diag_t) = parse_source("function main() { let b: Bool = true }");
        assert!(diag_t.is_empty(), "errors: {:?}", diag_t);
        match &prog_t.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Literal(Literal {
                        kind: LiteralKind::Bool(true),
                        ..
                    }) => {}
                    other => panic!("expected Bool(true), got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }

        let (prog_f, diag_f) = parse_source("function main() { let b: Bool = false }");
        assert!(diag_f.is_empty(), "errors: {:?}", diag_f);
        match &prog_f.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Literal(Literal {
                        kind: LiteralKind::Bool(false),
                        ..
                    }) => {}
                    other => panic!("expected Bool(false), got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_multiple_params() {
        let (program, diagnostics) = parse_source("function foo(a: Int, b: Float, c: String) { }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.params.len(), 3);
                assert_eq!(f.params[0].name, "a");
                assert_eq!(f.params[1].name, "b");
                assert_eq!(f.params[2].name, "c");
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_no_params() {
        let (program, diagnostics) = parse_source("function foo() { }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert!(f.params.is_empty());
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_chained_comparison() {
        // 1 + 2 > 3 should parse as Gt(Add(1, 2), 3)
        let (program, diagnostics) = parse_source("function main() { let b: Bool = 1 + 2 > 3 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Binary(b) => {
                        assert_eq!(b.op, BinaryOp::Gt, "top-level op should be Gt");
                        match &b.left {
                            Expr::Binary(inner) => {
                                assert_eq!(inner.op, BinaryOp::Add, "left of Gt should be Add");
                            }
                            other => panic!("expected Binary Add in left, got {:?}", other),
                        }
                    }
                    other => panic!("expected Binary Gt, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_error_recovery() {
        // First function has invalid syntax; second should still parse.
        let source = "function bad( { }\nfunction good() { }";
        let (program, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "should have at least one diagnostic"
        );
        // The valid function should still be recovered
        let good_fns: Vec<_> = program
            .declarations
            .iter()
            .filter_map(|d| match d {
                Declaration::Function(f) if f.name == "good" => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(
            good_fns.len(),
            1,
            "good() should be parsed despite earlier error"
        );
    }

    #[test]
    fn parse_assignment() {
        let (program, diagnostics) = parse_source("function main() { x = 42 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::Expression(e) => match &e.expr {
                    Expr::Assignment(a) => {
                        assert_eq!(a.name, "x");
                        match &a.value {
                            Expr::Literal(Literal {
                                kind: LiteralKind::Int(42),
                                ..
                            }) => {}
                            other => panic!("expected Literal(42), got {:?}", other),
                        }
                    }
                    other => panic!("expected Assignment, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_while_loop() {
        let source = "function main() { while x < 10 { print(x) } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.body.statements.len(), 1);
                match &f.body.statements[0] {
                    Statement::While(w) => {
                        // condition should be a Binary Lt
                        match &w.condition {
                            Expr::Binary(b) => {
                                assert_eq!(b.op, BinaryOp::Lt);
                            }
                            other => panic!("expected Binary Lt condition, got {:?}", other),
                        }
                        // body should contain one statement (the print call)
                        assert_eq!(w.body.statements.len(), 1);
                    }
                    other => panic!("expected While, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_for_loop() {
        let source = "function main() { for i in 0..10 { print(i) } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.body.statements.len(), 1);
                match &f.body.statements[0] {
                    Statement::For(for_stmt) => {
                        assert_eq!(for_stmt.var_name, "i");
                        assert!(
                            for_stmt.var_type.is_none(),
                            "type-inferred for loop should have no var_type"
                        );
                        match &for_stmt.source {
                            ForSource::Range { start, end } => {
                                match start {
                                    Expr::Literal(Literal {
                                        kind: LiteralKind::Int(0),
                                        ..
                                    }) => {}
                                    other => panic!("expected range start 0, got {:?}", other),
                                }
                                match end {
                                    Expr::Literal(Literal {
                                        kind: LiteralKind::Int(10),
                                        ..
                                    }) => {}
                                    other => panic!("expected range end 10, got {:?}", other),
                                }
                            }
                            other => panic!("expected Range, got {:?}", other),
                        }
                        assert_eq!(for_stmt.body.statements.len(), 1);
                    }
                    other => panic!("expected For, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_else_if_chain() {
        let source = "function main() {\n  if x == 1 { } else if x == 2 { } else { }\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.body.statements.len(), 1);
                match &f.body.statements[0] {
                    Statement::If(if_stmt) => {
                        // First condition: x == 1
                        match &if_stmt.condition {
                            Expr::Binary(b) => assert_eq!(b.op, BinaryOp::Eq),
                            other => panic!("expected Binary Eq, got {:?}", other),
                        }
                        // else branch should be ElseIf
                        match &if_stmt.else_branch {
                            Some(ElseBranch::ElseIf(elif)) => {
                                // Second condition: x == 2
                                match &elif.condition {
                                    Expr::Binary(b) => assert_eq!(b.op, BinaryOp::Eq),
                                    other => {
                                        panic!("expected Binary Eq in else-if, got {:?}", other)
                                    }
                                }
                                // The else-if should have a plain else block
                                match &elif.else_branch {
                                    Some(ElseBranch::Block(_)) => {}
                                    other => panic!(
                                        "expected ElseBranch::Block for final else, got {:?}",
                                        other
                                    ),
                                }
                            }
                            other => panic!("expected ElseBranch::ElseIf, got {:?}", other),
                        }
                    }
                    other => panic!("expected If, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_struct_decl() {
        let source = "struct Point { Int x\n Int y }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.name, "Point");
                assert_eq!(s.fields.len(), 2);
                assert_eq!(s.fields[0].name, "x");
                assert_eq!(s.fields[1].name, "y");
                match &s.fields[0].type_annotation {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Int"),
                    _ => panic!("expected Named type"),
                }
                match &s.fields[1].type_annotation {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Int"),
                    _ => panic!("expected Named type"),
                }
            }
            _ => panic!("expected Struct"),
        }
    }

    #[test]
    fn parse_enum_decl() {
        let source = "enum Color { Red\n Green\n Blue }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                assert_eq!(e.name, "Color");
                assert_eq!(e.variants.len(), 3);
                assert_eq!(e.variants[0].name, "Red");
                assert_eq!(e.variants[1].name, "Green");
                assert_eq!(e.variants[2].name, "Blue");
                // No fields on any variant
                assert!(e.variants[0].fields.is_empty());
                assert!(e.variants[1].fields.is_empty());
                assert!(e.variants[2].fields.is_empty());
            }
            _ => panic!("expected Enum"),
        }
    }

    #[test]
    fn parse_enum_with_fields() {
        let source = "enum Shape { Circle(Float)\n Rect(Float, Float) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                assert_eq!(e.name, "Shape");
                assert_eq!(e.variants.len(), 2);
                assert_eq!(e.variants[0].name, "Circle");
                assert_eq!(e.variants[0].fields.len(), 1);
                match &e.variants[0].fields[0] {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Float"),
                    _ => panic!("expected Named type"),
                }
                assert_eq!(e.variants[1].name, "Rect");
                assert_eq!(e.variants[1].fields.len(), 2);
                match &e.variants[1].fields[0] {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Float"),
                    _ => panic!("expected Named type"),
                }
                match &e.variants[1].fields[1] {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Float"),
                    _ => panic!("expected Named type"),
                }
            }
            _ => panic!("expected Enum"),
        }
    }

    #[test]
    fn parse_impl_block() {
        let source = r#"impl Point { function display(self) -> String { return "hi" } }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Impl(impl_blk) => {
                assert_eq!(impl_blk.type_name, "Point");
                assert_eq!(impl_blk.methods.len(), 1);
                let method = &impl_blk.methods[0];
                assert_eq!(method.name, "display");
                assert_eq!(method.params.len(), 1);
                assert_eq!(method.params[0].name, "self");
                assert!(method.return_type.is_some());
                match &method.return_type {
                    Some(TypeExpr::Named(n)) => assert_eq!(n.name, "String"),
                    other => panic!("expected Named(String) return type, got {:?}", other),
                }
            }
            _ => panic!("expected Impl"),
        }
    }

    #[test]
    fn parse_field_access() {
        let source = "function main() { print(p.x) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::Expression(e) => match &e.expr {
                    Expr::Call(c) => {
                        assert_eq!(c.args.len(), 1);
                        match &c.args[0] {
                            Expr::FieldAccess(fa) => {
                                assert_eq!(fa.field, "x");
                                match &fa.object {
                                    Expr::Ident(id) => assert_eq!(id.name, "p"),
                                    other => panic!("expected Ident(p), got {:?}", other),
                                }
                            }
                            other => panic!("expected FieldAccess, got {:?}", other),
                        }
                    }
                    other => panic!("expected Call, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_method_call() {
        let source = "function main() { print(p.display()) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::Expression(e) => match &e.expr {
                    Expr::Call(c) => {
                        assert_eq!(c.args.len(), 1);
                        match &c.args[0] {
                            Expr::MethodCall(mc) => {
                                assert_eq!(mc.method, "display");
                                assert!(mc.args.is_empty());
                                match &mc.object {
                                    Expr::Ident(id) => assert_eq!(id.name, "p"),
                                    other => panic!("expected Ident(p), got {:?}", other),
                                }
                            }
                            other => panic!("expected MethodCall, got {:?}", other),
                        }
                    }
                    other => panic!("expected Call, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_struct_literal() {
        let source = "function main() { let p: Point = Point(1, 2) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => {
                    assert_eq!(v.simple_name().unwrap(), "p");
                    match &v.initializer {
                        Expr::StructLiteral(sl) => {
                            assert_eq!(sl.name, "Point");
                            assert_eq!(sl.args.len(), 2);
                            match &sl.args[0] {
                                Expr::Literal(Literal {
                                    kind: LiteralKind::Int(1),
                                    ..
                                }) => {}
                                other => panic!("expected Literal(1), got {:?}", other),
                            }
                            match &sl.args[1] {
                                Expr::Literal(Literal {
                                    kind: LiteralKind::Int(2),
                                    ..
                                }) => {}
                                other => panic!("expected Literal(2), got {:?}", other),
                            }
                        }
                        other => panic!("expected StructLiteral, got {:?}", other),
                    }
                }
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_match_expr() {
        let source =
            "function main() { match x {\n  1 -> print(\"one\")\n  _ -> print(\"other\")\n} }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.body.statements.len(), 1);
                match &f.body.statements[0] {
                    Statement::Expression(e) => match &e.expr {
                        Expr::Match(m) => {
                            // subject should be ident x
                            match &m.subject {
                                Expr::Ident(id) => assert_eq!(id.name, "x"),
                                other => panic!("expected Ident(x), got {:?}", other),
                            }
                            assert_eq!(m.arms.len(), 2);
                            // First arm: literal 1
                            match &m.arms[0].pattern {
                                Pattern::Literal(Literal {
                                    kind: LiteralKind::Int(1),
                                    ..
                                }) => {}
                                other => panic!("expected Pattern::Literal(1), got {:?}", other),
                            }
                            // Second arm: wildcard _
                            match &m.arms[1].pattern {
                                Pattern::Wildcard(_) => {}
                                other => panic!("expected Pattern::Wildcard, got {:?}", other),
                            }
                        }
                        other => panic!("expected Match, got {:?}", other),
                    },
                    other => panic!("expected Expression, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_match_variant_pattern() {
        let source =
            "function main() { match shape {\n  Circle(r) -> print(r)\n  _ -> print(0)\n} }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::Expression(e) => match &e.expr {
                    Expr::Match(m) => {
                        assert_eq!(m.arms.len(), 2);
                        match &m.arms[0].pattern {
                            Pattern::Variant(vp) => {
                                assert_eq!(vp.variant, "Circle");
                                assert_eq!(vp.bindings.len(), 1);
                                assert_eq!(vp.bindings[0], "r");
                            }
                            other => panic!("expected Pattern::Variant, got {:?}", other),
                        }
                    }
                    other => panic!("expected Match, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_break() {
        let source = "function main() { while true { break } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::While(w) => {
                    assert_eq!(w.body.statements.len(), 1);
                    match &w.body.statements[0] {
                        Statement::Break(_) => {}
                        other => panic!("expected Break, got {:?}", other),
                    }
                }
                other => panic!("expected While, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_continue() {
        let source = "function main() { while true { continue } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::While(w) => {
                    assert_eq!(w.body.statements.len(), 1);
                    match &w.body.statements[0] {
                        Statement::Continue(_) => {}
                        other => panic!("expected Continue, got {:?}", other),
                    }
                }
                other => panic!("expected While, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_break_in_for() {
        let source = "function main() { for i in 0..10 { break } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::For(for_stmt) => {
                    assert_eq!(for_stmt.body.statements.len(), 1);
                    match &for_stmt.body.statements[0] {
                        Statement::Break(_) => {}
                        other => panic!("expected Break, got {:?}", other),
                    }
                }
                other => panic!("expected For, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_self_in_method() {
        let source = "impl Foo { function bar(self) { print(self) } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Impl(impl_blk) => {
                assert_eq!(impl_blk.type_name, "Foo");
                assert_eq!(impl_blk.methods.len(), 1);
                let method = &impl_blk.methods[0];
                assert_eq!(method.name, "bar");
                // self parameter
                assert_eq!(method.params.len(), 1);
                assert_eq!(method.params[0].name, "self");
                // body should contain print(self)
                assert_eq!(method.body.statements.len(), 1);
                match &method.body.statements[0] {
                    Statement::Expression(e) => match &e.expr {
                        Expr::Call(c) => {
                            assert_eq!(c.args.len(), 1);
                            match &c.args[0] {
                                Expr::Ident(id) => assert_eq!(id.name, "self"),
                                other => panic!("expected Ident(self), got {:?}", other),
                            }
                        }
                        other => panic!("expected Call, got {:?}", other),
                    },
                    other => panic!("expected Expression, got {:?}", other),
                }
            }
            _ => panic!("expected Impl"),
        }
    }

    /// Parsing a lambda expression produces an `Expr::Lambda` node with the
    /// correct parameters, return type, and body.
    #[test]
    fn parse_lambda_expr() {
        let source =
            "function main() { let f: (Int) -> Int = function(x: Int) -> Int { return x * 2 } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(func) => {
                assert_eq!(func.body.statements.len(), 1);
                match &func.body.statements[0] {
                    Statement::VarDecl(v) => {
                        assert_eq!(v.simple_name().unwrap(), "f");
                        match &v.initializer {
                            Expr::Lambda(lambda) => {
                                assert_eq!(lambda.params.len(), 1);
                                assert_eq!(lambda.params[0].name, "x");
                                assert!(lambda.return_type.is_some());
                                assert_eq!(lambda.body.statements.len(), 1);
                            }
                            other => panic!("expected Lambda, got {:?}", other),
                        }
                    }
                    other => panic!("expected VarDecl, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    /// A function parameter with a function type `(Int) -> Int` is parsed as
    /// `TypeExpr::Function`.
    #[test]
    fn parse_function_type() {
        let source = "function apply(f: (Int) -> Int, x: Int) -> Int { return f(x) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(func) => {
                assert_eq!(func.name, "apply");
                assert_eq!(func.params.len(), 2);
                assert_eq!(func.params[0].name, "f");
                match &func.params[0].type_annotation {
                    TypeExpr::Function(ft) => {
                        assert_eq!(ft.param_types.len(), 1);
                        assert!(
                            matches!(&ft.param_types[0], TypeExpr::Named(n) if n.name == "Int")
                        );
                        assert!(matches!(&*ft.return_type, TypeExpr::Named(n) if n.name == "Int"));
                    }
                    other => panic!("expected Function type, got {:?}", other),
                }
                assert_eq!(func.params[1].name, "x");
            }
            _ => panic!("expected Function"),
        }
    }

    /// A lambda with no return type annotation defaults to having `return_type: None`.
    #[test]
    fn parse_lambda_no_return_type() {
        let source = "function main() { let f: () -> Void = function() { print(1) } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(func) => match &func.body.statements[0] {
                Statement::VarDecl(v) => {
                    assert_eq!(v.simple_name().unwrap(), "f");
                    match &v.initializer {
                        Expr::Lambda(lambda) => {
                            assert!(lambda.params.is_empty());
                            assert!(lambda.return_type.is_none());
                            assert_eq!(lambda.body.statements.len(), 1);
                        }
                        other => panic!("expected Lambda, got {:?}", other),
                    }
                }
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    /// A generic function declaration parses type parameters correctly.
    #[test]
    fn parse_generic_function() {
        let (program, diagnostics) = parse_source("function identity<T>(x: T) -> T { return x }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.name, "identity");
                assert_eq!(f.type_params, vec!["T".to_string()]);
                assert_eq!(f.params.len(), 1);
                assert!(f.return_type.is_some());
            }
            _ => panic!("expected Function"),
        }
    }

    /// A generic struct declaration parses multiple type parameters.
    #[test]
    fn parse_generic_struct() {
        let (program, diagnostics) = parse_source("struct Pair<A, B> {\n  A first\n  B second\n}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.name, "Pair");
                assert_eq!(s.type_params, vec!["A".to_string(), "B".to_string()]);
                assert_eq!(s.fields.len(), 2);
                assert_eq!(s.fields[0].name, "first");
                assert_eq!(s.fields[1].name, "second");
            }
            _ => panic!("expected Struct"),
        }
    }

    /// A generic enum declaration parses its type parameter.
    #[test]
    fn parse_generic_enum() {
        let (program, diagnostics) = parse_source("enum Option<T> {\n  Some(T)\n  None\n}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                assert_eq!(e.name, "Option");
                assert_eq!(e.type_params, vec!["T".to_string()]);
                assert_eq!(e.variants.len(), 2);
                assert_eq!(e.variants[0].name, "Some");
                assert_eq!(e.variants[1].name, "None");
            }
            _ => panic!("expected Enum"),
        }
    }

    /// A generic type used in a variable annotation parses as `TypeExpr::Generic`.
    #[test]
    fn parse_generic_type_in_annotation() {
        let (program, diagnostics) = parse_source(
            "enum Option<T> {\n  Some(T)\n  None\n}\nfunction main() { let x: Option<Int> = None }",
        );
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        // Find main function
        let main_fn = program
            .declarations
            .iter()
            .find_map(|d| {
                if let Declaration::Function(f) = d
                    && f.name == "main"
                {
                    return Some(f);
                }
                None
            })
            .expect("expected main function");
        match &main_fn.body.statements[0] {
            Statement::VarDecl(v) => {
                assert_eq!(v.simple_name().unwrap(), "x");
                match &v.type_annotation {
                    Some(TypeExpr::Generic(gt)) => {
                        assert_eq!(gt.name, "Option");
                        assert_eq!(gt.type_args.len(), 1);
                    }
                    other => panic!("expected Some(TypeExpr::Generic), got {:?}", other),
                }
            }
            other => panic!("expected VarDecl, got {:?}", other),
        }
    }

    #[test]
    fn parse_list_literal() {
        let tokens = tokenize("function main() { [1, 2, 3] }", SourceId(0));
        let (program, errors) = parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);
        let main_fn = program
            .declarations
            .iter()
            .find_map(|d| {
                if let Declaration::Function(f) = d
                    && f.name == "main"
                {
                    return Some(f);
                }
                None
            })
            .expect("expected main function");
        match &main_fn.body.statements[0] {
            Statement::Expression(expr_stmt) => match &expr_stmt.expr {
                Expr::ListLiteral(list) => {
                    assert_eq!(list.elements.len(), 3);
                }
                other => panic!("expected ListLiteral, got {:?}", other),
            },
            other => panic!("expected Expression, got {:?}", other),
        }
    }

    #[test]
    fn parse_empty_list() {
        let tokens = tokenize("function main() { [] }", SourceId(0));
        let (program, errors) = parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);
        let main_fn = program
            .declarations
            .iter()
            .find_map(|d| {
                if let Declaration::Function(f) = d
                    && f.name == "main"
                {
                    return Some(f);
                }
                None
            })
            .expect("expected main function");
        match &main_fn.body.statements[0] {
            Statement::Expression(expr_stmt) => match &expr_stmt.expr {
                Expr::ListLiteral(list) => {
                    assert_eq!(list.elements.len(), 0);
                }
                other => panic!("expected ListLiteral, got {:?}", other),
            },
            other => panic!("expected Expression, got {:?}", other),
        }
    }

    /// A trait declaration with one method signature is parsed correctly.
    #[test]
    fn parse_trait_decl() {
        let source = "trait Display {\n  function to_string(self) -> String\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Trait(t) => {
                assert_eq!(t.name, "Display");
                assert_eq!(t.methods.len(), 1);
                assert_eq!(t.methods[0].name, "to_string");
                assert_eq!(t.methods[0].params.len(), 1);
                assert_eq!(t.methods[0].params[0].name, "self");
                assert!(t.methods[0].return_type.is_some());
                match &t.methods[0].return_type {
                    Some(TypeExpr::Named(n)) => assert_eq!(n.name, "String"),
                    other => panic!("expected Named(String) return type, got {:?}", other),
                }
            }
            other => panic!("expected Trait, got {:?}", other),
        }
    }

    /// `impl Display for Point { ... }` sets `trait_name = Some("Display")`.
    #[test]
    fn parse_trait_impl() {
        let source =
            r#"impl Display for Point { function to_string(self) -> String { return "hi" } }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Impl(imp) => {
                assert_eq!(imp.trait_name, Some("Display".to_string()));
                assert_eq!(imp.type_name, "Point");
                assert_eq!(imp.methods.len(), 1);
                assert_eq!(imp.methods[0].name, "to_string");
            }
            other => panic!("expected Impl, got {:?}", other),
        }
    }

    /// A generic function with trait bounds parses `type_param_bounds` correctly.
    #[test]
    fn parse_trait_bounds() {
        let source = r#"function show<T: Display>(item: T) -> String { return "hi" }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.name, "show");
                assert_eq!(f.type_params, vec!["T".to_string()]);
                assert_eq!(f.type_param_bounds.len(), 1);
                assert_eq!(f.type_param_bounds[0].0, "T");
                assert_eq!(f.type_param_bounds[0].1, vec!["Display".to_string()]);
            }
            other => panic!("expected Function, got {:?}", other),
        }
    }

    // --- String escape tests ---

    /// A string with a literal backslash-n (`\\n`) should be unescaped as a newline.
    #[test]
    fn parse_string_escape_newline() {
        let source = r#"function main() { let s: String = "hello\nworld" }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Literal(Literal {
                        kind: LiteralKind::String(s),
                        ..
                    }) => {
                        assert_eq!(s, "hello\nworld");
                    }
                    other => panic!("expected string literal, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    /// A string with a literal backslash-backslash (`\\\\`) should be unescaped as a single backslash.
    #[test]
    fn parse_string_escape_backslash() {
        let source = r#"function main() { let s: String = "a\\b" }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Literal(Literal {
                        kind: LiteralKind::String(s),
                        ..
                    }) => {
                        assert_eq!(s, "a\\b");
                    }
                    other => panic!("expected string literal, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    /// A string with `\\\\n` should be unescaped as backslash followed by n (not a newline).
    #[test]
    fn parse_string_escape_backslash_n() {
        let source = r#"function main() { let s: String = "\\n" }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Literal(Literal {
                        kind: LiteralKind::String(s),
                        ..
                    }) => {
                        // \\n in source -> \n in the token text -> unescape -> backslash + n
                        // Actually: source `"\\n"` -> token text `\\n` -> unescape: `\` then `n` -> `\n` (newline)
                        // Wait: in the raw Rust string r#""\\n""#, the Phoenix source contains: "\\n"
                        // The lexer sees: backslash backslash n, stored as \\n in token text
                        // After stripping quotes we get: \\n
                        // unescape: sees \, then \, outputs \; then sees n, outputs n -> \n... no.
                        // unescape: sees \, next is \, outputs \. Then sees n, outputs n. Result: "\n" (backslash-n literally? No...)
                        // Actually unescape: first char is \, next char is \, so push \. Then next char is n, just push n. Result is `\n` where \ is a literal backslash.
                        // But wait: in Rust, "\n" is a newline. "\\n" is backslash-n. So we want the result to be literally backslash followed by n.
                        assert_eq!(s.len(), 2);
                        assert_eq!(s.as_bytes()[0], b'\\');
                        assert_eq!(s.as_bytes()[1], b'n');
                    }
                    other => panic!("expected string literal, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    /// A string with a tab escape (`\\t`) is unescaped correctly.
    #[test]
    fn parse_string_escape_tab() {
        let source = r#"function main() { let s: String = "a\tb" }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Literal(Literal {
                        kind: LiteralKind::String(s),
                        ..
                    }) => {
                        assert_eq!(s, "a\tb");
                    }
                    other => panic!("expected string literal, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    /// An escaped quote inside a string is handled correctly.
    #[test]
    fn parse_string_escape_quote() {
        let source = r#"function main() { let s: String = "say \"hi\"" }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Literal(Literal {
                        kind: LiteralKind::String(s),
                        ..
                    }) => {
                        assert_eq!(s, "say \"hi\"");
                    }
                    other => panic!("expected string literal, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    // --- Phase 1.8 feature tests ---

    /// `type Id = Int` produces a TypeAlias declaration.
    #[test]
    fn parse_type_alias_simple() {
        let (program, diagnostics) = parse_source("type Id = Int");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::TypeAlias(ta) => {
                assert_eq!(ta.name, "Id");
                assert!(ta.type_params.is_empty());
                match &ta.target {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Int"),
                    other => panic!("expected Named(Int), got {:?}", other),
                }
            }
            other => panic!("expected TypeAlias, got {:?}", other),
        }
    }

    /// `type Res<T> = Result<T, String>` parses generic type params.
    #[test]
    fn parse_type_alias_generic() {
        let (program, diagnostics) = parse_source("type Res<T> = Result<T, String>");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::TypeAlias(ta) => {
                assert_eq!(ta.name, "Res");
                assert_eq!(ta.type_params, vec!["T".to_string()]);
                match &ta.target {
                    TypeExpr::Generic(g) => {
                        assert_eq!(g.name, "Result");
                        assert_eq!(g.type_args.len(), 2);
                    }
                    other => panic!("expected Generic(Result<T, String>), got {:?}", other),
                }
            }
            other => panic!("expected TypeAlias, got {:?}", other),
        }
    }

    /// `p.x = 10` produces a FieldAssignment expression.
    #[test]
    fn parse_field_assignment() {
        let (program, diagnostics) = parse_source("function main() { p.x = 10 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::Expression(e) => match &e.expr {
                    Expr::FieldAssignment(fa) => {
                        assert_eq!(fa.field, "x");
                        match &fa.object {
                            Expr::Ident(id) => assert_eq!(id.name, "p"),
                            other => panic!("expected Ident(p), got {:?}", other),
                        }
                    }
                    other => panic!("expected FieldAssignment, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    /// `"hello {name}"` produces a StringInterpolation expression.
    #[test]
    fn parse_string_interpolation() {
        let source = r#"function main() { let s: String = "hello {name}" }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::StringInterpolation(si) => {
                        assert!(
                            si.segments.len() >= 2,
                            "expected at least 2 segments, got {:?}",
                            si.segments
                        );
                    }
                    other => panic!("expected StringInterpolation, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    /// `expr?` produces a Try expression.
    #[test]
    fn parse_try_operator() {
        let source = r#"function foo() -> Result<Int, String> { let x: Int = bar()? }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Try(t) => match &t.operand {
                        Expr::Call(c) => assert_eq!(c.args.len(), 0),
                        other => panic!("expected Call, got {:?}", other),
                    },
                    other => panic!("expected Try, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    // ── Snapshot tests for error messages ──────────────────────────────

    #[test]
    fn snapshot_error_missing_function_name() {
        let (_, diags) = parse_source("function { }");
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }

    #[test]
    fn snapshot_error_missing_closing_brace() {
        let (_, diags) = parse_source("function foo() {");
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }

    #[test]
    fn snapshot_error_missing_paren() {
        let (_, diags) = parse_source("function foo( { }");
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }

    #[test]
    fn snapshot_error_unexpected_token_in_expression() {
        let (_, diags) = parse_source("function main() { let x: Int = }");
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }

    #[test]
    fn snapshot_error_compound_assignment() {
        let (_, diags) = parse_source("function main() { let mut x: Int = 1\n x += 2 }");
        let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        insta::assert_debug_snapshot!(messages);
    }
}
