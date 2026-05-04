use serde::Serialize;

/// A unique identifier for a source file.
///
/// Each source file loaded into the [`crate::source::SourceMap`] is assigned a
/// distinct `SourceId`. The inner `usize` is the index into the source map's
/// internal file list.
///
/// **Dense-iteration invariant:** ids are allocated in order starting at
/// `0`, so for any given map every id in `0..source_map.len()` is a
/// valid handle. [`crate::source::SourceMap::len`]'s doc relies on this
/// — callers (e.g. the LSP's `build_source_id_to_url_from_map`) iterate
/// raw indices to enumerate every file. Swapping the inner representation
/// for anything other than a dense `usize` requires updating those
/// callers in lock-step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct SourceId(pub usize);

/// A byte-offset range `[start, end)` within a single source file.
///
/// Spans track which source file they belong to via [`SourceId`] and store
/// inclusive-start / exclusive-end byte positions. They are used throughout the
/// compiler to attach source-location information to tokens, AST nodes, and
/// diagnostics.
///
/// `Span::BUILTIN` is the sentinel for synthesized nodes with no source
/// location (e.g. compiler-generated `Option`/`Result`). `Span` deliberately
/// does *not* derive `Default` — `SourceId(0)` is the first real source file
/// in any non-empty `SourceMap`, so a `Span::default()` would silently
/// collide with real source positions if accidentally spread into a real
/// AST node. Use `Span::BUILTIN` explicitly when a sentinel is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct Span {
    /// The source file this span belongs to.
    pub source_id: SourceId,
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl Span {
    /// A sentinel span representing a built-in symbol with no source location.
    pub const BUILTIN: Span = Span {
        source_id: SourceId(0),
        start: 0,
        end: 0,
    };

    /// Creates a new span in the given source file covering bytes `[start, end)`.
    ///
    /// # Panics
    /// Debug-asserts that `start <= end`.
    pub fn new(source_id: SourceId, start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "Span::new: start ({}) > end ({})", start, end);
        Self {
            source_id,
            start,
            end,
        }
    }

    /// Returns the length of this span in bytes.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Returns `true` if the span covers zero bytes.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Merge two spans into one that covers both.
    ///
    /// Both spans must belong to the same source file (debug-asserted).
    /// The resulting span starts at the minimum of the two starts and ends
    /// at the maximum of the two ends, so it covers the full extent of both
    /// input spans (and any gap between them).
    pub fn merge(self, other: Span) -> Span {
        debug_assert_eq!(self.source_id, other.source_id);
        Span {
            source_id: self.source_id,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_correct_span() {
        let sid = SourceId(0);
        let span = Span::new(sid, 5, 10);
        assert_eq!(span.source_id, sid);
        assert_eq!(span.start, 5);
        assert_eq!(span.end, 10);
    }

    #[test]
    fn len_returns_correct_length() {
        let span = Span::new(SourceId(0), 3, 15);
        assert_eq!(span.len(), 12);
    }

    #[test]
    fn len_of_zero_width_span() {
        let span = Span::new(SourceId(0), 7, 7);
        assert_eq!(span.len(), 0);
    }

    #[test]
    fn is_empty_for_empty_span() {
        let span = Span::new(SourceId(0), 4, 4);
        assert!(span.is_empty());
    }

    #[test]
    fn is_empty_for_non_empty_span() {
        let span = Span::new(SourceId(0), 4, 5);
        assert!(!span.is_empty());
    }

    #[test]
    fn merge_overlapping_spans() {
        let a = Span::new(SourceId(0), 2, 8);
        let b = Span::new(SourceId(0), 5, 12);
        let merged = a.merge(b);
        assert_eq!(merged.start, 2);
        assert_eq!(merged.end, 12);
        assert_eq!(merged.source_id, SourceId(0));
    }

    #[test]
    fn merge_adjacent_spans() {
        let a = Span::new(SourceId(1), 0, 5);
        let b = Span::new(SourceId(1), 5, 10);
        let merged = a.merge(b);
        assert_eq!(merged.start, 0);
        assert_eq!(merged.end, 10);
    }

    #[test]
    fn merge_non_overlapping_spans() {
        let a = Span::new(SourceId(0), 0, 3);
        let b = Span::new(SourceId(0), 7, 10);
        let merged = a.merge(b);
        // The merged span covers the gap between the two spans.
        assert_eq!(merged.start, 0);
        assert_eq!(merged.end, 10);
    }

    #[test]
    fn merge_is_commutative() {
        let a = Span::new(SourceId(0), 1, 4);
        let b = Span::new(SourceId(0), 6, 9);
        assert_eq!(a.merge(b), b.merge(a));
    }

    #[test]
    fn merge_with_subset_span() {
        let outer = Span::new(SourceId(0), 0, 20);
        let inner = Span::new(SourceId(0), 5, 10);
        let merged = outer.merge(inner);
        assert_eq!(merged.start, 0);
        assert_eq!(merged.end, 20);
    }
}
