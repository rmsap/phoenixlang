//! Parser for the Phoenix programming language.
//!
//! Implements a hand-written recursive-descent parser with Pratt parsing for
//! operator precedence. Produces an [`ast::Program`] AST from a token stream.
//! Parse errors are collected as diagnostics rather than aborting, allowing
//! multiple errors to be reported in a single pass.

pub mod ast;
pub mod expr;
pub mod free_vars;
pub mod parser;
pub mod stmt;
pub mod types;

pub use ast::Program;
pub use parser::parse;
