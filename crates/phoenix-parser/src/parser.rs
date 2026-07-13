use crate::api_version::validate_api_version;
use crate::ast::{
    Annotation, AnnotationArg, Block, Declaration, DerivedType, EndpointDecl, EndpointErrorVariant,
    EnumDecl, EnumVariant, ExternFnSig, ExternJsBlock, FieldDecl, FunctionDecl, HeaderParam,
    HttpMethod, ImplBlock, ImportDecl, ImportItem, ImportItems, InlineTraitImpl, NamedType,
    PaginationMode, Param, Program, QueryParam, ResponseStatus, SchemaDecl, SchemaTable,
    StructDecl, TraitDecl, TraitMethodSig, TypeAliasDecl, TypeExpr, TypeModifier, Visibility,
};
use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::span::{SourceId, Span};
use phoenix_lexer::token::{Token, TokenKind};

/// Strips the single surrounding double-quote pair from a string-literal token's
/// raw text (e.g. `"X-Request-Id"` → `X-Request-Id`).
///
/// Uses `strip_prefix`/`strip_suffix` rather than `trim_matches('"')` so a value
/// that legitimately ends in an escaped quote (`"say \""` → `say \"`) is not
/// over-trimmed: `trim_matches` would greedily eat the trailing escaped quote
/// too. Inner escape sequences are otherwise left intact (the lexer keeps them).
fn strip_string_literal_quotes(text: &str) -> String {
    text.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text)
        .to_string()
}

