//! Phoenix Language Server.
//!
//! Provides IDE features for `.phx` files via the Language Server Protocol:
//! diagnostics, hover, autocomplete, go-to-definition, find references, and rename.
#![warn(missing_docs)]

use std::collections::HashMap;
use std::sync::Mutex;

use phoenix_common::source::SourceMap;
use phoenix_common::span::{SourceId, Span};
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker::{self, CheckResult, SymbolKind};
use phoenix_sema::types::Type;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// Per-document state: the parsed program, check result, and source map.
struct DocumentState {
    /// The raw source text of the document.
    source: String,
    /// Source map for byte-offset ↔ line/column conversion.
    source_map: SourceMap,
    /// The source file identifier within the source map.
    source_id: SourceId,
    /// Semantic analysis results (types, diagnostics, symbol references).
    check_result: CheckResult,
}

/// The Phoenix language server backend.
struct Backend {
    /// The LSP client handle for sending notifications (e.g., diagnostics).
    client: Client,
    /// Per-document analysis state, keyed by document URI.
    documents: Mutex<HashMap<Url, DocumentState>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
        }
    }

    /// Re-parses and re-checks a document, publishing diagnostics.
    async fn analyze(&self, uri: Url, text: String) {
        let path = uri
            .to_file_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| uri.to_string());

        let mut source_map = SourceMap::new();
        let source_id = source_map.add(&path, &text);
        let tokens = tokenize(&text, source_id);
        let (program, parse_errors) = parser::parse(&tokens);

        // Convert parse errors to LSP diagnostics
        let mut diagnostics: Vec<Diagnostic> = parse_errors
            .iter()
            .map(|d| to_lsp_diagnostic(d, &source_map, source_id))
            .collect();

        // Run type checker
        let check_result = checker::check(&program);
        diagnostics.extend(
            check_result
                .diagnostics
                .iter()
                .map(|d| to_lsp_diagnostic(d, &source_map, source_id)),
        );

        // Publish diagnostics
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;

        // Store state for other handlers
        let state = DocumentState {
            source: text,
            source_map,
            source_id,
            check_result,
        };
        self.documents
            .lock()
            .expect("document lock poisoned")
            .insert(uri, state);
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    /// Declares server capabilities: full text sync, hover, completion,
    /// go-to-definition, find-references, and rename.
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Logs a startup message after the client confirms initialization.
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Phoenix LSP initialized")
            .await;
    }

    /// Graceful shutdown — no cleanup needed.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Analyzes a newly opened document and publishes diagnostics.
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.analyze(params.text_document.uri, params.text_document.text)
            .await;
    }

    /// Re-analyzes a document after edits and publishes updated diagnostics.
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.analyze(params.text_document.uri, change.text).await;
        }
    }

    /// Removes document state and clears diagnostics when a file is closed.
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .lock()
            .expect("document lock poisoned")
            .remove(&params.text_document.uri);
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    /// Returns the resolved type at the cursor position as a Markdown hover.
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };

        let offset = position_to_offset(&state.source, pos);

        // Check if cursor is on an expression with a known type
        for (span, ty) in &state.check_result.expr_types {
            if span.source_id == state.source_id && span.start <= offset && offset < span.end {
                let type_str = format_type(ty);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("```phoenix\n{}\n```", type_str),
                    }),
                    range: Some(span_to_range(span, &state.source_map, state.source_id)),
                }));
            }
        }

        Ok(None)
    }

    /// Returns completion items: struct/enum/function names and keywords.
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };

        let mut items = Vec::new();

        // Suggest struct names
        for name in state.check_result.structs.keys() {
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::STRUCT),
                ..Default::default()
            });
        }

        // Suggest enum names
        for name in state.check_result.enums.keys() {
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::ENUM),
                ..Default::default()
            });
        }

        // Suggest function names
        for (name, info) in &state.check_result.functions {
            let params: Vec<String> = info
                .param_names
                .iter()
                .zip(info.params.iter())
                .map(|(n, t)| format!("{}: {}", n, format_type(t)))
                .collect();
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(format!(
                    "({}) -> {}",
                    params.join(", "),
                    format_type(&info.return_type)
                )),
                ..Default::default()
            });
        }

        // Suggest keywords
        for kw in &[
            "function", "struct", "enum", "endpoint", "trait", "impl", "type", "let", "mut", "if",
            "else", "while", "for", "in", "match", "return", "break", "continue", "body",
            "response", "error", "query", "where", "schema", "true", "false", "self",
        ] {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    /// Jumps to the definition of the symbol under the cursor.
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };

        let offset = position_to_offset(&state.source, pos);

        // Find symbol reference at cursor
        for (span, sym_ref) in &state.check_result.symbol_references {
            if span.source_id == state.source_id
                && span.start <= offset
                && offset < span.end
                && let Some(def_span) =
                    find_definition_span(&sym_ref.kind, &sym_ref.name, &state.check_result)
            {
                let range = span_to_range(&def_span, &state.source_map, state.source_id);
                return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri: uri.clone(),
                    range,
                })));
            }
        }

        Ok(None)
    }

    /// Returns all locations where the symbol under the cursor is referenced.
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };

        let offset = position_to_offset(&state.source, pos);

        // Find which symbol the cursor is on
        let target_ref = state
            .check_result
            .symbol_references
            .iter()
            .find(|(span, _)| {
                span.source_id == state.source_id && span.start <= offset && offset < span.end
            })
            .map(|(_, r)| r);

        let Some(target) = target_ref else {
            return Ok(None);
        };

        // Find all references to the same symbol
        let locations: Vec<Location> = state
            .check_result
            .symbol_references
            .iter()
            .filter(|(_, r)| r.name == target.name && r.kind == target.kind)
            .map(|(span, _)| Location {
                uri: uri.clone(),
                range: span_to_range(span, &state.source_map, state.source_id),
            })
            .collect();

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    /// Renames a symbol across all its references in the document.
    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;
        let docs = self.documents.lock().expect("document lock poisoned");
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };

        let offset = position_to_offset(&state.source, pos);

        // Find which symbol the cursor is on
        let target_ref = state
            .check_result
            .symbol_references
            .iter()
            .find(|(span, _)| {
                span.source_id == state.source_id && span.start <= offset && offset < span.end
            })
            .map(|(_, r)| r);

        let Some(target) = target_ref else {
            return Ok(None);
        };

        // Collect all edits
        let edits: Vec<TextEdit> = state
            .check_result
            .symbol_references
            .iter()
            .filter(|(_, r)| r.name == target.name && r.kind == target.kind)
            .map(|(span, _)| TextEdit {
                range: span_to_range(span, &state.source_map, state.source_id),
                new_text: new_name.clone(),
            })
            .collect();

        if edits.is_empty() {
            return Ok(None);
        }

        let mut changes = HashMap::new();
        changes.insert(uri.clone(), edits);
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }
}

