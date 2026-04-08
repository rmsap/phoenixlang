use crate::ast::*;
use crate::parser::Parser;
use phoenix_lexer::token::TokenKind;

impl<'src> Parser<'src> {
    /// Parses a single statement inside a block.
    ///
    /// Returns `None` when the current token cannot start a valid statement,
    /// in which case a diagnostic is recorded and the caller should synchronize.
    pub fn parse_statement(&mut self) -> Option<Statement> {
        match self.peek().kind {
            TokenKind::Let => self.parse_var_decl().map(Statement::VarDecl),
            TokenKind::Return => self.parse_return_stmt().map(Statement::Return),
            TokenKind::If => self.parse_if_stmt().map(Statement::If),
            TokenKind::While => self.parse_while_stmt().map(Statement::While),
            TokenKind::For => self.parse_for_stmt().map(Statement::For),
            // `break` — immediately exits the innermost enclosing loop.
            TokenKind::Break => {
                let span = self.advance().span;
                self.eat(TokenKind::Newline);
                Some(Statement::Break(span))
            }
            // `continue` — skips the rest of the loop body and jumps to the
            // next iteration of the innermost enclosing loop.
            TokenKind::Continue => {
                let span = self.advance().span;
                self.eat(TokenKind::Newline);
                Some(Statement::Continue(span))
            }
            _ => self.parse_expr_stmt().map(Statement::Expression),
        }
    }

    /// Parses a `let` variable declaration.
    ///
    /// Syntax:
    ///   `let [mut] name [: Type] = initializer`
    ///   `let TypeName { field1, field2 } = initializer`
    ///
    /// When the `: Type` annotation is omitted, the type is inferred from the
    /// initializer expression during semantic analysis.
    fn parse_var_decl(&mut self) -> Option<VarDecl> {
        let start = self.peek().span;
        self.expect(TokenKind::Let)?;
        let is_mut = self.eat(TokenKind::Mut);

        // Check for struct destructuring: uppercase Ident followed by `{`
        let next = self.peek();
        let is_destructure = next.kind == TokenKind::Ident
            && next.text.chars().next().is_some_and(|c| c.is_uppercase())
            && self.peek_at(1).kind == TokenKind::LBrace;

        if is_destructure {
            // Parse: TypeName { field1, field2 } = initializer
            let type_name_token = self.advance(); // consume the type name
            self.expect(TokenKind::LBrace)?; // consume `{`

            let mut field_names = Vec::new();
            loop {
                if self.peek().kind == TokenKind::RBrace {
                    break;
                }
                let field_token = self.expect(TokenKind::Ident)?;
                field_names.push(field_token.text.clone());
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RBrace)?;
            self.expect(TokenKind::Eq)?;
            let initializer = self.parse_expr()?;
            let span = start.merge(initializer.span());
            self.eat(TokenKind::Newline);
            Some(VarDecl {
                is_mut,
                type_annotation: None,
                target: VarDeclTarget::StructDestructure {
                    type_name: type_name_token.text.clone(),
                    field_names,
                },
                initializer,
                span,
            })
        } else {
            // Parse: name [: Type] = initializer
            let name_token = self.expect(TokenKind::Ident)?;

            // Optional type annotation: `: Type`
            let type_annotation = if self.eat(TokenKind::Colon) {
                Some(self.parse_type_expr()?)
            } else {
                None
            };

            self.expect(TokenKind::Eq)?;
            let initializer = self.parse_expr()?;
            let span = start.merge(initializer.span());
            self.eat(TokenKind::Newline);
            Some(VarDecl {
                is_mut,
                type_annotation,
                target: VarDeclTarget::Simple(name_token.text.clone()),
                initializer,
                span,
            })
        }
    }

    fn parse_return_stmt(&mut self) -> Option<ReturnStmt> {
        let start = self.peek().span;
        self.expect(TokenKind::Return)?;
        let value = if !matches!(
            self.peek().kind,
            TokenKind::Newline | TokenKind::RBrace | TokenKind::Eof
        ) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let span = match &value {
            Some(v) => start.merge(v.span()),
            None => start,
        };
        self.eat(TokenKind::Newline);
        Some(ReturnStmt { value, span })
    }

    /// Parses `if`/`else if`/`else` chains.
    fn parse_if_stmt(&mut self) -> Option<IfStmt> {
        let start = self.peek().span;
        self.expect(TokenKind::If)?;

        let condition = self.parse_expr()?;
        let then_block = self.parse_block()?;

        let else_branch = if self.eat(TokenKind::Else) {
            if self.peek().kind == TokenKind::If {
                // else if — parse recursively
                Some(ElseBranch::ElseIf(Box::new(self.parse_if_stmt()?)))
            } else {
                Some(ElseBranch::Block(self.parse_block()?))
            }
        } else {
            None
        };

        let end = match &else_branch {
            Some(ElseBranch::Block(b)) => b.span,
            Some(ElseBranch::ElseIf(elif)) => elif.span,
            None => then_block.span,
        };

        Some(IfStmt {
            condition,
            then_block,
            else_branch,
            span: start.merge(end),
        })
    }

    /// Parses `while condition { body } [else { else_body }]`.
    fn parse_while_stmt(&mut self) -> Option<WhileStmt> {
        let start = self.peek().span;
        self.expect(TokenKind::While)?;
        let condition = self.parse_expr()?;
        let body = self.parse_block()?;
        let else_block = if self.eat(TokenKind::Else) {
            Some(self.parse_block()?)
        } else {
            None
        };
        let end_span = else_block.as_ref().map_or(body.span, |b| b.span);
        let span = start.merge(end_span);
        Some(WhileStmt {
            condition,
            body,
            else_block,
            span,
        })
    }

    /// Parses `for name [: Type] in start..end { body }` or `for name in expr { body }`.
    fn parse_for_stmt(&mut self) -> Option<ForStmt> {
        let start = self.peek().span;
        self.expect(TokenKind::For)?;
        let var_name = self.expect(TokenKind::Ident)?;

        // Optional type annotation: `: Type`
        let var_type = if self.eat(TokenKind::Colon) {
            Some(self.parse_type_expr()?)
        } else {
            None
        };

        self.expect(TokenKind::In)?;
        let first_expr = self.parse_expr()?;

        // If followed by `..`, this is a range-based loop; otherwise iterable
        let source = if self.eat(TokenKind::DotDot) {
            let end_expr = self.parse_expr()?;
            ForSource::Range {
                start: first_expr,
                end: end_expr,
            }
        } else {
            ForSource::Iterable(first_expr)
        };

        let body = self.parse_block()?;
        let else_block = if self.eat(TokenKind::Else) {
            Some(self.parse_block()?)
        } else {
            None
        };
        let end_span = else_block.as_ref().map_or(body.span, |b| b.span);
        let span = start.merge(end_span);
        Some(ForStmt {
            var_name: var_name.text.clone(),
            var_type,
            source,
            body,
            else_block,
            span,
        })
    }

    fn parse_expr_stmt(&mut self) -> Option<ExprStmt> {
        let expr = self.parse_expr()?;
        let span = expr.span();
        self.eat(TokenKind::Newline);
        Some(ExprStmt { expr, span })
    }
}
