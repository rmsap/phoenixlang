//! Error types for the Cranelift code generation backend.

use std::fmt;

/// An error that occurred during native code generation.
#[derive(Debug)]
pub struct CompileError {
    /// Human-readable description of what went wrong.
    pub message: String,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "compile error: {}", self.message)
    }
}

impl std::error::Error for CompileError {}

impl CompileError {
    /// Create a new compile error with the given message.
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }

    /// Create a compile error from any `Display`-able value.
    pub fn from_display(e: impl fmt::Display) -> Self {
        Self {
            message: e.to_string(),
        }
    }
}

impl From<String> for CompileError {
    fn from(msg: String) -> Self {
        Self { message: msg }
    }
}

impl From<cranelift_module::ModuleError> for CompileError {
    fn from(e: cranelift_module::ModuleError) -> Self {
        Self {
            message: e.to_string(),
        }
    }
}