// ── Helper functions ─────────────────────────────────────────────────

/// Converts a Phoenix `Diagnostic` to an LSP `Diagnostic`.
fn to_lsp_diagnostic(
    diag: &phoenix_common::diagnostics::Diagnostic,
    source_map: &SourceMap,
    source_id: SourceId,
) -> Diagnostic {
    let range = span_to_range(&diag.span, source_map, source_id);
    let severity = match diag.severity {
        phoenix_common::diagnostics::Severity::Error => DiagnosticSeverity::ERROR,
        phoenix_common::diagnostics::Severity::Warning => DiagnosticSeverity::WARNING,
    };
    Diagnostic {
        range,
        severity: Some(severity),
        source: Some("phoenix".to_string()),
        message: diag.message.clone(),
        ..Default::default()
    }
}

/// Converts a Phoenix `Span` to an LSP `Range`.
fn span_to_range(span: &Span, source_map: &SourceMap, source_id: SourceId) -> Range {
    let start = source_map.line_col(source_id, span.start);
    let end = source_map.line_col(source_id, span.end);
    Range {
        start: Position {
            line: start.line.saturating_sub(1) as u32,
            character: start.col.saturating_sub(1) as u32,
        },
        end: Position {
            line: end.line.saturating_sub(1) as u32,
            character: end.col.saturating_sub(1) as u32,
        },
    }
}

