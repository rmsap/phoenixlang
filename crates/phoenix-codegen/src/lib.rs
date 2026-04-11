//! Code generation backends for Phoenix Gen.
//!
//! This crate generates typed code for target languages from a parsed and
//! type-checked Phoenix program containing `endpoint` declarations.
//!
//! Supported targets:
//! - **TypeScript** ([`typescript`]) — generates interfaces, derived types,
//!   a fetch-based client SDK, server handler interfaces, and Express router.
//! - **Python** ([`python`]) — generates Pydantic models, a typed httpx client,
//!   a handler Protocol class, and a FastAPI router.
//! - **Go** ([`go`]) — generates structs with JSON tags, an HTTP client,
//!   a Handlers interface, and a `net/http` router.
//! - **OpenAPI** ([`openapi`]) — generates an OpenAPI 3.1 JSON specification.
#![warn(missing_docs)]

/// Go code generation backend (structs, net/http client/server).
pub mod go;
/// OpenAPI 3.1 JSON specification generation.
pub mod openapi;
/// Python code generation backend (Pydantic, FastAPI, httpx).
pub mod python;
/// TypeScript code generation backend (interfaces, fetch client, Express router).
pub mod typescript;

pub use go::{GoFiles, generate_go};
pub use openapi::generate_openapi;
pub use python::{PythonFiles, generate_python};
pub use typescript::{GeneratedFiles, generate_typescript};

/// Controls which subset of files to generate.
///
/// When generating code for a target language, the output consists of shared
/// type definitions, a client SDK, and server-side handlers/router. This enum
/// lets callers request only the client or server subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GenMode {
    /// Generate all files (types + client + server). Default.
    #[default]
    Both,
    /// Generate only shared types and the client SDK.
    ClientOnly,
    /// Generate only shared types, handler interfaces, and the server router.
    ServerOnly,
}

/// Capitalizes the first character of a string.
///
/// Returns an empty string for empty input.
pub(crate) fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
