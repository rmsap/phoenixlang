use crate::ast::{FunctionType, GenericType, NamedType, TypeExpr};
use crate::parser::Parser;
use phoenix_lexer::token::TokenKind;

impl<'src> Parser<'src> {
    /// Parses a type expression.
    ///
    /// Supports named types (`Int`, `String`, custom types) and function
    /// types (`(Int, Int) -> Bool`).
    pub fn parse_type_expr(&mut self) -> Option<TypeExpr> {
        let token = self.peek();
        match token.kind {
            // Function type: (ParamType, ParamType) -> ReturnType
            TokenKind::LParen => self.parse_function_type(),

            // Named type: Int, Float, String, Bool, Void, or user-defined
            // May also be a generic type application: Option<Int>, Pair<Int, String>
            TokenKind::IntType
            | TokenKind::FloatType
            | TokenKind::StringType
            | TokenKind::BoolType
            | TokenKind::Void
            | TokenKind::Ident => {
                let token = self.advance();
                // Check for generic type application: Name<Type, Type>
                if self.peek().kind == TokenKind::Lt {
                    let start = token.span;
                    self.advance(); // consume '<'
                    let type_args =
                        self.parse_comma_separated(TokenKind::Gt, |p| p.parse_type_expr());
                    let end = self.expect(TokenKind::Gt)?.span;
                    Some(TypeExpr::Generic(GenericType {
                        name: token.text.clone(),
                        type_args,
                        span: start.merge(end),
                    }))
                } else {
                    Some(TypeExpr::Named(NamedType {
                        name: token.text.clone(),
                        span: token.span,
                    }))
                }
            }
            _ => {
                self.error_at_current("expected type name");
                None
            }
        }
    }

    /// Parses a function type: `(ParamType, ParamType) -> ReturnType`.
    fn parse_function_type(&mut self) -> Option<TypeExpr> {
        let start = self.peek().span;
        self.expect(TokenKind::LParen)?;

        let param_types = self.parse_comma_separated(TokenKind::RParen, |p| p.parse_type_expr());
        self.expect(TokenKind::RParen)?;
        self.expect(TokenKind::Arrow)?;
        let return_type = self.parse_type_expr()?;
        let span = start.merge(return_type_span(&return_type));

        Some(TypeExpr::Function(FunctionType {
            param_types,
            return_type: Box::new(return_type),
            span,
        }))
    }

    /// Returns `true` if the current token could begin a type expression.
    pub fn is_type_start(&self) -> bool {
        matches!(
            self.peek().kind,
            TokenKind::IntType
                | TokenKind::FloatType
                | TokenKind::StringType
                | TokenKind::BoolType
                | TokenKind::Void
                | TokenKind::Ident
                | TokenKind::LParen
        )
    }
}

/// Helper to extract the span from a TypeExpr.
fn return_type_span(t: &TypeExpr) -> phoenix_common::span::Span {
    match t {
        TypeExpr::Named(n) => n.span,
        TypeExpr::Function(f) => f.span,
        TypeExpr::Generic(g) => g.span,
    }
}