/// Converts an LSP `Position` (0-based line/col) to a byte offset in the source.
///
/// Handles both `\n` and `\r\n` line endings by scanning byte-by-byte rather
/// than relying on `str::lines()` (which strips `\r`).
fn position_to_offset(source: &str, pos: Position) -> usize {
    let target_line = pos.line as usize;
    let target_col = pos.character as usize;
    let mut line = 0;
    let mut col = 0;
    for (i, b) in source.bytes().enumerate() {
        if line == target_line && col == target_col {
            return i;
        }
        if b == b'\n' {
            if line == target_line {
                // Cursor is past end of this line; clamp to end
                return i;
            }
            line += 1;
            col = 0;
        } else if b == b'\r' {
            // Skip \r — the following \n (if any) will advance the line
        } else {
            col += 1;
        }
    }
    // If we get here, cursor is at or past the end of the source
    source.len()
}

/// Formats a Phoenix `Type` as a readable string for hover display.
fn format_type(ty: &Type) -> String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::String => "String".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::Void => "Void".to_string(),
        Type::Named(name) => name.clone(),
        Type::Generic(name, args) => {
            let args_str: Vec<String> = args.iter().map(format_type).collect();
            format!("{}<{}>", name, args_str.join(", "))
        }
        Type::Function(params, ret) => {
            let params_str: Vec<String> = params.iter().map(format_type).collect();
            format!("({}) -> {}", params_str.join(", "), format_type(ret))
        }
        Type::TypeVar(name) => name.clone(),
        Type::Dyn(name) => format!("dyn {}", name),
        Type::Error => "?".to_string(),
    }
}

