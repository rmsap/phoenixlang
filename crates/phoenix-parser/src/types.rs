use crate::ast::{DynType, FunctionType, GenericType, NamedType, TypeExpr};
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

            // Trait object type: `dyn TraitName`
            TokenKind::Dyn => self.parse_dyn_type(),

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

    /// Parses a trait-object type: `dyn TraitName`.
    ///
    /// The returned [`DynType`] spans the `dyn` keyword through the trait
    /// name. Multi-bound object types (`dyn Foo + Bar`) and supertrait
    /// upcasting are out of scope — see `docs/design-decisions.md`.
    ///
    /// **Error recovery:** if the token following `dyn` is not an
    /// identifier, this records a diagnostic and returns `None` *without
    /// consuming* the offending token. That leaves recovery to the
    /// surrounding production (e.g. `parse_function_type` then re-encounters
    /// the same `)` and emits its own diagnostic, which is the right
    /// shape — the `dyn` site only knows "no trait name", not what the
    /// outer context expects). Pinned by `dyn_without_trait_name_errors`.
    fn parse_dyn_type(&mut self) -> Option<TypeExpr> {
        let dyn_span = self.advance().span; // consume `dyn`
        if self.peek().kind != TokenKind::Ident {
            self.error_at_current("expected trait name after `dyn`");
            return None;
        }
        let name_tok = self.advance();
        Some(TypeExpr::Dyn(DynType {
            trait_name: name_tok.text.clone(),
            span: dyn_span.merge(name_tok.span),
        }))
    }

    /// Parses a function type: `(ParamType, ParamType) -> ReturnType`.
    fn parse_function_type(&mut self) -> Option<TypeExpr> {
        let start = self.peek().span;
        self.expect(TokenKind::LParen)?;

        let param_types = self.parse_comma_separated(TokenKind::RParen, |p| p.parse_type_expr());
        self.expect(TokenKind::RParen)?;
        self.expect(TokenKind::Arrow)?;
        let return_type = self.parse_type_expr()?;
        let span = start.merge(return_type.span());

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
                | TokenKind::Dyn
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::TypeExpr;
    use phoenix_common::span::SourceId;
    use phoenix_lexer::tokenize;

    /// Tokenize `src` and return a `'static` slice of the tokens. The
    /// tokens are leaked — fine for tests, where each run is short-lived.
    fn lex_static(src: &str) -> &'static [phoenix_lexer::token::Token] {
        Vec::leak(tokenize(src, SourceId(0)))
    }

    #[test]
    fn parses_bare_dyn_trait() {
        let tokens = lex_static("dyn Drawable");
        let mut parser = Parser::new(tokens);
        let ty = parser.parse_type_expr();
        let Some(TypeExpr::Dyn(d)) = ty else {
            panic!("expected TypeExpr::Dyn, got {ty:?}");
        };
        assert_eq!(d.trait_name, "Drawable");
    }

    #[test]
    fn dyn_span_covers_keyword_through_trait_name() {
        let tokens = lex_static("dyn Drawable");
        let mut parser = Parser::new(tokens);
        let Some(TypeExpr::Dyn(d)) = parser.parse_type_expr() else {
            panic!("expected TypeExpr::Dyn");
        };
        assert_eq!(d.span.start, 0, "span must start at `dyn`");
        assert_eq!(
            d.span.end,
            "dyn Drawable".len(),
            "span must end after the trait identifier"
        );
    }

    #[test]
    fn dyn_without_trait_name_errors() {
        // `dyn )` — the parser should reject the `)` after `dyn` with a
        // trait-name diagnostic and return `None` rather than panicking or
        // silently synthesizing a trait.
        let tokens = lex_static("dyn )");
        let mut parser = Parser::new(tokens);
        let ty = parser.parse_type_expr();
        assert!(ty.is_none(), "expected None on missing trait name");
        assert!(
            !parser.diagnostics.is_empty(),
            "expected a diagnostic for `dyn` without trait name"
        );
    }

    #[test]
    fn dyn_in_generic_arg_is_parsed_as_dyn() {
        // `List<dyn Drawable>` — the inner type argument goes through the
        // same `parse_type_expr` entry as a bare type, so `dyn` must be
        // recognized at generic-arg position.
        let tokens = lex_static("List<dyn Drawable>");
        let mut parser = Parser::new(tokens);
        let ty = parser.parse_type_expr();
        let Some(TypeExpr::Generic(g)) = ty else {
            panic!("expected TypeExpr::Generic, got {ty:?}");
        };
        assert_eq!(g.name, "List");
        assert_eq!(g.type_args.len(), 1);
        assert!(
            matches!(&g.type_args[0], TypeExpr::Dyn(d) if d.trait_name == "Drawable"),
            "List's type arg should be dyn Drawable, got {:?}",
            g.type_args[0]
        );
    }

    #[test]
    fn dyn_in_function_return_position() {
        // `(Int) -> dyn Drawable` — the return-type slot of a function
        // type is another `parse_type_expr` recursion point.
        let tokens = lex_static("(Int) -> dyn Drawable");
        let mut parser = Parser::new(tokens);
        let ty = parser.parse_type_expr();
        let Some(TypeExpr::Function(f)) = ty else {
            panic!("expected TypeExpr::Function, got {ty:?}");
        };
        assert!(
            matches!(&*f.return_type, TypeExpr::Dyn(d) if d.trait_name == "Drawable"),
            "return type should be dyn Drawable, got {:?}",
            f.return_type
        );
    }

    #[test]
    fn dyn_in_function_param_position() {
        let tokens = lex_static("(dyn Drawable) -> Int");
        let mut parser = Parser::new(tokens);
        let ty = parser.parse_type_expr();
        let Some(TypeExpr::Function(f)) = ty else {
            panic!("expected TypeExpr::Function, got {ty:?}");
        };
        assert_eq!(f.param_types.len(), 1);
        assert!(
            matches!(&f.param_types[0], TypeExpr::Dyn(d) if d.trait_name == "Drawable"),
            "param 0 should be dyn Drawable, got {:?}",
            f.param_types[0]
        );
    }

    #[test]
    fn is_type_start_recognizes_dyn() {
        let tokens = lex_static("dyn Foo");
        let parser = Parser::new(tokens);
        assert!(parser.is_type_start());
    }
}
