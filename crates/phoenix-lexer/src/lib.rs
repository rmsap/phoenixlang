//! Lexer for the Phoenix programming language.
//!
//! Converts source text into a stream of [`token::Token`]s. The lexer handles
//! newline-based statement termination, `#`-style comments, and suppresses
//! insignificant newlines inside parentheses, braces, and after continuation
//! operators.
#![warn(missing_docs)]

/// Core lexer implementation: tokenization and newline handling.
pub mod lexer;
/// Token types and the `Token` struct.
pub mod token;

pub use lexer::{Lexer, tokenize};
pub use token::{Token, TokenKind};
