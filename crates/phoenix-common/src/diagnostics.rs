use crate::span::Span;

/// Severity level for diagnostics.
///
/// Determines how a [`Diagnostic`] is presented to the user (e.g. as an
/// error that prevents compilation or a warning that allows it to continue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A fatal error — compilation cannot proceed.
    Error,
    /// A non-fatal warning — compilation may still succeed.
    Warning,
}

/// A diagnostic message attached to a source location.
///
/// Diagnostics carry a [`Severity`], a human-readable message, the [`Span`]
/// in the source where the problem was detected, and an optional hint that
/// suggests how to fix the issue.
///
/// # Display
///
/// The [`std::fmt::Display`] implementation formats the diagnostic as:
///
/// ```text
/// error: something went wrong
///   hint: try doing X instead
/// ```
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Whether this diagnostic is an error or a warning.
    pub severity: Severity,
    /// Human-readable description of the problem.
    pub message: String,
    /// The source location where the problem was detected.
    pub span: Span,
    /// An optional hint suggesting how to fix the problem.
    pub hint: Option<String>,
}

impl Diagnostic {
    /// Creates a new error-level diagnostic with the given message and span.
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            span,
            hint: None,
        }
    }

    /// Creates a new warning-level diagnostic with the given message and span.
    pub fn warning(message: impl Into<String>, span: Span) -> Self {
        Self {
            severity: Severity::Warning,
            message: message.into(),
            span,
            hint: None,
        }
    }

    /// Attaches a hint to this diagnostic, returning `self` for chaining.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{}: {}", prefix, self.message)?;
        if let Some(ref hint) = self.hint {
            write!(f, "\n  hint: {}", hint)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SourceId;

    fn dummy_span() -> Span {
        Span::new(SourceId(0), 0, 1)
    }

    #[test]
    fn error_has_correct_severity_and_message() {
        let d = Diagnostic::error("unexpected token", dummy_span());
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "unexpected token");
        assert!(d.hint.is_none());
    }

    #[test]
    fn warning_has_correct_severity_and_message() {
        let d = Diagnostic::warning("unused variable", dummy_span());
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.message, "unused variable");
        assert!(d.hint.is_none());
    }

    #[test]
    fn error_preserves_span() {
        let span = Span::new(SourceId(2), 10, 20);
        let d = Diagnostic::error("bad", span);
        assert_eq!(d.span, span);
    }

    #[test]
    fn with_hint_adds_hint() {
        let d =
            Diagnostic::error("type mismatch", dummy_span()).with_hint("expected i32, found bool");
        assert_eq!(d.hint.as_deref(), Some("expected i32, found bool"));
    }

    #[test]
    fn with_hint_replaces_previous_hint() {
        let d = Diagnostic::error("oops", dummy_span())
            .with_hint("first hint")
            .with_hint("second hint");
        assert_eq!(d.hint.as_deref(), Some("second hint"));
    }

    #[test]
    fn display_error_without_hint() {
        let d = Diagnostic::error("something broke", dummy_span());
        assert_eq!(d.to_string(), "error: something broke");
    }

    #[test]
    fn display_warning_without_hint() {
        let d = Diagnostic::warning("this is suspicious", dummy_span());
        assert_eq!(d.to_string(), "warning: this is suspicious");
    }

    #[test]
    fn display_error_with_hint() {
        let d = Diagnostic::error("missing semicolon", dummy_span()).with_hint("add `;` here");
        assert_eq!(
            d.to_string(),
            "error: missing semicolon\n  hint: add `;` here"
        );
    }

    #[test]
    fn display_warning_with_hint() {
        let d = Diagnostic::warning("unused import", dummy_span()).with_hint("remove this line");
        assert_eq!(
            d.to_string(),
            "warning: unused import\n  hint: remove this line"
        );
    }
}
