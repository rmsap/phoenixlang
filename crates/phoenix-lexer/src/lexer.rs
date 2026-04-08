use crate::token::{Token, TokenKind};
use phoenix_common::span::{SourceId, Span};

/// A lazy, pull-based lexer for Phoenix source code.
///
/// The lexer operates on a borrowed `&str` slice and produces [`Token`]s one
/// at a time through its [`Iterator`] implementation.  It automatically:
///
/// * Skips whitespace (except newlines, which are significant).
/// * Collapses consecutive blank lines into a single [`TokenKind::Newline`].
/// * Suppresses newlines inside parentheses `()` and braces `{}`, or after
///   continuation operators (e.g. `+`, `,`, `->`).
/// * Strips `//`-style line comments and `/* */` block comments.
pub struct Lexer<'src> {
    source: &'src str,
    bytes: &'src [u8],
    pos: usize,
    source_id: SourceId,
    /// Track nesting depth of `()` and `[]` to suppress newlines inside them.
    paren_depth: u32,
    /// The kind of the last emitted non-newline token (for newline suppression).
    last_token_kind: Option<TokenKind>,
}

impl<'src> Lexer<'src> {
    /// Creates a new lexer over `source`.
    ///
    /// `source_id` is embedded in every [`Span`] the lexer produces so that
    /// downstream code can map tokens back to the correct source file.
    pub fn new(source: &'src str, source_id: SourceId) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            source_id,
            paren_depth: 0,
            last_token_kind: None,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> u8 {
        let b = self.bytes[self.pos];
        self.pos += 1;
        b
    }

    /// Advances past a full UTF-8 character at the current position and returns it.
    /// This must be used instead of `advance()` when the current byte may be
    /// non-ASCII (i.e. the start of a multi-byte UTF-8 sequence).
    fn advance_char(&mut self) -> char {
        let ch = self.source[self.pos..].chars().next().unwrap_or('\0');
        self.pos += ch.len_utf8();
        ch
    }

    /// Returns the current character (potentially multi-byte) without advancing.
    fn peek_char(&self) -> Option<char> {
        self.source.get(self.pos..)?.chars().next()
    }

    fn span(&self, start: usize) -> Span {
        Span::new(self.source_id, start, self.pos)
    }

    fn text(&self, start: usize) -> &'src str {
        &self.source[start..self.pos]
    }

