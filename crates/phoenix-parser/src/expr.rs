use crate::ast::{
    AssignmentExpr, BinaryExpr, BinaryOp, CallExpr, ElseBranch, Expr, FieldAccessExpr,
    FieldAssignmentExpr, IdentExpr, IfExpr, LambdaExpr, ListLiteralExpr, Literal, LiteralKind,
    MapLiteralExpr, MatchArm, MatchBody, MatchExpr, MethodCallExpr, Pattern,
    StringInterpolationExpr, StringSegment, StructLiteralExpr, TryExpr, UnaryExpr, UnaryOp,
    VariantPattern,
};
use crate::parser::Parser;
use phoenix_common::span::SourceId;
use phoenix_lexer::token::TokenKind;

/// Returns `true` if the raw string content (between the quotes) contains
/// an unescaped `{` that starts an interpolation expression.
///
/// Escaped braces `{{` are not counted as interpolation markers.
fn contains_interpolation(raw: &str) -> bool {
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next(); // skip escaped char
        } else if c == '{' {
            if chars.peek() == Some(&'{') {
                chars.next(); // skip escaped {{
            } else {
                return true;
            }
        }
    }
    false
}

/// Parses the raw content of an interpolated string literal into segments.
///
/// Splits on unescaped `{expr}` boundaries, parsing each embedded expression
/// through the main parser. Escaped braces `{{` and `}}` produce literal
/// `{` and `}` characters respectively.
fn parse_interpolation_segments(
    raw: &str,
    parser: &mut Parser<'_>,
    source_id: SourceId,
) -> Vec<StringSegment> {
    let mut segments = Vec::new();
    let mut current_lit = String::new();
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            // Handle escape sequences in literal parts
            match chars.next() {
                Some('n') => current_lit.push('\n'),
                Some('t') => current_lit.push('\t'),
                Some('\\') => current_lit.push('\\'),
                Some('"') => current_lit.push('"'),
                Some('{') => current_lit.push('{'),
                Some('}') => current_lit.push('}'),
                Some(other) => {
                    current_lit.push('\\');
                    current_lit.push(other);
                }
                None => current_lit.push('\\'),
            }
        } else if c == '{' {
            if chars.peek() == Some(&'{') {
                chars.next();
                current_lit.push('{');
            } else {
                // Start of interpolation — collect until matching '}'
                if !current_lit.is_empty() {
                    segments.push(StringSegment::Literal(std::mem::take(&mut current_lit)));
                }
                let mut expr_src = String::new();
                let mut depth = 1;
                let mut in_string = false;
                while let Some(ec) = chars.next() {
                    if in_string {
                        expr_src.push(ec);
                        if ec == '\\' {
                            // Push the escaped char unconditionally
                            if let Some(esc) = chars.next() {
                                expr_src.push(esc);
                            }
                        } else if ec == '"' {
                            in_string = false;
                        }
                    } else if ec == '"' {
                        in_string = true;
                        expr_src.push(ec);
                    } else if ec == '{' {
                        depth += 1;
                        expr_src.push(ec);
                    } else if ec == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        expr_src.push(ec);
                    } else {
                        expr_src.push(ec);
                    }
                }
                // Parse the embedded expression using a sub-parser.
                let tokens = phoenix_lexer::lexer::tokenize(&expr_src, source_id);
                let mut sub_parser = Parser::new(&tokens);
                if let Some(expr) = sub_parser.parse_expr() {
                    segments.push(StringSegment::Expr(expr));
                } else {
                    // If parsing failed, report an error and emit the raw text
                    parser.diagnostics.extend(sub_parser.diagnostics);
                    segments.push(StringSegment::Literal(format!("{{{}}}", expr_src)));
                }
            }
        } else if c == '}' {
            if chars.peek() == Some(&'}') {
                chars.next();
                current_lit.push('}');
            } else {
                current_lit.push('}');
            }
        } else {
            current_lit.push(c);
        }
    }

    if !current_lit.is_empty() {
        segments.push(StringSegment::Literal(current_lit));
    }

    segments
}

