use super::*;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

fn test_uri() -> Url {
    Url::parse("file:///tmp/test.phx").expect("test uri parses")
}

/// Test-only convenience: resolve `(kind, name)` against
/// `current_module`'s scope and look up its definition span. Returns
/// `None` when the name isn't visible in `current_module` or has no
/// recorded def (e.g. `SymbolKind::Variable`). Production callers
/// compose `resolve_symbol_ref` + `find_definition_span_for` directly.
fn find_definition_span(
    kind: &SymbolKind,
    name: &str,
    cr: &ResolvedModule,
    current_module: &ModulePath,
) -> Option<Span> {
    let sym = SymbolRef {
        kind: kind.clone(),
        name: name.to_string(),
    };
    let resolved = resolve_symbol_ref(&sym, cr, current_module)?;
    find_definition_span_for(&resolved, cr)
}

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
fn position_to_offset_crlf() {
    let source = "line one\r\nline two";
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

#[test]
fn format_type_dyn_trait() {
    assert_eq!(
        format_type(&Type::Dyn("Drawable".to_string())),
        "dyn Drawable"
    );
}

#[test]
fn format_type_dyn_inside_generic() {
    let ty = Type::Generic("List".to_string(), vec![Type::Dyn("Drawable".to_string())]);
    assert_eq!(format_type(&ty), "List<dyn Drawable>");
}

#[test]
fn format_type_dyn_in_function_param() {
    let ty = Type::Function(
        vec![Type::Dyn("Drawable".to_string())],
        Box::new(Type::String),
    );
    assert_eq!(format_type(&ty), "(dyn Drawable) -> String");
}

#[test]
fn format_type_dyn_in_function_return() {
    let ty = Type::Function(
        vec![Type::Bool],
        Box::new(Type::Dyn("Drawable".to_string())),
    );
    assert_eq!(format_type(&ty), "(Bool) -> dyn Drawable");
}

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

#[test]
fn find_definition_span_function() {
    let tokens = tokenize(
        "function add(a: Int, b: Int) -> Int { a + b }\nfunction main() { }",
        SourceId(0),
    );
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let span = find_definition_span(
        &SymbolKind::Function,
        "add",
        &result.module,
        &ModulePath::entry(),
    );
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
    let span = find_definition_span(
        &SymbolKind::Struct,
        "User",
        &result.module,
        &ModulePath::entry(),
    );
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
    let span = find_definition_span(
        &SymbolKind::Function,
        "nonexistent",
        &result.module,
        &ModulePath::entry(),
    );
    assert!(span.is_none());
}

/// `SymbolKind::Variable` deliberately returns `None` — variable
/// definitions live in `VarInfo`, not in `ResolvedModule`. Pinned as
/// a regression marker so once the LSP gap-closing in
/// `docs/phases/phase-3.md` lifts variables into the resolved
/// schema this test fails and forces an update.
#[test]
fn find_definition_span_variable_returns_none() {
    let tokens = tokenize(
        "function main() { let x: Int = 1\nlet _y = x }",
        SourceId(0),
    );
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let span = find_definition_span(
        &SymbolKind::Variable,
        "x",
        &result.module,
        &ModulePath::entry(),
    );
    assert!(
        span.is_none(),
        "find_definition_span on a Variable kind must return None until \
         VarInfo is lifted into ResolvedModule; got {:?}",
        span
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
        &result.module,
        &ModulePath::entry(),
    );
    assert!(
        span.is_some(),
        "should find definition span for field Point.x"
    );
    let s = span.unwrap();
    assert!(s.start < s.end);
}

#[test]
fn find_definition_span_struct_with_dyn_field() {
    let src = "trait Drawable { function draw(self) -> String }\n\
               struct Scene { dyn Drawable hero }\n\
               function main() { }";
    let tokens = tokenize(src, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let span = find_definition_span(
        &SymbolKind::Struct,
        "Scene",
        &result.module,
        &ModulePath::entry(),
    );
    assert!(span.is_some(), "struct Scene with dyn field should resolve");
    let s = span.unwrap();
    assert!(s.start < s.end);
}

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
        &result.module,
        &ModulePath::entry(),
    );
    assert!(span.is_some(), "dyn-typed field `hero` should resolve");
    let s = span.unwrap();
    assert!(s.start < s.end);
}

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
        &result.module,
        &ModulePath::entry(),
    );
    assert!(
        span.is_some(),
        "should find definition span for method Point.display"
    );
    let s = span.unwrap();
    assert!(s.start < s.end);
}

