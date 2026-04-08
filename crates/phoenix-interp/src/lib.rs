#![allow(clippy::result_large_err)]
//! Tree-walk interpreter for the Phoenix programming language.
//!
//! Executes a type-checked AST directly by walking the tree. The main entry
//! point is [`interpreter::run`], which finds and calls `main()`. Supports
//! variables, functions, control flow, arithmetic, and built-in functions
//! (`print`, `toString`).

pub mod env;
mod eval_builtins;
pub mod interpreter;
pub mod value;

pub use interpreter::{RuntimeError, run, run_and_capture};
pub use value::Value;
