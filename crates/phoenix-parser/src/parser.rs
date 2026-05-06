use crate::ast::{
    Block, Declaration, DerivedType, EndpointDecl, EndpointErrorVariant, EnumDecl, EnumVariant,
    FieldDecl, FunctionDecl, HttpMethod, ImplBlock, ImportDecl, ImportItem, ImportItems,
    InlineTraitImpl, NamedType, Param, Program, QueryParam, SchemaDecl, SchemaTable, StructDecl,
    TraitDecl, TraitMethodSig, TypeAliasDecl, TypeExpr, TypeModifier, Visibility,
};
use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::span::{SourceId, Span};
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
        TokenKind::Dyn => "'dyn'",
        TokenKind::Type => "'type'",
        TokenKind::Endpoint => "'endpoint'",
        TokenKind::Body => "'body'",
        TokenKind::Response => "'response'",
        TokenKind::ErrorKw => "'error'",
        TokenKind::Omit => "'omit'",
        TokenKind::Pick => "'pick'",
        TokenKind::Partial => "'partial'",
        TokenKind::Query => "'query'",
        TokenKind::Where => "'where'",
        TokenKind::Schema => "'schema'",
        TokenKind::Get => "'GET'",
        TokenKind::Post => "'POST'",
        TokenKind::Put => "'PUT'",
        TokenKind::Patch => "'PATCH'",
        TokenKind::Delete => "'DELETE'",
        TokenKind::DocComment => "doc comment",
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
        TokenKind::And => "'&&'",
        TokenKind::Or => "'||'",
        TokenKind::Not => "'!'",
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
        TokenKind::PlusEq => "'+='",
        TokenKind::MinusEq => "'-='",
        TokenKind::StarEq => "'*='",
        TokenKind::SlashEq => "'/='",
        TokenKind::PercentEq => "'%='",
        TokenKind::Newline => "newline",
        TokenKind::Eof => "end of file",
        TokenKind::Error => "error token",
        TokenKind::Import => "'import'",
        TokenKind::Public => "'public'",
        TokenKind::As => "'as'",
        TokenKind::Defer => "'defer'",
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
    /// Parse errors collected during parsing for multi-error reporting.
    pub diagnostics: Vec<Diagnostic>,
    /// The source file ID for the tokens being parsed.
    ///
    /// Derived from the first token's span. Used by the string interpolation
    /// sub-parser so it doesn't need to extract the ID from individual tokens.
    pub source_id: SourceId,
}

impl<'src> Parser<'src> {
    /// Creates a new parser over the given token slice.
    ///
    /// The token slice must end with a [`TokenKind::Eof`] token (the lexer
    /// guarantees this).
    pub fn new(tokens: &'src [Token]) -> Self {
        let source_id = tokens
            .first()
            .map(|t| t.span.source_id)
            .unwrap_or(SourceId(0));
        Self {
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
            source_id,
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

    /// Parses a top-level declaration, optionally preceded by a doc comment
    /// and an optional `public` visibility modifier.
    fn parse_declaration(&mut self) -> Option<Declaration> {
        // Consume a doc comment if present — it attaches to the next declaration.
        let doc_comment = self.try_consume_doc_comment();

        // Consume optional `public` visibility modifier. Capture its span
        // before advancing so we can emit a precise diagnostic if `public`
        // precedes a non-public-able decl (e.g. `impl`, `import`).
        let public_span = if self.peek().kind == TokenKind::Public {
            let span = self.peek().span;
            self.advance();
            Some(span)
        } else {
            None
        };
        let visibility = if public_span.is_some() {
            Visibility::Public
        } else {
            Visibility::Private
        };

        match self.peek().kind {
            TokenKind::Function => self
                .parse_function_decl(visibility)
                .map(Declaration::Function),
            TokenKind::Struct => self
                .parse_struct_decl(doc_comment, visibility)
                .map(Declaration::Struct),
            TokenKind::Enum => self
                .parse_enum_decl(doc_comment, visibility)
                .map(Declaration::Enum),
            TokenKind::Impl => {
                self.reject_public_modifier(
                    public_span,
                    "`public` cannot precede `impl` — impl visibility is derived from the trait and the type",
                );
                self.parse_impl_block().map(Declaration::Impl)
            }
            TokenKind::Trait => self.parse_trait_decl(visibility).map(Declaration::Trait),
            TokenKind::Type => self
                .parse_type_alias_decl(visibility)
                .map(Declaration::TypeAlias),
            TokenKind::Endpoint => {
                self.reject_public_modifier(public_span, "`public` cannot precede `endpoint`");
                self.parse_endpoint_decl(doc_comment)
                    .map(Declaration::Endpoint)
            }
            TokenKind::Schema => {
                self.reject_public_modifier(public_span, "`public` cannot precede `schema`");
                self.parse_schema_decl().map(Declaration::Schema)
            }
            TokenKind::Import => {
                self.reject_public_modifier(
                    public_span,
                    "`import` declarations cannot be marked `public`",
                );
                self.parse_import_decl().map(Declaration::Import)
            }
            _ => {
                self.error_at_current("expected a declaration (e.g. `function`, `struct`, `enum`, `impl`, `trait`, `type`, `endpoint`, `schema`, `import`)");
                None
            }
        }
    }

    /// Emit a diagnostic at `public_span` if a `public` keyword preceded a
    /// declaration that does not support visibility. Called from the
    /// declaration-kind arms that don't carry a `Visibility` field.
    fn reject_public_modifier(&mut self, public_span: Option<Span>, message: &'static str) {
        if let Some(span) = public_span {
            self.diagnostics.push(Diagnostic::error(message, span));
        }
    }

    /// Parses an `import` declaration:
    /// `import a.b.c { Foo, Bar as Baz }` or `import a.b.c { * }`.
    fn parse_import_decl(&mut self) -> Option<ImportDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Import)?;

        // Dotted module path: IDENT (. IDENT)*
        let mut path = Vec::new();
        let first = self.expect(TokenKind::Ident)?;
        path.push(first.text.clone());
        while self.eat(TokenKind::Dot) {
            let segment = self.expect(TokenKind::Ident)?;
            path.push(segment.text.clone());
        }

        let lbrace_span = self.expect(TokenKind::LBrace)?.span;
        self.skip_newlines();

        let items = if self.eat(TokenKind::Star) {
            self.skip_newlines();
            ImportItems::Wildcard
        } else {
            let mut items = Vec::new();
            loop {
                self.skip_newlines();
                if self.peek().kind == TokenKind::RBrace {
                    break;
                }
                let name_tok = self.expect(TokenKind::Ident)?;
                let name = name_tok.text.clone();
                let item_start = name_tok.span;
                let (alias, item_end) = if self.eat(TokenKind::As) {
                    let alias_tok = self.expect(TokenKind::Ident)?;
                    (Some(alias_tok.text.clone()), alias_tok.span)
                } else {
                    (None, name_tok.span)
                };
                items.push(ImportItem {
                    name,
                    alias,
                    span: item_start.merge(item_end),
                });
                self.skip_newlines();
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            if items.is_empty() {
                // Anchor at the brace pair itself so the caret points at
                // `{ }`, not at the entire `import a.b { }` declaration.
                let brace_span = if self.peek().kind == TokenKind::RBrace {
                    lbrace_span.merge(self.peek().span)
                } else {
                    lbrace_span
                };
                self.diagnostics.push(Diagnostic::error(
                    "import list cannot be empty — use `{ * }` for a wildcard import or list at least one item",
                    brace_span,
                ));
            }
            ImportItems::Named(items)
        };

        self.skip_newlines();
        let end = self.expect(TokenKind::RBrace)?.span;

        Some(ImportDecl {
            path,
            items,
            span: start.merge(end),
        })
    }

    /// Consumes a [`TokenKind::DocComment`] token if one is present at the
    /// current position, and returns its trimmed text content.
    ///
    /// Doc comments (`/** ... */`) may precede `struct`, `enum`, and `endpoint`
    /// declarations. This method is called by [`parse_declaration`](Self::parse_declaration)
    /// before dispatching to the specific declaration parser, so that the
    /// doc comment text can be threaded through as a parameter.
    ///
    /// Any newlines following the doc comment are also consumed so that the
    /// next token seen by the caller is the declaration keyword.
    ///
    /// Returns `None` if the current token is not a doc comment.
    fn try_consume_doc_comment(&mut self) -> Option<String> {
        if self.peek().kind == TokenKind::DocComment {
            let tok = self.advance();
            self.skip_newlines();
            Some(tok.text.clone())
        } else {
            None
        }
    }

    /// Parses a function declaration including optional type parameters, params, return type, and body.
    ///
    /// `visibility` is supplied by the caller — top-level declarations and
    /// inline methods (struct/enum bodies, inherent `impl` blocks) pass the
    /// modifier observed before the `function` keyword. Methods inside
    /// `impl Trait for Type` blocks always pass [`Visibility::Private`]; the
    /// caller rejects an explicit `public` modifier in that position because
    /// trait-impl method visibility is fixed by the trait contract.
    fn parse_function_decl(&mut self, visibility: Visibility) -> Option<FunctionDecl> {
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
            name_span: name_token.span,
            type_params,
            type_param_bounds,
            params,
            return_type,
            body,
            visibility,
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
    ///
    /// `allow_method_visibility` controls whether an optional `public` modifier
    /// before each method is accepted (inherent `impl Type` blocks) or rejected
    /// with a diagnostic (trait `impl Trait for Type` blocks, where method
    /// visibility is fixed by the trait contract).
    fn parse_methods_block(&mut self, allow_method_visibility: bool) -> Option<Vec<FunctionDecl>> {
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let mut methods = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            let public_span = if self.peek().kind == TokenKind::Public {
                let span = self.peek().span;
                self.advance();
                Some(span)
            } else {
                None
            };
            let visibility = if !allow_method_visibility {
                self.reject_public_modifier(
                    public_span,
                    "`public` cannot precede a method in a trait `impl` block — trait-impl method visibility is fixed by the trait",
                );
                Visibility::Private
            } else if public_span.is_some() {
                Visibility::Public
            } else {
                Visibility::Private
            };
            if let Some(func) = self.parse_function_decl(visibility) {
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
    ///
    /// The returned reference has the `'src` lifetime of the token slice,
    /// so it does not keep `&self` borrowed — callers can hold the reference
    /// while calling `&mut self` methods.
    pub(crate) fn peek(&self) -> &'src Token {
        self.tokens
            .get(self.pos)
            .unwrap_or_else(|| self.tokens.last().expect("token stream must end with Eof"))
    }

    /// Returns a reference to the token `offset` positions ahead of the current
    /// position without consuming any tokens. Returns the `Eof` token if the
    /// offset is past the end of the token stream.
    pub(crate) fn peek_at(&self, offset: usize) -> &'src Token {
        self.tokens
            .get(self.pos + offset)
            .unwrap_or_else(|| self.tokens.last().expect("token stream must end with Eof"))
    }

    /// Consumes the current token and returns a reference to it.
    /// Does not advance past `Eof`.
    pub(crate) fn advance(&mut self) -> &'src Token {
        let token = self
            .tokens
            .get(self.pos)
            .unwrap_or_else(|| self.tokens.last().expect("token stream must end with Eof"));
        if token.kind != TokenKind::Eof {
            self.pos += 1;
        }
        token
    }

    /// Consumes the current token if it matches `kind`, returning a reference.
    /// Records a diagnostic and returns `None` on mismatch.
    pub(crate) fn expect(&mut self, kind: TokenKind) -> Option<&'src Token> {
        if self.peek().kind == kind {
            Some(self.advance())
        } else {
            self.error_at_current(&format!("expected {}", token_kind_display(&kind)));
            None
        }
    }