/// Why an `extern js` module specifier is rejected, if it is —
/// `None` means the specifier is usable. Best-effort syntax hygiene, not npm
/// validation (that lands with `[js-dependencies]`): each arm rejects a string
/// that would otherwise flow downstream verbatim and misbehave far from the
/// mistake.
fn extern_module_specifier_error(specifier: &str) -> Option<&'static str> {
    if specifier.is_empty() {
        // Names no module.
        Some(
            "`extern js` module specifier must not be empty — omit the string for the \
             ambient `js` host, or name an npm package",
        )
    } else if specifier == "js" {
        // The ambient host's module name: accepting it would be a silent alias
        // for the no-specifier form that reads as an npm package named `js`
        // downstream.
        Some(
            "`\"js\"` is not a module specifier — it names the ambient `js` host; omit \
             the string (`extern js { ... }`) to bind browser/Node globals",
        )
    } else if specifier == "wasi_snapshot_preview1" {
        // The WASM import namespace the runtime's WASI shim owns. An extern
        // namespace would clobber it in the generated glue's imports object (a
        // duplicate object key, later wins), breaking instantiation far from
        // the mistake.
        Some(
            "`\"wasi_snapshot_preview1\"` is not a module specifier — that import \
             namespace is reserved for the Phoenix runtime's WASI shim; name an npm \
             package",
        )
    } else if specifier.contains('\\') || specifier.contains(char::is_whitespace) {
        // No npm specifier has whitespace or a backslash, and the raw text
        // keeps escape sequences intact (`strip_string_literal_quotes` strips
        // only the quotes) — so an escaped spelling like `"left\u002Dpad"`
        // would never match the package it means. Rejecting backslashes also
        // keeps an escaped spelling of a reserved specifier from sneaking past
        // the arms above.
        Some(
            "`extern js` module specifier must not contain whitespace or escape \
             sequences — write the npm package specifier literally (e.g. \"left-pad\", \
             \"@scope/pkg\")",
        )
    } else if specifier.contains(['{', '}']) {
        // `{name}` is string interpolation in expression position, but the
        // specifier is taken raw, so `extern js "{pkg}"` would silently bind a
        // literal `{pkg}` namespace no package can ever match (braces are
        // invalid in npm specifiers anyway).
        Some(
            "`extern js` module specifier must not contain `{` or `}` — string \
             interpolation is not supported here; write the npm package specifier \
             literally (e.g. \"left-pad\", \"@scope/pkg\")",
        )
    } else {
        None
    }
}

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
        TokenKind::Headers => "'headers'",
        TokenKind::Pagination => "'pagination'",
        TokenKind::Where => "'where'",
        TokenKind::Schema => "'schema'",
        TokenKind::Api => "'api'",
        TokenKind::Get => "'GET'",
        TokenKind::Post => "'POST'",
        TokenKind::Put => "'PUT'",
        TokenKind::Patch => "'PATCH'",
        TokenKind::Delete => "'DELETE'",
        TokenKind::DocComment => "doc comment",
        TokenKind::At => "'@'",
        TokenKind::IntType => "Int",
        TokenKind::FloatType => "Float",
        TokenKind::StringType => "String",
        TokenKind::BoolType => "Bool",
        TokenKind::FileType => "File",
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
        TokenKind::Extern => "'extern'",
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
            // An `api version "..." { ... }` block fans out to MULTIPLE
            // top-level endpoint declarations (one per endpoint inside it),
            // each tagged with the block's version. `parse_declaration`
            // returns a single `Declaration`, so we detect and handle the
            // block here at the loop level (least disruptive: every other
            // path still goes through `parse_declaration`). A leading doc
            // comment / `public` before `api` is handled inside the block
            // parser via a peek; here we only need to detect `api` after an
            // optional doc comment.
            if self.is_at_api_version_block() {
                match self.parse_api_version_block() {
                    Some(endpoints) => {
                        declarations.extend(endpoints.into_iter().map(Declaration::Endpoint));
                    }
                    None => self.synchronize(),
                }
                self.skip_newlines();
                continue;
            }
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

    /// Returns `true` if the parser is positioned at the start of an
    /// `api version "..." { ... }` block, looking past an optional leading
    /// doc comment (and any newlines after it) and an optional `public`
    /// modifier. Used by `parse_program` to route to `parse_api_version_block`,
    /// which fans the block out into multiple top-level endpoint declarations.
    fn is_at_api_version_block(&self) -> bool {
        let mut offset = 0;
        // Skip a leading doc comment and any following newlines.
        if self.peek_at(offset).kind == TokenKind::DocComment {
            offset += 1;
            while self.peek_at(offset).kind == TokenKind::Newline {
                offset += 1;
            }
        }
        // Skip an (invalid but recoverable) `public` modifier; the block
        // parser reports the error.
        if self.peek_at(offset).kind == TokenKind::Public {
            offset += 1;
        }
        self.peek_at(offset).kind == TokenKind::Api
    }

    /// Parses an `api version "<string>" { <endpoint decls> }` block and
    /// returns each contained endpoint, tagged with the block's version via
    /// `EndpointDecl::api_version`. The block is flattened into top-level
    /// endpoint declarations by the caller (`parse_program`) so downstream
    /// consumers keep seeing a flat list — see the architecture note there.
    fn parse_api_version_block(&mut self) -> Option<Vec<EndpointDecl>> {
        // A doc comment may precede the whole block; it does not attach to any
        // single endpoint, so we consume and discard it for now.
        let _block_doc = self.try_consume_doc_comment();

        // `public` cannot precede `api` — mirror the endpoint/schema pattern.
        let public_span = if self.peek().kind == TokenKind::Public {
            let span = self.peek().span;
            self.advance();
            Some(span)
        } else {
            None
        };
        self.reject_public_modifier(public_span, "`public` cannot precede `api`");

        self.expect(TokenKind::Api)?;

        // Track header validity across the (contextual) `version` keyword and
        // the version string. A malformed header yields at most ONE diagnostic
        // and, crucially, NEVER early-returns: the `{ ... }` block still
        // follows and must be fully consumed, else its endpoints would be
        // re-parsed as top-level (unversioned) declarations and its closing `}`
        // would cascade into a spurious error. A rejected header parses the
        // body normally but drops its endpoints (tagging nothing).
        let mut version_valid = true;

        // `version` is a contextual identifier (not a reserved keyword), so we
        // match it by text rather than by token kind. The match is
        // case-sensitive, so spell out the expected lowercase form to steer a
        // user who wrote `Version`/`VERSION`.
        if self.peek().kind == TokenKind::Ident && self.peek().text == "version" {
            self.advance();
        } else {
            self.error_at_current("expected the keyword `version` (lowercase) after `api`");
            version_valid = false;
        }

        // The version string literal (raw — sema normalizes/prefixes later).
        // Validate it here, at the block header, where the block is still a
        // single construct — so a bad version produces exactly ONE diagnostic
        // rather than one per flattened endpoint. The path-safety rules live in
        // `api_version::validate_api_version`, shared with sema's prefixing so
        // the two can't drift apart. We only validate (and only report a
        // missing string) when the header is otherwise well-formed, so a
        // malformed header stays at a single diagnostic.
        let version = if self.peek().kind == TokenKind::StringLiteral {
            let version_token = self.advance();
            let version_span = version_token.span;
            let version = strip_string_literal_quotes(&version_token.text);
            if version_valid && let Err(message) = validate_api_version(&version) {
                self.diagnostics
                    .push(Diagnostic::error(message, version_span));
                version_valid = false;
            }
            version
        } else {
            if version_valid {
                self.error_at_current("expected a version string after `version`");
                version_valid = false;
            }
            String::new()
        };

        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        // The block contains ONLY endpoint declarations.
        let mut endpoints = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::RBrace | TokenKind::Eof => break,
                _ => {
                    // Endpoints inside the block may carry doc comments too.
                    let doc_comment = self.try_consume_doc_comment();
                    if self.peek().kind != TokenKind::Endpoint {
                        self.error_at_current(
                            "`api version` block may contain only endpoint declarations",
                        );
                        self.recover_to_next_block_item();
                        self.skip_newlines();
                        continue;
                    }
                    match self.parse_endpoint_decl(doc_comment) {
                        // Parse the endpoint to consume it, but drop it when the
                        // version header was rejected — the block is already an
                        // error, and emitting unversioned/spuriously-routed
                        // endpoints from it would only add noise.
                        Some(mut endpoint) if version_valid => {
                            endpoint.api_version = Some(version.clone());
                            endpoints.push(endpoint);
                        }
                        Some(_) => {}
                        None => {
                            // The endpoint failed mid-parse; its own diagnostic
                            // was already emitted. Recover to the next block
                            // item rather than aborting the whole block, so a
                            // following endpoint is still parsed and the block's
                            // closing `}` is consumed normally instead of
                            // cascading into spurious top-level errors.
                            self.recover_to_next_block_item();
                        }
                    }
                }
            }
            self.skip_newlines();
        }

        self.expect(TokenKind::RBrace)?;
        Some(endpoints)
    }

    /// Skips a malformed (non-endpoint) declaration inside an `api version`
    /// block, stopping at the next plausible item boundary: a top-level
    /// `endpoint`, a doc comment, the block's own closing `}`, or EOF. Brace
    /// depth is tracked so a skipped declaration's own `{ ... }` body does not
    /// prematurely terminate the block (otherwise `struct Foo { ... }` would
    /// leave the block's real `}` to cascade into spurious top-level errors).
    fn recover_to_next_block_item(&mut self) {
        let mut depth: usize = 0;
        loop {
            match self.peek().kind {
                TokenKind::Eof => break,
                TokenKind::LBrace => {
                    depth += 1;
                    self.advance();
                }
                TokenKind::RBrace => {
                    if depth == 0 {
                        // The block's own closing brace — stop before it.
                        break;
                    }
                    depth -= 1;
                    self.advance();
                }
                TokenKind::Endpoint | TokenKind::DocComment if depth == 0 => break,
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// Parses a top-level declaration, optionally preceded by a doc comment
    /// and an optional `public` visibility modifier.
    fn parse_declaration(&mut self) -> Option<Declaration> {
        // Consume a doc comment if present — it attaches to the next declaration.
        let doc_comment = self.try_consume_doc_comment();

        // Consume leading annotations (`@name` / `@name(args)`). They attach to
        // the declaration kinds that carry an `annotations` field (function,
        // struct, enum); other kinds reject them below.
        let annotations = self.parse_annotations();

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
            TokenKind::Function => self.parse_function_decl(visibility).map(|mut f| {
                f.annotations = annotations;
                Declaration::Function(f)
            }),
            TokenKind::Struct => self
                .parse_struct_decl(doc_comment, visibility)
                .map(|mut s| {
                    s.annotations = annotations;
                    Declaration::Struct(s)
                }),
            TokenKind::Enum => self.parse_enum_decl(doc_comment, visibility).map(|mut e| {
                e.annotations = annotations;
                Declaration::Enum(e)
            }),
            TokenKind::Impl => {
                self.reject_public_modifier(
                    public_span,
                    "`public` cannot precede `impl` — impl visibility is derived from the trait and the type",
                );
                self.reject_annotations(&annotations, "annotations cannot precede `impl`");
                self.parse_impl_block().map(Declaration::Impl)
            }
            TokenKind::Trait => {
                self.reject_annotations(&annotations, "annotations cannot precede `trait`");
                self.parse_trait_decl(visibility).map(Declaration::Trait)
            }
            TokenKind::Type => {
                self.reject_annotations(&annotations, "annotations cannot precede `type`");
                self.parse_type_alias_decl(visibility)
                    .map(Declaration::TypeAlias)
            }
            TokenKind::Endpoint => {
                self.reject_public_modifier(public_span, "`public` cannot precede `endpoint`");
                self.reject_annotations(&annotations, "annotations cannot precede `endpoint`");
                self.parse_endpoint_decl(doc_comment)
                    .map(Declaration::Endpoint)
            }
            TokenKind::Schema => {
                self.reject_public_modifier(public_span, "`public` cannot precede `schema`");
                self.reject_annotations(&annotations, "annotations cannot precede `schema`");
                self.parse_schema_decl().map(Declaration::Schema)
            }
            TokenKind::Import => {
                self.reject_public_modifier(
                    public_span,
                    "`import` declarations cannot be marked `public`",
                );
                self.reject_annotations(&annotations, "annotations cannot precede `import`");
                self.parse_import_decl().map(Declaration::Import)
            }
            TokenKind::Extern => {
                self.reject_public_modifier(
                    public_span,
                    "`extern js` blocks cannot be marked `public` — the declared functions are external, not Phoenix declarations",
                );
                self.reject_annotations(
                    &annotations,
                    "annotations cannot precede `extern js` blocks",
                );
                self.parse_extern_js_block().map(Declaration::ExternJs)
            }
            _ => {
                self.error_at_current("expected a declaration (e.g. `function`, `struct`, `enum`, `impl`, `trait`, `type`, `endpoint`, `schema`, `import`, `extern js`)");
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

    /// Emit a diagnostic at the first annotation's span if annotations preceded
    /// a declaration kind that does not carry an `annotations` field. Called
    /// from the declaration-kind arms that don't accept annotations.
    fn reject_annotations(&mut self, annotations: &[Annotation], message: &'static str) {
        if let Some(first) = annotations.first() {
            self.diagnostics
                .push(Diagnostic::error(message, first.span));
        }
    }

    /// Emit a diagnostic if a doc comment or annotations were consumed before a
    /// struct- or enum-body member that does not carry them (a method or an
    /// inline `impl`). Phoenix attaches doc comments and annotations to fields
    /// and to top-level declarations, not to members nested in a type body.
    /// `member` names the offending construct ("a method"). The annotation span
    /// is preferred; the doc-comment span is the fallback. The member itself is
    /// still parsed by the caller, so only the misplaced metadata is dropped.
    fn reject_member_metadata(
        &mut self,
        doc_span: Option<Span>,
        annotations: &[Annotation],
        member: &str,
    ) {
        if let Some(first) = annotations.first() {
            self.diagnostics.push(Diagnostic::error(
                format!("annotations cannot precede {member}"),
                first.span,
            ));
        } else if let Some(span) = doc_span {
            self.diagnostics.push(Diagnostic::error(
                format!("a doc comment cannot precede {member}"),
                span,
            ));
        }
    }

    /// Parses an `import` declaration:
    /// `import a.b.c { Foo, Bar as Baz }`, `import a.b.c { * }`, or the
    /// namespace forms `import a.b.c` / `import a.b.c as d` (no braces).
    fn parse_import_decl(&mut self) -> Option<ImportDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Import)?;

        // Dotted module path: IDENT (. IDENT)*
        let mut path = Vec::new();
        let first = self.expect(TokenKind::Ident)?;
        path.push(first.text.clone());
        let mut path_end = first.span;
        while self.eat(TokenKind::Dot) {
            let segment = self.expect(TokenKind::Ident)?;
            path.push(segment.text.clone());
            path_end = segment.span;
        }

        // Namespace import: no `{ ... }` follows the path. Bind the module
        // itself under its last segment (or an explicit `as` alias).
        if self.peek().kind != TokenKind::LBrace {
            // Catch the common mistake of writing the import list on the line
            // *after* the path (`import a.b⏎{ Foo }`). The brace must hug the
            // path on the same line; without this guard the path alone would
            // parse as a namespace import and the `{ ... }` would be
            // mis-parsed as a separate declaration, yielding a misleading
            // "unexpected `{`" error one line down. A stray `as` alias is not
            // checked here — that author clearly meant a namespace import.
            if self.peek().kind == TokenKind::Newline {
                let mut ahead = 1;
                while self.peek_at(ahead).kind == TokenKind::Newline {
                    ahead += 1;
                }
                if self.peek_at(ahead).kind == TokenKind::LBrace {
                    self.diagnostics.push(Diagnostic::error(
                        "the import list `{ ... }` must be on the same line as the import path",
                        self.peek_at(ahead).span,
                    ));
                    return None;
                }
            }
            let (alias, end) = if self.eat(TokenKind::As) {
                let alias_tok = self.expect(TokenKind::Ident)?;
                (Some(alias_tok.text.clone()), alias_tok.span)
            } else {
                (None, path_end)
            };
            // A namespace import must end here: only a newline or EOF may
            // follow. Rejecting a stray token (rather than treating "not a
            // brace" as "definitely a namespace import") keeps the caret on
            // the offending token instead of letting it be mis-parsed as the
            // start of the next declaration.
            if !matches!(self.peek().kind, TokenKind::Newline | TokenKind::Eof) {
                self.error_at_current(
                    "expected `{`, `as`, or end of declaration after the import path",
                );
                return None;
            }
            return Some(ImportDecl {
                path,
                items: ImportItems::Namespace { alias },
                span: start.merge(end),
            });
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

    /// Parses an `extern js { ... }` block of bodyless JavaScript function
    /// signatures.
    ///
    /// ```text
    /// extern js {
    ///   function alert(message: String)
    ///   function setTimeout(callback: (Void) -> Void, ms: Int)
    /// }
    /// ```
    ///
    /// An optional string literal after `js` names a host module — an npm
    /// package specifier (`extern js "left-pad" { ... }`;
    /// omitting it binds the block to the ambient `js` host.
    ///
    /// The `js` language tag is matched as a contextual identifier (not a
    /// reserved keyword) so `js` stays usable as a variable name; only
    /// `extern js` is recognized today. A signature that fails to parse is
    /// recovered via [`synchronize_stmt`](Self::synchronize_stmt); the loop
    /// makes guaranteed progress every iteration (same anti-hang discipline as
    /// [`parse_block`](Self::parse_block) — see that loop and `synchronize_stmt`).
    ///
    /// A doc comment preceding a signature is consumed and discarded, matching
    /// how [`parse_declaration`](Self::parse_declaration) treats a leading doc
    /// comment on a top-level function (functions don't yet carry doc text), so
    /// a documented extern signature doesn't draw a spurious "expected
    /// `function`" error.
    fn parse_extern_js_block(&mut self) -> Option<ExternJsBlock> {
        let start = self.expect(TokenKind::Extern)?.span;

        // Contextual `js` language tag — a plain identifier, not a keyword.
        // Use a lookahead rather than `expect(Ident)` so that an omitted tag
        // (`extern { ... }`) gets the tailored "expected `js`" message rather
        // than a generic "expected identifier", and so the block is still
        // parsed best-effort instead of aborting on the missing tag.
        if self.peek().kind == TokenKind::Ident {
            let lang_tok = self.advance();
            if lang_tok.text != "js" {
                self.diagnostics.push(Diagnostic::error(
                    "expected `js` after `extern` — only `extern js` interop blocks are supported",
                    lang_tok.span,
                ));
                // Continue best-effort: still parse the block below so a typo
                // in the language tag doesn't cascade into a stream of errors.
            }
        } else {
            self.diagnostics.push(Diagnostic::error(
                "expected `js` after `extern` — only `extern js` interop blocks are supported",
                self.peek().span,
            ));
            // Continue best-effort into the block below (the `{` expectation
            // follows), so a missing tag doesn't cascade into further errors.
        }

        // Optional module specifier: `extern js "left-pad" { ... }` binds the
        // block's signatures to an npm package host module;
        // absent, the block binds to the ambient `js` host. A malformed
        // specifier ([`extern_module_specifier_error`] explains each rejection)
        // is diagnosed best-effort: the block still parses, with no module, so
        // one bad specifier doesn't cascade.
        let module = if self.peek().kind == TokenKind::StringLiteral {
            let tok = self.advance();
            let specifier = strip_string_literal_quotes(&tok.text);
            match extern_module_specifier_error(&specifier) {
                Some(message) => {
                    self.diagnostics.push(Diagnostic::error(message, tok.span));
                    None
                }
                None => Some(specifier),
            }
        } else {
            None
        };

        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut items = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            // A doc comment before a signature is consumed and discarded,
            // mirroring top-level declarations (`parse_declaration` swallows a
            // leading doc comment for every decl, and functions don't yet carry
            // doc text). The `continue` makes progress — the doc-comment token
            // is consumed — so it can't stall the anti-hang loop.
            if self.try_consume_doc_comment().is_some() {
                continue;
            }
            if let Some(sig) = self.parse_extern_fn_sig() {
                items.push(sig);
            } else {
                self.synchronize_stmt();
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(ExternJsBlock {
            module,
            items,
            span: start.merge(end),
        })
    }

    /// Parses one bodyless function signature inside an `extern js` block:
    /// `function setTimeout(callback: (Void) -> Void, ms: Int)`.
    ///
    /// Reuses [`parse_params`](Self::parse_params) and
    /// [`parse_type_expr`](Self::parse_type_expr) so extern parameters and
    /// return types accept exactly the same grammar as ordinary functions.
    /// Three constructs that ordinary functions allow are rejected here with
    /// targeted diagnostics, because the JavaScript host cannot honor them:
    /// generic type parameters (`function f<T>(...)`), parameter default
    /// values (`function f(x: Int = 5)`), and a `self` receiver
    /// (`function f(self)`) — extern signatures declare free functions, not
    /// methods. A `{ ... }` body following the signature is likewise a
    /// targeted error — extern signatures declare a JavaScript function with
    /// no Phoenix implementation — but the stray block is still consumed so
    /// parsing continues cleanly.
    fn parse_extern_fn_sig(&mut self) -> Option<ExternFnSig> {
        let start = self.expect(TokenKind::Function)?.span;
        let name_token = self.expect(TokenKind::Ident)?;
        let name = name_token.text.clone();
        let name_span = name_token.span;

        // Generic type parameters are not permitted: the JS host has no
        // monomorphization. Diagnose `<...>` explicitly (rather than letting
        // the `(` expectation below fire a confusing "expected `(`") and
        // consume the list — bounds included (`<T: Trait>`) — via the
        // bounds-aware parser so a bounded generic doesn't trip a second
        // spurious "expected `>`" diagnostic. The result is discarded.
        if self.peek().kind == TokenKind::Lt {
            self.diagnostics.push(Diagnostic::error(
                "`extern js` function signatures cannot have generic type parameters — the JavaScript host has no type information to monomorphize against",
                self.peek().span,
            ));
            self.parse_type_params_with_bounds();
        }

        self.expect(TokenKind::LParen)?;
        let params = self.parse_params();
        let mut end = self.expect(TokenKind::RParen)?.span;

        // Validate each parameter against the extern-js restrictions:
        //   * No default values — the JS host cannot evaluate a Phoenix
        //     default expression.
        //   * No `self` receiver — extern signatures declare free JavaScript
        //     functions, not methods, so `self` is meaningless here.
        for param in &params {
            if let Some(default) = &param.default_value {
                self.diagnostics.push(Diagnostic::error(
                    "`extern js` function parameters cannot have default values — the JavaScript host cannot evaluate a Phoenix default expression",
                    default.span(),
                ));
            }
            // `param.name == "self"` is reliable because `self` lexes as
            // `SelfKw`, never `Ident`: only the `SelfKw` branch in
            // `parse_params` produces a param named "self", so an ordinary
            // parameter can never collide with this check.
            if param.name == "self" {
                self.diagnostics.push(Diagnostic::error(
                    "`extern js` function signatures cannot take `self` — they declare free JavaScript functions, not methods",
                    param.span,
                ));
            }
        }

        let return_type = if self.eat(TokenKind::Arrow) {
            let rt = self.parse_type_expr();
            if let Some(t) = &rt {
                end = t.span();
            }
            rt
        } else {
            None
        };

        // A body is not allowed on an extern signature. Diagnose it, then
        // consume the stray block so the surrounding block loop continues.
        if self.peek().kind == TokenKind::LBrace {
            self.diagnostics.push(Diagnostic::error(
                "`extern js` function signatures cannot have a body — they declare a JavaScript function with no Phoenix implementation",
                self.peek().span,
            ));
            if let Some(body) = self.parse_block() {
                end = body.span;
            }
        }

        Some(ExternFnSig {
            name,
            name_span,
            params,
            return_type,
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

    /// Parses a run of leading annotations (`@name` or `@name(args)`).
    ///
    /// Newlines between consecutive annotations — and between the final
    /// annotation and the declaration or field it precedes — are consumed, so
    /// each annotation may sit on its own line. Returns an empty vector when
    /// the cursor is not on an `@`. Argument lists accept literals and bare
    /// identifiers; the semantic checker validates names, targets, and arities.
    fn parse_annotations(&mut self) -> Vec<Annotation> {
        let mut annotations = Vec::new();
        while self.peek().kind == TokenKind::At {
            let start = self.peek().span;
            self.advance(); // consume `@`
            let Some(name_tok) = self.expect_ident_or_contextual() else {
                // `expect_ident_or_contextual` already recorded a diagnostic.
                break;
            };
            let name = name_tok.text.clone();
            let mut end = name_tok.span;
            let mut args = Vec::new();
            if self.peek().kind == TokenKind::LParen {
                self.advance(); // consume `(`
                self.skip_newlines();
                args = self.parse_comma_separated(TokenKind::RParen, |p| p.parse_annotation_arg());
                self.skip_newlines();
                if let Some(rparen) = self.expect(TokenKind::RParen) {
                    end = rparen.span;
                }
            }
            annotations.push(Annotation {
                name,
                args,
                span: start.merge(end),
            });
            self.skip_newlines();
        }
        annotations
    }

    /// Parses a single annotation argument: a string/int/float/bool literal or
    /// a bare identifier. Records a diagnostic and returns `None` on anything
    /// else, which ends the surrounding comma-separated list.
    ///
    /// A numeric literal may carry a leading `-` (e.g. `@range(-40, 120)`); the
    /// sign lexes as a separate `Minus` operator token, so it is consumed here
    /// and applied to the literal. A `-` before a non-numeric argument is an
    /// error.
    fn parse_annotation_arg(&mut self) -> Option<AnnotationArg> {
        let negate = self.eat(TokenKind::Minus);
        match self.peek().kind {
            TokenKind::StringLiteral if !negate => {
                let raw = self.advance().text.clone();
                Some(AnnotationArg::String(strip_string_literal_quotes(&raw)))
            }
            TokenKind::IntLiteral => {
                let tok_text = self.advance().text.clone();
                match tok_text.parse::<i64>() {
                    Ok(n) => Some(AnnotationArg::Int(if negate { -n } else { n })),
                    Err(_) => {
                        self.error_at_current(
                            "integer literal is out of range for an annotation argument",
                        );
                        None
                    }
                }
            }
            TokenKind::FloatLiteral => {
                let tok_text = self.advance().text.clone();
                // A `FloatLiteral` lexeme always parses: an out-of-range
                // magnitude yields infinity rather than an error, so the
                // fallback below is unreachable in practice and exists only to
                // avoid an `unwrap` on a token whose shape the lexer guarantees.
                let n = tok_text.parse::<f64>().unwrap_or(f64::INFINITY);
                Some(AnnotationArg::Float(if negate { -n } else { n }))
            }
            TokenKind::True if !negate => {
                self.advance();
                Some(AnnotationArg::Bool(true))
            }
            TokenKind::False if !negate => {
                self.advance();
                Some(AnnotationArg::Bool(false))
            }
            TokenKind::Ident if !negate => Some(AnnotationArg::Ident(self.advance().text.clone())),
            _ => {
                self.error_at_current(
                    "expected an annotation argument (string, number, boolean, or identifier)",
                );
                None
            }
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
            // Top-level functions get their annotations attached by
            // `parse_declaration`; inline methods carry none.
            annotations: Vec::new(),
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
                        modifiers: Vec::new(),
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

    /// Migration aid for the 2026-06-10 field-syntax unification: if the
    /// cursor sits on the *old* type-first shape (`Int x`,
    /// `Option<String> bio`, `dyn Drawable hero` — a plausible type
    /// directly followed by an identifier), emit a targeted "write
    /// `x: Int`" diagnostic before the generic expect-failures fire.
    /// Detection is a bounded-lookahead heuristic (a type keyword,
    /// capitalized type name, or `dyn Trait`, plus an optional balanced
    /// `<...>` argument list); it adds a clearer first line, and the
    /// caller's recovery path keeps the parse moving either way.
    /// Returns `true` if it fired (callers skip the field parse and go
    /// straight to recovery, so the targeted hint isn't followed by a
    /// redundant generic "expected identifier").
    fn note_type_first_field(&mut self, what: &str) -> bool {
        let is_upper_ident = |tok: &Token| {
            tok.kind == TokenKind::Ident
                && tok
                    .text
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
        };
        // Offset just past the plausible type's head token(s).
        let mut end = match self.peek().kind {
            TokenKind::IntType
            | TokenKind::FloatType
            | TokenKind::StringType
            | TokenKind::BoolType => 1,
            TokenKind::Ident if is_upper_ident(self.peek()) => 1,
            TokenKind::Dyn if is_upper_ident(self.peek_at(1)) => 2,
            _ => return false,
        };
        // Optional generic argument list: scan a balanced `<...>` (the
        // lexer has no `>>` token, so nested closers are plain `Gt`s).
        // Bounded so a stray `<` can't drag the lookahead across the
        // whole body; on anything that can't appear inside a type
        // argument list, this isn't the old field shape — bail.
        if self.peek_at(end).kind == TokenKind::Lt {
            let mut depth = 0usize;
            loop {
                match self.peek_at(end).kind {
                    TokenKind::Lt => depth += 1,
                    TokenKind::Gt => {
                        depth -= 1;
                        if depth == 0 {
                            end += 1;
                            break;
                        }
                    }
                    TokenKind::Ident
                    | TokenKind::IntType
                    | TokenKind::FloatType
                    | TokenKind::StringType
                    | TokenKind::BoolType
                    | TokenKind::Dyn
                    | TokenKind::Comma => {}
                    _ => return false,
                }
                end += 1;
                if end > 24 {
                    return false;
                }
            }
        }
        if self.peek_at(end).kind == TokenKind::Ident {
            let mut ty = String::new();
            for i in 0..end {
                let tok = self.peek_at(i);
                match tok.kind {
                    TokenKind::Dyn => ty.push_str("dyn "),
                    TokenKind::Comma => ty.push_str(", "),
                    _ => ty.push_str(&tok.text),
                }
            }
            let name = self.peek_at(end).text.to_string();
            self.error_at_current(&format!(
                "{what} declarations use `name: Type` — write `{name}: {ty}`, \
                 not `{ty} {name}` (field syntax was unified with parameter / \
                 `let` annotations)"
            ));
            return true;
        }
        false
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
            // Consume a leading doc comment and annotations up front. They
            // attach to a *field*; a method or inline `impl` carries neither, so
            // `reject_member_metadata` reports a targeted error (and the member
            // still parses) when one of those follows instead. Consuming here
            // also preserves the field ordering rule — doc first, then
            // annotations: `@skip /** doc */ name` leaves the doc comment as the
            // next token, which the field arm rejects as a misplaced field.
            let doc_span = (self.peek().kind == TokenKind::DocComment).then(|| self.peek().span);
            let field_doc = self.try_consume_doc_comment();
            let field_annotations = self.parse_annotations();

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
                self.reject_member_metadata(doc_span, &field_annotations, "a method");
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
                self.reject_member_metadata(doc_span, &field_annotations, "an inline `impl`");
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
                // Field: [doc_comment] [annotations] [public] name ':' Type
                // [where <constraint-expr>] — colon syntax, unified with params
                // / let bindings / the endpoint DSL (see design-decisions
                // §Field declarations, 2026-06-10). The doc comment and
                // annotations were consumed at the top of the loop.
                let fstart = self.peek().span;
                let field_vis = if self.eat(TokenKind::Public) {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                if self.note_type_first_field("struct field") {
                    self.synchronize_stmt();
                } else if let Some(name_tok) = self.expect_ident_or_contextual()
                    && self.expect(TokenKind::Colon).is_some()
                    && let Some(type_expr) = self.parse_type_expr()
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
                        .unwrap_or_else(|| type_expr.span());
                    let span = fstart.merge(end_span);
                    fields.push(FieldDecl {
                        type_annotation: type_expr,
                        name: name_tok.text.clone(),
                        constraint,
                        doc_comment: field_doc,
                        annotations: field_annotations,
                        visibility: field_vis,
                        span,
                    });
                } else {
                    // Guaranteed progress: a malformed field used to leave
                    // the cursor untouched, spinning this loop forever (the
                    // parser-hang bug surfaced 2026-06-10). Skip to the next
                    // newline / `}` so every iteration consumes something.
                    self.synchronize_stmt();
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
            // Attached by `parse_declaration`.
            annotations: Vec::new(),
            visibility,
            span: start.merge(end),
        })
    }

    /// Parses an enum declaration. Variants, methods, and inline `impl` blocks
    /// are newline-separated inside the braces (Phoenix does not use commas as
    /// variant separators); a variant may carry a parenthesised payload:
    ///
    /// ```text
    /// enum Shape {
    ///   Circle(Float)
    ///   Rectangle(Float, Float)
    ///   function area(self) -> Float { ... }
    /// }
    /// ```
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
            // An enum carries doc comments and annotations on the declaration
            // itself (written before `enum`), never on members inside the body:
            // variants, methods, and inline `impl`s all reject them. Consume any
            // leading doc comment / annotations here so the offending member can
            // report a targeted error rather than a generic "expected Ident".
            let doc_span = (self.peek().kind == TokenKind::DocComment).then(|| self.peek().span);
            self.try_consume_doc_comment(); // dropped; only its presence (doc_span) matters
            let member_annotations = self.parse_annotations();

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
                self.reject_member_metadata(doc_span, &member_annotations, "a method");
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
                self.reject_member_metadata(doc_span, &member_annotations, "an inline `impl`");
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
                self.reject_member_metadata(doc_span, &member_annotations, "an enum variant");
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
                } else {
                    // Guaranteed progress: a malformed variant (e.g. a stray
                    // `,` between variants, which Phoenix does not use as a
                    // separator) used to leave the cursor untouched, spinning
                    // this loop forever — the same parser-hang class fixed for
                    // struct fields on 2026-06-10. `expect` above already
                    // recorded the diagnostic; skip the single offending token
                    // (not `synchronize_stmt`, which would overshoot — a comma
                    // suppresses the following newline, so the next variant sits
                    // right after it) so the surviving variants still parse.
                    self.advance();
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
            // Attached by `parse_declaration`.
            annotations: Vec::new(),
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
        let path = strip_string_literal_quotes(&path_token.text);

        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut query_params = Vec::new();
        let mut headers = Vec::new();
        let mut response_headers = Vec::new();
        let mut body = None;
        let mut response = None;
        let mut response_statuses = Vec::new();
        let mut errors = Vec::new();
        let mut pagination = None;
        let mut has_query = false;
        let mut has_headers = false;
        let mut has_response_headers = false;
        let mut has_body = false;
        let mut has_response = false;
        let mut has_error = false;
        let mut has_pagination = false;

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
                TokenKind::Headers => {
                    if has_headers {
                        self.error_at_current("duplicate `headers` section in endpoint");
                    }
                    has_headers = true;
                    headers = self.parse_headers_block();
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
                    // Block form: `response { <status>[: Type] ... }`. An `{`
                    // following `response` (rather than a type name or
                    // `headers`) selects the multi-status block; newlines were
                    // just skipped, so the `{` may sit on the next line. The
                    // block form
                    // populates `response_statuses`; a trailing inline `headers`
                    // block is rejected right below (multi-status is mutually
                    // exclusive with response headers — decision 4).
                    if self.peek().kind == TokenKind::LBrace {
                        response_statuses = self.parse_response_block();
                        // In the bare form a same-line `headers { ... }` binds as
                        // RESPONSE headers, so reject the same spelling here with a
                        // targeted error and consume the block to recover —
                        // otherwise it would re-dispatch as the standalone REQUEST
                        // `headers` section and silently turn the would-be response
                        // headers into handler/client inputs. (A `headers` block on
                        // its own line is still the request section, exactly as for
                        // the bare form — we deliberately do NOT skip newlines
                        // before this check.)
                        if self.peek().kind == TokenKind::Headers {
                            self.error_at_current(
                                "a multi-status `response { }` block cannot declare response headers — both wrap the return value in a generated envelope; see docs/known-issues.md",
                            );
                            let _ = self.parse_headers_block();
                        }
                    } else
                    // `response headers { ... }` with no response type: response
                    // headers are bundled with the body into a typed envelope, so
                    // they require a response type. Catch this here (before
                    // `parse_type_expr` reports a generic "expected type name")
                    // with a targeted message, and consume the block to recover
                    // cleanly so its entries don't re-dispatch as a request
                    // section.
                    if self.peek().kind == TokenKind::Headers {
                        self.error_at_current(
                            "response headers require a response type (write `response <Type> headers { ... }`)",
                        );
                        let _ = self.parse_headers_block();
                    } else {
                        response = self.parse_type_expr();
                        // Optional inline response-headers block: `response Type headers { ... }`.
                        // The `headers` keyword must immediately follow the response type on
                        // the SAME line to bind here. A `headers` block on a new line is the
                        // standalone request section (handled by the top-level dispatch arm),
                        // so we deliberately do NOT skip newlines before this check — that
                        // keeps section ordering free: a request `headers` block placed after
                        // `response` stays a request header instead of silently rebinding to
                        // the response.
                        if self.peek().kind == TokenKind::Headers {
                            if has_response_headers {
                                self.error_at_current(
                                    "duplicate response `headers` section in endpoint",
                                );
                            }
                            has_response_headers = true;
                            response_headers = self.parse_headers_block();
                        }
                    }
                }
                TokenKind::Pagination => {
                    if has_pagination {
                        self.error_at_current("duplicate `pagination` section in endpoint");
                    }
                    has_pagination = true;
                    pagination = self.parse_pagination_block();
                }
                TokenKind::ErrorKw => {
                    if has_error {
                        self.error_at_current("duplicate `error` section in endpoint");
                    }
                    has_error = true;
                    errors = self.parse_error_block();
                }
                _ => {
                    self.error_at_current(
                        "expected `query`, `headers`, `body`, `response`, `pagination`, `error`, or `}`",
                    );
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
            // Top-level endpoints have no version prefix. The
            // `api version "..." { }` block-parser overwrites this with
            // `Some(version)` for endpoints declared inside a block.
            api_version: None,
            query_params,
            headers,
            body,
            response,
            response_statuses,
            response_headers,
            pagination,
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
        // `parse_type_expr` attaches any trailing `omit`/`pick`/`partial` chain to
        // the base `Named` (the same grammar a `response` projection uses). Pull
        // those modifiers up into the `DerivedType` so body resolution sees them
        // exactly as before.
        let base = self.parse_type_expr()?;
        let (base_type, modifiers) = match base {
            TypeExpr::Named(n) if !n.modifiers.is_empty() => {
                let modifiers = n.modifiers.clone();
                let bare = TypeExpr::Named(NamedType {
                    name: n.name,
                    span: n.span,
                    modifiers: Vec::new(),
                });
                (bare, modifiers)
            }
            other => (other, Vec::new()),
        };

        let end = modifiers
            .last()
            .map(TypeModifier::span)
            .unwrap_or_else(|| base_type.span());

        Some(DerivedType {
            base_type,
            modifiers,
            span: start.merge(end),
        })
    }

    /// Parses a (possibly empty) chain of `omit`/`pick`/`partial` projection
    /// modifiers. Shared by the type-expression parser (so a `Named` type may
    /// carry projection wherever it appears) and, transitively, the `body` parser.
    pub(crate) fn parse_type_modifiers(&mut self) -> Option<Vec<TypeModifier>> {
        let mut modifiers = Vec::new();
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
                    // Optional field list for selective partial.
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
        Some(modifiers)
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
    /// newlines, each optionally followed by a comma — so the one-line
    /// `error { NotFound(404), Conflict(409) }` also parses, matching the
    /// forgiving field-list style of `omit { a, b }` and the comma handling
    /// of [`parse_response_block`](Self::parse_response_block).
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
            let entry_pos = self.pos;
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
            // Recovery: a malformed variant can fail its FIRST `expect` without
            // consuming anything (`expect` records a diagnostic but does not
            // advance on mismatch), and `skip_newlines` below would not move
            // either — without this skip the loop re-examined the same token
            // forever, hanging the compiler on any malformed entry.
            if self.pos == entry_pos {
                self.advance();
            }
            self.eat(TokenKind::Comma);
            self.skip_newlines();
        }
        let _ = self.expect(TokenKind::RBrace);
        errors
    }

    /// Parses the block form of a `response` section: `response { ... }`.
    ///
    /// Expected syntax:
    ///
    /// ```text
    /// response {
    ///     200: User
    ///     201: User
    ///     204
    /// }
    /// ```
    ///
    /// Each entry is a success status code (an integer literal) optionally
    /// followed by `: <Type>`. A typeless entry (just `204`) records `ty: None`.
    /// Entries are separated by newlines, each optionally followed by a comma —
    /// so the one-line `response { 200: User, 201: User }` also parses,
    /// matching [`parse_error_block`](Self::parse_error_block) and the
    /// field-list style of `omit { a, b }`.
    ///
    /// The leading `response` keyword is already consumed by the caller; this
    /// method expects the opening `{`. The shared-body-type, 2xx-only, and
    /// duplicate-status rules are NOT enforced here — they are sema's job. The
    /// parser does reject what only it can see cleanly: an empty `response { }`
    /// (which would otherwise silently behave as "no response declared") and a
    /// status literal that does not fit a `u16` (reported by its written text,
    /// not folded to `0`). A malformed entry missing its status integer is
    /// reported as a parse error and skipped for clean recovery.
    ///
    /// Returns an empty vector if the opening `{` is missing (a diagnostic is
    /// recorded in that case). Mirrors the `{ ... }` loop style of
    /// [`parse_query_block`](Self::parse_query_block) and the integer-reading of
    /// [`parse_error_block`](Self::parse_error_block).
    fn parse_response_block(&mut self) -> Vec<ResponseStatus> {
        let mut statuses = Vec::new();
        let Some(lbrace) = self.expect(TokenKind::LBrace) else {
            return statuses;
        };
        let lbrace_span = lbrace.span;
        // Distinct from `statuses.is_empty()`: malformed entries are dropped
        // during recovery (with their own diagnostics), and an
        // all-entries-malformed block should not ALSO cascade the empty-block
        // error below.
        let mut saw_entry = false;
        self.skip_newlines();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            // Status code: a required integer literal. Mirror the integer
            // reading in `parse_error_block` (parse the token text). On a
            // missing integer, `expect` records a diagnostic; skip the offending
            // token to recover so it doesn't re-dispatch as another section.
            if let Some(code_tok) = self.expect(TokenKind::IntLiteral) {
                saw_entry = true;
                let mut span = code_tok.span;
                // Optional `: <Type>` makes the entry typed; its absence makes
                // it typeless (e.g. `204`).
                let ty = if self.eat(TokenKind::Colon) {
                    let ty = self.parse_type_expr();
                    if let Some(t) = &ty {
                        span = span.merge(t.span());
                    }
                    ty
                } else if self.peek().kind == TokenKind::Ident {
                    // `200 User` — a forgotten colon. Without this arm the
                    // identifier would fall through to the NEXT iteration's
                    // integer expect and report an unhelpful "expected integer
                    // literal, found identifier". Name the actual mistake, then
                    // parse the type anyway so the entry recovers as typed (a
                    // newline-separated typeless entry never hits this arm:
                    // newlines are tokens, so `peek` only sees an identifier
                    // when it shares the entry's line).
                    let ident = self.peek();
                    self.diagnostics.push(Diagnostic::error(
                        format!(
                            "missing `:` between status and type — write `{}: {}`",
                            code_tok.text, ident.text
                        ),
                        ident.span,
                    ));
                    let ty = self.parse_type_expr();
                    if let Some(t) = &ty {
                        span = span.merge(t.span());
                    }
                    ty
                } else {
                    None
                };
                // An out-of-range literal (e.g. `70000`) cannot be an HTTP
                // status. Report it by its written text and drop the entry —
                // folding to a sentinel like `0` would make sema complain about
                // a status the user never wrote.
                match code_tok.text.parse::<u16>() {
                    Ok(status) => statuses.push(ResponseStatus { status, ty, span }),
                    Err(_) => self.diagnostics.push(Diagnostic::error(
                        format!("invalid HTTP status code `{}`", code_tok.text),
                        code_tok.span,
                    )),
                }
            } else {
                // No status integer (e.g. `response { : User }`): the diagnostic
                // is already recorded by `expect`; advance past the bad token to
                // avoid an infinite loop and re-dispatch.
                self.advance();
            }
            self.eat(TokenKind::Comma);
            self.skip_newlines();
        }
        let _ = self.expect(TokenKind::RBrace);
        if !saw_entry {
            self.diagnostics.push(Diagnostic::error(
                "`response { }` must declare at least one status",
                lbrace_span,
            ));
        }
        statuses
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
            // `name ':' Type ['=' default]` — colon syntax, unified with
            // struct fields (design-decisions §Field declarations).
            if self.note_type_first_field("query parameter") {
                self.synchronize_stmt();
            } else if let Some(name_tok) = self.expect(TokenKind::Ident)
                && self.expect(TokenKind::Colon).is_some()
                && let Some(type_expr) = self.parse_type_expr()
            {
                let default_value = if self.eat(TokenKind::Eq) {
                    self.parse_expr()
                } else {
                    None
                };
                let pend = default_value
                    .as_ref()
                    .map(|e| e.span())
                    .unwrap_or_else(|| type_expr.span());
                params.push(QueryParam {
                    type_annotation: type_expr,
                    name: name_tok.text.clone(),
                    default_value,
                    span: pstart.merge(pend),
                });
            } else {
                // Guaranteed progress on malformed entries (same
                // no-progress-hang fix as struct fields).
                self.synchronize_stmt();
            }
            self.skip_newlines();
        }
        let _ = self.expect(TokenKind::RBrace);
        params
    }

    /// Parses a `headers { ... }` block, used for both request and response
    /// headers (the grammar is identical).
    ///
    /// Expected syntax:
    ///
    /// ```text
    /// headers {
    ///     authorization: String
    ///     rateLimit: String as "X-RateLimit-Limit"
    ///     contentType: String as "Content-Type" = "application/json"
    /// }
    /// ```
    ///
    /// Each entry is `name: Type [as "Wire-Name"] [= default]`. The `as "..."`
    /// clause records an explicit wire-name override (quotes stripped); when
    /// absent the wire name is auto-derived later in sema. The `= default`
    /// clause is meaningful only for request headers but is accepted for both.
    ///
    /// Mirrors [`parse_query_block`](Self::parse_query_block); the leading
    /// `headers` keyword is consumed here. Malformed entries are skipped after a
    /// diagnostic, matching the query-block recovery strategy.
    fn parse_headers_block(&mut self) -> Vec<HeaderParam> {
        let mut params = Vec::new();
        if self.expect(TokenKind::Headers).is_none() {
            return params;
        }
        if self.expect(TokenKind::LBrace).is_none() {
            return params;
        }
        self.skip_newlines();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            let pstart = self.peek().span;
            // `name ':' Type [as "Wire-Name"] ['=' default]` — colon
            // syntax, unified with struct fields (design-decisions
            // §Field declarations).
            if self.note_type_first_field("header") {
                self.synchronize_stmt();
            } else if let Some(name_tok) = self.expect(TokenKind::Ident)
                && self.expect(TokenKind::Colon).is_some()
                && let Some(type_expr) = self.parse_type_expr()
            {
                let name = name_tok.text.clone();

                // Optional explicit wire-name override: `as "Wire-Name"`.
                let mut wire_name = None;
                let mut wire_span = None;
                if self.peek().kind == TokenKind::As {
                    self.advance();
                    if let Some(lit) = self.expect(TokenKind::StringLiteral) {
                        wire_name = Some(strip_string_literal_quotes(&lit.text));
                        wire_span = Some(lit.span);
                    }
                }

                // Optional default value: `= <expr>`.
                let default_value = if self.eat(TokenKind::Eq) {
                    self.parse_expr()
                } else {
                    None
                };

                let pend = default_value
                    .as_ref()
                    .map(|e| e.span())
                    .or(wire_span)
                    .unwrap_or_else(|| type_expr.span());
                params.push(HeaderParam {
                    type_annotation: type_expr,
                    name,
                    wire_name,
                    default_value,
                    span: pstart.merge(pend),
                });
            } else {
                // Guaranteed progress on malformed entries (same
                // no-progress-hang fix as struct fields).
                self.synchronize_stmt();
            }
            self.skip_newlines();
        }
        let _ = self.expect(TokenKind::RBrace);
        params
    }

    /// Parses a `pagination { offset|cursor }` endpoint block.
    ///
    /// The block contains exactly one mode word — the contextual identifiers
    /// `offset` or `cursor` (plain `Ident`s, not reserved keywords). Returns the
    /// parsed [`PaginationMode`], or `None` on error (empty block, an unknown
    /// word, or extra tokens). Surrounding newlines are permitted.
    fn parse_pagination_block(&mut self) -> Option<PaginationMode> {
        self.expect(TokenKind::Pagination)?;
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let tok = self.peek();
        let mode = if tok.kind == TokenKind::Ident && tok.text == "offset" {
            self.advance();
            Some(PaginationMode::Offset)
        } else if tok.kind == TokenKind::Ident && tok.text == "cursor" {
            self.advance();
            Some(PaginationMode::Cursor)
        } else {
            self.error_at_current("expected `offset` or `cursor` in `pagination` block");
            // Consume the offending word (unless it's already the closing brace or
            // EOF) so recovery resumes at `}` — otherwise the same token would
            // re-trigger "expected `}`" here and the endpoint loop's catch-all,
            // piling up redundant diagnostics for one typo.
            if !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                self.advance();
            }
            None
        };

        self.skip_newlines();
        // Reject extra tokens (e.g. a second mode word) before the closing brace.
        if self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            self.error_at_current("expected `}` after pagination mode");
        }
        let _ = self.expect(TokenKind::RBrace);
        mode
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
                | TokenKind::Api
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
        let source = "struct Point { x: Int\n y: Int }";
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

    /// The pre-2026-06-10 type-first field syntax (`Int x`) gets the
    /// targeted migration diagnostic, not just a generic expect
    /// failure — and the parse terminates (regression pin for the
    /// struct-body infinite loop on malformed fields).
    #[test]
    fn struct_type_first_field_gets_migration_diagnostic() {
        let (_, diagnostics) = parse_source("struct Point { Int x\n Int y }");
        assert!(
            !diagnostics.is_empty(),
            "type-first fields must be a parse error"
        );
        assert!(
            diagnostics[0].message.contains("`x: Int`"),
            "expected the migration hint naming `x: Int`, got: {}",
            diagnostics[0].message
        );
    }

    /// The migration hint also covers generic-typed old-syntax fields
    /// (`Option<String> bio`) — the balanced `<...>` lookahead — and
    /// reconstructs the full type in the suggested rewrite.
    #[test]
    fn struct_type_first_generic_field_gets_migration_diagnostic() {
        let (_, diagnostics) = parse_source("struct S { Option<String> bio }");
        assert!(!diagnostics.is_empty());
        assert!(
            diagnostics[0].message.contains("`bio: Option<String>`"),
            "expected the migration hint naming `bio: Option<String>`, got: {}",
            diagnostics[0].message
        );
        // Nested arguments (two-`Gt` closer) and commas survive the
        // reconstruction, in a query block as well as a struct body.
        let (_, diagnostics) = parse_source(
            "endpoint e: GET \"/x\" { query { Map<String, List<Int>> m = 1 } response Int }",
        );
        assert!(!diagnostics.is_empty());
        assert!(
            diagnostics[0]
                .message
                .contains("`m: Map<String, List<Int>>`"),
            "expected the migration hint naming the full generic type, got: {}",
            diagnostics[0].message
        );
    }

    /// The migration hint also covers `dyn Trait`-typed old-syntax
    /// fields (`dyn Drawable hero`).
    #[test]
    fn struct_type_first_dyn_field_gets_migration_diagnostic() {
        let (_, diagnostics) = parse_source("struct Scene { dyn Drawable hero }");
        assert!(!diagnostics.is_empty());
        assert!(
            diagnostics[0].message.contains("`hero: dyn Drawable`"),
            "expected the migration hint naming `hero: dyn Drawable`, got: {}",
            diagnostics[0].message
        );
    }

    /// Arbitrary garbage in a struct body produces diagnostics and
    /// terminates — every loop iteration must consume at least one
    /// token. (The pre-fix parser spun forever re-peeking the first
    /// unexpected token; surfaced 2026-06-10 via a hung test suite.)
    #[test]
    fn struct_body_garbage_terminates_with_diagnostics() {
        for source in [
            "struct P { : }",
            "struct P { 42 }",
            "struct P { x = 1 }",
            "struct P { where }",
            "endpoint e: GET \"/x\" { query { ??? } response Int }",
            "endpoint e: GET \"/x\" { headers { 12 34 } response Int }",
        ] {
            let (_, diagnostics) = parse_source(source);
            assert!(
                !diagnostics.is_empty(),
                "expected diagnostics for {source:?}"
            );
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

    /// A comma between enum variants is not Phoenix syntax. The parser must
    /// reject it with a diagnostic and still terminate — a malformed variant
    /// once spun the variant loop forever (parser-hang class, see the variant
    /// arm of `parse_enum_decl`). This test would hang on a regression. The
    /// stray comma is skipped, so the surviving variants still parse.
    #[test]
    fn parse_enum_comma_separated_variants_is_rejected_and_terminates() {
        let source = "enum Color { Red, Green }";
        let (program, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for comma-separated enum variants"
        );
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                let names: Vec<&str> = e.variants.iter().map(|v| v.name.as_str()).collect();
                assert_eq!(names, vec!["Red", "Green"]);
            }
            other => panic!("expected Enum, got {:?}", other),
        }
    }

    /// The multi-line spelling of the same malformed input must behave
    /// identically — a comma suppresses the following newline, so both cases
    /// reduce to the same token stream and recover the same way.
    #[test]
    fn parse_enum_trailing_comma_recovers_following_variant() {
        let source = "enum Color {\n  Red,\n  Green\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for the stray comma"
        );
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                let names: Vec<&str> = e.variants.iter().map(|v| v.name.as_str()).collect();
                assert_eq!(names, vec!["Red", "Green"]);
            }
            other => panic!("expected Enum, got {:?}", other),
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

    /// `obj.method<Int>(x)` — a turbofish on a method call records the
    /// explicit type args on the `MethodCallExpr`.
    #[test]
    fn parse_method_call_with_turbofish() {
        let source = "function main() { let y: Int = b.wrap<Int>(42) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        let Declaration::Function(f) = &program.declarations[0] else {
            panic!("expected Function");
        };
        let Statement::VarDecl(v) = &f.body.statements[0] else {
            panic!("expected VarDecl, got {:?}", f.body.statements[0]);
        };
        match &v.initializer {
            Expr::MethodCall(mc) => {
                assert_eq!(mc.method, "wrap");
                assert_eq!(mc.type_args.len(), 1, "expected one explicit type arg");
                assert_eq!(mc.args.len(), 1);
            }
            other => panic!("expected MethodCall, got {:?}", other),
        }
    }

    /// `a.b < c` must stay a comparison (the turbofish only commits when the
    /// `<...>` is immediately followed by `(`), not be mis-parsed as a
    /// type-argument list.
    #[test]
    fn field_access_less_than_is_comparison_not_turbofish() {
        let source = "function main() { let z: Bool = a.b < c }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        let Declaration::Function(f) = &program.declarations[0] else {
            panic!("expected Function");
        };
        let Statement::VarDecl(v) = &f.body.statements[0] else {
            panic!("expected VarDecl, got {:?}", f.body.statements[0]);
        };
        match &v.initializer {
            Expr::Binary(b) => assert_eq!(b.op, BinaryOp::Lt),
            other => panic!("expected a `<` comparison, got {:?}", other),
        }
    }

    /// `a.b < c > d` is a chain of comparisons, not a turbofish — the
    /// trailing `(` requirement keeps the backtracking sound.
    #[test]
    fn field_access_comparison_chain_is_not_turbofish() {
        let source = "function main() { let z: Bool = a.b < c > d }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        let Declaration::Function(f) = &program.declarations[0] else {
            panic!("expected Function");
        };
        let Statement::VarDecl(v) = &f.body.statements[0] else {
            panic!("expected VarDecl, got {:?}", f.body.statements[0]);
        };
        match &v.initializer {
            Expr::Binary(b) => assert_eq!(b.op, BinaryOp::Gt),
            other => panic!("expected a comparison chain, got {:?}", other),
        }
    }

    /// The one residual ambiguity of bare-angle turbofish: a *parenthesised*
    /// right operand supplies the trailing `(`, so `a.b < c > (d)` commits as
    /// the method call `a.b<c>(d)` rather than the comparison `(a.b < c) > (d)`.
    /// This pins the accepted trade-off so it can't regress silently — see
    /// `try_parse_method_turbofish`.
    #[test]
    fn field_access_comparison_then_paren_commits_as_turbofish() {
        let source = "function main() { let z: Bool = a.b < c > (d) }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        let Declaration::Function(f) = &program.declarations[0] else {
            panic!("expected Function");
        };
        let Statement::VarDecl(v) = &f.body.statements[0] else {
            panic!("expected VarDecl, got {:?}", f.body.statements[0]);
        };
        match &v.initializer {
            Expr::MethodCall(mc) => {
                assert_eq!(mc.method, "b");
                assert_eq!(mc.type_args.len(), 1, "expected the `<c>` turbofish");
                assert_eq!(mc.args.len(), 1, "expected the `(d)` argument list");
            }
            other => panic!("expected a turbofish method call, got {:?}", other),
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
        let (program, diagnostics) =
            parse_source("struct Pair<A, B> {\n  first: A\n  second: B\n}");
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
                page: Int = 1
                search: String
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
    fn parse_endpoint_error_block_comma_separated() {
        // The one-line comma-separated spelling (the natural habit from
        // `omit { a, b }`) must parse identically to the newline form,
        // trailing comma included. Regression: before the comma support, the
        // variant loop had no recovery advance, so this exact source HUNG the
        // parser (the comma was re-examined forever).
        let source = r#"endpoint getUser: GET "/api/users/{id}" {
            response User
            error { NotFound(404), Forbidden(403), }
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
    fn parse_endpoint_error_block_malformed_entry_recovers() {
        // A malformed variant (`NotFound 404` — missing parens) must produce a
        // diagnostic and TERMINATE: the loop's consumed-nothing guard advances
        // past tokens the variant grammar rejects, where it previously spun
        // forever. The well-formed variant around it still parses.
        let source = r#"endpoint getUser: GET "/api/users/{id}" {
            response User
            error {
                NotFound 404
                Forbidden(403)
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "a malformed error variant should be a parse error"
        );
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert!(
                    ep.errors.iter().any(|e| e.name == "Forbidden"),
                    "the well-formed variant must survive recovery, got: {:?}",
                    ep.errors
                );
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
                verbose: Bool = false
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
        let source = "/** A 2D point. */\nstruct Point { x: Int\n y: Int }";
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

    // ── Annotation attachment tests ──────────────────────────────────

    #[test]
    fn parse_marker_annotation_on_struct() {
        let source = "@jsonSerializable\nstruct User { name: String }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                assert_eq!(s.annotations.len(), 1);
                assert_eq!(s.annotations[0].name, "jsonSerializable");
                assert!(s.annotations[0].args.is_empty());
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    #[test]
    fn parse_annotation_with_string_arg_on_field() {
        let source = "struct User {\n  @jsonName(\"user_name\")\n  name: String\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                let field = &s.fields[0];
                assert_eq!(field.name, "name");
                assert_eq!(field.annotations.len(), 1);
                assert_eq!(field.annotations[0].name, "jsonName");
                assert_eq!(
                    field.annotations[0].args,
                    vec![AnnotationArg::String("user_name".to_string())]
                );
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    #[test]
    fn parse_multiple_annotations_and_doc_on_field() {
        let source = "struct User {\n  /** the name */\n  @skip\n  @jsonName(\"n\")\n  public name: String\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => {
                let field = &s.fields[0];
                assert!(field.doc_comment.is_some());
                assert_eq!(field.visibility, Visibility::Public);
                let names: Vec<&str> = field.annotations.iter().map(|a| a.name.as_str()).collect();
                assert_eq!(names, vec!["skip", "jsonName"]);
            }
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    #[test]
    fn parse_annotation_on_function() {
        let source = "@test\nfunction testThing() { }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.annotations.len(), 1);
                assert_eq!(f.annotations[0].name, "test");
            }
            other => panic!("expected Function, got {:?}", other),
        }
    }

    #[test]
    fn parse_annotation_multiple_arg_kinds() {
        let source = "@every(15, true, info)\nfunction job() { }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(
                    f.annotations[0].args,
                    vec![
                        AnnotationArg::Int(15),
                        AnnotationArg::Bool(true),
                        AnnotationArg::Ident("info".to_string()),
                    ]
                );
            }
            other => panic!("expected Function, got {:?}", other),
        }
    }

    #[test]
    fn parse_annotation_empty_parens_has_no_args() {
        // `@marker()` is accepted and behaves like the bare `@marker` form.
        let source = "@marker()\nfunction job() { }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(f.annotations[0].name, "marker");
                assert!(f.annotations[0].args.is_empty());
            }
            other => panic!("expected Function, got {:?}", other),
        }
    }

    #[test]
    fn parse_annotation_negative_numeric_args() {
        // A leading `-` negates int and float literal arguments.
        let source = "@range(-40, -1.5)\nfunction job() { }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => {
                assert_eq!(
                    f.annotations[0].args,
                    vec![AnnotationArg::Int(-40), AnnotationArg::Float(-1.5)]
                );
            }
            other => panic!("expected Function, got {:?}", other),
        }
    }

    #[test]
    fn parse_annotation_minus_before_non_numeric_is_rejected() {
        // `-` is only meaningful before a numeric literal.
        let source = "@tag(-yes)\nfunction job() { }";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("expected an annotation argument")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_annotation_int_arg_out_of_range_is_rejected() {
        let source = "@big(99999999999999999999)\nfunction job() { }";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("out of range for an annotation argument")),
            "expected out-of-range diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn annotation_before_struct_method_is_rejected() {
        // Annotations attach to fields, not methods nested in a struct body.
        // The method itself still parses; only the annotation is rejected.
        let source = "struct S {\n  @skip\n  function f(self) -> Int { return 1 }\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("annotations cannot precede a method")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::Struct(s) => assert_eq!(s.methods.len(), 1, "method should still parse"),
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    #[test]
    fn annotation_before_enum_variant_is_rejected() {
        // Enums carry annotations on the declaration, never on a variant.
        let source = "enum E {\n  @skip\n  Red\n  Green\n}";
        let (program, diagnostics) = parse_source(source);
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("annotations cannot precede an enum variant")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::Enum(e) => {
                let names: Vec<&str> = e.variants.iter().map(|v| v.name.as_str()).collect();
                assert_eq!(names, vec!["Red", "Green"], "variants should still parse");
            }
            other => panic!("expected Enum, got {:?}", other),
        }
    }

    #[test]
    fn annotation_before_trait_is_rejected() {
        let source = "@skip\ntrait Drawable { function draw(self) -> String }";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("annotations cannot precede `trait`")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn annotation_before_type_alias_is_rejected() {
        let source = "@skip\ntype Id = Int";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("annotations cannot precede `type`")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn annotation_before_schema_is_rejected() {
        let source = "@skip\nschema db { }";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("annotations cannot precede `schema`")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn annotation_before_extern_js_is_rejected() {
        let source = "@skip\nextern js {\n  function alert(message: String)\n}";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("annotations cannot precede `extern js` blocks")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn annotation_before_import_is_rejected() {
        let source = "@skip\nimport a.b { Foo }";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("annotations cannot precede `import`")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn annotation_before_endpoint_is_rejected() {
        let source = "@skip\nendpoint deleteUser: DELETE \"/api/users/{id}\" {\n response Void\n}";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("annotations cannot precede `endpoint`")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn annotation_before_impl_is_rejected() {
        let source = "@skip\nimpl Foo { function bar() { } }";
        let (_program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("annotations cannot precede `impl`")),
            "expected rejection diagnostic, got: {:?}",
            diagnostics
        );
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
struct User { id: Int  name: String  email: String  age: Int }
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
struct User { id: Int  name: String  email: String  age: Int }
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
struct Comment { id: Int  text: String }
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
        term: String
        page: Int = 1
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
        search: Option<String>
        limit: Int = 10
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
    age: Int where self >= 0 && self <= 150
    name: String
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
    email: String where self.contains("@") && self.length > 3
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
    id: Int
    name: String where self.length > 0 && self.length <= 100
    email: String where self.contains("@")
    age: Int where self >= 0
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
        let source = "struct Item { price: Int where self > 0 }";
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
        let source = "struct Range { x: Int where self < 0 || self > 100 }";
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
        let source = "struct Item { price: Float where self > 0.0 && self < 1000.0 }";
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Struct(s) => assert!(s.fields[0].constraint.is_some()),
            other => panic!("expected Struct, got {:?}", other),
        }
    }

    // ── api version block parsing tests ──────────────────────────────

    /// A single-endpoint `api version` block flattens to one top-level
    /// endpoint tagged with the version. The path is NOT prefixed (that is
    /// sema's job); the parser only tags `api_version`.
    #[test]
    fn parse_api_version_block_single() {
        let source = r#"api version "v1" {
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "a");
                assert_eq!(ep.path, "/posts");
                assert_eq!(ep.api_version.as_deref(), Some("v1"));
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// A block with two endpoints tags both with the same version.
    #[test]
    fn parse_api_version_multiple_endpoints() {
        let source = r#"api version "v1" {
            endpoint a: GET "/posts" { response Post }
            endpoint b: GET "/users" { response User }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 2);
        for decl in &program.declarations {
            match decl {
                Declaration::Endpoint(ep) => {
                    assert_eq!(ep.api_version.as_deref(), Some("v1"));
                }
                other => panic!("expected Endpoint, got {:?}", other),
            }
        }
    }

    /// Two `api version` blocks plus a top-level endpoint coexist; each
    /// endpoint is tagged with its block's version (or `None` at top level).
    #[test]
    fn parse_multiple_api_version_blocks() {
        let source = r#"api version "v1" {
            endpoint a: GET "/posts" { response Post }
        }
        api version "v2" {
            endpoint b: GET "/posts" { response Post }
        }
        endpoint c: GET "/health" { response Health }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 3);
        let versions: Vec<(String, Option<String>)> = program
            .declarations
            .iter()
            .map(|d| match d {
                Declaration::Endpoint(ep) => (ep.name.clone(), ep.api_version.clone()),
                other => panic!("expected Endpoint, got {:?}", other),
            })
            .collect();
        assert_eq!(versions[0], ("a".to_string(), Some("v1".to_string())));
        assert_eq!(versions[1], ("b".to_string(), Some("v2".to_string())));
        assert_eq!(versions[2], ("c".to_string(), None));
    }

    /// The version string is captured raw, including a leading slash form;
    /// the parser does NOT normalize (sema does).
    #[test]
    fn parse_api_version_slash_form() {
        let source = r#"api version "/v1" {
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.api_version.as_deref(), Some("/v1"));
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// An empty version string is rejected with a parse error.
    #[test]
    fn parse_api_version_empty_rejected() {
        let source = r#"api version "" {
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for an empty version string"
        );
        // The block body is still consumed: its endpoint must NOT leak out as a
        // top-level declaration (which would also leave a dangling `}`).
        assert!(
            program.declarations.is_empty(),
            "rejected block must not emit declarations, got: {:?}",
            program.declarations
        );
    }

    /// `api` without the contextual `version` keyword is a parse error.
    /// Recovery consumes the block body, so it yields exactly one diagnostic
    /// and the contained endpoint does NOT leak out as a top-level declaration
    /// (which would also leave a dangling `}` to cascade).
    #[test]
    fn parse_api_requires_version_keyword() {
        let source = r#"api "v1" {
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert_eq!(
            diagnostics.len(),
            1,
            "expected exactly one diagnostic, got: {:?}",
            diagnostics
        );
        assert!(
            program.declarations.is_empty(),
            "rejected block must not emit declarations, got: {:?}",
            program.declarations
        );
    }

    /// `api version` with no version string is a parse error, and likewise
    /// recovers by consuming the block body — exactly one diagnostic, no
    /// leaked declarations.
    #[test]
    fn parse_api_requires_version_string() {
        let source = r#"api version {
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert_eq!(
            diagnostics.len(),
            1,
            "expected exactly one diagnostic, got: {:?}",
            diagnostics
        );
        assert!(
            program.declarations.is_empty(),
            "rejected block must not emit declarations, got: {:?}",
            program.declarations
        );
    }

    /// A version string that is only slashes (or whitespace) is rejected — it
    /// would otherwise normalize to an empty leading path segment (`//posts`).
    #[test]
    fn parse_api_version_slash_only_rejected() {
        for bad in ["/", "//", " "] {
            let source = format!(
                r#"api version "{bad}" {{
                    endpoint a: GET "/posts" {{ response Post }}
                }}"#
            );
            let (program, diagnostics) = parse_source(&source);
            assert!(
                !diagnostics.is_empty(),
                "expected a diagnostic for version string {bad:?}"
            );
            assert!(
                program.declarations.is_empty(),
                "rejected block must not emit declarations for {bad:?}, got: {:?}",
                program.declarations
            );
        }
    }

    /// A non-endpoint declaration inside an `api version` block is rejected.
    /// Recovery skips the offending declaration's own brace-delimited body so a
    /// following endpoint is still parsed and the block's closing `}` does not
    /// cascade into spurious top-level errors — exactly one diagnostic.
    #[test]
    fn parse_api_version_rejects_non_endpoint() {
        let source = r#"api version "v1" {
            struct Nope { id: Int }
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert_eq!(
            diagnostics.len(),
            1,
            "expected exactly one diagnostic, got: {:?}",
            diagnostics
        );
        // The endpoint following the malformed declaration is still recovered
        // and tagged with the block's version.
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "a");
                assert_eq!(ep.api_version.as_deref(), Some("v1"));
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// A malformed *endpoint* inside the block (here: missing its path string)
    /// is reported, then recovery skips the wreckage to the next item so a
    /// following valid endpoint is still parsed and tagged — and the block's
    /// own `}` is consumed normally rather than cascading into spurious
    /// top-level errors.
    #[test]
    fn parse_api_version_recovers_from_malformed_endpoint() {
        let source = r#"api version "v1" {
            endpoint bad: GET { response Post }
            endpoint good: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for the malformed endpoint"
        );
        // The valid endpoint after the wreckage is still recovered, tagged with
        // the block's version, and is the only declaration produced.
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "good");
                assert_eq!(ep.api_version.as_deref(), Some("v1"));
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// A version string containing anything outside the path-segment-safe
    /// charset is rejected — whitespace, path-parameter syntax (`{`/`}`), the
    /// query/fragment delimiters (`?`/`#`), and `..` traversal would each
    /// splice a malformed segment, a phantom path param, or an escape into
    /// every route.
    #[test]
    fn parse_api_version_invalid_chars_rejected() {
        for bad in [
            "v 1", "v1/{id}", "{tenant}", "v1?x=1", "v1#frag", "v1/../v2",
        ] {
            let source = format!(
                r#"api version "{bad}" {{
                    endpoint a: GET "/posts" {{ response Post }}
                }}"#
            );
            let (program, diagnostics) = parse_source(&source);
            assert!(
                !diagnostics.is_empty(),
                "expected a diagnostic for version string {bad:?}"
            );
            assert!(
                program.declarations.is_empty(),
                "rejected block must not emit declarations for {bad:?}, got: {:?}",
                program.declarations
            );
        }
    }

    /// A multi-segment version whose individual segments are malformed is
    /// rejected: an internal empty segment (`v1//beta` -> `/v1//beta/posts`), a
    /// `.` segment, or a `..` segment all pass the char allowlist yet would
    /// malform the route or inject a traversal. `.` remains legal *inside* a
    /// segment (covered by `parse_api_version_slash_form` et al.).
    #[test]
    fn parse_api_version_bad_segments_rejected() {
        for bad in ["v1//beta", "v1/./beta", "v1/../beta", "v1/."] {
            let source = format!(
                r#"api version "{bad}" {{
                    endpoint a: GET "/posts" {{ response Post }}
                }}"#
            );
            let (program, diagnostics) = parse_source(&source);
            assert!(
                !diagnostics.is_empty(),
                "expected a diagnostic for version string {bad:?}"
            );
            assert!(
                program.declarations.is_empty(),
                "rejected block must not emit declarations for {bad:?}, got: {:?}",
                program.declarations
            );
        }
    }

    /// A doc comment preceding the `api version` block is consumed and dropped
    /// without error (it attaches to no single endpoint). Pins the current
    /// behavior so a future change that starts threading it through is a
    /// deliberate, test-visible decision.
    #[test]
    fn parse_api_version_block_doc_comment_dropped() {
        let source = r#"/** Versioned slice of the API. */
        api version "v1" {
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "a");
                // The block doc comment does NOT leak onto the endpoint.
                assert_eq!(ep.doc_comment, None);
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// A doc comment on an endpoint *inside* the block DOES attach to that
    /// endpoint — only the block-level doc comment (preceding `api`) is dropped.
    /// This pins the threading distinction so the two cases can't silently swap.
    #[test]
    fn parse_api_version_inner_endpoint_doc_comment_kept() {
        let source = r#"api version "v1" {
            /** List all posts. */
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.name, "a");
                assert_eq!(ep.api_version.as_deref(), Some("v1"));
                assert!(
                    ep.doc_comment
                        .as_deref()
                        .is_some_and(|d| d.contains("List all posts")),
                    "inner endpoint doc comment should be preserved, got: {:?}",
                    ep.doc_comment
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// `public` cannot precede `api`; the block parser reports it but still
    /// recovers the contained endpoints.
    #[test]
    fn parse_api_version_rejects_public() {
        let source = r#"public api version "v1" {
            endpoint a: GET "/posts" { response Post }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for `public` before `api`"
        );
        // Recovery still tags the endpoint with its version.
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.api_version.as_deref(), Some("v1"));
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    /// An empty `api version` block parses with no error and contributes no
    /// declarations (a harmless no-op).
    #[test]
    fn parse_api_version_empty_block() {
        let source = r#"api version "v1" {
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 0);
    }

    // ── Schema declaration tests ────────────────────────────────────

    /// A simple schema with one table parses.
    #[test]
    fn parse_schema_basic() {
        let source = r#"
struct User { id: Int  name: String }
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
        token: String primary key
        userId: Int
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
struct User { id: Int  name: String }
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
            query { page: Int = 1 }
            query { limit: Int = 20 }
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

    // ── Header tests ──────────────────────────────────────────────────

    #[test]
    fn parse_endpoint_request_headers() {
        let source = r#"endpoint createUser: POST "/api/users" {
            headers {
                authorization: String
                idempotencyKey: String
            }
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.headers.len(), 2);
                assert_eq!(ep.headers[0].name, "authorization");
                assert!(ep.headers[0].wire_name.is_none());
                assert!(ep.headers[0].default_value.is_none());
                match &ep.headers[0].type_annotation {
                    TypeExpr::Named(n) => assert_eq!(n.name, "String"),
                    other => panic!("expected Named(String), got {:?}", other),
                }
                assert_eq!(ep.headers[1].name, "idempotencyKey");
                assert!(ep.headers[1].wire_name.is_none());
                assert!(ep.response_headers.is_empty());
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_header_wire_override() {
        let source = r#"endpoint createUser: POST "/api/users" {
            headers {
                rateLimit: String as "X-RateLimit-Limit"
            }
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.headers.len(), 1);
                assert_eq!(ep.headers[0].name, "rateLimit");
                assert_eq!(
                    ep.headers[0].wire_name.as_deref(),
                    Some("X-RateLimit-Limit")
                );
                assert!(ep.headers[0].default_value.is_none());
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_header_override_and_default() {
        let source = r#"endpoint createUser: POST "/api/users" {
            headers {
                contentType: String as "Content-Type" = "application/json"
            }
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.headers.len(), 1);
                assert_eq!(ep.headers[0].name, "contentType");
                assert_eq!(ep.headers[0].wire_name.as_deref(), Some("Content-Type"));
                match &ep.headers[0].default_value {
                    Some(Expr::Literal(Literal {
                        kind: LiteralKind::String(s),
                        ..
                    })) => assert_eq!(s, "application/json"),
                    other => panic!(
                        "expected default String(\"application/json\"), got {:?}",
                        other
                    ),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_response_headers() {
        let source = r#"endpoint getPost: GET "/api/posts/{id}" {
            response Post headers {
                ratelimitRemaining: Int as "X-RateLimit-Remaining"
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                // Response type is still the bare body type.
                match ep.response.as_ref().expect("should have response") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Post"),
                    other => panic!("expected Named(Post), got {:?}", other),
                }
                assert!(ep.headers.is_empty(), "no request headers expected");
                assert_eq!(ep.response_headers.len(), 1);
                assert_eq!(ep.response_headers[0].name, "ratelimitRemaining");
                assert_eq!(
                    ep.response_headers[0].wire_name.as_deref(),
                    Some("X-RateLimit-Remaining")
                );
                match &ep.response_headers[0].type_annotation {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Int"),
                    other => panic!("expected Named(Int), got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_request_and_response_headers() {
        let source = r#"endpoint getPost: GET "/api/posts/{id}" {
            headers {
                authorization: String
            }
            response Post headers {
                ratelimitRemaining: Int as "X-RateLimit-Remaining"
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.headers.len(), 1, "request headers go to `headers`");
                assert_eq!(ep.headers[0].name, "authorization");
                assert_eq!(
                    ep.response_headers.len(),
                    1,
                    "response headers go to `response_headers`"
                );
                assert_eq!(ep.response_headers[0].name, "ratelimitRemaining");
                match ep.response.as_ref().expect("should have response") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "Post"),
                    other => panic!("expected Named(Post), got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_duplicate_headers() {
        let source = r#"endpoint listUsers: GET "/api/users" {
            headers { authorization: String }
            headers { idempotencyKey: String }
        }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate `headers`")),
            "should report duplicate headers: {:?}",
            diagnostics
        );
    }

    // ── Pagination tests ──────────────────────────────────────────────

    #[test]
    fn parse_pagination_offset() {
        let source = r#"endpoint p: GET "/x" { response List<Post> pagination { offset } }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.pagination, Some(PaginationMode::Offset));
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_pagination_cursor() {
        let source = r#"endpoint p: GET "/x" { response List<Post> pagination { cursor } }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.pagination, Some(PaginationMode::Cursor));
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_pagination_unknown_mode_rejected() {
        let source = r#"endpoint p: GET "/x" { response List<Post> pagination { bogus } }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("expected `offset` or `cursor`")),
            "should reject unknown pagination mode: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_pagination_empty_rejected() {
        let source = r#"endpoint p: GET "/x" { response List<Post> pagination { } }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("expected `offset` or `cursor`")),
            "should reject empty pagination block: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_pagination_duplicate_rejected() {
        let source = r#"endpoint p: GET "/x" {
            response List<Post>
            pagination { offset }
            pagination { cursor }
        }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate `pagination`")),
            "should report duplicate pagination: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_endpoint_without_pagination() {
        let source = r#"endpoint p: GET "/x" { response List<Post> }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.pagination, None);
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_request_headers_after_response() {
        // A `headers` block on its OWN line (not immediately after the response
        // type) is the standalone REQUEST section, even when it textually follows
        // `response`. Only `response Type headers { ... }` on the same line binds
        // as response headers — this keeps section ordering free and prevents a
        // request header from silently rebinding to the response.
        let source = r#"endpoint getPost: GET "/api/posts/{id}" {
            response Post
            headers {
                authorization: String
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(
                    ep.headers.len(),
                    1,
                    "a `headers` block on a new line is a request header"
                );
                assert_eq!(ep.headers[0].name, "authorization");
                assert!(
                    ep.response_headers.is_empty(),
                    "must not rebind to response headers"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_endpoint_response_headers_without_type_errors() {
        // Response headers are bundled with the body into an envelope, so a bare
        // `response headers { ... }` with no type is rejected with a targeted
        // message (not a generic "expected type name"), and must not leave a
        // dangling response-headers section behind.
        let source = r#"endpoint ping: GET "/api/ping" {
            response headers {
                ratelimitRemaining: Int as "X-RateLimit-Remaining"
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("response headers require a response type")),
            "should report missing response type: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert!(ep.response.is_none(), "no response type was given");
                assert!(
                    ep.response_headers.is_empty(),
                    "the malformed block must be consumed, not bound as response headers"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    // ── Multi-status response block tests ─────────────────────────────

    #[test]
    fn parse_response_block_multi_status() {
        let source = r#"endpoint upsertUser: PUT "/api/users/{id}" {
            response {
                200: User
                201: User
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert!(
                    ep.response.is_none(),
                    "block form leaves bare `response` None"
                );
                assert_eq!(ep.response_statuses.len(), 2);
                assert_eq!(ep.response_statuses[0].status, 200);
                match ep.response_statuses[0].ty.as_ref().expect("typed entry") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User), got {:?}", other),
                }
                assert_eq!(ep.response_statuses[1].status, 201);
                match ep.response_statuses[1].ty.as_ref().expect("typed entry") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User), got {:?}", other),
                }
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_block_with_typeless() {
        let source = r#"endpoint updateUser: PUT "/api/users/{id}" {
            response {
                200: User
                204
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert!(ep.response.is_none());
                assert_eq!(ep.response_statuses.len(), 2);
                assert_eq!(ep.response_statuses[0].status, 200);
                match ep.response_statuses[0].ty.as_ref().expect("typed entry") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User), got {:?}", other),
                }
                assert_eq!(ep.response_statuses[1].status, 204);
                assert!(
                    ep.response_statuses[1].ty.is_none(),
                    "204 is a typeless entry"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_block_all_typeless() {
        let source = r#"endpoint acceptJob: POST "/api/jobs" {
            response {
                202
                204
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert!(ep.response.is_none());
                assert_eq!(ep.response_statuses.len(), 2);
                assert_eq!(ep.response_statuses[0].status, 202);
                assert!(ep.response_statuses[0].ty.is_none());
                assert_eq!(ep.response_statuses[1].status, 204);
                assert!(ep.response_statuses[1].ty.is_none());
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_bare_response_unchanged() {
        let source = r#"endpoint getUser: GET "/api/users/{id}" {
            response User
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                match ep.response.as_ref().expect("should have response") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User), got {:?}", other),
                }
                assert!(
                    ep.response_statuses.is_empty(),
                    "bare form leaves `response_statuses` empty"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_bare_response_with_headers_unchanged() {
        let source = r#"endpoint getUser: GET "/api/users/{id}" {
            response User headers {
                x: String
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                match ep.response.as_ref().expect("should have response") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User), got {:?}", other),
                }
                assert_eq!(ep.response_headers.len(), 1);
                assert_eq!(ep.response_headers[0].name, "x");
                assert!(
                    ep.response_statuses.is_empty(),
                    "bare form leaves `response_statuses` empty"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_block_missing_status_errors() {
        let source = r#"endpoint badEndpoint: GET "/api/bad" {
            response {
                : User
            }
        }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            !diagnostics.is_empty(),
            "missing status integer should be a parse error"
        );
    }

    #[test]
    fn parse_response_block_empty_errors() {
        // An empty `response { }` declares nothing; without a diagnostic it
        // would silently behave as "no response declared".
        let source = r#"endpoint badEndpoint: GET "/api/bad" {
            response { }
        }"#;
        let (_, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("at least one status")),
            "empty response block should be a parse error, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_response_block_inline_headers_rejected() {
        // A same-line `headers { ... }` after the block is the response-header
        // spelling of the bare form; multi-status cannot carry response headers,
        // so it must be a targeted parse error — NOT silently re-dispatched as
        // the request `headers` section (which would turn the would-be response
        // headers into handler/client inputs).
        let source = r#"endpoint upsertUser: PUT "/api/users/{id}" {
            response { 200: User } headers {
                x: String
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot declare response headers")),
            "inline headers after a response block should be a parse error, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.response_statuses.len(), 1);
                assert!(
                    ep.response_headers.is_empty(),
                    "the rejected block must not become response headers"
                );
                assert!(
                    ep.headers.is_empty(),
                    "the rejected block must not become request headers"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_block_newline_headers_stays_request_section() {
        // A `headers { ... }` block on its OWN line after a response block is the
        // standalone request-headers section (section ordering is free), exactly
        // as for the bare `response <Type>` form.
        let source = r#"endpoint upsertUser: PUT "/api/users/{id}" {
            response { 200: User }
            headers {
                x: String
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.response_statuses.len(), 1);
                assert_eq!(ep.headers.len(), 1, "request headers section expected");
                assert!(ep.response_headers.is_empty());
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_block_out_of_range_status_errors() {
        // A literal that does not fit a u16 is reported by its written text,
        // not folded to a sentinel `0` that sema would then complain about.
        let source = r#"endpoint badEndpoint: GET "/api/bad" {
            response {
                70000: User
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("invalid HTTP status code `70000`")),
            "out-of-range status should be a parse error, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => assert!(
                ep.response_statuses.is_empty(),
                "the invalid entry must be dropped, not folded to status 0"
            ),
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_block_missing_colon_hint() {
        // `200 User` (forgotten colon) must name the actual mistake — not let
        // `User` fall through to the next iteration's integer expect and report
        // "expected integer literal" — and recover the entry as typed.
        let source = r#"endpoint upsertUser: PUT "/api/users/{id}" {
            response {
                200 User
                204
            }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("missing `:`") && d.message.contains("`200: User`")),
            "forgotten colon should get a targeted hint, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.response_statuses.len(), 2, "both entries recovered");
                match ep.response_statuses[0].ty.as_ref().expect("recovers typed") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User), got {:?}", other),
                }
                assert_eq!(ep.response_statuses[1].status, 204);
                assert!(
                    ep.response_statuses[1].ty.is_none(),
                    "a newline-separated typeless entry must not trip the hint"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    #[test]
    fn parse_response_block_comma_separated() {
        // The one-line comma-separated spelling (the natural habit from
        // `omit { a, b }`, and what the roadmap sketch used) must parse
        // identically to the newline form, trailing comma included. A comma
        // after a typeless entry must not turn it typed, and a comma must not
        // trip the missing-colon hint.
        let source = r#"endpoint upsertUser: PUT "/api/users/{id}" {
            response { 200: User, 201: User, 204, }
        }"#;
        let (program, diagnostics) = parse_source(source);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Endpoint(ep) => {
                assert_eq!(ep.response_statuses.len(), 3);
                assert_eq!(ep.response_statuses[0].status, 200);
                match ep.response_statuses[0].ty.as_ref().expect("typed entry") {
                    TypeExpr::Named(n) => assert_eq!(n.name, "User"),
                    other => panic!("expected Named(User), got {:?}", other),
                }
                assert_eq!(ep.response_statuses[1].status, 201);
                assert!(ep.response_statuses[1].ty.is_some());
                assert_eq!(ep.response_statuses[2].status, 204);
                assert!(
                    ep.response_statuses[2].ty.is_none(),
                    "a comma after a typeless entry must keep it typeless"
                );
            }
            other => panic!("expected Endpoint, got {:?}", other),
        }
    }

    // ── Field doc comment tests ───────────────────────────────────────

    #[test]
    fn parse_struct_field_doc_comment() {
        let source = r#"struct User {
            /** The user's full name */
            name: String
            age: Int
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
            id: Int
            /** Display name */
            name: String
            age: Int
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
        let (program, diagnostics) = parse_source("struct Scene { hero: dyn Drawable }");
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
    fn parse_import_namespace_bare() {
        // `import models.user` — namespace import, no braces; bound name is
        // the last path segment.
        let (program, diagnostics) = parse_source("import models.user\nfunction main() {}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Import(imp) => {
                assert_eq!(imp.path, vec!["models", "user"]);
                match &imp.items {
                    ImportItems::Namespace { alias } => assert!(alias.is_none()),
                    other => panic!("expected Namespace items, got {:?}", other),
                }
            }
            other => panic!("expected Import decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_namespace_aliased() {
        // `import models.user as people` — namespace import with an alias.
        let (program, diagnostics) =
            parse_source("import models.user as people\nfunction main() {}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Import(imp) => {
                assert_eq!(imp.path, vec!["models", "user"]);
                match &imp.items {
                    ImportItems::Namespace { alias } => {
                        assert_eq!(alias.as_deref(), Some("people"));
                    }
                    other => panic!("expected Namespace items, got {:?}", other),
                }
            }
            other => panic!("expected Import decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_namespace_single_segment() {
        // `import json` — single-segment namespace import (the idiomatic
        // stdlib form).
        let (program, diagnostics) = parse_source("import json\nfunction main() {}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Import(imp) => {
                assert_eq!(imp.path, vec!["json"]);
                match &imp.items {
                    ImportItems::Namespace { alias } => assert!(alias.is_none()),
                    other => panic!("expected Namespace items, got {:?}", other),
                }
            }
            other => panic!("expected Import decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_namespace_rejects_trailing_garbage() {
        // `import models.user foo` — a stray token after the path is neither
        // a brace, an `as` alias, nor end-of-declaration; it must be a parse
        // error rather than silently binding `models.user` and leaving `foo`
        // to be mis-parsed as the next declaration.
        let (_program, diagnostics) = parse_source("import models.user foo\nfunction main() {}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("after the import path")),
            "expected a trailing-token diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_import_namespace_rejects_missing_alias() {
        // `import models.user as` — the `as` keyword with no following
        // identifier must produce a parse error, not bind a namespace with a
        // bogus or missing alias.
        let (_program, diagnostics) = parse_source("import models.user as\nfunction main() {}");
        assert!(
            !diagnostics.is_empty(),
            "expected a diagnostic for the missing alias identifier"
        );
    }

    #[test]
    fn parse_import_rejects_list_on_next_line() {
        // `import models.user⏎{ User }` — the import list on the line after
        // the path is a common mistake. It must surface a diagnostic that
        // names the problem directly, rather than silently parsing the path
        // as a namespace import and mis-reporting the `{` as a stray token in
        // the next declaration.
        let (_program, diagnostics) =
            parse_source("import models.user\n{ User }\nfunction main() {}");
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("must be on the same line as the import path")),
            "expected a same-line import-list diagnostic, got: {:?}",
            diagnostics
        );
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
            public name: String
            passwordHash: String
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
            parse_source("struct P { x: Int }\npublic impl P { function f(self) {} }");
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

    // ── extern js interop tests ──────────────────

    #[test]
    fn parse_extern_js_basic() {
        let (program, diagnostics) =
            parse_source("extern js {\n  function alert(message: String)\n}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                let sig = &block.items[0];
                assert_eq!(sig.name, "alert");
                assert_eq!(sig.params.len(), 1);
                assert_eq!(sig.params[0].name, "message");
                assert!(sig.return_type.is_none());
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_extern_js_ambient_has_no_module() {
        // `extern js { ... }` (no specifier) binds the ambient host — module None.
        let (program, diagnostics) =
            parse_source("extern js {\n  function alert(message: String)\n}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::ExternJs(block) => assert!(block.module.is_none()),
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_extern_js_with_module_specifier() {
        // `extern js "left-pad" { ... }` names an npm package host module.
        let src = "extern js \"left-pad\" {\n  \
                   function leftPad(s: String, width: Int) -> String\n}";
        let (program, diagnostics) = parse_source(src);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.module.as_deref(), Some("left-pad"));
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "leftPad");
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_extern_js_scoped_and_subpath_specifiers() {
        // Scoped (`@scope/pkg`) and subpath (`pkg/sub`) specifiers are accepted
        // verbatim — validation against `[js-dependencies]` is a later PR.
        for spec in ["@scope/pkg", "pkg/sub/mod"] {
            let src = format!("extern js \"{spec}\" {{\n  function f() -> Int\n}}");
            let (program, diagnostics) = parse_source(&src);
            assert!(
                diagnostics.is_empty(),
                "errors for `{spec}`: {:?}",
                diagnostics
            );
            match &program.declarations[0] {
                Declaration::ExternJs(block) => assert_eq!(block.module.as_deref(), Some(spec)),
                other => panic!("expected ExternJs decl, got {:?}", other),
            }
        }
    }

    #[test]
    fn parse_extern_js_empty_specifier_errors() {
        // `extern js ""` names no module — a diagnostic, and the block falls back
        // to no module (parsed best-effort).
        let (program, diagnostics) = parse_source("extern js \"\" {\n  function f() -> Int\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("must not be empty")),
            "expected an empty-specifier error; got {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => assert!(block.module.is_none()),
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_extern_js_specifier_with_escapes_or_whitespace_errors() {
        // The lexer keeps escape sequences raw (`strip_string_literal_quotes`
        // strips only the quotes), so a specifier with a backslash — e.g. the
        // escaped spelling `"left\u002Dpad"` — would flow downstream verbatim
        // and never match the npm package it means; whitespace likewise names
        // no npm package. Both are rejected best-effort (block parses, no
        // module). Rejecting backslashes also keeps an escaped spelling of a
        // reserved specifier (`"js"`, `"wasi_snapshot_preview1"`) from
        // bypassing its dedicated check.
        for src in [
            "extern js \"left\\u002Dpad\" {\n  function f() -> Int\n}",
            "extern js \" \" {\n  function f() -> Int\n}",
            "extern js \"left pad\" {\n  function f() -> Int\n}",
        ] {
            let (program, diagnostics) = parse_source(src);
            assert!(
                diagnostics
                    .iter()
                    .any(|d| d.message.contains("whitespace or escape sequences")),
                "expected a malformed-specifier error for `{src}`; got {:?}",
                diagnostics
            );
            match &program.declarations[0] {
                Declaration::ExternJs(block) => assert!(block.module.is_none()),
                other => panic!("expected ExternJs decl, got {:?}", other),
            }
        }
    }

    #[test]
    fn parse_extern_js_specifier_with_interpolation_braces_errors() {
        // `{name}` is string interpolation in expression position, but the
        // specifier is taken raw (the lexer keeps braces inside the literal
        // token), so `extern js "{pkg}"` would silently bind a literal `{pkg}`
        // namespace no package can ever match. Rejected best-effort like the
        // whitespace/backslash cases (block parses, no module).
        for src in [
            "extern js \"{pkg}\" {\n  function f() -> Int\n}",
            "extern js \"left-{sep}pad\" {\n  function f() -> Int\n}",
        ] {
            let (program, diagnostics) = parse_source(src);
            assert!(
                diagnostics.iter().any(|d| d
                    .message
                    .contains("string interpolation is not supported here")),
                "expected a brace-specifier error for `{src}`; got {:?}",
                diagnostics
            );
            match &program.declarations[0] {
                Declaration::ExternJs(block) => assert!(block.module.is_none()),
                other => panic!("expected ExternJs decl, got {:?}", other),
            }
        }
    }

    #[test]
    fn parse_extern_js_reserved_js_specifier_errors() {
        // `extern js "js"` spells out the ambient host's module name — rejected
        // (it would silently alias `extern js { ... }` and read as an npm
        // package named `js`); the block falls back to no module, best-effort.
        let (program, diagnostics) = parse_source("extern js \"js\" {\n  function f() -> Int\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("not a module specifier")),
            "expected a reserved-specifier error; got {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => assert!(block.module.is_none()),
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_extern_js_reserved_wasi_specifier_errors() {
        // `extern js "wasi_snapshot_preview1"` claims the import namespace the
        // runtime's WASI shim owns — in the generated glue's imports object the
        // extern namespace would be a duplicate key (later wins), clobbering the
        // shim and failing instantiation far from the mistake. Rejected like the
        // reserved `"js"`; the block falls back to no module, best-effort.
        let (program, diagnostics) =
            parse_source("extern js \"wasi_snapshot_preview1\" {\n  function f() -> Int\n}");
        assert!(
            diagnostics.iter().any(|d| d
                .message
                .contains("reserved for the Phoenix runtime's WASI shim")),
            "expected a reserved-specifier error; got {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => assert!(block.module.is_none()),
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn parse_extern_js_multiple_sigs_closure_param_and_return() {
        // Exercises a closure-typed param `(Void) -> Void`, a second param,
        // and a separate signature with a return type.
        let src = "extern js {\n  \
                   function setTimeout(callback: (Void) -> Void, ms: Int)\n  \
                   function now() -> Int\n}";
        let (program, diagnostics) = parse_source(src);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 2);
                assert_eq!(block.items[0].name, "setTimeout");
                assert_eq!(block.items[0].params.len(), 2);
                assert_eq!(block.items[0].params[0].name, "callback");
                assert_eq!(block.items[0].params[1].name, "ms");
                assert!(block.items[0].return_type.is_none());
                assert_eq!(block.items[1].name, "now");
                assert!(block.items[1].params.is_empty());
                assert!(block.items[1].return_type.is_some());
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_closure_return_type() {
        // A closure-typed *return* position (`-> (Int) -> Bool`) must parse
        // through the same `parse_type_expr` reuse as params — locks in that
        // `end`-span tracking and the arrow handling hold for return types,
        // not just for parameters.
        let (program, diagnostics) =
            parse_source("extern js {\n  function make() -> (Int) -> Bool\n}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                let sig = &block.items[0];
                assert_eq!(sig.name, "make");
                assert!(
                    matches!(sig.return_type, Some(TypeExpr::Function(_))),
                    "expected a function-typed return, got {:?}",
                    sig.return_type
                );
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_multiple_sigs_no_newline_separator() {
        // The block loop must not depend on a newline between signatures: two
        // `function`s on one line still parse as two distinct items, since each
        // `parse_extern_fn_sig` stops at the next `function` token.
        let (program, diagnostics) = parse_source("extern js { function f() function g() }");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 2);
                assert_eq!(block.items[0].name, "f");
                assert_eq!(block.items[1].name, "g");
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_signature_body_is_rejected() {
        // A `{ ... }` body on an extern signature is a targeted error, but the
        // stray block is consumed and the signature is still recorded.
        let (program, diagnostics) =
            parse_source("extern js {\n  function alert(message: String) { }\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot have a body")),
            "expected a body-rejection diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "alert");
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_return_type_and_body_both_present() {
        // A signature with both a return type *and* a (rejected) body exercises
        // the `end`-span ordering: `parse_type_expr` first sets `end` to the
        // return type, then the rejected body overwrites it with the block
        // span. The body is still a targeted error, and the signature — return
        // type included — is recorded for recovery.
        let (program, diagnostics) = parse_source("extern js {\n  function f() -> Int { }\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot have a body")),
            "expected a body-rejection diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "f");
                assert!(
                    block.items[0].return_type.is_some(),
                    "return type should be recorded despite the rejected body"
                );
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_malformed_body_after_valid_header_recovers() {
        // A valid signature header followed by a *malformed* `{ ... }` body
        // exercises the body-rejection path where `parse_block` recovers
        // mid-block rather than returning a clean span: the "cannot have a
        // body" diagnostic still fires, `parse_block` consumes through the
        // closing brace, and the signature is recorded. Distinct from
        // `extern_js_return_type_and_body_both_present`, which uses a
        // well-formed empty body. The test completing at all also proves the
        // block loop makes progress through the recovered body.
        let (program, diagnostics) = parse_source("extern js {\n  function f() { let }\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot have a body")),
            "expected a body-rejection diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "f");
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_omitted_tag_is_rejected() {
        // An omitted language tag (`extern { ... }`) gets the tailored
        // "expected `js`" diagnostic — not a generic "expected identifier" —
        // and the block is still parsed best-effort.
        let (program, diagnostics) = parse_source("extern {\n  function f()\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("only `extern js`")),
            "expected an `extern js`-only diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "f");
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_omitted_braces_is_rejected() {
        // The `js` tag is present but the `{` is omitted (`extern js function
        // f()`). `expect(LBrace)` emits a diagnostic and the block is dropped —
        // distinct from the omitted-tag and unclosed-block recovery paths.
        let (_, diagnostics) = parse_source("extern js function f()");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("expected '{'")),
            "expected a missing-brace diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn extern_js_non_js_tag_is_rejected() {
        let (_, diagnostics) = parse_source("extern wasm {\n  function f()\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("only `extern js`")),
            "expected an `extern js`-only diagnostic, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn public_on_extern_js_rejected() {
        let (_, diagnostics) = parse_source("public extern js {\n  function f()\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot be marked `public`")),
            "expected diagnostic about `public extern`, got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn parse_extern_js_empty_block() {
        // An empty `extern js { }` is valid: no signatures, no diagnostics.
        let (program, diagnostics) = parse_source("extern js {\n}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        assert_eq!(program.declarations.len(), 1);
        match &program.declarations[0] {
            Declaration::ExternJs(block) => assert!(block.items.is_empty()),
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_doc_comment_on_sig_is_discarded() {
        // A doc comment before a signature is consumed and discarded (matching
        // how top-level functions treat a leading doc comment), not flagged as a
        // spurious "expected `function`" error. A trailing doc comment with no
        // following signature is likewise swallowed cleanly before the `}`.
        let src = "extern js {\n  \
                   /** Pop a dialog. */\n  function alert(message: String)\n  \
                   /** trailing, no sig */\n}";
        let (program, diagnostics) = parse_source(src);
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "alert");
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_param_default_value_is_rejected() {
        // The JS host cannot evaluate a Phoenix default; the parser rejects it
        // but still records the signature (with the parsed param) for recovery.
        let (program, diagnostics) = parse_source("extern js {\n  function f(x: Int = 5)\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot have default values")),
            "expected a default-value rejection diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "f");
                assert_eq!(block.items[0].params.len(), 1);
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_generic_type_params_are_rejected() {
        // Generic type parameters get a targeted diagnostic (not a confusing
        // "expected `(`"), and the rest of the signature still parses.
        let (program, diagnostics) = parse_source("extern js {\n  function f<T>(x: T)\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot have generic type parameters")),
            "expected a generics rejection diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "f");
                assert_eq!(block.items[0].params.len(), 1);
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_body_garbage_terminates_with_diagnostics() {
        // Anti-hang: a malformed extern-block body must produce a diagnostic
        // and terminate. The test completing at all is the anti-hang proof;
        // see `parse_extern_js_block`'s doc comment for why every loop
        // iteration is guaranteed to make progress.
        for source in [
            "extern js { 42 }",
            "extern js { let x }",
            "extern js { function }",
            "extern js { function f( }",
            "extern js {",
        ] {
            let (_, diagnostics) = parse_source(source);
            assert!(
                !diagnostics.is_empty(),
                "expected diagnostics for {source:?}"
            );
        }
    }

    #[test]
    fn extern_js_self_param_is_rejected() {
        // Extern signatures declare free functions, not methods; a `self`
        // receiver is rejected but the signature is still recorded.
        let (program, diagnostics) = parse_source("extern js {\n  function f(self)\n}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("cannot take `self`")),
            "expected a `self`-rejection diagnostic, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "f");
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_js_bounded_generics_single_diagnostic() {
        // A bounded generic (`<T: Trait>`) is consumed cleanly via the
        // bounds-aware parser, so it produces exactly the one targeted
        // generics-rejection diagnostic — no spurious secondary "expected `>`".
        let (program, diagnostics) = parse_source("extern js {\n  function f<T: Display>(x: T)\n}");
        let generics_errors = diagnostics
            .iter()
            .filter(|d| d.message.contains("cannot have generic type parameters"))
            .count();
        assert_eq!(
            generics_errors, 1,
            "expected exactly one generics diagnostic, got: {:?}",
            diagnostics
        );
        assert_eq!(
            diagnostics.len(),
            1,
            "expected no other diagnostics, got: {:?}",
            diagnostics
        );
        match &program.declarations[0] {
            Declaration::ExternJs(block) => {
                assert_eq!(block.items.len(), 1);
                assert_eq!(block.items[0].name, "f");
                assert_eq!(block.items[0].params.len(), 1);
            }
            other => panic!("expected ExternJs decl, got {:?}", other),
        }
    }

    #[test]
    fn js_remains_usable_as_identifier() {
        // The `js` language tag is contextual: outside `extern js`, `js` is an
        // ordinary identifier and stays usable as a variable name.
        let (program, diagnostics) = parse_source("function main() {\n  let js = 5\n}");
        assert!(diagnostics.is_empty(), "errors: {:?}", diagnostics);
        match &program.declarations[0] {
            Declaration::Function(f) => assert_eq!(f.name, "main"),
            other => panic!("expected Function decl, got {:?}", other),
        }
    }

    #[test]
    fn extern_is_reserved_as_identifier() {
        // Counterpart to `js_remains_usable_as_identifier`: `extern` is now a
        // full keyword (unlike the contextual `js` tag), so it can no longer be
        // used as an ordinary identifier. `let extern = 5` must diagnose rather
        // than bind a variable named `extern`.
        let (_, diagnostics) = parse_source("function main() {\n  let extern = 5\n}");
        assert!(
            !diagnostics.is_empty(),
            "expected `extern` to be rejected as an identifier"
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
            n: Int
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
        let src = "struct P { x: Int }\nimpl P {
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
        let src = "struct P { x: Int }
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
struct P { x: Int
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
struct P { x: Int
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
