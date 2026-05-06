//! Semantic analysis for the Phoenix programming language.
//!
//! Performs name resolution and type checking over a parsed AST. The main
//! entry point is [`checker::check`], which returns a list of diagnostics
//! for any semantic errors found (undefined variables, type mismatches,
//! mutability violations, etc.).
#![warn(missing_docs)]

mod check_builtins_list;
mod check_builtins_map;
mod check_builtins_option;
mod check_builtins_result;
mod check_builtins_string;
mod check_endpoint;
mod check_expr;
mod check_expr_call;
mod check_register;
mod check_stmt;
mod check_types;
/// The semantic checker: two-pass name resolution and type checking.
pub mod checker;
#[cfg(test)]
mod checker_tests;
mod defer;
mod expr_walk;
mod field_privacy;
mod id_alloc;
mod impl_classify;
mod import_resolve;
mod module_scope;
mod object_safety;
mod orphan;
/// The post-sema handoff type [`ResolvedModule`](resolved::ResolvedModule).
pub mod resolved;
/// Lexical scope stack for variable name resolution.
pub mod scope;
/// The Phoenix type system representation.
pub mod types;

pub use checker::{
    DefaultValue, DerivedField, EndpointInfo, EnumInfo, FieldInfo, FunctionInfo, MethodInfo,
    QueryParamInfo, ResolvedDerivedType, StructInfo, SymbolKind, SymbolRef, TraitInfo,
    TraitMethodInfo, TypeAliasInfo, check,
};
pub use resolved::{Analysis, ResolvedModule};
