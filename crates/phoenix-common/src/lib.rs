//! Shared types used across all Phoenix compiler crates.
//!
//! Provides [`span::Span`] for source locations, [`diagnostics::Diagnostic`]
//! for error/warning messages, and [`source::SourceMap`] for managing loaded
//! source files.
#![warn(missing_docs)]

/// Error and warning diagnostics produced by the compiler pipeline.
pub mod diagnostics;
/// Stable post-sema identifiers (`FuncId`, `StructId`, `EnumId`,
/// `TraitId`) shared across sema, IR, and the backends.
pub mod ids;
/// Module-path identity (`ModulePath`) shared across sema, IR, the resolver,
/// and backends, plus the `module_qualify` mangling helper.
pub mod module_path;
/// Source file registry for multi-file compilation.
pub mod source;
/// Byte-offset spans and source file identifiers.
pub mod span;

pub use diagnostics::Diagnostic;
pub use ids::{EnumId, FuncId, StructId, TraitId};
pub use module_path::{ModulePath, module_qualify};
pub use source::SourceMap;
pub use span::{SourceId, Span};