    /// Consumes the current token if it is an identifier or a contextual keyword
    /// that can be used as a name (e.g. struct field names, variable names).
    ///
    /// Gen keywords like `body`, `response`, `query`, `error`, `omit`, `pick`,
    /// `partial`, `where`, and `schema` are only special inside endpoint blocks.
    /// Everywhere else they should be usable as ordinary identifiers.
    pub(crate) fn expect_ident_or_contextual(&mut self) -> Option<&'src Token> {
        if matches!(
            self.peek().kind,
            TokenKind::Ident
                | TokenKind::Body
                | TokenKind::Response
                | TokenKind::Query
                | TokenKind::ErrorKw
                | TokenKind::Omit
                | TokenKind::Pick
                | TokenKind::Partial
                | TokenKind::Where
                | TokenKind::Schema
        ) {
            Some(self.advance())
        } else {
            self.error_at_current(&format!(
                "expected {}",
                token_kind_display(&TokenKind::Ident)
            ));
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
    ///
    /// The optional `doc_comment` parameter carries the text of a preceding
    /// `/** ... */` doc comment, if one was consumed by
    /// [`try_consume_doc_comment`](Self::try_consume_doc_comment). It is
    /// stored on the resulting [`StructDecl`] for use by code generators and
    /// documentation tools.
    pub(crate) fn parse_struct_decl(
        &mut self,
        doc_comment: Option<String>,
        visibility: Visibility,
    ) -> Option<StructDecl> {
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
            // Hoist `public` consumption when followed by `function` (public
            // method) or `impl` (rejected — inline trait impls have no
            // visibility of their own). For fields, the field branch below
            // consumes its own `public`.
            let public_span = if self.peek().kind == TokenKind::Public
                && matches!(self.peek_at(1).kind, TokenKind::Function | TokenKind::Impl)
            {
                let span = self.peek().span;
                self.advance();
                Some(span)
            } else {
                None
            };

            if self.peek().kind == TokenKind::Function {
                let visibility = if public_span.is_some() {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                if let Some(func) = self.parse_function_decl(visibility) {
                    methods.push(func);
                } else {
                    self.synchronize_stmt();
                }
            } else if self.peek().kind == TokenKind::Impl {
                self.reject_public_modifier(
                    public_span,
                    "`public` cannot precede inline `impl` — trait-impl method visibility is fixed by the trait",
                );
                if let Some(ti) = self.parse_inline_trait_impl() {
                    trait_impls.push(ti);
                } else {
                    self.synchronize_stmt();
                }
            } else {
                // Field: [doc_comment] [public] Type name [where <constraint-expr>]
                let field_doc = self.try_consume_doc_comment();
                let fstart = self.peek().span;
                let field_vis = if self.eat(TokenKind::Public) {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                if let Some(type_expr) = self.parse_type_expr()
                    && let Some(name_tok) = self.expect_ident_or_contextual()
                {
                    let constraint = if self.peek().kind == TokenKind::Where {
                        self.advance(); // consume 'where'
                        self.parse_expr()
                    } else {
                        None
                    };
                    let end_span = constraint
                        .as_ref()
                        .map(|e| e.span())
                        .unwrap_or(name_tok.span);
                    let span = fstart.merge(end_span);
                    fields.push(FieldDecl {
                        type_annotation: type_expr,
                        name: name_tok.text.clone(),
                        constraint,
                        doc_comment: field_doc,
                        visibility: field_vis,
                        span,
                    });
                }
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(StructDecl {
            name: name_token.text.clone(),
            name_span: name_token.span,
            type_params,
            fields,
            methods,
            trait_impls,
            doc_comment,
            visibility,
            span: start.merge(end),
        })
    }

    /// Parses an enum declaration: `enum Name { Variant, Variant(Type), methods, impl blocks }`.
    ///
    /// The optional `doc_comment` parameter carries the text of a preceding
    /// `/** ... */` doc comment, if one was consumed by
    /// [`try_consume_doc_comment`](Self::try_consume_doc_comment). It is
    /// stored on the resulting [`EnumDecl`] for use by code generators and
    /// documentation tools.
    pub(crate) fn parse_enum_decl(
        &mut self,
        doc_comment: Option<String>,
        visibility: Visibility,
    ) -> Option<EnumDecl> {
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
            // Hoist `public` consumption when followed by `function` (public
            // method) or `impl` (rejected — inline trait impls have no
            // visibility of their own). Variants do not accept `public`.
            let public_span = if self.peek().kind == TokenKind::Public
                && matches!(self.peek_at(1).kind, TokenKind::Function | TokenKind::Impl)
            {
                let span = self.peek().span;
                self.advance();
                Some(span)
            } else {
                None
            };

            if self.peek().kind == TokenKind::Function {
                let visibility = if public_span.is_some() {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                if let Some(func) = self.parse_function_decl(visibility) {
                    methods.push(func);
                } else {
                    self.synchronize_stmt();
                }
            } else if self.peek().kind == TokenKind::Impl {
                self.reject_public_modifier(
                    public_span,
                    "`public` cannot precede inline `impl` — trait-impl method visibility is fixed by the trait",
                );
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
            name_span: name_token.span,
            type_params,
            variants,
            methods,
            trait_impls,
            doc_comment,
            visibility,
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

        // Inherent `impl Type` blocks accept per-method `public`; trait
        // `impl Trait for Type` blocks reject it (trait controls visibility).
        let allow_method_visibility = trait_name.is_none();
        let methods = self.parse_methods_block(allow_method_visibility)?;
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
        // Inline trait impls reject per-method `public` (trait controls visibility).
        let methods = self.parse_methods_block(false)?;
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
    pub(crate) fn parse_trait_decl(&mut self, visibility: Visibility) -> Option<TraitDecl> {
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
            name_span: name_token.span,
            type_params,
            methods,
            visibility,
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
    pub(crate) fn parse_type_alias_decl(
        &mut self,
        visibility: Visibility,
    ) -> Option<TypeAliasDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Type)?;
        let name_token = self.expect(TokenKind::Ident)?;
        let type_params = self.parse_type_params();
        self.expect(TokenKind::Eq)?;
        let target = self.parse_type_expr()?;
        let end = target.span();
        self.eat(TokenKind::Newline);
        Some(TypeAliasDecl {
            name: name_token.text.clone(),
            name_span: name_token.span,
            type_params,
            target,
            visibility,
            span: start.merge(end),
        })
    }

    // ── Endpoint parsing ─────────────────────────────────────────────

    /// Parses an endpoint declaration.
    ///
    /// Expected syntax:
    ///
    /// ```text
    /// endpoint <name>: <METHOD> "<path>" {
    ///     [body <TypeExpr> [omit|pick|partial { ... }]*]
    ///     [response <TypeExpr>]
    ///     [query { <Type> <name> [= <default>], ... }]
    ///     [error { <Name>(<code>), ... }]
    /// }
    /// ```
    ///
    /// The inner sections (`body`, `response`, `query`, `error`) are all
    /// optional and may appear in any order.  Duplicate sections produce a
    /// diagnostic.  This method delegates to
    /// [`parse_body_type`](Self::parse_body_type),
    /// [`parse_query_block`](Self::parse_query_block), and
    /// [`parse_error_block`](Self::parse_error_block) for the respective
    /// sections.
    ///
    /// The `doc_comment` parameter carries an optional preceding `/** ... */`
    /// comment, stored on the resulting [`EndpointDecl`].
    ///
    /// Returns `None` if a required token is missing (e.g. the HTTP method or
    /// the path string literal), after recording a diagnostic.
    fn parse_endpoint_decl(&mut self, doc_comment: Option<String>) -> Option<EndpointDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Endpoint)?;
        let name_token = self.expect(TokenKind::Ident)?;
        self.expect(TokenKind::Colon)?;

        // Parse HTTP method
        let method = match self.peek().kind {
            TokenKind::Get => {
                self.advance();
                HttpMethod::Get
            }
            TokenKind::Post => {
                self.advance();
                HttpMethod::Post
            }
            TokenKind::Put => {
                self.advance();
                HttpMethod::Put
            }
            TokenKind::Patch => {
                self.advance();
                HttpMethod::Patch
            }
            TokenKind::Delete => {
                self.advance();
                HttpMethod::Delete
            }
            _ => {
                self.error_at_current("expected HTTP method (GET, POST, PUT, PATCH, DELETE)");
                return None;
            }
        };

        // Parse URL path
        let path_token = self.expect(TokenKind::StringLiteral)?;
        // Strip surrounding quotes from the string literal
        let path = path_token.text.trim_matches('"').to_string();

        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut query_params = Vec::new();
        let mut body = None;
        let mut response = None;
        let mut errors = Vec::new();
        let mut has_query = false;
        let mut has_body = false;
        let mut has_response = false;
        let mut has_error = false;

        // Parse inner sections
        loop {
            match self.peek().kind {
                TokenKind::RBrace | TokenKind::Eof => break,
                TokenKind::Query => {
                    if has_query {
                        self.error_at_current("duplicate `query` section in endpoint");
                    }
                    has_query = true;
                    query_params = self.parse_query_block();
                }
                TokenKind::Body => {
                    if has_body {
                        self.error_at_current("duplicate `body` section in endpoint");
                    }
                    has_body = true;
                    body = self.parse_body_type();
                }
                TokenKind::Response => {
                    if has_response {
                        self.error_at_current("duplicate `response` section in endpoint");
                    }
                    has_response = true;
                    self.advance();
                    self.skip_newlines();
                    response = self.parse_type_expr();
                }
                TokenKind::ErrorKw => {
                    if has_error {
                        self.error_at_current("duplicate `error` section in endpoint");
                    }
                    has_error = true;
                    errors = self.parse_error_block();
                }
                _ => {
                    self.error_at_current("expected `query`, `body`, `response`, `error`, or `}`");
                    self.advance();
                }
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(EndpointDecl {
            name: name_token.text.clone(),
            name_span: name_token.span,
            method,
            path,
            query_params,
            body,
            response,
            errors,
            doc_comment,
            span: start.merge(end),
        })
    }

    /// Parses the `body` section of an endpoint, producing a [`DerivedType`].
    ///
    /// Expected syntax:
    ///
    /// ```text
    /// body <BaseType> [omit { field, ... }] [pick { field, ... }] [partial [{ field, ... }]]
    /// ```
    ///
    /// The base type is parsed via [`parse_type_expr`](Self::parse_type_expr),
    /// followed by zero or more chained type modifiers (`omit`, `pick`,
    /// `partial`). Modifiers are applied left-to-right and may be combined
    /// freely (e.g. `User omit { id } partial`).
    ///
    /// Returns `None` if the `body` keyword or base type is missing.
    fn parse_body_type(&mut self) -> Option<DerivedType> {
        let start = self.peek().span;
        self.expect(TokenKind::Body)?;
        self.skip_newlines();
        let base_type = self.parse_type_expr()?;
        let mut modifiers = Vec::new();

        // Parse chained modifiers: omit, pick, partial
        loop {
            match self.peek().kind {
                TokenKind::Omit => {
                    let mstart = self.peek().span;
                    self.advance();
                    let fields = self.parse_field_list()?;
                    let mend = self.peek().span;
                    modifiers.push(TypeModifier::Omit {
                        fields,
                        span: mstart.merge(mend),
                    });
                }
                TokenKind::Pick => {
                    let mstart = self.peek().span;
                    self.advance();
                    let fields = self.parse_field_list()?;
                    let mend = self.peek().span;
                    modifiers.push(TypeModifier::Pick {
                        fields,
                        span: mstart.merge(mend),
                    });
                }
                TokenKind::Partial => {
                    let mstart = self.peek().span;
                    self.advance();
                    // Optional field list for selective partial
                    let fields = if self.peek().kind == TokenKind::LBrace {
                        Some(self.parse_field_list()?)
                    } else {
                        None
                    };
                    let mend = self.peek().span;
                    modifiers.push(TypeModifier::Partial {
                        fields,
                        span: mstart.merge(mend),
                    });
                }
                _ => break,
            }
        }

        let end = modifiers
            .last()
            .map(|m| match m {
                TypeModifier::Omit { span, .. }
                | TypeModifier::Pick { span, .. }
                | TypeModifier::Partial { span, .. } => *span,
            })
            .unwrap_or_else(|| base_type.span());

        Some(DerivedType {
            base_type,
            modifiers,
            span: start.merge(end),
        })
    }

    /// Parses a brace-delimited, comma-or-newline-separated list of field names.
    ///
    /// Expected syntax:
    ///
    /// ```text
    /// { field1, field2, field3 }
    /// ```
    ///
    /// Used by the `omit`, `pick`, and `partial` type modifier parsers to
    /// collect the set of field names the modifier applies to.
    ///
    /// Returns `None` if the opening `{` or closing `}` is missing.
    fn parse_field_list(&mut self) -> Option<Vec<String>> {
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();
        let mut fields = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            let tok = self.expect(TokenKind::Ident)?;
            fields.push(tok.text.clone());
            self.eat(TokenKind::Comma);
            self.skip_newlines();
        }
        self.expect(TokenKind::RBrace)?;
        Some(fields)
    }

    /// Parses an `error` block inside an endpoint declaration.
    ///
    /// Expected syntax:
    ///
    /// ```text
    /// error {
    ///     NotFound(404)
    ///     Conflict(409)
    /// }
    /// ```
    ///
    /// Each variant is an identifier followed by a parenthesised integer
    /// literal representing the HTTP status code. Variants are separated by
    /// newlines.
    ///
    /// Returns an empty vector if the `error` keyword or opening `{` is
    /// missing (a diagnostic is recorded in that case).
    fn parse_error_block(&mut self) -> Vec<EndpointErrorVariant> {
        let mut errors = Vec::new();
        if self.expect(TokenKind::ErrorKw).is_none() {
            return errors;
        }
        if self.expect(TokenKind::LBrace).is_none() {
            return errors;
        }
        self.skip_newlines();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            let estart = self.peek().span;
            if let Some(name_tok) = self.expect(TokenKind::Ident)
                && self.expect(TokenKind::LParen).is_some()
                && let Some(code_tok) = self.expect(TokenKind::IntLiteral)
            {
                let code = code_tok.text.parse::<i64>().unwrap_or(0);
                let eend = self
                    .expect(TokenKind::RParen)
                    .map(|t| t.span)
                    .unwrap_or(code_tok.span);
                errors.push(EndpointErrorVariant {
                    name: name_tok.text.clone(),
                    status_code: code,
                    span: estart.merge(eend),
                });
            }
            self.skip_newlines();
        }
        let _ = self.expect(TokenKind::RBrace);
        errors
    }

    /// Parses a `query` block inside an endpoint declaration.
    ///
    /// Expected syntax:
    ///
    /// ```text
    /// query {
    ///     Int page = 1
    ///     Option<String> search
    /// }
    /// ```
    ///
    /// Each query parameter consists of a type expression, a name, and an
    /// optional default value (`= <expr>`). Parameters are separated by
    /// newlines.
    ///
    /// Returns an empty vector if the `query` keyword or opening `{` is
    /// missing (a diagnostic is recorded in that case).
    fn parse_query_block(&mut self) -> Vec<QueryParam> {
        let mut params = Vec::new();
        if self.expect(TokenKind::Query).is_none() {
            return params;
        }
        if self.expect(TokenKind::LBrace).is_none() {
            return params;
        }
        self.skip_newlines();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            let pstart = self.peek().span;
            if let Some(type_expr) = self.parse_type_expr()
                && let Some(name_tok) = self.expect(TokenKind::Ident)
            {
                let default_value = if self.eat(TokenKind::Eq) {
                    self.parse_expr()
                } else {
                    None
                };
                let pend = default_value
                    .as_ref()
                    .map(|e| e.span())
                    .unwrap_or(name_tok.span);
                params.push(QueryParam {
                    type_annotation: type_expr,
                    name: name_tok.text.clone(),
                    default_value,
                    span: pstart.merge(pend),
                });
            }
            self.skip_newlines();
        }
        let _ = self.expect(TokenKind::RBrace);
        params
    }

    // ── Schema declaration parsing ────────────────────────────────

    /// Parses a `schema` declaration: `schema name { table ... }`.
    ///
    /// The schema body contains table declarations. Table constraint bodies
    /// are parsed as opaque token sequences for forward compatibility with
    /// Phase 4.
    fn parse_schema_decl(&mut self) -> Option<SchemaDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Schema)?;

        let name_tok = self.expect(TokenKind::Ident)?;
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut tables = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            if self.peek().kind == TokenKind::Ident && self.peek().text == "table" {
                if let Some(table) = self.parse_schema_table() {
                    tables.push(table);
                }
            } else {
                self.advance(); // skip unrecognized tokens
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(SchemaDecl {
            name: name_tok.text.clone(),
            tables,
            span: start.merge(end),
        })
    }

    /// Parses a single `table` declaration within a schema block.
    ///
    /// Syntax: `table name [from TypeName] { ... }`.
    /// The body is consumed as raw token lines for forward compatibility.
    fn parse_schema_table(&mut self) -> Option<SchemaTable> {
        let start = self.peek().span;
        self.advance(); // consume "table" ident

        let name_tok = self.expect(TokenKind::Ident)?;

        // Optional `from TypeName`
        let source_type = if self.peek().kind == TokenKind::Ident && self.peek().text == "from" {
            self.advance(); // consume "from"
            Some(self.expect(TokenKind::Ident)?.text.clone())
        } else {
            None
        };

        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        // Consume body as raw token lines (opaque for forward compatibility)
        let mut body_tokens: Vec<Vec<String>> = Vec::new();
        let mut current_line: Vec<String> = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            if self.peek().kind == TokenKind::Newline {
                if !current_line.is_empty() {
                    body_tokens.push(std::mem::take(&mut current_line));
                }
                self.advance();
            } else {
                current_line.push(self.peek().text.clone());
                self.advance();
            }
        }
        if !current_line.is_empty() {
            body_tokens.push(current_line);
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(SchemaTable {
            name: name_tok.text.clone(),
            source_type,
            body_tokens,
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
                | TokenKind::Type
                | TokenKind::Endpoint
                | TokenKind::Schema
                | TokenKind::Import
                | TokenKind::Public => break,
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
    use crate::ast::*;
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
        let (program, diagnostics) =
            parse_source("function add(a: Int, b: Int) -> Int { return a }");
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
                    Statement::Expression(e) => match &e.expr {
                        Expr::If(if_expr) => {
                            assert!(if_expr.else_branch.is_some());
                        }
                        other => panic!("expected Expr::If, got {:?}", other),
                    },
                    other => panic!("expected Expression, got {:?}", other),
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
                Statement::Expression(e) => match &e.expr {
                    Expr::If(if_expr) => {
                        assert!(if_expr.else_branch.is_none(), "should have no else block");
                        assert_eq!(if_expr.then_block.statements.len(), 1);
                    }
                    other => panic!("expected Expr::If, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
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
                Statement::Expression(e) => match &e.expr {
                    Expr::If(outer) => {
                        assert_eq!(outer.then_block.statements.len(), 1);
                        match &outer.then_block.statements[0] {
                            Statement::Expression(inner_e) => match &inner_e.expr {
                                Expr::If(inner) => {
                                    assert_eq!(inner.then_block.statements.len(), 1);
                                }
                                other => panic!("expected inner Expr::If, got {:?}", other),
                            },
                            other => panic!("expected inner Expression, got {:?}", other),
                        }
                    }
                    other => panic!("expected Expr::If, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
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
        let (program, diagnostics) = parse_source("function main() { let b: Bool = !true }");
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
                    Statement::Expression(e) => match &e.expr {
                        Expr::If(if_expr) => {
                            // First condition: x == 1
                            match &if_expr.condition {
                                Expr::Binary(b) => assert_eq!(b.op, BinaryOp::Eq),
                                other => panic!("expected Binary Eq, got {:?}", other),
                            }
                            // else branch should be ElseIf
                            match &if_expr.else_branch {
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
                        other => panic!("expected Expr::If, got {:?}", other),
                    },
                    other => panic!("expected Expression, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    // ─── If as a first-class expression ──────────────────────────────────

    #[test]
    fn parse_if_expr_as_var_decl_init() {
        let source = "function main() { let x: Int = if true { 1 } else { 2 } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::If(if_expr) => {
                        assert!(if_expr.else_branch.is_some());
                    }
                    other => panic!("expected Expr::If initializer, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_if_expr_in_arithmetic() {
        let source = "function main() { let x: Int = 1 + if true { 2 } else { 3 } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::Binary(b) => {
                        assert_eq!(b.op, BinaryOp::Add);
                        match &b.right {
                            Expr::If(_) => {}
                            other => panic!("expected Expr::If on rhs, got {:?}", other),
                        }
                    }
                    other => panic!("expected Binary, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_if_expr_as_call_arg() {
        let source = "function main() { print(if true { 1 } else { 2 }) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::Expression(e) => match &e.expr {
                    Expr::Call(c) => {
                        assert_eq!(c.args.len(), 1);
                        match &c.args[0] {
                            Expr::If(_) => {}
                            other => panic!("expected Expr::If arg, got {:?}", other),
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
    fn parse_if_expr_as_function_tail() {
        // `if` in tail position yielding a value.
        let source =
            "function fib(n: Int) -> Int { if n <= 1 { n } else { fib(n - 1) + fib(n - 2) } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.body.statements.len(), 1);
                match &f.body.statements[0] {
                    Statement::Expression(e) => match &e.expr {
                        Expr::If(if_expr) => {
                            assert!(if_expr.else_branch.is_some());
                        }
                        other => panic!("expected Expr::If, got {:?}", other),
                    },
                    other => panic!("expected Expression, got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn parse_if_expr_else_if_chain_as_init() {
        let source =
            "function main() { let x: Int = if false { 1 } else if true { 2 } else { 3 } }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => match &v.initializer {
                    Expr::If(outer) => match &outer.else_branch {
                        Some(ElseBranch::ElseIf(elif)) => {
                            assert!(matches!(&elif.else_branch, Some(ElseBranch::Block(_))));
                        }
                        other => panic!("expected ElseBranch::ElseIf, got {:?}", other),
                    },
                    other => panic!("expected Expr::If, got {:?}", other),
                },
                other => panic!("expected VarDecl, got {:?}", other),
            },
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
    fn parse_compound_assignment() {
        let (program, diagnostics) =
            parse_source("function main() { let mut x: Int = 1\n x += 2 }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        // x += 2 desugars to x = x + 2, which is an Assignment expression
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[1] {
                Statement::Expression(es) => match &es.expr {
                    Expr::Assignment(a) => {
                        assert_eq!(a.name, "x");
                        match &a.value {
                            Expr::Binary(b) => assert_eq!(b.op, BinaryOp::Add),
                            other => panic!("expected Binary, got {:?}", other),
                        }
                    }
                    other => panic!("expected Assignment, got {:?}", other),
                },
                other => panic!("expected Expression, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    // ── Endpoint parsing tests ───────────────────────────────────────

    #[test]
    fn parse_endpoint_get_response_only() {
        let source = r#"endpoint getUser: GET "/api/users/{id}" {
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "getUser");
                assert_eq!(ep.method, HttpMethod::Get);
                assert_eq!(ep.path, "/api/users/{id}");
                assert!(ep.body.is_none());
                assert!(ep.query_params.is_empty());
                assert!(ep.errors.is_empty());
                assert!(ep.doc_comment.is_none());
                match &ep.response {
                    Some(TypeExpr::Named(n)) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User) response, got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_post_body_and_response() {
        let source = r#"endpoint createUser: POST "/api/users" {
            body User
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "createUser");
                assert_eq!(ep.method, HttpMethod::Post);
                assert_eq!(ep.path, "/api/users");
                let body = ep.body.as_ref().expect("should have body");
                match &body.base_type {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User) body base type, got {:?}", other),
                }
                assert!(body.modifiers.is_empty());
                match &ep.response {
                    Some(TypeExpr::Named(n)) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User) response, got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_body_omit() {
        let source = r#"endpoint createUser: POST "/api/users" {
            body User omit { id }
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                let body = ep.body.as_ref().expect("should have body");
                match &body.base_type {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User) body base type, got {:?}", other),
                }
                assert_eq!(body.modifiers.len(), 1);
                match &body.modifiers[0] {
                    TypeModifier::Omit { fields, .. } => {
                        assert_eq!(fields, &["id"]);
                    }
                    other => panic!("expected Omit modifier, got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_body_pick() {
        let source = r#"endpoint updateName: PUT "/api/users/{id}" {
            body User pick { name, email }
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.method, HttpMethod::Put);
                let body = ep.body.as_ref().expect("should have body");
                assert_eq!(body.modifiers.len(), 1);
                match &body.modifiers[0] {
                    TypeModifier::Pick { fields, .. } => {
                        assert_eq!(fields, &["name", "email"]);
                    }
                    other => panic!("expected Pick modifier, got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_body_partial_bare() {
        let source = r#"endpoint patchUser: PATCH "/api/users/{id}" {
            body User partial
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.method, HttpMethod::Patch);
                let body = ep.body.as_ref().expect("should have body");
                assert_eq!(body.modifiers.len(), 1);
                match &body.modifiers[0] {
                    TypeModifier::Partial { fields, .. } => {
                        assert!(fields.is_none(), "bare partial should have no field list");
                    }
                    other => panic!("expected Partial modifier, got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_body_chained_omit_partial() {
        let source = r#"endpoint patchUser: PATCH "/api/users/{id}" {
            body User omit { id } partial
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                let body = ep.body.as_ref().expect("should have body");
                assert_eq!(body.modifiers.len(), 2);
                match &body.modifiers[0] {
                    TypeModifier::Omit { fields, .. } => {
                        assert_eq!(fields, &["id"]);
                    }
                    other => panic!("expected Omit modifier first, got {:?}", other),
                }
                match &body.modifiers[1] {
                    TypeModifier::Partial { fields, .. } => {
                        assert!(fields.is_none(), "bare partial should have no field list");
                    }
                    other => panic!("expected Partial modifier second, got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_query_params() {
        let source = r#"endpoint listUsers: GET "/api/users" {
            query {
                Int page = 1
                String search
            }
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.query_params.len(), 2);
                assert_eq!(ep.query_params[0].name, "page");
                match &ep.query_params[0].type_annotation {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Int"),
                    other => panic!("expected Named(Int), got {:?}", other),
                }
                match &ep.query_params[0].default_value {
                    Some(Expr::Literal(Literal {
                        kind: LiteralKind::Int(1),
                        ..
                    })) => {}
                    other => panic!("expected default value Int(1), got {:?}", other),
                }
                assert_eq!(ep.query_params[1].name, "search");
                match &ep.query_params[1].type_annotation {
                    TypeExpr::Named(n) => assert_eq!(n.name, "String"),
                    other => panic!("expected Named(String), got {:?}", other),
                }
                assert!(
                    ep.query_params[1].default_value.is_none(),
                    "search should have no default"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_error_block() {
        let source = r#"endpoint getUser: GET "/api/users/{id}" {
            response User
            error {
                NotFound(404)
                Forbidden(403)
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.errors.len(), 2);
                assert_eq!(ep.errors[0].name, "NotFound");
                assert_eq!(ep.errors[0].status_code, 404);
                assert_eq!(ep.errors[1].name, "Forbidden");
                assert_eq!(ep.errors[1].status_code, 403);
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_with_doc_comment() {
        let source = r#"/** Fetches a single user by ID. */
        endpoint getUser: GET "/api/users/{id}" {
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "getUser");
                assert!(
                    ep.doc_comment.is_some(),
                    "endpoint should have a doc comment"
                );
                let doc = ep.doc_comment.as_ref().unwrap();
                assert!(
                    doc.contains("Fetches a single user by ID"),
                    "doc comment should contain expected text, got: {:?}",
                    doc
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_full() {
        let source = r#"endpoint createUser: POST "/api/users" {
            query {
                Bool verbose = false
            }
            body User omit { id }
            response User
            error {
                Conflict(409)
                BadRequest(400)
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "createUser");
                assert_eq!(ep.method, HttpMethod::Post);
                assert_eq!(ep.path, "/api/users");
                // query
                assert_eq!(ep.query_params.len(), 1);
                assert_eq!(ep.query_params[0].name, "verbose");
                // body
                let body = ep.body.as_ref().expect("should have body");
                match &body.base_type {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User) body, got {:?}", other),
                }
                assert_eq!(body.modifiers.len(), 1);
                match &body.modifiers[0] {
                    TypeModifier::Omit { fields, .. } => assert_eq!(fields, &["id"]),
                    other => panic!("expected Omit modifier, got {:?}", other),
                }
                // response
                match &ep.response {
                    Some(TypeExpr::Named(n)) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User) response, got {:?}", other),
                }
                // errors
                assert_eq!(ep.errors.len(), 2);
                assert_eq!(ep.errors[0].name, "Conflict");
                assert_eq!(ep.errors[0].status_code, 409);
                assert_eq!(ep.errors[1].name, "BadRequest");
                assert_eq!(ep.errors[1].status_code, 400);
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    // ── Endpoint error cases ─────────────────────────────────────────

    #[test]
    fn parse_endpoint_missing_http_method() {
        let source = r#"endpoint foo: "/path" { response User }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "should produce a diagnostic for missing HTTP method"
        );
    }

    #[test]
    fn parse_endpoint_missing_path() {
        let source = r#"endpoint foo: GET { response User }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "should produce a diagnostic for missing path string"
        );
    }

    #[test]
    fn parse_endpoint_body_on_get() {
        // Body on a GET endpoint should parse successfully — validation is in the
        // type checker, not the parser.
        let source = r#"endpoint getUser: GET "/api/users/{id}" {
            body User
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.method, HttpMethod::Get);
                assert!(ep.body.is_some(), "body should be parsed even on GET");
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    // ── Doc comment attachment tests ─────────────────────────────────

    #[test]
    fn parse_doc_comment_on_struct() {
        let source = "/** A 2D point. */\nstruct Point { Int x\n Int y }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.name, "Point");
                assert!(s.doc_comment.is_some(), "struct should have a doc comment");
                let doc = s.doc_comment.as_ref().unwrap();
                assert!(
                    doc.contains("A 2D point"),
                    "doc comment should contain expected text, got: {:?}",
                    doc
                );
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    #[test]
    fn parse_doc_comment_on_enum() {
        let source = "/** Primary colors. */\nenum Color { Red\n Green\n Blue }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                assert_eq!(e.name, "Color");
                assert!(e.doc_comment.is_some(), "enum should have a doc comment");
                let doc = e.doc_comment.as_ref().unwrap();
                assert!(
                    doc.contains("Primary colors"),
                    "doc comment should contain expected text, got: {:?}",
                    doc
                );
            }
            other => panic!("expected Enum, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_delete_method() {
        let source = r#"endpoint deleteUser: DELETE "/api/users/{id}" {
            response Void
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.method, HttpMethod::Delete);
                assert_eq!(ep.path, "/api/users/{id}");
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// A minimal endpoint with no body, response, query, or errors parses successfully.
    #[test]
    fn parse_endpoint_minimal_empty_body() {
        let source = r#"endpoint healthCheck: GET "/api/health" { }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "healthCheck");
                assert_eq!(ep.method, HttpMethod::Get);
                assert!(ep.body.is_none());
                assert!(ep.response.is_none());
                assert!(ep.query_params.is_empty());
                assert!(ep.errors.is_empty());
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// Body with `pick` followed by `partial` chains correctly.
    #[test]
    fn parse_endpoint_body_pick_then_partial() {
        let source = r#"
struct User { Int id  String name  String email  Int age }
endpoint updateEmail: PATCH "/api/users/{id}" {
    body User pick { name, email } partial { email }
    response User
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[1] {
            Declaration::Endpoint(ep) => {
                let body = ep.body.as_ref().unwrap();
                assert_eq!(body.modifiers.len(), 2);
                assert!(
                    matches!(&body.modifiers[0], TypeModifier::Pick { fields, .. } if fields.len() == 2)
                );
                assert!(
                    matches!(&body.modifiers[1], TypeModifier::Partial { fields: Some(f), .. } if f.len() == 1)
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// Body with selective partial (field list) parses field names correctly.
    #[test]
    fn parse_endpoint_selective_partial() {
        let source = r#"
struct User { Int id  String name  String email  Int age }
endpoint patchUser: PATCH "/api/users/{id}" {
    body User omit { id } partial { email, age }
    response User
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[1] {
            Declaration::Endpoint(ep) => {
                let body = ep.body.as_ref().unwrap();
                assert_eq!(body.modifiers.len(), 2);
                match &body.modifiers[1] {
                    TypeModifier::Partial {
                        fields: Some(names),
                        ..
                    } => {
                        assert_eq!(names, &vec!["email".to_string(), "age".to_string()]);
                    }
                    other => panic!("expected selective Partial, got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// Path with multiple parameters parses correctly.
    #[test]
    fn parse_endpoint_multiple_path_params() {
        let source = r#"
struct Comment { Int id  String text }
endpoint getComment: GET "/api/users/{userId}/posts/{postId}/comments/{commentId}" {
    response Comment
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[1] {
            Declaration::Endpoint(ep) => {
                assert_eq!(
                    ep.path,
                    "/api/users/{userId}/posts/{postId}/comments/{commentId}"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// Endpoint with only query params (no body, no response) parses.
    #[test]
    fn parse_endpoint_query_only() {
        let source = r#"endpoint search: GET "/api/search" {
    query {
        String term
        Int page = 1
    }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.query_params.len(), 2);
                assert!(ep.body.is_none());
                assert!(ep.response.is_none());
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// Endpoint with only errors (no body, no response) parses.
    #[test]
    fn parse_endpoint_errors_only() {
        let source = r#"endpoint deleteUser: DELETE "/api/users/{id}" {
    error {
        NotFound(404)
        Forbidden(403)
    }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.errors.len(), 2);
                assert!(ep.body.is_none());
                assert!(ep.response.is_none());
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// All five HTTP methods parse correctly.
    #[test]
    fn parse_endpoint_all_http_methods() {
        for (method_str, expected) in [
            ("GET", HttpMethod::Get),
            ("POST", HttpMethod::Post),
            ("PUT", HttpMethod::Put),
            ("PATCH", HttpMethod::Patch),
            ("DELETE", HttpMethod::Delete),
        ] {
            let source = format!(r#"endpoint test: {method_str} "/api/test" {{ }}"#);
            let (program, diagnostics) = parse_source(&source);
            assert!(
                diagnostics.is_empty(),
                "errors for {method_str}: {:?}",
                diagnostics
            );
            match &program.declarations[0] {
                Declaration::Endpoint(ep) => assert_eq!(ep.method, expected),
                other => panic!("expected Endpoint, got {:?}", other),
            }
        }
    }

    /// Query param with Option type parses correctly.
    #[test]
    fn parse_endpoint_query_option_type() {
        let source = r#"endpoint list: GET "/api/items" {
    query {
        Option<String> search
        Int limit = 10
    }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.query_params.len(), 2);
                assert_eq!(ep.query_params[0].name, "search");
                assert!(ep.query_params[0].default_value.is_none());
                assert_eq!(ep.query_params[1].name, "limit");
                assert!(ep.query_params[1].default_value.is_some());
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    // ── Where constraint tests ──────────────────────────────────────

    /// A struct field with a `where` constraint parses correctly.
    #[test]
    fn parse_field_with_where_constraint() {
        let source = r#"struct User {
    Int age where self >= 0 && self <= 150
    String name
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert!(
                    s.fields[0].constraint.is_some(),
                    "age should have constraint"
                );
                assert!(
                    s.fields[1].constraint.is_none(),
                    "name should have no constraint"
                );
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    /// String field with `self.length` and `self.contains` constraints.
    #[test]
    fn parse_field_with_string_constraint() {
        let source = r#"struct User {
    String email where self.contains("@") && self.length > 3
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert!(s.fields[0].constraint.is_some());
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    /// Multiple fields, some with constraints and some without.
    #[test]
    fn parse_struct_mixed_constraints() {
        let source = r#"struct User {
    Int id
    String name where self.length > 0 && self.length <= 100
    String email where self.contains("@")
    Int age where self >= 0
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert!(s.fields[0].constraint.is_none());
                assert!(s.fields[1].constraint.is_some());
                assert!(s.fields[2].constraint.is_some());
                assert!(s.fields[3].constraint.is_some());
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    /// Single constraint (no `and`).
    #[test]
    fn parse_field_single_constraint() {
        let source = "struct Item { Int price where self > 0 }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert!(s.fields[0].constraint.is_some());
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    /// `or` constraint parses.
    #[test]
    fn parse_field_or_constraint() {
        let source = "struct Range { Int x where self < 0 || self > 100 }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => assert!(s.fields[0].constraint.is_some()),
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    /// Float field with constraint.
    #[test]
    fn parse_field_float_constraint() {
        let source = "struct Item { Float price where self > 0.0 && self < 1000.0 }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => assert!(s.fields[0].constraint.is_some()),
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    // ── Schema declaration tests ────────────────────────────────────

    /// A simple schema with one table parses.
    #[test]
    fn parse_schema_basic() {
        let source = r#"
struct User { Int id  String name }
schema db {
    table users from User {
        primary key id
        unique email
    }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[1] {
            Declaration::Schema(s) => {
                assert_eq!(s.name, "db");
                assert_eq!(s.tables.len(), 1);
                assert_eq!(s.tables[0].name, "users");
                assert_eq!(s.tables[0].source_type, Some("User".to_string()));
                assert!(!s.tables[0].body_tokens.is_empty());
            }
            other => panic!("expected Schema, got {:?}", other),
        }
    }

    /// Schema with multiple tables parses.
    #[test]
    fn parse_schema_multiple_tables() {
        let source = r#"
schema db {
    table users from User {
        primary key id
    }
    table posts from Post {
        primary key id
        foreign key authorId references users(id)
    }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Schema(s) => {
                assert_eq!(s.tables.len(), 2);
                assert_eq!(s.tables[0].name, "users");
                assert_eq!(s.tables[1].name, "posts");
            }
            other => panic!("expected Schema, got {:?}", other),
        }
    }

    /// Schema with standalone table (no `from` clause) parses.
    #[test]
    fn parse_schema_standalone_table() {
        let source = r#"
schema db {
    table sessions {
        String token primary key
        Int userId
    }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Schema(s) => {
                assert_eq!(s.tables[0].name, "sessions");
                assert!(s.tables[0].source_type.is_none());
            }
            other => panic!("expected Schema, got {:?}", other),
        }
    }

    /// Empty schema parses.
    #[test]
    fn parse_schema_empty() {
        let source = "schema db { }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Schema(s) => {
                assert_eq!(s.name, "db");
                assert!(s.tables.is_empty());
            }
            other => panic!("expected Schema, got {:?}", other),
        }
    }

    /// Schema coexists with structs and endpoints without parse errors.
    #[test]
    fn parse_schema_alongside_other_declarations() {
        let source = r#"
struct User { Int id  String name }
endpoint getUser: GET "/api/users/{id}" {
    response User
}
schema db {
    table users from User {
        primary key id
    }
}
"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 3);
        assert!(matches!(&program.declarations[0], Declaration::Struct(_)));
        assert!(matches!(&program.declarations[1], Declaration::Endpoint(_)));
        assert!(matches!(&program.declarations[2], Declaration::Schema(_)));
    }

    /// Table with empty body parses.
    #[test]
    fn parse_schema_table_empty_body() {
        let source = r#"
schema db {
    table users from User { }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Schema(s) => {
                assert_eq!(s.tables[0].name, "users");
                assert_eq!(s.tables[0].source_type, Some("User".to_string()));
                assert!(s.tables[0].body_tokens.is_empty());
            }
            other => panic!("expected Schema, got {:?}", other),
        }
    }

    /// Body tokens are correctly captured as separate token strings per line.
    #[test]
    fn parse_schema_body_tokens_content() {
        let source = r#"
schema db {
    table users from User {
        primary key id
        unique email
    }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Schema(s) => {
                let tokens = &s.tables[0].body_tokens;
                assert_eq!(tokens.len(), 2, "should have 2 constraint lines");
                assert_eq!(tokens[0], vec!["primary", "key", "id"]);
                assert_eq!(tokens[1], vec!["unique", "email"]);
            }
            other => panic!("expected Schema, got {:?}", other),
        }
    }

    /// Complex constraint syntax (foreign key with cascade) is captured.
    #[test]
    fn parse_schema_complex_constraints() {
        let source = r#"
schema db {
    table posts from Post {
        primary key id
        foreign key authorId references users(id) on delete cascade
    }
}"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Schema(s) => {
                let tokens = &s.tables[0].body_tokens;
                assert_eq!(tokens.len(), 2);
                // First line: primary key id
                assert_eq!(tokens[0], vec!["primary", "key", "id"]);
                // Second line: foreign key authorId references users(id) on delete cascade
                assert!(
                    tokens[1].len() >= 5,
                    "complex constraint should have multiple tokens"
                );
                assert_eq!(tokens[1][0], "foreign");
            }
            other => panic!("expected Schema, got {:?}", other),
        }
    }

    // ── Duplicate section tests ────────────────────────────────────────

    #[test]
    fn parse_endpoint_duplicate_body() {
        let source = r#"endpoint createUser: POST "/api/users" {
            body User
            body User
            response User
        }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate `body`")),
            "should report duplicate body: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_endpoint_duplicate_response() {
        let source = r#"endpoint getUser: GET "/api/users/{id}" {
            response User
            response User
        }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate `response`")),
            "should report duplicate response: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_endpoint_duplicate_query() {
        let source = r#"endpoint listUsers: GET "/api/users" {
            query { Int page = 1 }
            query { Int limit = 20 }
        }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate `query`")),
            "should report duplicate query: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_endpoint_duplicate_error() {
        let source = r#"endpoint createUser: POST "/api/users" {
            body User
            error { BadRequest(400) }
            error { Conflict(409) }
        }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate `error`")),
            "should report duplicate error: {:?}",
            diagnostics
        );
    }

    // ── Field doc comment tests ───────────────────────────────────────

    #[test]
    fn parse_struct_field_doc_comment() {
        let source = r#"struct User {
            /** The user's full name */
            String name
            Int age
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.fields.len(), 2);
                assert_eq!(
                    s.fields[0].doc_comment.as_deref(),
                    Some("The user's full name")
                );
                assert!(s.fields[1].doc_comment.is_none());
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    #[test]
    fn parse_struct_multiple_field_doc_comments() {
        let source = r#"struct User {
            /** Unique identifier */
            Int id
            /** Display name */
            String name
            Int age
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.fields.len(), 3);
                assert_eq!(
                    s.fields[0].doc_comment.as_deref(),
                    Some("Unique identifier")
                );
                assert_eq!(s.fields[1].doc_comment.as_deref(), Some("Display name"));
                assert!(s.fields[2].doc_comment.is_none());
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    // ── Multiline field list in omit/pick ─────────────────────────────

    #[test]
    fn parse_endpoint_multiline_omit() {
        let source = r#"endpoint createUser: POST "/api/users" {
            body User omit {
                id,
                createdAt,
            }
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                let body = ep.body.as_ref().expect("should have body");
                match &body.modifiers[0] {
                    TypeModifier::Omit { fields, .. } => {
                        assert_eq!(fields, &["id", "createdAt"]);
                    }
                    other => panic!("expected Omit, got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    // ── `dyn Trait` integration smoke tests ────────────────────────────
    //
    // The unit tests in `crates/phoenix-parser/src/types.rs` cover the
    // core `parse_type_expr` recursion points (bare, generic arg,
    // function param/return). These integration tests exercise the
    // declaration-level productions that *route into* `parse_type_expr`,
    // pinning that `dyn` is reachable wherever the design promises it
    // (see `docs/dyn-trait.md` "When the wrap happens").

    #[test]
    fn parse_dyn_in_struct_field() {
        let (program, diagnostics) = parse_source("struct Scene { dyn Drawable hero }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.fields.len(), 1);
                assert_eq!(s.fields[0].name, "hero");
                match &s.fields[0].type_annotation {
                    TypeExpr::Dyn(d) => assert_eq!(d.trait_name, "Drawable"),
                    other => panic!("expected TypeExpr::Dyn, got {:?}", other),
                }
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    #[test]
    fn parse_dyn_in_enum_variant_field() {
        let (program, diagnostics) = parse_source("enum Slot { Held(dyn Drawable)\n Empty }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                assert_eq!(e.variants.len(), 2);
                assert_eq!(e.variants[0].name, "Held");
                assert_eq!(e.variants[0].fields.len(), 1);
                match &e.variants[0].fields[0] {
                    TypeExpr::Dyn(d) => assert_eq!(d.trait_name, "Drawable"),
                    other => panic!("expected TypeExpr::Dyn, got {:?}", other),
                }
            }
            other => panic!("expected Enum, got {:?}", other),
        }
    }

    #[test]
    fn parse_dyn_in_let_annotation() {
        let (program, diagnostics) =
            parse_source("function main() { let d: dyn Drawable = Circle(1) }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match &f.body.statements[0] {
                Statement::VarDecl(v) => {
                    let ann = v
                        .type_annotation
                        .as_ref()
                        .expect("let must carry a `dyn` annotation");
                    match ann {
                        TypeExpr::Dyn(d) => assert_eq!(d.trait_name, "Drawable"),
                        other => panic!("expected TypeExpr::Dyn, got {:?}", other),
                    }
                }
                other => panic!("expected VarDecl, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    #[test]
    fn parse_dyn_in_function_return_annotation() {
        // Already covered as a unit test on `parse_type_expr`, but this
        // pins that the *function-decl* production hands its return
        // type through `parse_type_expr` rather than a parallel path.
        let (program, diagnostics) =
            parse_source("function choose() -> dyn Drawable { return Circle(1) }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => match f
                .return_type
                .as_ref()
                .expect("function must have return type")
            {
                TypeExpr::Dyn(d) => assert_eq!(d.trait_name, "Drawable"),
                other => panic!("expected TypeExpr::Dyn, got {:?}", other),
            },
            other => panic!("expected Function, got {:?}", other),
        }
    }

    /// Named-argument syntax on method calls is not supported today
    /// (`MethodCallExpr` carries only positional `args`).  IR lowering
    /// and sema both assume positional-only here, so a future parser
    /// change that silently admits `c.bump(by: 5)` would flow through
    /// paths that ignore the name.  This test pins the parse error so
    /// the ambiguity must be resolved deliberately when named-args on
    /// methods lands.
    #[test]
    fn parse_named_arg_on_method_call_is_rejected() {
        let (_, diagnostics) =
            parse_source("function main() { let c: Counter = Counter(0); c.bump(by: 5) }");
        assert!(
            !diagnostics.is_empty(),
            "expected a parse error for `c.bump(by: 5)`, got none — if named-args on \
             methods were intentionally enabled, update `MethodCallExpr` + \
             `merge_method_call_args` to thread names before removing this test",
        );
    }

    // ── import + visibility tests ────────────────────────────

    #[test]
    fn parse_import_named() {
        let (program, diagnostics) =
            parse_source("import models.user { User, createUser }\nfunction main() {}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Import(imp) => {
                assert_eq!(imp.path, vec!["models", "user"]);
                match &imp.items {
                    ImportItems::Named(items) => {
                        assert_eq!(items.len(), 2);
                        assert_eq!(items[0].name, "User");
                        assert!(items[0].alias.is_none());
                        assert_eq!(items[1].name, "createUser");
                    }
                    other => panic!("expected Named items, got {:?}", other),
                }
            }
            other => panic!("expected Import decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_with_alias() {
        let (program, diagnostics) =
            parse_source("import models.user { User as UserModel }\nfunction main() {}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Import(imp) => match &imp.items {
                ImportItems::Named(items) => {
                    assert_eq!(items[0].name, "User");
                    assert_eq!(items[0].alias.as_deref(), Some("UserModel"));
                }
                other => panic!("expected Named items, got {:?}", other),
            },
            other => panic!("expected Import decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_wildcard() {
        let (program, diagnostics) = parse_source("import models.user { * }\nfunction main() {}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Import(imp) => match &imp.items {
                ImportItems::Wildcard => {}
                other => panic!("expected Wildcard items, got {:?}", other),
            },
            other => panic!("expected Import decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_single_segment_path() {
        let (program, diagnostics) = parse_source("import helpers { add }\nfunction main() {}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Import(imp) => assert_eq!(imp.path, vec!["helpers"]),
            other => panic!("expected Import decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_public_function() {
        let (program, diagnostics) =
            parse_source("public function add(a: Int, b: Int) -> Int { a + b }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => assert_eq!(f.visibility, Visibility::Public),
            other => panic!("expected Function decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_private_function_default() {
        let (program, _) = parse_source("function add(a: Int, b: Int) -> Int { a + b }");
        match &program.declarations[0] {
            Declaration::Function(f) => assert_eq!(f.visibility, Visibility::Private),
            other => panic!("expected Function decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_public_struct_with_field_visibilities() {
        let src = "public struct User {
            public String name
            String passwordHash
        }";
        let (program, diagnostics) = parse_source(src);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.visibility, Visibility::Public);
                assert_eq!(s.fields[0].name, "name");
                assert_eq!(s.fields[0].visibility, Visibility::Public);
                assert_eq!(s.fields[1].name, "passwordHash");
                assert_eq!(s.fields[1].visibility, Visibility::Private);
            }
            other => panic!("expected Struct decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_public_enum_trait_alias() {
        let src = "public enum Color { Red Green Blue }
public trait Display { function show(self) -> String }
public type UserId = Int";
        let (program, diagnostics) = parse_source(src);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Enum(e) => assert_eq!(e.visibility, Visibility::Public),
            other => panic!("expected Enum, got {:?}", other),
        }
        match &program.declarations[1] {
            Declaration::Trait(t) => assert_eq!(t.visibility, Visibility::Public),
            other => panic!("expected Trait, got {:?}", other),
        }
        match &program.declarations[2] {
            Declaration::TypeAlias(a) => assert_eq!(a.visibility, Visibility::Public),
            other => panic!("expected TypeAlias, got {:?}", other),
        }
    }

    #[test]
    fn public_on_impl_block_rejected() {
        let (_, diagnostics) =
            parse_source("struct P { Int x }\npublic impl P { function f(self) {} }");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("`public` cannot precede `impl`")),
            "expected diagnostic about `public impl`, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn public_on_import_rejected() {
        let (_, diagnostics) = parse_source("public import a.b { Foo }\nfunction main() {}");
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("`import` declarations cannot be marked `public`")),
            "expected diagnostic about `public import`, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn import_empty_list_rejected() {
        let (_, diagnostics) = parse_source("import a.b { }\nfunction main() {}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("import list cannot be empty")),
            "expected diagnostic about empty import list, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn import_missing_path_recovers() {
        // `import { Foo }` — no module path.  Parser should error and not panic.
        let (_, diagnostics) = parse_source("import { Foo }\nfunction main() {}");
        assert!(!diagnostics.is_empty(), "expected at least one diagnostic");
    }

    #[test]
    fn import_unterminated_brace_recovers() {
        // `import a.b {` — unterminated.  Synchronization should recover at the
        // following `function` keyword without panicking.
        let (_, diagnostics) = parse_source("import a.b {\nfunction main() {}");
        assert!(!diagnostics.is_empty(), "expected at least one diagnostic");
    }

    #[test]
    fn import_double_comma_recovers() {
        let (_, diagnostics) = parse_source("import a.b { Foo,, Bar }\nfunction main() {}");
        assert!(!diagnostics.is_empty(), "expected at least one diagnostic");
    }

    #[test]
    fn malformed_import_does_not_swallow_following_import() {
        // Synchronize must stop at the next `import`, not skip past it.
        let src = "import { Foo }\nimport b.c { Bar }\nfunction main() {}";
        let (program, _diagnostics) = parse_source(src);
        // The second, well-formed import should have parsed successfully.
        let import_count = program
            .declarations
            .iter()
            .filter(|d| matches!(d, Declaration::Import(_)))
            .count();
        assert!(
            import_count >= 1,
            "expected at least one Import decl from recovery; got {} decls: {:?}",
            program.declarations.len(),
            program.declarations
        );
    }

    #[test]
    fn parse_import_named_trailing_comma_allowed() {
        // `import a.b { Foo, Bar, }` — trailing comma in the named-list form
        // is accepted (consistent with other comma-separated lists in the
        // language).
        let (program, diagnostics) = parse_source("import a.b { Foo, Bar, }\nfunction main() {}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Import(imp) => match &imp.items {
                ImportItems::Named(items) => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items[0].name, "Foo");
                    assert_eq!(items[1].name, "Bar");
                }
                other => panic!("expected Named items, got {:?}", other),
            },
            other => panic!("expected Import decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_wildcard_then_extra_token_is_rejected() {
        // `import a.b { *, Foo }` — wildcard cannot be combined with named
        // items. After eating `*`, the parser expects `}`, so the trailing
        // `, Foo }` triggers a diagnostic (the `,` is not the expected
        // closing brace).
        let (_, diagnostics) = parse_source("import a.b { *, Foo }\nfunction main() {}");
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for `*, Foo`; got none"
        );
    }

    #[test]
    fn double_public_modifier_rejected() {
        // `public public function foo()` — only one visibility modifier is
        // allowed. The first `public` is consumed by `parse_declaration`;
        // the second is then in the declaration-keyword position and the
        // parser must reject it (no recovery beyond emitting a diagnostic).
        let (_, diagnostics) = parse_source("public public function foo() {}");
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for `public public function`; got none"
        );
    }

    #[test]
    fn parse_public_method_in_struct_body() {
        let src = "struct Counter {
            Int n
            public function bump(self) -> Int { self.n + 1 }
            function reset(self) { }
        }";
        let (program, diagnostics) = parse_source(src);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.methods.len(), 2);
                assert_eq!(s.methods[0].name, "bump");
                assert_eq!(s.methods[0].visibility, Visibility::Public);
                assert_eq!(s.methods[1].name, "reset");
                assert_eq!(s.methods[1].visibility, Visibility::Private);
            }
            other => panic!("expected Struct decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_public_method_in_enum_body() {
        let src = "enum Color {
            Red Green Blue
            public function name(self) -> String { \"red\" }
            function isPrimary(self) -> Bool { true }
        }";
        let (program, diagnostics) = parse_source(src);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                assert_eq!(e.methods.len(), 2);
                assert_eq!(e.methods[0].name, "name");
                assert_eq!(e.methods[0].visibility, Visibility::Public);
                assert_eq!(e.methods[1].name, "isPrimary");
                assert_eq!(e.methods[1].visibility, Visibility::Private);
            }
            other => panic!("expected Enum decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_public_method_in_inherent_impl_block() {
        let src = "struct P { Int x }\nimpl P {
            public function get(self) -> Int { self.x }
            function helper(self) -> Int { self.x + 1 }
        }";
        let (program, diagnostics) = parse_source(src);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[1] {
            Declaration::Impl(i) => {
                assert!(i.trait_name.is_none(), "expected inherent impl");
                assert_eq!(i.methods.len(), 2);
                assert_eq!(i.methods[0].name, "get");
                assert_eq!(i.methods[0].visibility, Visibility::Public);
                assert_eq!(i.methods[1].name, "helper");
                assert_eq!(i.methods[1].visibility, Visibility::Private);
            }
            other => panic!("expected Impl decl, got {:?}", other),
        }
    }

    #[test]
    fn public_method_in_trait_impl_rejected() {
        let src = "struct P { Int x }
trait T { function f(self) -> Int }
impl T for P {
    public function f(self) -> Int { self.x }
}";
        let (_, diagnostics) = parse_source(src);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("trait `impl` block")),
            "expected diagnostic about `public` in trait impl, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn public_method_in_inline_trait_impl_rejected() {
        let src = "trait Greet { function hello(self) -> String }
struct P { Int x
    impl Greet {
        public function hello(self) -> String { \"hi\" }
    }
}";
        let (_, diagnostics) = parse_source(src);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("trait `impl` block")),
            "expected diagnostic about `public` in inline trait impl, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn public_on_inline_impl_in_struct_body_rejected() {
        // `public impl Trait { ... }` inside a struct body is not valid —
        // inline trait impls do not carry visibility (the trait does).
        let src = "trait T { function f(self) }
struct P { Int x
    public impl T { function f(self) {} }
}";
        let (_, diagnostics) = parse_source(src);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("`public` cannot precede inline `impl`")),
            "expected diagnostic about `public impl`, got: {:?}",
            diagnostics
        );
    }
}
