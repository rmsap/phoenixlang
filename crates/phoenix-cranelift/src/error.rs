//! Error types for the Cranelift code generation backend.

use std::fmt;

/// Discriminator for `CompileError` so callers can branch on specific
/// failures without grepping the message text. New variants should be
/// added only when a caller actually needs to react differently — most
/// failures stay `Generic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileErrorKind {
    /// Unspecific compile failure; the `message` field carries the
    /// detail. Default for `CompileError::new`.
    Generic,
    /// The wasm32-linear backend couldn't locate `phoenix_runtime.wasm`.
    /// Integration tests gate skip-vs-fail behavior on this kind so a
    /// future copy-edit to the diagnostic text doesn't silently break
    /// the gate. See `wasm::runtime_discovery`.
    RuntimeWasmNotFound,
}

/// An error that occurred during native code generation.
#[derive(Debug)]
pub struct CompileError {
    /// Human-readable description of what went wrong.
    pub message: String,
    /// Machine-readable kind so callers can branch on specific failure
    /// modes without inspecting `message`.
    pub kind: CompileErrorKind,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "compile error: {}", self.message)
    }
}

impl std::error::Error for CompileError {}

impl CompileError {
    /// Create a new compile error with the given message. The kind
    /// defaults to [`CompileErrorKind::Generic`]; use
    /// [`Self::with_kind`] to specialize.
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            kind: CompileErrorKind::Generic,
        }
    }

    /// Create a compile error with an explicit kind.
    pub fn with_kind(msg: impl Into<String>, kind: CompileErrorKind) -> Self {
        Self {
            message: msg.into(),
            kind,
        }
    }

    /// Create a compile error from any `Display`-able value.
    pub fn from_display(e: impl fmt::Display) -> Self {
        Self {
            message: e.to_string(),
            kind: CompileErrorKind::Generic,
        }
    }
}

impl From<String> for CompileError {
    fn from(msg: String) -> Self {
        Self {
            message: msg,
            kind: CompileErrorKind::Generic,
        }
    }
}

impl From<cranelift_module::ModuleError> for CompileError {
    fn from(e: cranelift_module::ModuleError) -> Self {
        Self {
            message: e.to_string(),
            kind: CompileErrorKind::Generic,
        }
    }
}
