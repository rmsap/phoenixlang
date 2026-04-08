//! Shared types used across all Phoenix compiler crates.
//!
//! Provides [`span::Span`] for source locations, [`diagnostics::Diagnostic`]
//! for error/warning messages, and [`source::SourceMap`] for managing loaded
//! source files.

pub mod diagnostics;
pub mod source;
pub mod span;

pub use diagnostics::Diagnostic;
pub use source::SourceMap;
pub use span::{SourceId, Span};