/// Finds the definition span for a symbol given its kind and name.
fn find_definition_span(kind: &SymbolKind, name: &str, cr: &CheckResult) -> Option<Span> {
    match kind {
        SymbolKind::Function => cr.functions.get(name).map(|f| f.definition_span),
        SymbolKind::Struct => cr.structs.get(name).map(|s| s.definition_span),
        SymbolKind::Enum => cr.enums.get(name).map(|e| e.definition_span),
        SymbolKind::Variable => None, // Variable definitions tracked via VarInfo, not in CheckResult
        SymbolKind::Field { struct_name } => cr.structs.get(struct_name).and_then(|s| {
            s.fields
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.definition_span)
        }),
        SymbolKind::Method { type_name } => cr
            .methods
            .get(type_name)
            .and_then(|ms| ms.get(name))
            .map(|m| m.definition_span),
        SymbolKind::EnumVariant { enum_name } => cr.enums.get(enum_name).map(|e| e.definition_span),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_to_offset_first_line() {
        let source = "hello world";
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            0
        );
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 0,
                    character: 5
                }
            ),
            5
        );
    }

    #[test]
    fn position_to_offset_second_line() {
        let source = "line one\nline two";
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 1,
                    character: 0
                }
            ),
            9
        );
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 1,
                    character: 4
                }
            ),
            13
        );
    }

    #[test]
    fn position_to_offset_past_end() {
        let source = "short";
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 5,
                    character: 0
                }
            ),
            5
        );
    }

    #[test]
    fn format_type_primitives() {
        assert_eq!(format_type(&Type::Int), "Int");
        assert_eq!(format_type(&Type::Float), "Float");
        assert_eq!(format_type(&Type::String), "String");
        assert_eq!(format_type(&Type::Bool), "Bool");
        assert_eq!(format_type(&Type::Void), "Void");
    }

    #[test]
    fn format_type_generic() {
        let ty = Type::Generic("List".to_string(), vec![Type::Int]);
        assert_eq!(format_type(&ty), "List<Int>");
    }

    #[test]
    fn format_type_named() {
        assert_eq!(format_type(&Type::Named("User".to_string())), "User");
    }

    #[test]
    fn find_definition_span_function() {
        let tokens = tokenize(
            "function add(a: Int, b: Int) -> Int { a + b }\nfunction main() { }",
            SourceId(0),
        );
        let (program, _) = parser::parse(&tokens);
        let result = checker::check(&program);
        let span = find_definition_span(&SymbolKind::Function, "add", &result);
        assert!(
            span.is_some(),
            "should find definition span for function add"
        );
        let s = span.unwrap();
        assert!(s.start < s.end);
    }

    #[test]
    fn find_definition_span_struct() {
        let tokens = tokenize("struct User { Int id }\nfunction main() { }", SourceId(0));
        let (program, _) = parser::parse(&tokens);
        let result = checker::check(&program);
        let span = find_definition_span(&SymbolKind::Struct, "User", &result);
        assert!(
            span.is_some(),
            "should find definition span for struct User"
        );
    }

    #[test]
    fn find_definition_span_nonexistent() {
        let tokens = tokenize("function main() { }", SourceId(0));
        let (program, _) = parser::parse(&tokens);
        let result = checker::check(&program);
        let span = find_definition_span(&SymbolKind::Function, "nonexistent", &result);
        assert!(span.is_none());
    }

    #[test]
    fn position_to_offset_crlf() {
        // Windows-style line endings: \r\n
        let source = "line one\r\nline two";
        // line 0, char 0 → byte 0
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            0
        );
        // line 0, char 5 → byte 5
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 0,
                    character: 5
                }
            ),
            5
        );
        // line 1, char 0 → byte 10 (after "line one\r\n")
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 1,
                    character: 0
                }
            ),
            10
        );
        // line 1, char 4 → byte 14
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 1,
                    character: 4
                }
            ),
            14
        );
    }

    #[test]
    fn find_definition_span_field() {
        let tokens = tokenize(
            "struct Point { Int x  Int y }\nfunction main() { let p: Point = Point(1, 2)\nprint(p.x) }",
            SourceId(0),
        );
        let (program, _) = parser::parse(&tokens);
        let result = checker::check(&program);
        let span = find_definition_span(
            &SymbolKind::Field {
                struct_name: "Point".to_string(),
            },
            "x",
            &result,
        );
        assert!(
            span.is_some(),
            "should find definition span for field Point.x"
        );
        // The span should cover just the field declaration "Int x" region
        let s = span.unwrap();
        assert!(s.start < s.end);
    }

    // ── format_type edge cases ──────────────────────────────────────

    #[test]
    fn format_type_function() {
        let ty = Type::Function(vec![Type::Int, Type::String], Box::new(Type::Bool));
        assert_eq!(format_type(&ty), "(Int, String) -> Bool");
    }

    #[test]
    fn format_type_nested_generic() {
        let ty = Type::Generic(
            "Result".to_string(),
            vec![
                Type::Generic("List".to_string(), vec![Type::Int]),
                Type::String,
            ],
        );
        assert_eq!(format_type(&ty), "Result<List<Int>, String>");
    }

    #[test]
    fn format_type_typevar() {
        assert_eq!(format_type(&Type::TypeVar("T".to_string())), "T");
    }

    #[test]
    fn format_type_error() {
        assert_eq!(format_type(&Type::Error), "?");
    }

    /// Hover over a `dyn Trait` value must render as `dyn Trait` — not as
    /// the erased trait name or an `unknown` fallback. Pins the user-visible
    /// hover text to the source syntax so editor tooltips mirror what the
    /// programmer wrote.
    #[test]
    fn format_type_dyn_trait() {
        assert_eq!(
            format_type(&Type::Dyn("Drawable".to_string())),
            "dyn Drawable"
        );
    }

    /// `dyn Trait` nested inside a generic (e.g. `List<dyn Drawable>`)
    /// should render naturally too — not collapse the `dyn` into the
    /// generic's arg list or lose the keyword.
    #[test]
    fn format_type_dyn_inside_generic() {
        let ty = Type::Generic("List".to_string(), vec![Type::Dyn("Drawable".to_string())]);
        assert_eq!(format_type(&ty), "List<dyn Drawable>");
    }

    /// Hover on a function value whose parameter is `dyn Trait` — the
    /// signature-rendering path must preserve the `dyn` keyword so
    /// editor tooltips over higher-order function types are accurate.
    #[test]
    fn format_type_dyn_in_function_param() {
        let ty = Type::Function(
            vec![Type::Dyn("Drawable".to_string())],
            Box::new(Type::String),
        );
        assert_eq!(format_type(&ty), "(dyn Drawable) -> String");
    }

    /// Hover on a function value returning `dyn Trait` — same contract
    /// at return position.
    #[test]
    fn format_type_dyn_in_function_return() {
        let ty = Type::Function(
            vec![Type::Bool],
            Box::new(Type::Dyn("Drawable".to_string())),
        );
        assert_eq!(format_type(&ty), "(Bool) -> dyn Drawable");
    }

    /// Deeply nested dyn: `Option<List<dyn Drawable>>`. Exercises two
    /// levels of generic recursion around a `dyn` leaf.
    #[test]
    fn format_type_dyn_two_levels_nested() {
        let ty = Type::Generic(
            "Option".to_string(),
            vec![Type::Generic(
                "List".to_string(),
                vec![Type::Dyn("Drawable".to_string())],
            )],
        );
        assert_eq!(format_type(&ty), "Option<List<dyn Drawable>>");
    }

    /// `find_definition_span` on a struct that declares a `dyn Trait`
    /// field still locates the struct's own definition span. Goto-def
    /// on the struct name from a `dyn Trait`-carrying type should
    /// behave identically to a plain struct. (Separate feature gap:
    /// goto-def *on the trait name inside `dyn Trait`* requires a
    /// `SymbolKind::Trait` variant, not yet modelled.)
    #[test]
    fn find_definition_span_struct_with_dyn_field() {
        let src = "trait Drawable { function draw(self) -> String }\n\
                   struct Scene { dyn Drawable hero }\n\
                   function main() { }";
        let tokens = tokenize(src, SourceId(0));
        let (program, _) = parser::parse(&tokens);
        let result = checker::check(&program);
        let span = find_definition_span(&SymbolKind::Struct, "Scene", &result);
        assert!(span.is_some(), "struct Scene with dyn field should resolve");
        let s = span.unwrap();
        assert!(s.start < s.end);
    }

    /// A struct field typed `dyn Trait` resolves to the correct field
    /// span — exercises `SymbolKind::Field` on a `dyn`-carrying
    /// layout, pinning that the dyn work didn't break the per-field
    /// resolver.
    #[test]
    fn find_definition_span_dyn_field() {
        let src = "trait Drawable { function draw(self) -> String }\n\
                   struct Scene { dyn Drawable hero }\n\
                   function main() { }";
        let tokens = tokenize(src, SourceId(0));
        let (program, _) = parser::parse(&tokens);
        let result = checker::check(&program);
        let span = find_definition_span(
            &SymbolKind::Field {
                struct_name: "Scene".to_string(),
            },
            "hero",
            &result,
        );
        assert!(span.is_some(), "dyn-typed field `hero` should resolve");
        let s = span.unwrap();
        assert!(s.start < s.end);
    }

    // ── find_definition_span: method and enum variant ───────────────

    #[test]
    fn find_definition_span_method() {
        let src = "struct Point { Int x  Int y }\nimpl Point {\n  function display(self) -> String {\n    return \"hello\"\n  }\n}\nfunction main() { }";
        let tokens = tokenize(src, SourceId(0));
        let (program, _) = parser::parse(&tokens);
        let result = checker::check(&program);
        let span = find_definition_span(
            &SymbolKind::Method {
                type_name: "Point".to_string(),
            },
            "display",
            &result,
        );
        assert!(
            span.is_some(),
            "should find definition span for method Point.display"
        );
        let s = span.unwrap();
        assert!(s.start < s.end);
    }

    #[test]
    fn find_definition_span_enum_variant() {
        let src = "enum Shape {\n  Circle(Float)\n  Rect(Float, Float)\n}\nfunction main() { }";
        let tokens = tokenize(src, SourceId(0));
        let (program, _) = parser::parse(&tokens);
        let result = checker::check(&program);
        // EnumVariant lookup returns the enum's definition span
        let span = find_definition_span(
            &SymbolKind::EnumVariant {
                enum_name: "Shape".to_string(),
            },
            "Circle",
            &result,
        );
        assert!(
            span.is_some(),
            "should find definition span for enum variant Shape::Circle"
        );
        let s = span.unwrap();
        assert!(s.start < s.end);
    }

    // ── to_lsp_diagnostic conversion ────────────────────────────────

    #[test]
    fn to_lsp_diagnostic_error() {
        let src = "hello\nworld";
        let mut source_map = SourceMap::new();
        let source_id = source_map.add("test.phx", src);
        // Span covering "world" (bytes 6..11, line 2 col 1..6 in 1-based)
        let span = Span::new(source_id, 6, 11);
        let diag = phoenix_common::diagnostics::Diagnostic::error("undefined variable", span);
        let lsp_diag = to_lsp_diagnostic(&diag, &source_map, source_id);
        assert_eq!(lsp_diag.message, "undefined variable");
        assert_eq!(lsp_diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp_diag.source, Some("phoenix".to_string()));
        // "world" is on line 1 (0-based), columns 0..5 (0-based)
        assert_eq!(lsp_diag.range.start.line, 1);
        assert_eq!(lsp_diag.range.start.character, 0);
        assert_eq!(lsp_diag.range.end.line, 1);
        assert_eq!(lsp_diag.range.end.character, 5);
    }

    #[test]
    fn to_lsp_diagnostic_warning() {
        let src = "let x: Int = 42";
        let mut source_map = SourceMap::new();
        let source_id = source_map.add("test.phx", src);
        // Span covering "x" (bytes 4..5)
        let span = Span::new(source_id, 4, 5);
        let diag = phoenix_common::diagnostics::Diagnostic::warning("unused variable", span);
        let lsp_diag = to_lsp_diagnostic(&diag, &source_map, source_id);
        assert_eq!(lsp_diag.message, "unused variable");
        assert_eq!(lsp_diag.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(lsp_diag.source, Some("phoenix".to_string()));
        // "x" is on line 0 (0-based), char 4 start; end offset 5 → 0-based char 5
        assert_eq!(lsp_diag.range.start.line, 0);
        assert_eq!(lsp_diag.range.start.character, 4);
        assert_eq!(lsp_diag.range.end.line, 0);
        assert_eq!(lsp_diag.range.end.character, 5);
    }

    // ── span_to_range conversion ────────────────────────────────────

    #[test]
    fn span_to_range_first_line() {
        let src = "function main() { }";
        let mut source_map = SourceMap::new();
        let source_id = source_map.add("test.phx", src);
        // Span covering "main" (bytes 9..13)
        let span = Span::new(source_id, 9, 13);
        let range = span_to_range(&span, &source_map, source_id);
        // 1-based line=1, col=10 → 0-based line=0, char=9
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 9);
        // 1-based line=1, col=14 → 0-based line=0, char=13
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 13);
    }

    #[test]
    fn span_to_range_later_line() {
        let src = "line one\nline two\nline three";
        let mut source_map = SourceMap::new();
        let source_id = source_map.add("test.phx", src);
        // Span covering "two" on the second line (bytes 14..17)
        let span = Span::new(source_id, 14, 17);
        let range = span_to_range(&span, &source_map, source_id);
        // "two" starts at line 2 (1-based), col 6 (1-based) → 0-based line=1, char=5
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 5);
        // ends at line 2 (1-based), col 9 (1-based) → 0-based line=1, char=8
        assert_eq!(range.end.line, 1);
        assert_eq!(range.end.character, 8);
    }

    // ── position_to_offset edge cases ───────────────────────────────

    #[test]
    fn position_to_offset_empty_string() {
        let source = "";
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            0
        );
    }

    #[test]
    fn position_to_offset_column_past_end_of_line() {
        let source = "hi\nbye";
        // Column 99 on line 0 should clamp to end of line (byte 2, the '\n')
        assert_eq!(
            position_to_offset(
                source,
                Position {
                    line: 0,
                    character: 99
                }
            ),
            2
        );
    }
}
