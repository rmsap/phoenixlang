//! SSA-style intermediate representation for the Phoenix compiler.
//!
//! This crate defines a flat, typed IR with basic blocks, explicit control
//! flow, and SSA values.  The main entry point is [`lower::lower`], which
//! takes a parsed AST and a [`phoenix_sema::ResolvedModule`] and produces an
//! [`IrModule`].
//!
//! # Architecture
//!
//! The IR sits between semantic analysis and code generation:
//!
//! ```text
//! Source → Lexer → Parser (AST) → Sema (ResolvedModule)
//!                                       ↓
//!                                   IR Lowering (this crate)
//!                                       ↓
//!                                   IrModule
//!                                       ↓
//!                              Cranelift backend (phoenix-cranelift) / WASM (future)
//! ```
#![warn(missing_docs)]

/// Basic block definitions.
pub mod block;
/// Pretty-printer for the IR.
pub mod display;
/// IR instructions and SSA value identifiers.
pub mod instruction;
/// Top-level IR module and function definitions.
pub mod module;
/// Block terminators (control flow).
pub mod terminator;
/// IR-level type representation.
pub mod types;
/// Type-safe allocator for SSA [`instruction::ValueId`]s.
pub mod value_alloc;
/// IR verification (structural invariants).
pub mod verify;

/// AST-to-IR lowering pass.
pub mod lower;
mod lower_decl;
mod lower_dyn;
mod lower_expr;
mod lower_match;
mod lower_stmt;
// Monomorphization pass — specializes generic functions at every concrete
// call site. See the module-level docs in `monomorphize.rs` for the algorithm.
mod monomorphize;

#[cfg(test)]
mod tests;

/// Lower a type-checked Phoenix program into an [`IrModule`].
pub use lower::lower;
/// The top-level IR container for a compilation unit.
pub use module::IrModule;