/// Pins the documented limitation in [`find_definition_span_for`]:
/// an `EnumVariant` lookup returns the *enum's* `definition_span`,
/// not the variant's own — variant-precise spans aren't tracked on
/// `EnumInfo`. Asserting equality (rather than just `is_some`) so a
/// future change that starts returning the variant's span breaks
/// this test deliberately and forces the doc + LSP behaviour to
/// move in lock-step.
#[test]
fn find_definition_span_enum_variant_returns_enum_span() {
    let src = "enum Shape {\n  Circle(Float)\n  Rect(Float, Float)\n}\nfunction main() { }";
    let tokens = tokenize(src, SourceId(0));
    let (program, _) = parser::parse(&tokens);
    let result = checker::check(&program);
    let span = find_definition_span(
        &SymbolKind::EnumVariant {
            enum_name: "Shape".to_string(),
        },
        "Circle",
        &result.module,
        &ModulePath::entry(),
    )
    .expect("enum variant should resolve to the enum's definition span");
    let enum_span = result
        .module
        .enum_info_by_name("Shape")
        .expect("Shape enum is recorded")
        .definition_span;
    assert_eq!(
        span, enum_span,
        "EnumVariant lookup must return the *enum's* definition span \
         until variant-precise spans are tracked on EnumInfo; got {:?}, \
         expected {:?}",
        span, enum_span
    );
}