/// Processes escape sequences in a string literal, replacing `\\n`, `\\t`,
/// `\\\\`, `\\"`, `{{`, and `}}` with their corresponding characters in a
/// single pass.
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else if c == '{' && chars.peek() == Some(&'{') {
            chars.next(); // consume second '{'
            out.push('{');
        } else if c == '}' && chars.peek() == Some(&'}') {
            chars.next(); // consume second '}'
            out.push('}');
        } else {
            out.push(c);
        }
    }
    out
}

/// Returns the left and right binding powers for an infix operator, or `None`
/// if the token is not an infix operator. Used by the Pratt parser to determine
/// precedence and associativity.
fn infix_binding_power(op: &TokenKind) -> Option<(u8, u8)> {
    match op {
        TokenKind::Or => Some((3, 4)),
        TokenKind::And => Some((5, 6)),
        TokenKind::EqEq | TokenKind::NotEq => Some((7, 8)),
        TokenKind::Lt | TokenKind::Gt | TokenKind::LtEq | TokenKind::GtEq => Some((9, 10)),
        TokenKind::Plus | TokenKind::Minus => Some((11, 12)),
        TokenKind::Star | TokenKind::Slash | TokenKind::Percent => Some((13, 14)),
        _ => None,
    }
}

/// Returns the binding power for a prefix (unary) operator, or `None` if the
/// token is not a prefix operator.
fn prefix_binding_power(op: &TokenKind) -> Option<u8> {
    match op {
        TokenKind::Minus => Some(15),
        TokenKind::Not => Some(15),
        _ => None,
    }
}

/// Maps a [`TokenKind`] to its corresponding [`BinaryOp`], or `None` if the
/// token does not represent a binary operator.
fn token_to_binary_op(kind: &TokenKind) -> Option<BinaryOp> {
    match kind {
        TokenKind::Plus => Some(BinaryOp::Add),
        TokenKind::Minus => Some(BinaryOp::Sub),
        TokenKind::Star => Some(BinaryOp::Mul),
        TokenKind::Slash => Some(BinaryOp::Div),
        TokenKind::Percent => Some(BinaryOp::Mod),
        TokenKind::EqEq => Some(BinaryOp::Eq),
        TokenKind::NotEq => Some(BinaryOp::NotEq),
        TokenKind::Lt => Some(BinaryOp::Lt),
        TokenKind::Gt => Some(BinaryOp::Gt),
        TokenKind::LtEq => Some(BinaryOp::LtEq),
        TokenKind::GtEq => Some(BinaryOp::GtEq),
        TokenKind::And => Some(BinaryOp::And),
        TokenKind::Or => Some(BinaryOp::Or),
        _ => None,
    }
}

