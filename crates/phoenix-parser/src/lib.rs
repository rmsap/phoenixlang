//! Parser for the Phoenix programming language.
//!
//! Implements a hand-written recursive-descent parser with Pratt parsing for
//! operator precedence. Produces an [`ast::Program`] AST from a token stream.
//! Parse errors are collected as diagnostics rather than aborting, allowing
//! multiple errors to be reported in a single pass.
#![warn(missing_docs)]

/// Abstract syntax tree node definitions for the Phoenix language.
pub mod ast;
/// Expression parsing: Pratt parser for operator precedence and all expression forms.
pub mod expr;
/// Free-variable analysis for lambda/closure capture tracking.
pub mod free_vars;
/// Core recursive-descent parser that produces AST nodes from a token stream.
pub mod parser;
/// Statement parsing: variable declarations, control flow, and blocks.
pub mod stmt;
/// Type expression parsing: named types, generic applications, and function types.
pub mod types;

pub use ast::Program;
pub use parser::parse;
