//! Shared error type and helpers for the IR interpreter.

use std::fmt;

/// A runtime error from the IR interpreter.
#[derive(Debug)]
pub struct IrRuntimeError {
    /// Human-readable error message.
    pub message: String,
}

impl fmt::Display for IrRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for IrRuntimeError {}

/// Convenience alias used throughout the interpreter.
pub type Result<T> = std::result::Result<T, IrRuntimeError>;

/// Construct an error result with the given message.
pub fn error<T>(msg: impl Into<String>) -> Result<T> {
    Err(IrRuntimeError {
        message: msg.into(),
    })
}
