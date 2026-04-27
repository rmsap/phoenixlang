use crate::source::SourceMap;
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

/// A labeled secondary source location attached to a [`Diagnostic`].
///
/// Notes carry their own [`Span`] (which may live in a different
/// source file than the diagnostic's primary span) plus a short
/// human-readable label such as `"defined here"` or
/// `"first import here"`. The renderer is responsible for resolving
/// each note's span against the [`SourceMap`] so multi-file
/// diagnostics print one `file:line:col` prefix per span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Source location the note points at.
    pub span: Span,
    /// Short label describing why this span is relevant.
    pub message: String,
}

impl Note {
    /// Creates a new note pointing at `span` with the given label.
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

/// A diagnostic message attached to one or more source locations.
///
/// A diagnostic carries a [`Severity`], a primary message + [`Span`],
/// optional secondary [`Note`]s, an optional fix-up `suggestion`, and
/// an optional one-line `hint`. Rich diagnostics (multi-span "defined
/// here" reports, quick-fix suggestions) are constructed via the
/// fluent builder API:
///
/// ```ignore
/// Diagnostic::error("symbol `foo` is private", use_span)
///     .with_note(decl_span, "defined here")
///     .with_suggestion("import the public version `bar` instead")
/// ```
///
/// Construction is allocation-light: [`with_note`](Self::with_note)
/// and [`with_suggestion`](Self::with_suggestion) consume `self` and
/// return it, so a builder chain is a single move.
///
/// # Display
///
/// Two rendering paths share the same suffix logic (hint, suggestion,
/// notes) so output stays consistent:
///
/// * The plain [`std::fmt::Display`] impl renders without span
///   resolution — useful for tests and error pipelines that don't
///   carry a [`SourceMap`]. Notes appear as `note: <message>` with no
///   `file:line:col` prefix.
/// * [`Diagnostic::display_with`] returns a [`Display`](std::fmt::Display)
///   adapter that resolves every span (primary and notes) against the
///   given [`SourceMap`], so the rendered output prefixes the primary
///   line and each note line with `file:line:col`. This is the canonical
///   form used by drivers and tooling.
///
/// Span resolution can't live in the bare `Display` impl because it
/// would force every consumer to thread a `SourceMap` through
/// formatting.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Whether this diagnostic is an error or a warning.
    pub severity: Severity,
    /// Human-readable description of the problem.
    pub message: String,
    /// The primary source location where the problem was detected.
    pub span: Span,
    /// An optional one-line hint suggesting how to fix the problem.
    /// Free-form text — not a literal source replacement (use
    /// [`suggestion`](Self::suggestion) for that).
    pub hint: Option<String>,
    /// Secondary, labeled source locations (e.g. "defined here").
    /// May be empty.
    pub notes: Vec<Note>,
    /// Optional fix-up text intended as a literal replacement for the
    /// primary span. LSP clients can surface this as a quick-fix.
    pub suggestion: Option<String>,
}