#[test]
fn to_lsp_diagnostic_error() {
    let src = "hello\nworld";
    let mut source_map = SourceMap::new();
    let source_id = source_map.add("test.phx", src);
    let span = Span::new(source_id, 6, 11);
    let diag = phoenix_common::diagnostics::Diagnostic::error("undefined variable", span);
    let lsp_diag = to_lsp_diagnostic(&diag, &source_map, &HashMap::new(), &test_uri());
    assert_eq!(lsp_diag.message, "undefined variable");
    assert_eq!(lsp_diag.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(lsp_diag.source, Some("phoenix".to_string()));
    assert!(lsp_diag.related_information.is_none());
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
    let span = Span::new(source_id, 4, 5);
    let diag = phoenix_common::diagnostics::Diagnostic::warning("unused variable", span);
    let lsp_diag = to_lsp_diagnostic(&diag, &source_map, &HashMap::new(), &test_uri());
    assert_eq!(lsp_diag.message, "unused variable");
    assert_eq!(lsp_diag.severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(lsp_diag.source, Some("phoenix".to_string()));
    assert_eq!(lsp_diag.range.start.line, 0);
    assert_eq!(lsp_diag.range.start.character, 4);
    assert_eq!(lsp_diag.range.end.line, 0);
    assert_eq!(lsp_diag.range.end.character, 5);
}

#[test]
fn to_lsp_diagnostic_appends_hint_and_suggestion_to_message() {
    let src = "let x = 1";
    let mut source_map = SourceMap::new();
    let source_id = source_map.add("test.phx", src);
    let span = Span::new(source_id, 4, 5);
    let diag = phoenix_common::diagnostics::Diagnostic::error("type mismatch", span)
        .with_hint("expected Int, found Bool")
        .with_suggestion("change to `let x: Bool = true`");
    let lsp_diag = to_lsp_diagnostic(&diag, &source_map, &HashMap::new(), &test_uri());
    assert_eq!(
        lsp_diag.message,
        "type mismatch\n\
         hint: expected Int, found Bool\n\
         suggestion: change to `let x: Bool = true`"
    );
}

#[test]
fn to_lsp_diagnostic_forwards_notes_as_related_information() {
    let src = "let foo = 1\nlet foo = 2";
    let mut source_map = SourceMap::new();
    let source_id = source_map.add("test.phx", src);
    let primary = Span::new(source_id, 16, 19);
    let note_span = Span::new(source_id, 4, 7);
    let diag = phoenix_common::diagnostics::Diagnostic::error("`foo` redefined", primary)
        .with_note(note_span, "first defined here");
    let uri = test_uri();
    let mut id_map: HashMap<SourceId, Url> = HashMap::new();
    id_map.insert(source_id, uri.clone());
    let lsp_diag = to_lsp_diagnostic(&diag, &source_map, &id_map, &uri);
    let related = lsp_diag
        .related_information
        .expect("notes should produce related_information");
    assert_eq!(related.len(), 1);
    assert_eq!(related[0].message, "first defined here");
    assert_eq!(related[0].location.uri, uri);
    assert_eq!(related[0].location.range.start.line, 0);
    assert_eq!(related[0].location.range.start.character, 4);
    assert_eq!(related[0].location.range.end.line, 0);
    assert_eq!(related[0].location.range.end.character, 7);
}

#[test]
fn to_lsp_diagnostic_cross_file_note_uses_mapped_uri() {
    let mut source_map = SourceMap::new();
    let primary_id = source_map.add("primary.phx", "use other.foo");
    let other_id = source_map.add("other.phx", "// header\nlet foo = 1\n");
    let primary = Span::new(primary_id, 4, 9);
    let note_span = Span::new(other_id, 14, 17);
    let diag = phoenix_common::diagnostics::Diagnostic::error("symbol is private", primary)
        .with_note(note_span, "defined here");
    let primary_uri = test_uri();
    let other_uri = Url::parse("file:///tmp/other.phx").expect("other uri parses");
    let mut id_map: HashMap<SourceId, Url> = HashMap::new();
    id_map.insert(primary_id, primary_uri.clone());
    id_map.insert(other_id, other_uri.clone());
    let lsp_diag = to_lsp_diagnostic(&diag, &source_map, &id_map, &primary_uri);
    let related = lsp_diag
        .related_information
        .expect("notes should produce related_information");
    assert_eq!(related.len(), 1);
    assert_eq!(related[0].location.uri, other_uri);
    assert_eq!(related[0].location.range.start.line, 1);
    assert_eq!(related[0].location.range.start.character, 4);
    assert_eq!(related[0].location.range.end.line, 1);
    assert_eq!(related[0].location.range.end.character, 7);
}

#[test]
fn to_lsp_diagnostic_falls_back_when_source_id_unmapped() {
    let mut source_map = SourceMap::new();
    let primary_id = source_map.add("primary.phx", "use other.foo");
    let other_id = source_map.add("other.phx", "let foo = 1\n");
    let primary = Span::new(primary_id, 4, 9);
    let note_span = Span::new(other_id, 4, 7);
    let diag = phoenix_common::diagnostics::Diagnostic::error("oops", primary)
        .with_note(note_span, "see here");
    let fallback = test_uri();
    let mut id_map: HashMap<SourceId, Url> = HashMap::new();
    id_map.insert(primary_id, fallback.clone());
    let lsp_diag = to_lsp_diagnostic(&diag, &source_map, &id_map, &fallback);
    let related = lsp_diag.related_information.unwrap();
    assert_eq!(related[0].location.uri, fallback);
}

#[test]
fn span_to_range_first_line() {
    let src = "function main() { }";
    let mut source_map = SourceMap::new();
    let source_id = source_map.add("test.phx", src);
    let span = Span::new(source_id, 9, 13);
    let range = span_to_range(&span, &source_map);
    assert_eq!(range.start.line, 0);
    assert_eq!(range.start.character, 9);
    assert_eq!(range.end.line, 0);
    assert_eq!(range.end.character, 13);
}

#[test]
fn span_to_range_later_line() {
    let src = "line one\nline two\nline three";
    let mut source_map = SourceMap::new();
    let source_id = source_map.add("test.phx", src);
    let span = Span::new(source_id, 14, 17);
    let range = span_to_range(&span, &source_map);
    assert_eq!(range.start.line, 1);
    assert_eq!(range.start.character, 5);
    assert_eq!(range.end.line, 1);
    assert_eq!(range.end.character, 8);
}
