use crate::span::{SourceId, Span};

/// A source file loaded into memory.
///
/// Stores the file name, its full contents, and a pre-computed index of
/// line-start byte offsets so that [`SourceMap::line_col`] lookups are fast.
#[derive(Debug)]
struct SourceFile {
    /// Human-readable file name (e.g. `"main.phx"`).
    name: String,
    /// The full UTF-8 source text.
    contents: String,
    /// Byte offsets of each line start (for line/column lookup).
    line_starts: Vec<usize>,
}

/// Manages all loaded source files.
///
/// `SourceMap` is the central store for every source file the compiler reads.
/// Files are added with [`SourceMap::add`], which returns a [`SourceId`] that
/// can later be used to query the file's name, contents, or to convert byte
/// offsets into human-readable line/column positions.
#[derive(Debug, Default)]
pub struct SourceMap {
    /// Ordered list of loaded source files; the index is the `SourceId`.
    files: Vec<SourceFile>,
}

/// Line and column (both 1-based) in a source file.
///
/// Produced by [`SourceMap::line_col`] to turn raw byte offsets into
/// positions that are meaningful to humans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number (in bytes, not characters).
    pub col: usize,
}

impl SourceMap {
    /// Creates a new, empty source map.
    pub fn new() -> Self {
        Self { files: Vec::new() }
    }

    /// Add a source file and return its [`SourceId`].
    ///
    /// The returned ID can be used with all other `SourceMap` methods to
    /// reference this file.
    pub fn add(&mut self, name: impl Into<String>, contents: impl Into<String>) -> SourceId {
        let contents = contents.into();
        let line_starts = std::iter::once(0)
            .chain(
                contents
                    .char_indices()
                    .filter(|&(_, c)| c == '\n')
                    .map(|(i, _)| i + 1),
            )
            .collect();

        let id = SourceId(self.files.len());
        self.files.push(SourceFile {
            name: name.into(),
            contents,
            line_starts,
        });
        id
    }

    /// Get the full contents of the source file identified by `id`.
    ///
    /// # Panics
    /// Panics if `id` was not returned by a prior call to [`add`](Self::add)
    /// on this source map.
    pub fn contents(&self, id: SourceId) -> &str {
        &self.files[id.0].contents
    }

    /// Get the file name of the source file identified by `id`.
    ///
    /// # Panics
    /// Panics if `id` was not returned by a prior call to [`add`](Self::add)
    /// on this source map.
    pub fn name(&self, id: SourceId) -> &str {
        &self.files[id.0].name
    }

    /// Get the source text covered by `span`.
    ///
    /// The span's `source_id` determines which file is sliced, and
    /// `start..end` selects the byte range within that file.
    ///
    /// # Panics
    /// Panics if the span's source ID is invalid or its byte range is out of
    /// bounds or does not align to UTF-8 character boundaries.
    pub fn span_text(&self, span: Span) -> &str {
        &self.files[span.source_id.0].contents[span.start..span.end]
    }

