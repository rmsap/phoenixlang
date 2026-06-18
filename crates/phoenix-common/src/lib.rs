//! Shared types used across all Phoenix compiler crates.
//!
//! Provides [`span::Span`] for source locations, [`diagnostics::Diagnostic`]
//! for error/warning messages, and [`source::SourceMap`] for managing loaded
//! source files.
#![warn(missing_docs)]

/// Generic algorithm helpers shared across the AST and IR
/// interpreters (currently `merge_sort_by`, used by `List.sortBy` in
/// both interpreters).
pub mod algorithms;
/// Error and warning diagnostics produced by the compiler pipeline.
pub mod diagnostics;
/// Host-FFI exchange types (`HostValue`, `HostContext`, `HostRegistry`) shared
/// by the interpreters for `extern js` host calls.
pub mod host;
/// Identifier-casing helpers shared between sema and the codegen backends
/// (`capitalize`, the rule generated type names are built with, and
/// `to_screaming_snake`, the enum-value/const casing).
pub mod idents;
/// Stable post-sema identifiers (`FuncId`, `StructId`, `EnumId`,
/// `TraitId`) shared across sema, IR, and the backends.
pub mod ids;
/// Canonical map-key projection and last-wins dedup shared by the AST
/// and IR interpreters' `MapBuilder.freeze` / map-literal lowering.
pub mod map_key;
/// Module-path identity (`ModulePath`) shared across sema, IR, the resolver,
/// and backends, plus the `module_qualify` mangling helper.
pub mod module_path;
/// Source file registry for multi-file compilation.
pub mod source;
/// Byte-offset spans and source file identifiers.
pub mod span;

pub use diagnostics::Diagnostic;
pub use idents::{capitalize, to_screaming_snake};
pub use ids::{EnumId, FuncId, StructId, TraitId};
pub use module_path::{ModulePath, module_qualify};
pub use source::SourceMap;
pub use span::{SourceId, Span};