impl Diagnostic {
    /// Creates a new error-level diagnostic with the given message and span.
    ///
    /// Argument order is `(message, span)` — the "what" before the
    /// "where" — matching the existing call sites.
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            span,
            hint: None,
            notes: Vec::new(),
            suggestion: None,
        }
    }

    /// Creates a new warning-level diagnostic with the given message and span.
    pub fn warning(message: impl Into<String>, span: Span) -> Self {
        Self {
            severity: Severity::Warning,
            message: message.into(),
            span,
            hint: None,
            notes: Vec::new(),
            suggestion: None,
        }
    }

    /// Attaches a hint to this diagnostic, returning `self` for chaining.
    /// Replaces any previously-set hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Appends a secondary labeled span (a "note") to this diagnostic.
    /// Notes accumulate in the order they are added; multiple
    /// `with_note` calls produce multiple visible "note: ..." lines.
    pub fn with_note(mut self, span: Span, message: impl Into<String>) -> Self {
        self.notes.push(Note {
            span,
            message: message.into(),
        });
        self
    }

    /// Attaches a quick-fix suggestion (literal replacement text for
    /// the primary span). Replaces any previously-set suggestion.
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    /// Returns a [`Display`](std::fmt::Display) adapter that renders
    /// this diagnostic with `file:line:col` prefixes resolved against
    /// `source_map`. The primary line carries one prefix; each note
    /// carries its own (which may live in a different file than the
    /// primary span).
    pub fn display_with<'a>(&'a self, source_map: &'a SourceMap) -> DiagnosticDisplay<'a> {
        DiagnosticDisplay {
            diag: self,
            source_map,
        }
    }

    fn fmt_suffix(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        source_map: Option<&SourceMap>,
    ) -> std::fmt::Result {
        if let Some(ref hint) = self.hint {
            write!(f, "\n  hint: {}", hint)?;
        }
        if let Some(ref suggestion) = self.suggestion {
            write!(f, "\n  suggestion: {}", suggestion)?;
        }
        for note in &self.notes {
            match source_map {
                Some(sm) => {
                    let loc = sm.line_col(note.span.source_id, note.span.start);
                    write!(
                        f,
                        "\n  note: {}:{}:{}: {}",
                        sm.name(note.span.source_id),
                        loc.line,
                        loc.col,
                        note.message,
                    )?;
                }
                None => write!(f, "\n  note: {}", note.message)?,
            }
        }
        Ok(())
    }
}

fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", severity_label(self.severity), self.message)?;
        self.fmt_suffix(f, None)
    }
}

/// [`Display`](std::fmt::Display) adapter returned by
/// [`Diagnostic::display_with`].
///
/// Renders the diagnostic with every span resolved against the
/// supplied [`SourceMap`], producing output of the form:
///
/// ```text
/// <file>:<line>:<col>: error: <message>
///   hint: ...
///   suggestion: ...
///   note: <other_file>:<line>:<col>: <note message>
/// ```
pub struct DiagnosticDisplay<'a> {
    diag: &'a Diagnostic,
    source_map: &'a SourceMap,
}