    fn skip_whitespace_except_newlines(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\r' => {
                    self.advance();
                }
                _ => break,
            }
        }
    }

    /// Skips a line comment (`//` through end of line).
    fn skip_line_comment(&mut self) {
        while let Some(b) = self.peek() {
            if b == b'\n' {
                break;
            }
            self.advance();
        }
    }

    /// Skips a block comment (`/* ... */`). Supports nesting.
    /// Returns `true` if the comment was properly terminated, `false` if EOF
    /// was reached before the closing `*/`.
    fn skip_block_comment(&mut self) -> bool {
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                Some(b'/') if self.peek_next() == Some(b'*') => {
                    self.advance();
                    self.advance();
                    depth += 1;
                }
                Some(b'*') if self.peek_next() == Some(b'/') => {
                    self.advance();
                    self.advance();
                    depth -= 1;
                }
                Some(_) => {
                    self.advance();
                }
                None => return false, // unterminated block comment
            }
        }
        true
    }

    /// Returns `true` if a newline immediately following this token kind should
    /// be suppressed, allowing expressions to span multiple lines.
    fn suppresses_newline(kind: TokenKind) -> bool {
        matches!(
            kind,
            TokenKind::Plus
                | TokenKind::Minus
                | TokenKind::Star
                | TokenKind::Slash
                | TokenKind::Percent
                | TokenKind::Eq
                | TokenKind::EqEq
                | TokenKind::NotEq
                | TokenKind::Lt
                | TokenKind::Gt
                | TokenKind::LtEq
                | TokenKind::GtEq
                | TokenKind::And
                | TokenKind::Or
                | TokenKind::Not
                | TokenKind::Arrow
                | TokenKind::LParen
                | TokenKind::LBrace
                | TokenKind::LBracket
                | TokenKind::Comma
                | TokenKind::Colon
                | TokenKind::Dot
                | TokenKind::DotDot
                | TokenKind::Pipe
                | TokenKind::Question
        )
    }

    fn lex_number(&mut self) -> Token {
        let start = self.pos;
        let mut is_float = false;

        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.advance();
            } else if b == b'.' && !is_float {
                // Check that the next char is a digit (not a method call like 42.foo)
                if let Some(next) = self.peek_next() {
                    if next.is_ascii_digit() {
                        is_float = true;
                        self.advance(); // consume '.'
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        let kind = if is_float {
            TokenKind::FloatLiteral
        } else {
            TokenKind::IntLiteral
        };

        Token::new(kind, self.text(start), self.span(start))
    }

    fn lex_string(&mut self) -> Token {
        let start = self.pos;
        self.advance(); // consume opening '"'

        loop {
            match self.peek() {
                Some(b'"') => {
                    self.advance();
                    break;
                }
                Some(b'\\') => {
                    self.advance(); // consume '\'
                    if self.peek().is_none() {
                        // Backslash at EOF — unterminated string
                        return Token::new(TokenKind::Error, self.text(start), self.span(start));
                    }
                    self.advance(); // consume escaped char
                }
                Some(b'\n') | None => {
                    // Unterminated string
                    return Token::new(TokenKind::Error, self.text(start), self.span(start));
                }
                Some(_) => {
                    self.advance();
                }
            }
        }

        Token::new(TokenKind::StringLiteral, self.text(start), self.span(start))
    }

    fn lex_ident_or_keyword(&mut self) -> Token {
        let start = self.pos;

        while let Some(ch) = self.peek_char() {
            if ch.is_alphanumeric() || ch == '_' {
                self.advance_char();
            } else {
                break;
            }
        }

        let text = self.text(start);
        let kind = match text {
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "let" => TokenKind::Let,
            "mut" => TokenKind::Mut,
            "function" => TokenKind::Function,
            "return" => TokenKind::Return,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "struct" => TokenKind::Struct,
            "impl" => TokenKind::Impl,
            "enum" => TokenKind::Enum,
            "match" => TokenKind::Match,
            "self" => TokenKind::SelfKw,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "trait" => TokenKind::Trait,
            "type" => TokenKind::Type,
            "Int" => TokenKind::IntType,
            "Float" => TokenKind::FloatType,
            "String" => TokenKind::StringType,
            "Bool" => TokenKind::BoolType,
            "Void" => TokenKind::Void,
            "and" => TokenKind::And,
            "or" => TokenKind::Or,
            "not" => TokenKind::Not,
            _ => TokenKind::Ident,
        };

        Token::new(kind, text, self.span(start))
    }

    /// Lexes a potentially multi-character operator token.
    ///
    /// Handles `->`, `==`, `!=`, `<=`, `>=`, `|>`, and their single-character
    /// counterparts (`-`, `=`, `!`, `<`, `>`, `?`).
    fn lex_operator(&mut self) -> Token {
        let s = self.pos;
        let b = self.advance();
        match b {
            b'-' => {
                if self.peek() == Some(b'>') {
                    self.advance();
                    Token::new(TokenKind::Arrow, "->", self.span(s))
                } else {
                    Token::new(TokenKind::Minus, "-", self.span(s))
                }
            }
            b'=' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    Token::new(TokenKind::EqEq, "==", self.span(s))
                } else {
                    Token::new(TokenKind::Eq, "=", self.span(s))
                }
            }
            b'!' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    Token::new(TokenKind::NotEq, "!=", self.span(s))
                } else {
                    // Standalone `!` is not a valid operator — Phoenix uses
                    // the `not` keyword for boolean negation.
                    Token::new(TokenKind::Error, "!", self.span(s))
                }
            }
            b'<' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    Token::new(TokenKind::LtEq, "<=", self.span(s))
                } else {
                    Token::new(TokenKind::Lt, "<", self.span(s))
                }
            }
            b'>' => {
                if self.peek() == Some(b'=') {
                    self.advance();
                    Token::new(TokenKind::GtEq, ">=", self.span(s))
                } else {
                    Token::new(TokenKind::Gt, ">", self.span(s))
                }
            }
            b'?' => Token::new(TokenKind::Question, "?", self.span(s)),
            b'|' => {
                if self.peek() == Some(b'>') {
                    self.advance();
                    Token::new(TokenKind::Pipe, "|>", self.span(s))
                } else {
                    Token::new(TokenKind::Error, self.text(s), self.span(s))
                }
            }
            // SAFETY: This method is only called from `next_token` for the bytes
            // listed above, so this branch is unreachable.
            _ => unreachable!(),
        }
    }

    fn next_token(&mut self) -> Token {
        loop {
            self.skip_whitespace_except_newlines();

            // Handle line comments — skip the comment text and restart tokenization
            // so that subsequent whitespace/comments are also skipped.
            if self.peek() == Some(b'/') && self.peek_next() == Some(b'/') {
                self.skip_line_comment();
                continue;
            }
            // Handle block comments
            if self.peek() == Some(b'/') && self.peek_next() == Some(b'*') {
                let start = self.pos;
                self.advance(); // consume /
                self.advance(); // consume *
                if self.skip_block_comment() {
                    continue;
                } else {
                    return Token::new(TokenKind::Error, self.text(start), self.span(start));
                }
            }

            let Some(b) = self.peek() else {
                let start = self.pos;
                return Token::new(TokenKind::Eof, "", self.span(start));
            };

            return match b {
                b'\n' => {
                    let start = self.pos;
                    self.advance();
                    // Skip consecutive newlines and whitespace
                    while matches!(self.peek(), Some(b'\n' | b'\r' | b' ' | b'\t')) {
                        self.advance();
                    }

                    // Suppress newline if inside parens or after a continuation token.
                    // Note: brace depth is NOT checked — newlines are significant
                    // inside block bodies for statement termination.
                    if self.paren_depth > 0
                        || self.last_token_kind.is_none_or(Self::suppresses_newline)
                    {
                        continue;
                    }

                    Token::new(TokenKind::Newline, "\\n", self.span(start))
                }

                b'0'..=b'9' => self.lex_number(),
                b'"' => self.lex_string(),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident_or_keyword(),
                // Non-ASCII byte: could be a Unicode identifier start (e.g. ñ, ü, 日)
                0x80..=0xFF => {
                    if self.peek_char().is_some_and(|ch| ch.is_alphabetic()) {
                        self.lex_ident_or_keyword()
                    } else {
                        let s = self.pos;
                        self.advance_char();
                        Token::new(TokenKind::Error, self.text(s), self.span(s))
                    }
                }

                b'+' => {
                    let s = self.pos;
                    self.advance();
                    Token::new(TokenKind::Plus, "+", self.span(s))
                }
                b'*' => {
                    let s = self.pos;
                    self.advance();
                    Token::new(TokenKind::Star, "*", self.span(s))
                }
                // Note: `//` and `/*` comments are handled before this match
                // statement, so reaching here means this `/` is a division operator.
                b'/' => {
                    let s = self.pos;
                    self.advance();
                    Token::new(TokenKind::Slash, "/", self.span(s))
                }
                b'%' => {
                    let s = self.pos;
                    self.advance();
                    Token::new(TokenKind::Percent, "%", self.span(s))
                }
                b',' => {
                    let s = self.pos;
                    self.advance();
                    Token::new(TokenKind::Comma, ",", self.span(s))
                }
                b':' => {
                    let s = self.pos;
                    self.advance();
                    Token::new(TokenKind::Colon, ":", self.span(s))
                }
                b'.' => {
                    let s = self.pos;
                    self.advance();
                    if self.peek() == Some(b'.') {
                        self.advance();
                        Token::new(TokenKind::DotDot, "..", self.span(s))
                    } else {
                        Token::new(TokenKind::Dot, ".", self.span(s))
                    }
                }

                b'(' => {
                    let s = self.pos;
                    self.advance();
                    self.paren_depth += 1;
                    Token::new(TokenKind::LParen, "(", self.span(s))
                }
                b')' => {
                    let s = self.pos;
                    self.advance();
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    Token::new(TokenKind::RParen, ")", self.span(s))
                }
                b'{' => {
                    let s = self.pos;
                    self.advance();
                    Token::new(TokenKind::LBrace, "{", self.span(s))
                }
                b'}' => {
                    let s = self.pos;
                    self.advance();
                    Token::new(TokenKind::RBrace, "}", self.span(s))
                }

                b'[' => {
                    let s = self.pos;
                    self.advance();
                    self.paren_depth += 1;
                    Token::new(TokenKind::LBracket, "[", self.span(s))
                }
                b']' => {
                    let s = self.pos;
                    self.advance();
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    Token::new(TokenKind::RBracket, "]", self.span(s))
                }

                b'-' | b'=' | b'!' | b'<' | b'>' | b'?' | b'|' => self.lex_operator(),

                _ => {
                    let s = self.pos;
                    // All non-ASCII bytes are handled above (0x80..=0xFF),
                    // so this arm only sees ASCII — safe to advance one byte.
                    self.advance();
                    Token::new(TokenKind::Error, self.text(s), self.span(s))
                }
            }; // end match + return
        } // end loop
    }
}