    /// Convert a byte offset to a 1-based [`LineCol`] position.
    ///
    /// The column is measured in bytes from the start of the line, not in
    /// characters. For ASCII content the two are the same; for multi-byte
    /// UTF-8 content they may differ.
    ///
    /// # Panics
    /// Panics if `id` is invalid or `offset` is beyond the file's contents.
    pub fn line_col(&self, id: SourceId, offset: usize) -> LineCol {
        let file = &self.files[id.0];
        let line = file
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let col = offset - file.line_starts[line];
        LineCol {
            line: line + 1,
            col: col + 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_lookup() {
        let mut map = SourceMap::new();
        let id = map.add("test.phx", "hello\nworld\n");
        assert_eq!(map.line_col(id, 0), LineCol { line: 1, col: 1 });
        assert_eq!(map.line_col(id, 5), LineCol { line: 1, col: 6 });
        assert_eq!(map.line_col(id, 6), LineCol { line: 2, col: 1 });
        assert_eq!(map.line_col(id, 11), LineCol { line: 2, col: 6 });
    }

    // --- Multiple source files ---

    #[test]
    fn multiple_source_files_get_distinct_ids() {
        let mut map = SourceMap::new();
        let id0 = map.add("a.phx", "aaa");
        let id1 = map.add("b.phx", "bbb");
        let id2 = map.add("c.phx", "ccc");
        assert_ne!(id0, id1);
        assert_ne!(id1, id2);
        assert_ne!(id0, id2);
    }

    #[test]
    fn contents_returns_correct_data_for_each_file() {
        let mut map = SourceMap::new();
        let id0 = map.add("a.phx", "alpha");
        let id1 = map.add("b.phx", "beta");
        assert_eq!(map.contents(id0), "alpha");
        assert_eq!(map.contents(id1), "beta");
    }

    #[test]
    fn name_returns_correct_file_name() {
        let mut map = SourceMap::new();
        let id0 = map.add("first.phx", "x");
        let id1 = map.add("second.phx", "y");
        assert_eq!(map.name(id0), "first.phx");
        assert_eq!(map.name(id1), "second.phx");
    }

    // --- span_text ---

    #[test]
    fn span_text_extracts_correct_substring() {
        let mut map = SourceMap::new();
        let id = map.add("t.phx", "hello world");
        let span = Span::new(id, 6, 11);
        assert_eq!(map.span_text(span), "world");
    }

    #[test]
    fn span_text_full_contents() {
        let mut map = SourceMap::new();
        let id = map.add("t.phx", "abc");
        let span = Span::new(id, 0, 3);
        assert_eq!(map.span_text(span), "abc");
    }

    #[test]
    fn span_text_empty_span() {
        let mut map = SourceMap::new();
        let id = map.add("t.phx", "abc");
        let span = Span::new(id, 1, 1);
        assert_eq!(map.span_text(span), "");
    }

    // --- line_col edge cases ---

    #[test]
    fn line_col_offset_zero() {
        let mut map = SourceMap::new();
        let id = map.add("t.phx", "abc");
        assert_eq!(map.line_col(id, 0), LineCol { line: 1, col: 1 });
    }

    #[test]
    fn line_col_end_of_file() {
        let mut map = SourceMap::new();
        // "abc" is 3 bytes; offset 3 is one past the last byte.
        let id = map.add("t.phx", "abc");
        let lc = map.line_col(id, 3);
        assert_eq!(lc, LineCol { line: 1, col: 4 });
    }

    #[test]
    fn line_col_empty_file() {
        let mut map = SourceMap::new();
        let id = map.add("empty.phx", "");
        assert_eq!(map.line_col(id, 0), LineCol { line: 1, col: 1 });
    }

    #[test]
    fn line_col_file_with_no_newlines() {
        let mut map = SourceMap::new();
        let id = map.add("single.phx", "no newlines here");
        // Every offset should be on line 1.
        assert_eq!(map.line_col(id, 0), LineCol { line: 1, col: 1 });
        assert_eq!(map.line_col(id, 3), LineCol { line: 1, col: 4 });
        assert_eq!(map.line_col(id, 16), LineCol { line: 1, col: 17 });
    }

    #[test]
    fn line_col_at_newline_character() {
        let mut map = SourceMap::new();
        let id = map.add("t.phx", "ab\ncd\n");
        // '\n' at offset 2 is still on line 1 (col 3).
        assert_eq!(map.line_col(id, 2), LineCol { line: 1, col: 3 });
        // offset 3 is start of line 2.
        assert_eq!(map.line_col(id, 3), LineCol { line: 2, col: 1 });
    }

    // --- Unicode content ---

    #[test]
    fn line_col_unicode_multibyte_chars() {
        let mut map = SourceMap::new();
        // U+00E9 (e-acute) is 2 bytes in UTF-8.
        // U+1F600 (grinning face) is 4 bytes in UTF-8.
        let src = "caf\u{00E9}\n\u{1F600}ok";
        let id = map.add("uni.phx", src);

        // "caf" = 3 bytes, then \u{00E9} = 2 bytes => "cafe\u{0301}" ends at offset 5
        // '\n' at offset 5, line 2 starts at offset 6
        assert_eq!(map.line_col(id, 0), LineCol { line: 1, col: 1 });
        // byte offset 3 is start of the e-acute (still line 1)
        assert_eq!(map.line_col(id, 3), LineCol { line: 1, col: 4 });
        // byte offset 5 is the '\n'
        assert_eq!(map.line_col(id, 5), LineCol { line: 1, col: 6 });
        // byte offset 6 is start of line 2
        assert_eq!(map.line_col(id, 6), LineCol { line: 2, col: 1 });
        // The emoji is 4 bytes; 'o' starts at offset 10
        assert_eq!(map.line_col(id, 10), LineCol { line: 2, col: 5 });
    }

    #[test]
    fn span_text_with_unicode() {
        let mut map = SourceMap::new();
        let src = "caf\u{00E9} latte";
        let id = map.add("uni.phx", src);
        // Extract "caf\u{00E9}" which is bytes 0..5
        let span = Span::new(id, 0, 5);
        assert_eq!(map.span_text(span), "caf\u{00E9}");
    }

    #[test]
    fn line_col_multiple_lines_with_unicode() {
        let mut map = SourceMap::new();
        // line 1: "\u{00FC}ber" (4 bytes: 2+1+1+1 = 5... wait, \u{00FC} is 2 bytes)
        // Actually: \u{00FC} = 2 bytes, 'b' 'e' 'r' = 3 bytes => 5 bytes + '\n' = 6
        // line 2: "stra\u{00DF}e" => s(1) t(1) r(1) a(1) \u{00DF}(2) e(1) = 7 bytes
        let src = "\u{00FC}ber\nstra\u{00DF}e";
        let id = map.add("de.phx", src);
        // offset 0: line 1 col 1
        assert_eq!(map.line_col(id, 0), LineCol { line: 1, col: 1 });
        // '\n' is at offset 5
        assert_eq!(map.line_col(id, 5), LineCol { line: 1, col: 6 });
        // line 2 starts at offset 6
        assert_eq!(map.line_col(id, 6), LineCol { line: 2, col: 1 });
        // 's' 't' 'r' 'a' = 4 bytes, \u{00DF} starts at offset 10
        assert_eq!(map.line_col(id, 10), LineCol { line: 2, col: 5 });
    }
}