impl<'a> std::fmt::Display for DiagnosticDisplay<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let primary = self.diag.span;
        let loc = self.source_map.line_col(primary.source_id, primary.start);
        write!(
            f,
            "{}:{}:{}: {}: {}",
            self.source_map.name(primary.source_id),
            loc.line,
            loc.col,
            severity_label(self.diag.severity),
            self.diag.message,
        )?;
        self.diag.fmt_suffix(f, Some(self.source_map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SourceId;

    fn dummy_span() -> Span {
        Span::new(SourceId(0), 0, 1)
    }

    fn span_at(source: usize, start: usize, end: usize) -> Span {
        Span::new(SourceId(source), start, end)
    }

    #[test]
    fn error_has_correct_severity_and_message() {
        let d = Diagnostic::error("unexpected token", dummy_span());
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "unexpected token");
        assert!(d.hint.is_none());
        assert!(d.notes.is_empty());
        assert!(d.suggestion.is_none());
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
    fn with_note_appends() {
        let secondary = span_at(1, 50, 60);
        let d = Diagnostic::error("symbol is private", dummy_span())
            .with_note(secondary, "defined here");
        assert_eq!(d.notes.len(), 1);
        assert_eq!(d.notes[0].span, secondary);
        assert_eq!(d.notes[0].message, "defined here");
    }

    #[test]
    fn with_note_preserves_order_across_calls() {
        // Multiple notes must accumulate in source-readable order so
        // a "first defined here / shadowed here" reads top-to-bottom
        // the way the user wrote it.
        let first = span_at(0, 1, 2);
        let second = span_at(0, 3, 4);
        let d = Diagnostic::error("name conflict", dummy_span())
            .with_note(first, "first defined here")
            .with_note(second, "shadowed here");
        assert_eq!(d.notes.len(), 2);
        assert_eq!(d.notes[0].message, "first defined here");
        assert_eq!(d.notes[1].message, "shadowed here");
    }

    #[test]
    fn with_suggestion_attaches() {
        let d = Diagnostic::error("missing import", dummy_span())
            .with_suggestion("import models.user { User }");
        assert_eq!(d.suggestion.as_deref(), Some("import models.user { User }"));
    }

    #[test]
    fn with_suggestion_replaces_previous() {
        let d = Diagnostic::error("oops", dummy_span())
            .with_suggestion("first")
            .with_suggestion("second");
        assert_eq!(d.suggestion.as_deref(), Some("second"));
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

    #[test]
    fn display_with_notes_only() {
        let d = Diagnostic::error("symbol `foo` is private", dummy_span())
            .with_note(span_at(1, 50, 60), "defined here");
        assert_eq!(
            d.to_string(),
            "error: symbol `foo` is private\n  note: defined here"
        );
    }

    #[test]
    fn display_with_suggestion_only() {
        let d = Diagnostic::error("missing import", dummy_span())
            .with_suggestion("import models.user { User }");
        assert_eq!(
            d.to_string(),
            "error: missing import\n  suggestion: import models.user { User }"
        );
    }

    #[test]
    fn display_full_chain_renders_in_canonical_order() {
        // Order in the rendered output: hint, then suggestion, then
        // notes. Frozen here so changes to the order require updating
        // this test deliberately.
        let d = Diagnostic::error("name conflict", dummy_span())
            .with_hint("rename one of them")
            .with_suggestion("rename `foo` to `foo_legacy`")
            .with_note(span_at(0, 10, 20), "first defined here")
            .with_note(span_at(0, 30, 40), "shadowed here");
        assert_eq!(
            d.to_string(),
            "error: name conflict\n  \
             hint: rename one of them\n  \
             suggestion: rename `foo` to `foo_legacy`\n  \
             note: first defined here\n  \
             note: shadowed here"
        );
    }

    fn two_file_source_map() -> (SourceMap, SourceId, SourceId) {
        let mut sm = SourceMap::new();
        // file_a: a single line so the primary span lands at 1:1
        let a = sm.add("a.phx", "let foo = 1\n");
        // file_b: two lines; we'll point a note at line 2
        let b = sm.add("b.phx", "// line 1\nlet foo = 2\n");
        (sm, a, b)
    }

    #[test]
    fn display_with_resolves_primary_span() {
        let (sm, a, _b) = two_file_source_map();
        let d = Diagnostic::error("bad token", Span::new(a, 0, 3));
        assert_eq!(
            d.display_with(&sm).to_string(),
            "a.phx:1:1: error: bad token"
        );
    }

    #[test]
    fn display_with_resolves_note_in_other_file() {
        // The note's span lives in b.phx, not a.phx — the renderer
        // must use note.span.source_id, not the primary's, or the
        // note line will print the wrong file name and offset.
        let (sm, a, b) = two_file_source_map();
        // "let foo = 2" begins at byte 10 in b.phx (after "// line 1\n")
        let note_span = Span::new(b, 10, 13);
        let d = Diagnostic::error("symbol `foo` redefined", Span::new(a, 4, 7))
            .with_note(note_span, "first defined here");
        assert_eq!(
            d.display_with(&sm).to_string(),
            "a.phx:1:5: error: symbol `foo` redefined\n  \
             note: b.phx:2:1: first defined here"
        );
    }

    #[test]
    fn display_with_full_chain_resolves_all_spans() {
        let (sm, a, b) = two_file_source_map();
        let d = Diagnostic::error("conflict", Span::new(a, 0, 3))
            .with_hint("rename one")
            .with_suggestion("rename `foo`")
            .with_note(Span::new(b, 0, 9), "first")
            .with_note(Span::new(b, 10, 13), "second");
        assert_eq!(
            d.display_with(&sm).to_string(),
            "a.phx:1:1: error: conflict\n  \
             hint: rename one\n  \
             suggestion: rename `foo`\n  \
             note: b.phx:1:1: first\n  \
             note: b.phx:2:1: second"
        );
    }

    #[test]
    fn display_with_emits_warning_severity() {
        let (sm, a, _b) = two_file_source_map();
        let d = Diagnostic::warning("unused", Span::new(a, 4, 7));
        assert_eq!(
            d.display_with(&sm).to_string(),
            "a.phx:1:5: warning: unused"
        );
    }
}