/// Yields tokens one at a time until end-of-file.
///
/// When the lexer reaches EOF, `next()` returns `None`.  The final
/// [`TokenKind::Eof`] token is **not** yielded by the iterator; use
/// [`tokenize`] if you need an explicit EOF sentinel.
impl Iterator for Lexer<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        let token = self.next_token();
        if token.kind == TokenKind::Eof {
            return None;
        }
        if token.kind != TokenKind::Newline {
            self.last_token_kind = Some(token.kind);
        }
        Some(token)
    }
}

/// Convenience function that tokenizes the entire `source` into a `Vec<Token>`.
///
/// The returned vector always ends with a [`TokenKind::Eof`] token, which
/// simplifies parser look-ahead.
///
/// # Examples
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_lexer::token::TokenKind;
/// use phoenix_common::span::SourceId;
///
/// let tokens = tokenize("42", SourceId(0));
/// assert_eq!(tokens[0].kind, TokenKind::IntLiteral);
/// assert_eq!(tokens[1].kind, TokenKind::Eof);
/// ```
#[must_use]
pub fn tokenize(source: &str, source_id: SourceId) -> Vec<Token> {
    let mut lexer = Lexer::new(source, source_id);
    let mut tokens: Vec<Token> = (&mut lexer).collect();
    tokens.push(Token::new(
        TokenKind::Eof,
        "",
        Span::new(source_id, source.len(), source.len()),
    ));
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::TokenKind::*;

    fn token_kinds(source: &str) -> Vec<TokenKind> {
        let tokens = tokenize(source, SourceId(0));
        tokens.into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn simple_var_decl() {
        assert_eq!(
            token_kinds("let x: Int = 42"),
            vec![Let, Ident, Colon, IntType, Eq, IntLiteral, Eof]
        );
    }

    #[test]
    fn mutable_var_decl() {
        assert_eq!(
            token_kinds("let mut x: Int = 42"),
            vec![Let, Mut, Ident, Colon, IntType, Eq, IntLiteral, Eof]
        );
    }

    #[test]
    fn function_decl() {
        assert_eq!(
            token_kinds("function foo(x: Int) -> Int { return x }"),
            vec![
                Function, Ident, LParen, Ident, Colon, IntType, RParen, Arrow, IntType, LBrace,
                Return, Ident, RBrace, Eof
            ]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            token_kinds("1 + 2 * 3 == 7"),
            vec![
                IntLiteral, Plus, IntLiteral, Star, IntLiteral, EqEq, IntLiteral, Eof
            ]
        );
    }

    #[test]
    fn comparison_operators() {
        assert_eq!(
            token_kinds("x < 10 and y >= 5"),
            vec![Ident, Lt, IntLiteral, And, Ident, GtEq, IntLiteral, Eof]
        );
    }

    #[test]
    fn string_literal() {
        assert_eq!(
            token_kinds("let s: String = \"hello world\""),
            vec![Let, Ident, Colon, StringType, Eq, StringLiteral, Eof]
        );
    }

    #[test]
    fn float_literal() {
        assert_eq!(
            token_kinds("let x: Float = 3.14"),
            vec![Let, Ident, Colon, FloatType, Eq, FloatLiteral, Eof]
        );
    }

    #[test]
    fn newline_as_terminator() {
        let kinds = token_kinds("let x: Int = 42\nlet y: Int = 10");
        assert_eq!(
            kinds,
            vec![
                Let, Ident, Colon, IntType, Eq, IntLiteral, Newline, Let, Ident, Colon, IntType,
                Eq, IntLiteral, Eof
            ]
        );
    }

    #[test]
    fn newline_suppressed_after_operator() {
        let kinds = token_kinds("x +\ny");
        assert_eq!(kinds, vec![Ident, Plus, Ident, Eof]);
    }

    #[test]
    fn newline_suppressed_inside_parens() {
        let kinds = token_kinds("foo(\nx,\ny\n)");
        assert_eq!(kinds, vec![Ident, LParen, Ident, Comma, Ident, RParen, Eof]);
    }

    #[test]
    fn comments_skipped() {
        let kinds = token_kinds("let x: Int = 42 // this is a comment\nlet y: Int = 10");
        assert_eq!(
            kinds,
            vec![
                Let, Ident, Colon, IntType, Eq, IntLiteral, Newline, Let, Ident, Colon, IntType,
                Eq, IntLiteral, Eof
            ]
        );
    }

    #[test]
    fn boolean_literals() {
        assert_eq!(token_kinds("true false"), vec![True, False, Eof]);
    }

    #[test]
    fn logical_operators() {
        assert_eq!(
            token_kinds("not x and y or z"),
            vec![Not, Ident, And, Ident, Or, Ident, Eof]
        );
    }

    #[test]
    fn unterminated_string() {
        let kinds = token_kinds("\"hello");
        assert_eq!(kinds, vec![Error, Eof]);
    }

    #[test]
    fn unknown_character() {
        let kinds = token_kinds("@");
        assert_eq!(kinds, vec![Error, Eof]);
    }

    #[test]
    fn if_else() {
        assert_eq!(
            token_kinds("if x == 1 { return true } else { return false }"),
            vec![
                If, Ident, EqEq, IntLiteral, LBrace, Return, True, RBrace, Else, LBrace, Return,
                False, RBrace, Eof
            ]
        );
    }

    // ── New tests ────────────────────────────────────────────

    #[test]
    fn empty_input() {
        assert_eq!(token_kinds(""), vec![Eof]);
    }

    #[test]
    fn whitespace_only() {
        assert_eq!(token_kinds("   \t  \t  "), vec![Eof]);
    }

    #[test]
    fn empty_string_literal() {
        let tokens = tokenize("\"\"", SourceId(0));
        assert_eq!(tokens[0].kind, StringLiteral);
        assert_eq!(tokens[0].text, "\"\"");
        assert_eq!(tokens[1].kind, Eof);
    }

    #[test]
    fn escape_sequences() {
        let tokens = tokenize("\"hello\\nworld\"", SourceId(0));
        assert_eq!(tokens[0].kind, StringLiteral);
        assert_eq!(tokens[0].text, "\"hello\\nworld\"");
    }

    #[test]
    fn string_with_escaped_quote() {
        let tokens = tokenize("\"say \\\"hi\\\"\"", SourceId(0));
        assert_eq!(tokens[0].kind, StringLiteral);
        assert_eq!(tokens[0].text, "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn zero_literal() {
        let tokens = tokenize("0", SourceId(0));
        assert_eq!(tokens[0].kind, IntLiteral);
        assert_eq!(tokens[0].text, "0");
    }

    #[test]
    fn integer_no_space() {
        assert_eq!(token_kinds("1+2"), vec![IntLiteral, Plus, IntLiteral, Eof]);
    }

    #[test]
    fn arrow_operator() {
        let tokens = tokenize("->", SourceId(0));
        assert_eq!(tokens[0].kind, Arrow);
        assert_eq!(tokens[0].text, "->");
    }

    #[test]
    fn not_equals() {
        let tokens = tokenize("!=", SourceId(0));
        assert_eq!(tokens[0].kind, NotEq);
        assert_eq!(tokens[0].text, "!=");
    }

    #[test]
    fn underscore_ident() {
        assert_eq!(token_kinds("_foo"), vec![Ident, Eof]);
        assert_eq!(token_kinds("_"), vec![Ident, Eof]);
    }

    #[test]
    fn keyword_prefix_ident() {
        // "trueish" should be Ident, not True followed by something.
        assert_eq!(token_kinds("trueish"), vec![Ident, Eof]);
        assert_eq!(token_kinds("returning"), vec![Ident, Eof]);
    }

    #[test]
    fn multiple_newlines_collapse() {
        // Multiple blank lines should produce at most one Newline token.
        let kinds = token_kinds("a\n\n\nb");
        assert_eq!(kinds, vec![Ident, Newline, Ident, Eof]);
    }

    #[test]
    fn comment_at_eof() {
        // A comment at the very end of input (no trailing newline) should
        // not cause issues.
        let kinds = token_kinds("42 // comment");
        assert_eq!(kinds, vec![IntLiteral, Eof]);
    }

    #[test]
    fn nested_parens_suppress_newlines() {
        let kinds = token_kinds("foo((a,\nb))");
        assert_eq!(
            kinds,
            vec![
                Ident, LParen, LParen, Ident, Comma, Ident, RParen, RParen, Eof
            ]
        );
    }

    #[test]
    fn braces_preserve_newlines() {
        // Newlines inside braces are significant (statement terminators in blocks).
        // The newline right after `{` is suppressed (continuation token), but the
        // newline after the identifier is preserved.
        let kinds = token_kinds("{\na\n}");
        assert_eq!(kinds, vec![LBrace, Ident, Newline, RBrace, Eof]);
    }

    #[test]
    fn all_comparison_ops() {
        assert_eq!(
            token_kinds("< > <= >= == !="),
            vec![Lt, Gt, LtEq, GtEq, EqEq, NotEq, Eof]
        );
    }

    #[test]
    fn dot_after_int() {
        // `42.foo` should be IntLiteral Dot Ident, not a float.
        assert_eq!(token_kinds("42.foo"), vec![IntLiteral, Dot, Ident, Eof]);
    }

    #[test]
    fn bracket_tokens() {
        assert_eq!(
            token_kinds("[1, 2, 3]"),
            vec![
                LBracket, IntLiteral, Comma, IntLiteral, Comma, IntLiteral, RBracket, Eof
            ]
        );
    }

    #[test]
    fn empty_brackets() {
        assert_eq!(token_kinds("[]"), vec![LBracket, RBracket, Eof]);
    }

    #[test]
    fn newline_suppressed_inside_brackets() {
        let kinds = token_kinds("[1,\n2,\n3]");
        assert_eq!(
            kinds,
            vec![
                LBracket, IntLiteral, Comma, IntLiteral, Comma, IntLiteral, RBracket, Eof
            ]
        );
    }

    #[test]
    fn consecutive_comments() {
        let kinds = token_kinds("// first\n// second\n// third\n42");
        assert_eq!(kinds, vec![IntLiteral, Eof]);
    }

    #[test]
    fn block_comment() {
        let kinds = token_kinds("42 /* this is a block comment */ 10");
        assert_eq!(kinds, vec![IntLiteral, IntLiteral, Eof]);
    }

    #[test]
    fn block_comment_multiline() {
        // Block comment replaces the comment content; surrounding newlines remain
        let kinds = token_kinds("42\n/* multi\nline\ncomment */\n10");
        assert_eq!(kinds, vec![IntLiteral, Newline, Newline, IntLiteral, Eof]);
    }

    #[test]
    fn nested_block_comment() {
        let kinds = token_kinds("42 /* outer /* inner */ still comment */ 10");
        assert_eq!(kinds, vec![IntLiteral, IntLiteral, Eof]);
    }

    #[test]
    fn block_comment_preserves_slash() {
        // A lone / is still the division operator
        let kinds = token_kinds("10 / 2");
        assert_eq!(kinds, vec![IntLiteral, Slash, IntLiteral, Eof]);
    }

    // --- Phase 1.8 feature tests ---

    #[test]
    fn question_mark_token() {
        let tokens = tokenize("?", SourceId(0));
        assert_eq!(tokens[0].kind, Question);
        assert_eq!(tokens[0].text, "?");
    }

    #[test]
    fn type_keyword_token() {
        let kinds = token_kinds("type");
        assert_eq!(kinds, vec![Type, Eof]);
    }

    #[test]
    fn string_with_interpolation_braces() {
        // The lexer treats braces inside strings as part of the string literal token.
        let tokens = tokenize("\"hello {name}\"", SourceId(0));
        assert_eq!(tokens[0].kind, StringLiteral);
        assert_eq!(tokens[0].text, "\"hello {name}\"");
        assert_eq!(tokens[1].kind, Eof);
    }

    // ── Unicode support tests ──────────────────────────────────────

    #[test]
    fn unicode_identifier_latin() {
        let tokens = tokenize("ñ", SourceId(0));
        assert_eq!(tokens[0].kind, Ident);
        assert_eq!(tokens[0].text, "ñ");
    }

    #[test]
    fn unicode_identifier_accented() {
        let tokens = tokenize("café", SourceId(0));
        assert_eq!(tokens[0].kind, Ident);
        assert_eq!(tokens[0].text, "café");
    }

    #[test]
    fn unicode_identifier_german() {
        let tokens = tokenize("über", SourceId(0));
        assert_eq!(tokens[0].kind, Ident);
        assert_eq!(tokens[0].text, "über");
    }

    #[test]
    fn unicode_identifier_cjk() {
        let tokens = tokenize("日本語", SourceId(0));
        assert_eq!(tokens[0].kind, Ident);
        assert_eq!(tokens[0].text, "日本語");
    }

    #[test]
    fn unicode_identifier_mixed_with_ascii() {
        let tokens = tokenize("café2go", SourceId(0));
        assert_eq!(tokens[0].kind, Ident);
        assert_eq!(tokens[0].text, "café2go");
    }

    #[test]
    fn unicode_in_string_literal() {
        let tokens = tokenize("\"emoji: 🔥\"", SourceId(0));
        assert_eq!(tokens[0].kind, StringLiteral);
    }

    #[test]
    fn unicode_non_letter_produces_error() {
        // Non-letter Unicode (e.g. emoji) as an identifier should produce an error token
        let tokens = tokenize("🔥", SourceId(0));
        assert_eq!(tokens[0].kind, Error);
    }

    #[test]
    fn unterminated_block_comment_produces_error() {
        let tokens = tokenize("/* unterminated", SourceId(0));
        assert!(
            tokens.iter().any(|t| t.kind == Error),
            "expected Error token for unterminated block comment"
        );
    }

    #[test]
    fn terminated_block_comment_no_error() {
        let tokens = tokenize("/* ok */ 42", SourceId(0));
        assert!(!tokens.iter().any(|t| t.kind == Error));
        assert!(tokens.iter().any(|t| t.kind == IntLiteral));
    }

    // ── Additional coverage tests ─────────────────────────────────

    #[test]
    fn dotdot_token() {
        let tokens = tokenize("..", SourceId(0));
        assert_eq!(tokens[0].kind, DotDot);
        assert_eq!(tokens[0].text, "..");
    }

    #[test]
    fn dotdot_newline_suppression() {
        // DotDot should suppress the following newline (continuation token).
        let kinds = token_kinds("x ..\ny");
        assert_eq!(kinds, vec![Ident, DotDot, Ident, Eof]);
    }

    #[test]
    fn question_mark_newline_suppression() {
        // Question should suppress the following newline (continuation token).
        let kinds = token_kinds("foo()?\nx");
        assert_eq!(kinds, vec![Ident, LParen, RParen, Question, Ident, Eof]);
    }

    #[test]
    fn lone_pipe_error() {
        let tokens = tokenize("|", SourceId(0));
        assert_eq!(tokens[0].kind, Error);
    }

    #[test]
    fn three_dots() {
        let kinds = token_kinds("...");
        assert_eq!(kinds, vec![DotDot, Dot, Eof]);
    }

    #[test]
    fn empty_block_comment() {
        let kinds = token_kinds("42 /**/ 10");
        assert_eq!(kinds, vec![IntLiteral, IntLiteral, Eof]);
    }

    #[test]
    fn block_comment_with_newlines() {
        let kinds = token_kinds("42 /* line1\nline2 */ 10");
        assert_eq!(kinds, vec![IntLiteral, IntLiteral, Eof]);
    }

    #[test]
    fn deeply_nested_block_comments() {
        let kinds = token_kinds("42 /* a /* b /* c */ */ */ 10");
        assert_eq!(kinds, vec![IntLiteral, IntLiteral, Eof]);
    }

    #[test]
    fn keywords_are_case_sensitive() {
        // Capitalised or upper-case variants should be plain identifiers.
        assert_eq!(token_kinds("Let"), vec![Ident, Eof]);
        assert_eq!(token_kinds("TRUE"), vec![Ident, Eof]);
        assert_eq!(token_kinds("int"), vec![Ident, Eof]);
    }

    #[test]
    fn span_positions_correct() {
        // "let x = 42"
        //  0123456789..
        let tokens = tokenize("let x = 42", SourceId(0));
        // `let` spans [0, 3)
        assert_eq!(tokens[0].kind, Let);
        assert_eq!(tokens[0].span.start, 0);
        assert_eq!(tokens[0].span.end, 3);
        // `x` spans [4, 5)
        assert_eq!(tokens[1].kind, Ident);
        assert_eq!(tokens[1].span.start, 4);
        assert_eq!(tokens[1].span.end, 5);
        // `=` spans [6, 7)
        assert_eq!(tokens[2].kind, Eq);
        assert_eq!(tokens[2].span.start, 6);
        assert_eq!(tokens[2].span.end, 7);
        // `42` spans [8, 10)
        assert_eq!(tokens[3].kind, IntLiteral);
        assert_eq!(tokens[3].span.start, 8);
        assert_eq!(tokens[3].span.end, 10);
    }

    #[test]
    fn leading_zeros_in_integers() {
        let tokens = tokenize("0123", SourceId(0));
        assert_eq!(tokens[0].kind, IntLiteral);
        assert_eq!(tokens[0].text, "0123");
        assert_eq!(tokens[1].kind, Eof);
    }

    #[test]
    fn float_dotdot_disambiguation() {
        // `1..2` should be IntLiteral DotDot IntLiteral, not a float.
        let kinds = token_kinds("1..2");
        assert_eq!(kinds, vec![IntLiteral, DotDot, IntLiteral, Eof]);
    }

    #[test]
    fn consecutive_operators() {
        let kinds = token_kinds("+++");
        assert_eq!(kinds, vec![Plus, Plus, Plus, Eof]);
    }

    #[test]
    fn unmatched_closing_parens_no_panic() {
        // Should not panic even with extra closing parens.
        let kinds = token_kinds("()))))");
        assert_eq!(kinds[0], LParen);
        assert_eq!(kinds[1], RParen);
        // The remaining tokens should all be RParen; just verify no panic.
        for kind in &kinds[2..kinds.len() - 1] {
            assert_eq!(*kind, RParen);
        }
        assert_eq!(*kinds.last().unwrap(), Eof);
    }

    #[test]
    fn carriage_return_handling() {
        let kinds = token_kinds("a\r\nb");
        assert_eq!(kinds, vec![Ident, Newline, Ident, Eof]);
    }

    /// Standalone `!` is an error token — Phoenix uses `not` for negation.
    #[test]
    fn bang_is_error_token() {
        let kinds = token_kinds("!x");
        assert_eq!(kinds, vec![Error, Ident, Eof]);
    }

    /// `!=` is still valid (not affected by `!` being an error).
    #[test]
    fn bang_equals_still_valid() {
        let kinds = token_kinds("a != b");
        assert_eq!(kinds, vec![Ident, NotEq, Ident, Eof]);
    }
}
