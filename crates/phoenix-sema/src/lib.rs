//! Semantic analysis for the Phoenix programming language.
//!
//! Performs name resolution and type checking over a parsed AST. The main
//! entry point is [`checker::check`], which returns a list of diagnostics
//! for any semantic errors found (undefined variables, type mismatches,
//! mutability violations, etc.).

mod check_builtins_list;
mod check_builtins_map;
mod check_builtins_option;
mod check_builtins_result;
mod check_builtins_string;
mod check_expr;
mod check_register;
mod check_stmt;
mod check_types;
pub mod checker;
pub mod scope;
pub mod types;

pub use checker::{
    CheckResult, EnumInfo, FunctionInfo, MethodInfo, StructInfo, TraitInfo, TraitMethodInfo,
    TypeAliasInfo, check,
};
