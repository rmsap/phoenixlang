#![allow(clippy::result_large_err)]
#![warn(missing_docs)]
//! Tree-walk interpreter for the Phoenix programming language.
//!
//! Executes a type-checked AST directly by walking the tree. The main entry
//! point is [`interpreter::run`], which finds and calls `main()`. Supports
//! variables, functions, control flow, arithmetic, and built-in functions
//! (`print`, `toString`).

/// Variable environment with lexical scope stack and closure support.
pub mod env;
mod eval_builtins;
/// Core interpreter: AST walking, expression evaluation, and statement execution.
pub mod interpreter;
/// Runtime value representation for the Phoenix interpreter.
pub mod value;

pub use interpreter::{RuntimeError, run, run_and_capture};
pub use value::Value;