impl<'src> Parser<'src> {
    /// Parses a literal token (int, float, string, bool) into a [`Literal`].
    ///
    /// Returns `None` if the current token is not a literal. Handles string
    /// unescaping and numeric overflow diagnostics. Does NOT handle string
    /// interpolation — the caller must check for that when needed.
    fn parse_literal(&mut self) -> Option<Literal> {
        let token = self.peek();
        match token.kind {
            TokenKind::IntLiteral => {
                self.advance();
                let value: i64 = match token.text.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        self.error_at_current(&format!(
                            "integer literal `{}` is out of range",
                            token.text
                        ));
                        0
                    }
                };
                Some(Literal {
                    kind: LiteralKind::Int(value),
                    span: token.span,
                })
            }
            TokenKind::FloatLiteral => {
                self.advance();
                let value: f64 = match token.text.parse() {
                    Ok(v) => v,
                    Err(_) => {
                        self.error_at_current(&format!(
                            "float literal `{}` is out of range",
                            token.text
                        ));
                        0.0
                    }
                };
                Some(Literal {
                    kind: LiteralKind::Float(value),
                    span: token.span,
                })
            }
            TokenKind::StringLiteral => {
                self.advance();
                let raw = if token.text.len() >= 2 {
                    &token.text[1..token.text.len() - 1]
                } else {
                    ""
                };
                let value = unescape(raw);
                Some(Literal {
                    kind: LiteralKind::String(value),
                    span: token.span,
                })
            }
            TokenKind::True => {
                self.advance();
                Some(Literal {
                    kind: LiteralKind::Bool(true),
                    span: token.span,
                })
            }
            TokenKind::False => {
                self.advance();
                Some(Literal {
                    kind: LiteralKind::Bool(false),
                    span: token.span,
                })
            }
            _ => None,
        }
    }

    /// Parses an expression using Pratt (binding-power) parsing.
    ///
    /// This is the main entry point for expression parsing, dispatching to
    /// prefix, infix, and postfix sub-parsers with minimum binding power 0.
    /// Returns `None` and records a diagnostic when the current token cannot
    /// begin a valid expression.
    pub fn parse_expr(&mut self) -> Option<Expr> {
        self.parse_expr_bp(0)
    }

    /// Pratt-parses an expression with the given minimum binding power.
    ///
    /// Handles prefix, infix, postfix (`.`, `?`, `()`), compound assignment,
    /// and ternary operators.
    fn parse_expr_bp(&mut self, min_bp: u8) -> Option<Expr> {
        let mut lhs = self.parse_prefix()?;

        loop {
            let op_kind = self.peek().kind;

            // Assignment: ident = expr  or  field assignment: obj.field = expr
            if op_kind == TokenKind::Eq {
                if let Expr::Ident(ref ident) = lhs {
                    let name = ident.name.clone();
                    let start = ident.span;
                    self.advance();
                    let value = self.parse_expr()?;
                    let span = start.merge(value.span());
                    return Some(Expr::Assignment(Box::new(AssignmentExpr {
                        name,
                        value,
                        span,
                    })));
                }
                if let Expr::FieldAccess(ref fa) = lhs {
                    let object = fa.object.clone();
                    let field = fa.field.clone();
                    let start = fa.span;
                    self.advance();
                    let value = self.parse_expr()?;
                    let span = start.merge(value.span());
                    return Some(Expr::FieldAssignment(Box::new(FieldAssignmentExpr {
                        object,
                        field,
                        value,
                        span,
                    })));
                }
            }

            // Compound assignment: x += expr  desugars to  x = x + expr
            let compound_op = match op_kind {
                TokenKind::PlusEq => Some(BinaryOp::Add),
                TokenKind::MinusEq => Some(BinaryOp::Sub),
                TokenKind::StarEq => Some(BinaryOp::Mul),
                TokenKind::SlashEq => Some(BinaryOp::Div),
                TokenKind::PercentEq => Some(BinaryOp::Mod),
                _ => None,
            };
            if let Some(bin_op) = compound_op {
                if let Expr::Ident(ref ident) = lhs {
                    let name = ident.name.clone();
                    let start = ident.span;
                    self.advance();
                    let rhs = self.parse_expr()?;
                    let rhs_span = rhs.span();
                    let value = Expr::Binary(Box::new(BinaryExpr {
                        left: lhs,
                        op: bin_op,
                        right: rhs,
                        span: start.merge(rhs_span),
                    }));
                    let span = start.merge(rhs_span);
                    return Some(Expr::Assignment(Box::new(AssignmentExpr {
                        name,
                        value,
                        span,
                    })));
                }
                if let Expr::FieldAccess(ref fa) = lhs {
                    let object = fa.object.clone();
                    let field = fa.field.clone();
                    let start = fa.span;
                    self.advance();
                    let rhs = self.parse_expr()?;
                    let rhs_span = rhs.span();
                    let value = Expr::Binary(Box::new(BinaryExpr {
                        left: lhs,
                        op: bin_op,
                        right: rhs,
                        span: start.merge(rhs_span),
                    }));
                    let span = start.merge(rhs_span);
                    return Some(Expr::FieldAssignment(Box::new(FieldAssignmentExpr {
                        object,
                        field,
                        value,
                        span,
                    })));
                }
            }

            // Postfix `?` (try/propagation operator)
            if op_kind == TokenKind::Question {
                let end = self.advance().span;
                let span = lhs.span().merge(end);
                lhs = Expr::Try(Box::new(TryExpr { operand: lhs, span }));
                continue;
            }

            // Field access / method call: expr.field or expr.method(args)
            if op_kind == TokenKind::Dot {
                self.advance();
                let field_token = self.expect(TokenKind::Ident)?;
                let field_name = field_token.text.clone();

                // Check if this is a method call: expr.method(args)
                if self.peek().kind == TokenKind::LParen {
                    self.advance(); // consume '('
                    let args = self.parse_comma_separated(TokenKind::RParen, |p| p.parse_expr());
                    let end = self.expect(TokenKind::RParen)?.span;
                    let span = lhs.span().merge(end);
                    lhs = Expr::MethodCall(Box::new(MethodCallExpr {
                        object: lhs,
                        method: field_name,
                        args,
                        span,
                    }));
                } else {
                    let span = lhs.span().merge(field_token.span);
                    lhs = Expr::FieldAccess(Box::new(FieldAccessExpr {
                        object: lhs,
                        field: field_name,
                        span,
                    }));
                }
                continue;
            }

            // Pipe operator: lhs |> f(args) desugars to f(lhs, args)
            if op_kind == TokenKind::Pipe {
                // Binding power (1, 2): lowest precedence, left-associative
                if 1 < min_bp {
                    break;
                }
                self.advance(); // consume |>
                let rhs = self.parse_expr_bp(2)?;
                // The RHS must be a Call expression — insert lhs as first arg
                lhs = match rhs {
                    Expr::Call(mut call) => {
                        call.args.insert(0, lhs);
                        Expr::Call(call)
                    }
                    _ => {
                        self.error_at_current(
                            "pipe operator `|>` requires a function call on the right-hand side",
                        );
                        return None;
                    }
                };
                continue;
            }

            // Function call: expr(args)
            if op_kind == TokenKind::LParen {
                lhs = self.parse_call(lhs)?;
                continue;
            }

            // Infix operator
            let Some((l_bp, r_bp)) = infix_binding_power(&op_kind) else {
                break;
            };

            if l_bp < min_bp {
                break;
            }

            let op = token_to_binary_op(&op_kind)
                .expect("infix_binding_power matched this token, so it must be a valid binary op");
            self.advance();

            let rhs = self.parse_expr_bp(r_bp)?;
            let span = lhs.span().merge(rhs.span());
            lhs = Expr::Binary(Box::new(BinaryExpr {
                left: lhs,
                op,
                right: rhs,
                span,
            }));
        }

        Some(lhs)
    }

    /// Parses a prefix expression (literals, identifiers, unary ops, parenthesized
    /// expressions, list literals, struct/enum constructors, match, and lambdas).
    fn parse_prefix(&mut self) -> Option<Expr> {
        let token = self.peek();

        match token.kind {
            TokenKind::Minus | TokenKind::Not => {
                let bp = prefix_binding_power(&token.kind)
                    .expect("matched Minus|Not above, so binding power must exist");
                let op = match token.kind {
                    TokenKind::Minus => UnaryOp::Neg,
                    _ => UnaryOp::Not,
                };
                self.advance();
                let operand = self.parse_expr_bp(bp)?;
                let span = token.span.merge(operand.span());
                Some(Expr::Unary(Box::new(UnaryExpr { op, operand, span })))
            }

            TokenKind::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Some(expr)
            }

            TokenKind::LBracket => self.parse_list_literal(),

            // Map literal: `{key: value, ...}` or `{:}` (empty map)
            TokenKind::LBrace => self.parse_map_literal(),

            TokenKind::StringLiteral => {
                // Check for interpolation before falling through to parse_literal
                let raw = if token.text.len() >= 2 {
                    &token.text[1..token.text.len() - 1]
                } else {
                    ""
                };
                if contains_interpolation(raw) {
                    self.advance();
                    let segments = parse_interpolation_segments(raw, self, self.source_id);
                    return Some(Expr::StringInterpolation(StringInterpolationExpr {
                        segments,
                        span: token.span,
                    }));
                }
                Some(Expr::Literal(self.parse_literal()?))
            }
            TokenKind::IntLiteral
            | TokenKind::FloatLiteral
            | TokenKind::True
            | TokenKind::False => Some(Expr::Literal(self.parse_literal()?)),

            // Match expression
            TokenKind::Match => self.parse_match_expr(),

            // If expression
            TokenKind::If => self.parse_if_expr().map(|i| Expr::If(Box::new(i))),

            // Lambda expression: function(params) -> ReturnType { body }
            TokenKind::Function => self.parse_lambda_expr(),

            // Self keyword (used in methods)
            TokenKind::SelfKw => {
                self.advance();
                Some(Expr::Ident(IdentExpr {
                    name: "self".to_string(),
                    span: token.span,
                }))
            }

            // Identifier — could be variable, struct constructor, or enum variant
            TokenKind::Ident => self.parse_ident_or_constructor(),

            _ => {
                self.error_at_current("expected expression");
                None
            }
        }
    }

    /// Parses a list literal: `[elem1, elem2, ...]`.
    fn parse_list_literal(&mut self) -> Option<Expr> {
        let start = self.peek().span;
        self.advance(); // consume '['
        let elements = self.parse_comma_separated(TokenKind::RBracket, |p| p.parse_expr());
        let end = self.expect(TokenKind::RBracket)?.span;
        let span = start.merge(end);
        Some(Expr::ListLiteral(ListLiteralExpr { elements, span }))
    }

    /// Parses a map literal: `{key: value, ...}` or `{:}` (empty map).
    fn parse_map_literal(&mut self) -> Option<Expr> {
        let start = self.peek().span;
        self.advance(); // consume '{'
        // `{:}` is the empty map literal
        if self.peek().kind == TokenKind::Colon {
            self.advance(); // consume ':'
            let end = self.expect(TokenKind::RBrace)?.span;
            return Some(Expr::MapLiteral(MapLiteralExpr {
                entries: vec![],
                span: start.merge(end),
            }));
        }
        let entries = self.parse_comma_separated(TokenKind::RBrace, |p| {
            let key = p.parse_expr()?;
            p.expect(TokenKind::Colon)?;
            let value = p.parse_expr()?;
            Some((key, value))
        });
        let end = self.expect(TokenKind::RBrace)?.span;
        let span = start.merge(end);
        Some(Expr::MapLiteral(MapLiteralExpr { entries, span }))
    }

    /// Parses an identifier that may be a variable, struct constructor, or enum variant.
    ///
    /// Uppercase identifiers are checked for optional type arguments (`<T, U>`)
    /// and a parenthesised argument list. If both are absent, the identifier is
    /// returned as a plain `Ident` expression.
    fn parse_ident_or_constructor(&mut self) -> Option<Expr> {
        let token = self.peek();
        self.advance();
        let name = token.text.clone();

        // Check for struct/enum constructor with optional type args:
        // Name<Type, ...>(args) or Name(args)
        if name.starts_with(|c: char| c.is_uppercase()) {
            // Try to parse type args: <Type, Type>
            // This is speculative — if parsing fails, we backtrack both
            // position and diagnostics to avoid spurious errors.
            let mut type_args = Vec::new();
            if self.peek().kind == TokenKind::Lt {
                let saved_pos = self.pos;
                let saved_diag_len = self.diagnostics.len();
                self.advance(); // consume '<'
                let mut args_ok = true;
                loop {
                    if let Some(t) = self.parse_type_expr() {
                        type_args.push(t);
                    } else {
                        args_ok = false;
                        break;
                    }
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                if args_ok && self.peek().kind == TokenKind::Gt {
                    self.advance(); // consume '>'
                } else {
                    // Not a valid generic, backtrack position and diagnostics
                    self.pos = saved_pos;
                    self.diagnostics.truncate(saved_diag_len);
                    type_args.clear();
                }
            }

            if self.peek().kind == TokenKind::LParen {
                self.advance(); // consume '('
                let args = self.parse_comma_separated(TokenKind::RParen, |p| p.parse_expr());
                let end = self.expect(TokenKind::RParen)?.span;
                let span = token.span.merge(end);
                // Could be struct or enum variant — we disambiguate later in sema
                Some(Expr::StructLiteral(Box::new(StructLiteralExpr {
                    name,
                    type_args,
                    args,
                    span,
                })))
            } else {
                Some(Expr::Ident(IdentExpr {
                    name,
                    span: token.span,
                }))
            }
        } else {
            Some(Expr::Ident(IdentExpr {
                name,
                span: token.span,
            }))
        }
    }

    /// Parses a function call argument list after the opening `(` has been consumed.
    ///
    /// Supports both positional and named arguments. Named arguments use the
    /// syntax `name: expr` and must appear after all positional arguments.
    fn parse_call(&mut self, callee: Expr) -> Option<Expr> {
        let start = callee.span();
        self.expect(TokenKind::LParen)?;

        let mut args = Vec::new();
        let mut named_args: Vec<(String, Expr)> = Vec::new();
        let mut seen_named = false;

        if self.peek().kind != TokenKind::RParen {
            loop {
                // Check for named argument: `ident: expr`
                // We look ahead: if current is Ident and next is Colon, it's named.
                if self.peek().kind == TokenKind::Ident && self.peek_at(1).kind == TokenKind::Colon
                {
                    seen_named = true;
                    let name_token = self.advance(); // consume ident
                    self.advance(); // consume colon
                    let value = self.parse_expr()?;
                    named_args.push((name_token.text.clone(), value));
                } else {
                    if seen_named {
                        self.error_at_current("positional argument cannot follow a named argument");
                    }
                    let arg = self.parse_expr()?;
                    args.push(arg);
                }
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }

        let end = self.expect(TokenKind::RParen)?.span;
        let span = start.merge(end);

        Some(Expr::Call(Box::new(CallExpr {
            callee,
            args,
            named_args,
            span,
        })))
    }

    /// Parses an `if cond { then } [else { else_block } | else if ...]` expression.
    ///
    /// `if` is a first-class expression: its value is the value of the taken
    /// branch. When used as a statement, the outer `Statement::Expression`
    /// wrapper discards the value.
    pub(crate) fn parse_if_expr(&mut self) -> Option<IfExpr> {
        let start = self.peek().span;
        self.expect(TokenKind::If)?;

        let condition = self.parse_expr()?;
        let then_block = self.parse_block()?;

        let else_branch = if self.eat(TokenKind::Else) {
            if self.peek().kind == TokenKind::If {
                // else if — parse recursively
                Some(ElseBranch::ElseIf(Box::new(self.parse_if_expr()?)))
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

        Some(IfExpr {
            condition,
            then_block,
            else_branch,
            span: start.merge(end),
        })
    }

    /// Parses a `match subject { arm1, arm2, ... }` expression.
    fn parse_match_expr(&mut self) -> Option<Expr> {
        let start = self.peek().span;
        self.expect(TokenKind::Match)?;
        let subject = self.parse_expr()?;
        self.expect(TokenKind::LBrace)?;
        self.skip_newlines();

        let mut arms = Vec::new();
        while self.peek().kind != TokenKind::RBrace && self.peek().kind != TokenKind::Eof {
            if let Some(arm) = self.parse_match_arm() {
                arms.push(arm);
            } else {
                // Error recovery: skip the current token to avoid an infinite loop
                self.advance();
            }
            self.skip_newlines();
        }

        let end = self.expect(TokenKind::RBrace)?.span;
        Some(Expr::Match(Box::new(MatchExpr {
            subject,
            arms,
            span: start.merge(end),
        })))
    }

    /// Parses a single match arm: `pattern -> expr` or `pattern -> { block }`.
    fn parse_match_arm(&mut self) -> Option<MatchArm> {
        let start = self.peek().span;
        let pattern = self.parse_pattern()?;
        self.expect(TokenKind::Arrow)?;

        let body = if self.peek().kind == TokenKind::LBrace {
            MatchBody::Block(self.parse_block()?)
        } else {
            let expr = self.parse_expr()?;
            MatchBody::Expr(expr)
        };

        let end = match &body {
            MatchBody::Block(b) => b.span,
            MatchBody::Expr(e) => e.span(),
        };

        self.eat(TokenKind::Newline);

        Some(MatchArm {
            pattern,
            body,
            span: start.merge(end),
        })
    }

    /// Parses a match pattern: wildcard (`_`), variant destructuring, literal, or binding.
    fn parse_pattern(&mut self) -> Option<Pattern> {
        let token = self.peek();
        match token.kind {
            // Wildcard _
            TokenKind::Ident if token.text == "_" => {
                self.advance();
                Some(Pattern::Wildcard(token.span))
            }

            // Variant pattern or binding: starts with Ident
            TokenKind::Ident => self.parse_variant_pattern(),

            // Negative numeric literal patterns: -42, -3.14
            TokenKind::Minus => {
                let start = self.advance().span; // consume '-'
                let next = self.peek();
                match next.kind {
                    TokenKind::IntLiteral => {
                        self.advance();
                        let value: i64 = match next.text.parse::<i64>() {
                            Ok(v) => -v,
                            Err(_) => {
                                self.error_at_current(&format!(
                                    "integer literal `-{}` is out of range",
                                    next.text
                                ));
                                0
                            }
                        };
                        Some(Pattern::Literal(Literal {
                            kind: LiteralKind::Int(value),
                            span: start.merge(next.span),
                        }))
                    }
                    TokenKind::FloatLiteral => {
                        self.advance();
                        let value: f64 = match next.text.parse::<f64>() {
                            Ok(v) => -v,
                            Err(_) => {
                                self.error_at_current(&format!(
                                    "float literal `-{}` is out of range",
                                    next.text
                                ));
                                0.0
                            }
                        };
                        Some(Pattern::Literal(Literal {
                            kind: LiteralKind::Float(value),
                            span: start.merge(next.span),
                        }))
                    }
                    _ => {
                        self.error_at_current("expected numeric literal after '-' in pattern");
                        None
                    }
                }
            }

            // Literal patterns
            TokenKind::IntLiteral
            | TokenKind::FloatLiteral
            | TokenKind::StringLiteral
            | TokenKind::True
            | TokenKind::False => Some(Pattern::Literal(self.parse_literal()?)),

            _ => {
                self.error_at_current("expected pattern");
                None
            }
        }
    }

    /// Parses a variant pattern with optional parenthesised bindings, or falls
    /// back to a plain binding for lowercase identifiers.
    ///
    /// ```text
    /// Name(a, b)   // variant with field bindings
    /// Name         // variant with no fields (uppercase)
    /// x            // variable binding (lowercase)
    /// ```
    fn parse_variant_pattern(&mut self) -> Option<Pattern> {
        let token = self.peek();
        self.advance();
        let name = token.text.clone();

        // Variant with fields: Name(a, b, ...)
        if self.peek().kind == TokenKind::LParen {
            self.advance();
            let bindings = self.parse_comma_separated(TokenKind::RParen, |p| {
                p.expect(TokenKind::Ident).map(|tok| tok.text.clone())
            });
            let end = self.expect(TokenKind::RParen)?.span;
            Some(Pattern::Variant(VariantPattern {
                variant: name,
                bindings,
                span: token.span.merge(end),
            }))
        } else if name.starts_with(|c: char| c.is_uppercase()) {
            // Uppercase without parens = variant with no fields
            Some(Pattern::Variant(VariantPattern {
                variant: name,
                bindings: vec![],
                span: token.span,
            }))
        } else {
            // Lowercase = binding
            Some(Pattern::Binding(name, token.span))
        }
    }

    /// Parses a lambda (anonymous function): `function(params) -> RetType { body }`.
    ///
    /// Lambdas begin with the `function` keyword followed by a parenthesised
    /// parameter list, an optional `-> ReturnType` annotation, and a block
    /// body.  When no return type is specified the lambda is treated as
    /// returning `Void`.
    fn parse_lambda_expr(&mut self) -> Option<Expr> {
        let start = self.peek().span;
        self.expect(TokenKind::Function)?;
        self.expect(TokenKind::LParen)?;

        // Parse parameters (reuse existing parse_params infrastructure)
        let params = self.parse_params();
        self.expect(TokenKind::RParen)?;

        let return_type = if self.eat(TokenKind::Arrow) {
            self.parse_type_expr()
        } else {
            None
        };

        let body = self.parse_block()?;
        let span = start.merge(body.span);

        Some(Expr::Lambda(Box::new(LambdaExpr {
            params,
            return_type,
            body,
            span,
        })))
    }
}
