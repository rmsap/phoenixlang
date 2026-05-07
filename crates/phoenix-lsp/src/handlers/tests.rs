use super::*;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use phoenix_common::module_path::ModulePath;
use phoenix_common::source::SourceMap;
use phoenix_lexer::lexer::tokenize;
use phoenix_modules::resolve_with_overlay;
use phoenix_parser::parser;
use phoenix_sema::checker;

use crate::convert::{find_definition_span_for, resolve_symbol_ref};
use crate::project::{build_source_id_to_module, build_source_id_to_url};

fn test_uri() -> Url {
    Url::parse("file:///tmp/test.phx").expect("test uri parses")
}

/// Runs the project pipeline (resolver + check_modules) and builds
/// a `DocumentState` for the file at `target_path`.
fn build_document_state_for(
    entry_path: &Path,
    target_path: &Path,
    target_text: String,
) -> (DocumentState, Url) {
    let mut source_map = SourceMap::new();
    let modules = resolve_with_overlay(entry_path, &mut source_map, &HashMap::new())
        .expect("resolver should succeed in tests");
    let source_id_to_url = build_source_id_to_url(&modules);
    let source_id_to_module = build_source_id_to_module(&modules);

    let target_canon = target_path.canonicalize().unwrap();
    let target_module = modules
        .iter()
        .find(|m| m.file_path == target_canon)
        .expect("target file present in module list");
    let analysis = checker::check_modules(&modules);
    let target_uri = Url::from_file_path(&target_canon).unwrap();
    let state = DocumentState {
        source: target_text,
        source_map: Arc::new(source_map),
        source_id: target_module.source_id,
        current_module: target_module.module_path.clone(),
        check_result: Arc::new(analysis),
        source_id_to_url: Arc::new(source_id_to_url),
        source_id_to_module: Arc::new(source_id_to_module),
        canonical_path: Some(target_canon.clone()),
    };
    (state, target_uri)
}

#[test]
fn cross_file_goto_def_returns_other_files_url() {
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    std::fs::write(
        &lib_path,
        "public function add(a: Int, b: Int) -> Int { return a + b }\n",
    )
    .unwrap();
    let main_text = "import lib { add }\nfunction main() { add(1, 2) }\n";
    std::fs::write(&main_path, main_text).unwrap();

    let (state, _main_uri) =
        build_document_state_for(&main_path, &main_path, main_text.to_string());
    let lib_uri = Url::from_file_path(lib_path.canonicalize().unwrap()).unwrap();

    let call_offset = main_text.find("add(1").unwrap();
    let line_col = state.source_map.line_col(state.source_id, call_offset);
    let pos = Position {
        line: (line_col.line - 1) as u32,
        character: (line_col.col - 1) as u32,
    };

    let loc = goto_definition_at(&state, pos).expect("goto-def hits");
    assert_eq!(loc.uri, lib_uri, "goto-def URI should be lib.phx");
}

#[test]
fn cross_file_references_finds_uses_in_other_files() {
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    let lib_text = "public function add(a: Int, b: Int) -> Int { return a + b }\n";
    std::fs::write(&lib_path, lib_text).unwrap();
    let main_text = "import lib { add }\nfunction main() { add(1, 2) }\n";
    std::fs::write(&main_path, main_text).unwrap();

    let (state, lib_uri) = build_document_state_for(&main_path, &lib_path, lib_text.to_string());
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();

    let def_offset = lib_text.find("add(").unwrap();
    let lc = state.source_map.line_col(state.source_id, def_offset);
    let pos = Position {
        line: (lc.line - 1) as u32,
        character: (lc.col - 1) as u32,
    };

    let locs = references_at(&state, pos).expect("references hits");
    let uris: HashSet<Url> = locs.iter().map(|l| l.uri.clone()).collect();
    assert!(uris.contains(&lib_uri), "references should include lib.phx");
    assert!(
        uris.contains(&main_uri),
        "references should include main.phx (cross-file use site)"
    );
}

#[test]
fn cross_file_rename_emits_workspace_edit_for_both_files() {
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    let lib_text = "public function add(a: Int, b: Int) -> Int { return a + b }\n";
    std::fs::write(&lib_path, lib_text).unwrap();
    let main_text = "import lib { add }\nfunction main() { add(1, 2) }\n";
    std::fs::write(&main_path, main_text).unwrap();

    let (state, lib_uri) = build_document_state_for(&main_path, &lib_path, lib_text.to_string());
    let main_uri = Url::from_file_path(main_path.canonicalize().unwrap()).unwrap();

    let def_offset = lib_text.find("add(").unwrap();
    let lc = state.source_map.line_col(state.source_id, def_offset);
    let pos = Position {
        line: (lc.line - 1) as u32,
        character: (lc.col - 1) as u32,
    };

    let edit = rename_at(&state, pos, "sum").expect("rename hits");
    let changes = edit.changes.expect("rename produced changes");
    assert!(changes.contains_key(&lib_uri), "rename should edit lib.phx");
    assert!(
        changes.contains_key(&main_uri),
        "rename should edit main.phx (cross-file use site)"
    );
}

#[test]
fn completion_includes_module_keywords() {
    let state = keyword_only_state();
    let items = completion_items_for(&state);
    let labels: HashSet<String> = items.iter().map(|i| i.label.clone()).collect();
    assert!(labels.contains("import"));
    assert!(labels.contains("public"));
    assert!(labels.contains("as"));
}

#[test]
fn completion_no_scope_entry_for_non_entry_module_returns_only_keywords() {
    // Pins the leak fix: if `module_scopes` lacks an entry for a
    // non-entry module, the dump-everything fallback must NOT
    // kick in. Otherwise every globally-declared sibling name
    // shows up in completion regardless of imports.
    let mut source_map = SourceMap::new();
    let source_id = source_map.add("lib.phx", "function helper() {}");
    let analysis = checker::check(&parser::parse(&tokenize("function helper() {}", source_id)).0);
    let mut source_id_to_url = HashMap::new();
    let uri = test_uri();
    source_id_to_url.insert(source_id, uri.clone());
    let mut source_id_to_module = HashMap::new();
    source_id_to_module.insert(source_id, ModulePath(vec!["lib".into()]));
    // Non-entry module path with no `module_scopes` entry — the
    // gated fallback should refuse to dump globals.
    let state = DocumentState {
        source: "function helper() {}".to_string(),
        source_map: Arc::new(source_map),
        source_id,
        current_module: ModulePath(vec!["lib".into()]),
        check_result: Arc::new(analysis),
        source_id_to_url: Arc::new(source_id_to_url),
        source_id_to_module: Arc::new(source_id_to_module),
        canonical_path: None,
    };
    let items = completion_items_for(&state);
    let function_labels: HashSet<String> = items
        .iter()
        .filter(|i| i.kind == Some(CompletionItemKind::FUNCTION))
        .map(|i| i.label.clone())
        .collect();
    assert!(
        function_labels.is_empty(),
        "non-entry module without a scope entry must not leak globals; got {:?}",
        function_labels
    );
    // Keywords are always present.
    let labels: HashSet<String> = items.iter().map(|i| i.label.clone()).collect();
    assert!(labels.contains("function"));
}

/// Regression for the entry-vs-non-entry name-collision risk: the
/// entry module's qualified key for `add` is just `"add"` (see
/// `module_qualify`). If `find_definition_span_for` ever falls back to
/// a bare-name lookup when `resolve_visible` returns `None`, a
/// reference to an unresolved `add` from a sibling module would
/// silently resolve to the entry's `add`. This test pins the
/// "no resolution → no goto-def" behaviour.
#[test]
fn goto_def_does_not_leak_entry_function_to_unimported_sibling() {
    let td = tempfile::TempDir::new().unwrap();
    // Entry declares its own `add` (qualified key: "add").
    let main_path = td.path().join("main.phx");
    std::fs::write(
        &main_path,
        "function add(a: Int, b: Int) -> Int { return a + b }\n\
         function main() { add(1, 2) }\n",
    )
    .unwrap();
    // Sibling exists in the project tree but does NOT import `add`.
    // We force it to be reachable by having `main.phx` import it,
    // but the sibling itself never refers to `add`.
    let lib_path = td.path().join("lib.phx");
    std::fs::write(&lib_path, "public function helper() {}\n").unwrap();
    // Re-write main to import lib so lib is part of the module graph.
    std::fs::write(
        &main_path,
        "import lib { helper }\n\
         function add(a: Int, b: Int) -> Int { return a + b }\n\
         function main() { add(1, 2)  helper() }\n",
    )
    .unwrap();

    // Build a DocumentState targeting lib.phx.
    let lib_text = "public function helper() {}\n";
    let (state, _lib_uri) = build_document_state_for(&main_path, &lib_path, lib_text.to_string());

    // Synthesize a fake symbol-reference at lib.phx position 0:
    // sema would never emit one (the source has no `add`), so we
    // exercise the `resolve_symbol_ref` + `find_definition_span_for`
    // composition directly with a name that's *not* in lib's scope.
    let sym = phoenix_sema::checker::SymbolRef {
        kind: phoenix_sema::checker::SymbolKind::Function,
        name: "add".to_string(),
    };
    let resolved = resolve_symbol_ref(&sym, &state.check_result.module, &state.current_module);
    let span = resolved.and_then(|r| find_definition_span_for(&r, &state.check_result.module));
    assert!(
        span.is_none(),
        "an unimported name must not resolve to the entry module's \
         same-named def; got {:?}",
        span,
    );
}

/// Regression for the cross-module rename-corruption bug: two
/// modules each declare their own `helper` (no import relation
/// between them — `main.phx` imports `lib.phx` only as a sibling
/// for the resolver, with the import-list naming a *different*
/// symbol). Renaming the entry's `helper` must NOT touch
/// `lib::helper`, and references on the entry's `helper` must NOT
/// include `lib::helper`'s use sites.
#[test]
fn rename_does_not_conflate_same_named_function_in_other_module() {
    let td = tempfile::TempDir::new().unwrap();
    let lib_path = td.path().join("lib.phx");
    let lib_text = "public function exposed() {}\n\
                    public function helper() -> Int { return helper() }\n";
    std::fs::write(&lib_path, lib_text).unwrap();
    let main_path = td.path().join("main.phx");
    let main_text = "import lib { exposed }\n\
                     function helper() -> Int { return 1 }\n\
                     function main() { helper()  exposed() }\n";
    std::fs::write(&main_path, main_text).unwrap();

    let (state, main_uri) = build_document_state_for(&main_path, &main_path, main_text.to_string());
    let lib_uri = Url::from_file_path(lib_path.canonicalize().unwrap()).unwrap();

    // Cursor on the entry module's `helper` definition.
    let off = main_text.find("function helper").unwrap() + "function ".len();
    let lc = state.source_map.line_col(state.source_id, off);
    let pos = Position {
        line: (lc.line - 1) as u32,
        character: (lc.col - 1) as u32,
    };

    let edit = rename_at(&state, pos, "renamed").expect("rename hits the entry's helper");
    let changes = edit.changes.expect("rename produced changes");
    assert!(
        changes.contains_key(&main_uri),
        "rename should edit main.phx (entry's own helper)"
    );
    assert!(
        !changes.contains_key(&lib_uri),
        "rename of the entry's `helper` must NOT touch lib.phx's \
         unrelated `helper`; got changes for {:?}",
        changes.keys().collect::<Vec<_>>()
    );

    // And references — same regression.
    let locs = references_at(&state, pos).expect("references hit the entry's helper");
    let uris: HashSet<Url> = locs.iter().map(|l| l.uri.clone()).collect();
    assert!(uris.contains(&main_uri), "should include main.phx");
    assert!(
        !uris.contains(&lib_uri),
        "references for entry's `helper` must NOT include lib.phx; got {:?}",
        uris
    );
}

/// Companion to the rename collision test, but for `Field`. Two
/// distinct `Point` structs (one per module) each have a field
/// named `x`. The carrier name (`struct_name`) is also a bare
/// local string in `SymbolKind::Field`, so the qualifier comparison
/// has to disambiguate the carrier too.
#[test]
fn rename_does_not_conflate_same_named_field_across_modules() {
    let td = tempfile::TempDir::new().unwrap();
    let lib_path = td.path().join("lib.phx");
    let lib_text = "public struct Point { Int x  Int y }\n\
                    public function libUse() -> Int { let p: Point = Point(1, 2)\nreturn p.x }\n";
    std::fs::write(&lib_path, lib_text).unwrap();
    let main_path = td.path().join("main.phx");
    let main_text = "struct Point { Int x  Int y }\n\
                     function main() { let q: Point = Point(3, 4)\nlet _ = q.x }\n";
    std::fs::write(&main_path, main_text).unwrap();

    let (state, main_uri) = build_document_state_for(&main_path, &main_path, main_text.to_string());
    let lib_uri = Url::from_file_path(lib_path.canonicalize().unwrap()).unwrap();

    // Cursor on `q.x` in main.phx.
    let off = main_text.find("q.x").unwrap() + "q.".len();
    let lc = state.source_map.line_col(state.source_id, off);
    let pos = Position {
        line: (lc.line - 1) as u32,
        character: (lc.col - 1) as u32,
    };

    let edit = rename_at(&state, pos, "renamed").expect("rename hits the entry's Point.x");
    let changes = edit.changes.expect("rename produced changes");
    assert!(
        changes.contains_key(&main_uri),
        "rename should edit main.phx (entry's own Point.x use)"
    );
    assert!(
        !changes.contains_key(&lib_uri),
        "rename of entry's `Point.x` must NOT touch lib.phx's unrelated \
         `Point.x`; got changes for {:?}",
        changes.keys().collect::<Vec<_>>()
    );
}

#[test]
fn rename_returns_none_for_variable() {
    // Regression pin for the over-rename concern: variables aren't
    // tracked in `module_scopes`, so the LSP can only approximate
    // "same scope" by "same file". Renaming under that approximation
    // would silently rewrite every same-named local across every
    // function in the file. `rename_at` therefore refuses variable
    // rename until `VarInfo` is lifted into `ResolvedModule`.
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    // Two functions, each with a local `x`. If the LSP ever started
    // accepting the rename, this is the program that would expose
    // the bug — pin the refusal here so we notice if it changes.
    let main_text = "function a() { let x: Int = 1\nlet _ = x }\n\
                     function b() { let x: Int = 2\nlet _ = x }\n";
    std::fs::write(&main_path, main_text).unwrap();

    let (state, _uri) = build_document_state_for(&main_path, &main_path, main_text.to_string());
    // Cursor on the first `x` declaration.
    let off = main_text.find("let x").unwrap() + "let ".len();
    let lc = state.source_map.line_col(state.source_id, off);
    let pos = Position {
        line: (lc.line - 1) as u32,
        character: (lc.col - 1) as u32,
    };

    // Sanity: target_reference_at hits a `Variable` symbol here.
    let target = target_reference_at(&state, pos);
    if let Some(t) = target {
        assert!(
            matches!(t.kind, SymbolKind::Variable),
            "test setup expects cursor on a Variable, got {:?}",
            t.kind
        );
    }

    let edit = rename_at(&state, pos, "y");
    assert!(
        edit.is_none(),
        "rename_at must return None for SymbolKind::Variable to avoid \
         cross-scope overshoot; got {:?}",
        edit
    );
}

#[test]
fn completion_with_scope_entry_naming_unknown_qualified_key_is_skipped() {
    // Pins the "if scope contains a name whose qualified key isn't
    // in any of struct_by_name/enum_by_name/function_by_name, drop
    // it silently" branch in `completion_items_for`. Today this is
    // not reachable through normal sema output (every entry in
    // `module_scopes` is backed by a real declaration), but it's
    // the right defensive shape: a future schema change that adds
    // a new symbol category would otherwise ship as a silently
    // dropped completion item until somebody wires it up.
    let mut source_map = SourceMap::new();
    let source_id = source_map.add("test.phx", "function main() {}");
    let mut analysis = checker::check(&parser::parse(&tokenize("function main() {}", source_id)).0);
    // Inject a stale module_scopes entry pointing at a qualified
    // key that doesn't exist in any of the per-kind maps.
    let mut scope: HashMap<String, String> = HashMap::new();
    scope.insert("ghost".to_string(), "no_such_qualified_key".to_string());
    analysis
        .module
        .module_scopes
        .insert(ModulePath::entry(), scope);
    let mut source_id_to_url = HashMap::new();
    source_id_to_url.insert(source_id, test_uri());
    let mut source_id_to_module = HashMap::new();
    source_id_to_module.insert(source_id, ModulePath::entry());
    let state = DocumentState {
        source: "function main() {}".to_string(),
        source_map: Arc::new(source_map),
        source_id,
        current_module: ModulePath::entry(),
        check_result: Arc::new(analysis),
        source_id_to_url: Arc::new(source_id_to_url),
        source_id_to_module: Arc::new(source_id_to_module),
        canonical_path: None,
    };
    let items = completion_items_for(&state);
    let labels: HashSet<String> = items.iter().map(|i| i.label.clone()).collect();
    assert!(
        !labels.contains("ghost"),
        "completion must skip scope entries whose qualified key isn't \
         in any per-kind map; got: {:?}",
        labels
    );
    // Keywords still present.
    assert!(labels.contains("function"));
}

#[test]
fn completion_excludes_unimported_function_from_other_module() {
    let td = tempfile::TempDir::new().unwrap();
    let main_path = td.path().join("main.phx");
    let lib_path = td.path().join("lib.phx");
    std::fs::write(&lib_path, "public function helper() -> Int { return 42 }\n").unwrap();
    let main_text = "function main() {}\n";
    std::fs::write(&main_path, main_text).unwrap();

    let (state, _uri) = build_document_state_for(&main_path, &main_path, main_text.to_string());
    let items = completion_items_for(&state);
    let labels: HashSet<String> = items
        .iter()
        .filter(|i| i.kind == Some(CompletionItemKind::FUNCTION))
        .map(|i| i.label.clone())
        .collect();
    assert!(
        !labels.contains("helper"),
        "unimported function from other module must not appear in completion; got: {:?}",
        labels
    );
}

/// Build a minimal entry-module `DocumentState` used by tests that
/// only care about keyword completion (no cross-module setup).
fn keyword_only_state() -> DocumentState {
    let source = "function main() {}";
    let mut source_map = SourceMap::new();
    let source_id = source_map.add("test.phx", source);
    let analysis = checker::check(&parser::parse(&tokenize(source, source_id)).0);
    let mut source_id_to_url = HashMap::new();
    source_id_to_url.insert(source_id, test_uri());
    let mut source_id_to_module = HashMap::new();
    source_id_to_module.insert(source_id, ModulePath::entry());
    DocumentState {
        source: source.to_string(),
        source_map: Arc::new(source_map),
        source_id,
        current_module: ModulePath::entry(),
        check_result: Arc::new(analysis),
        source_id_to_url: Arc::new(source_id_to_url),
        source_id_to_module: Arc::new(source_id_to_module),
        canonical_path: None,
    }
}

/// **Drift detection.** Pins the LSP's keyword-completion list against
/// `phoenix_lexer::KEYWORDS`, the canonical lowercase user-facing
/// keyword set. If a future maintainer adds a keyword to the lexer
/// without the LSP's keyword loop picking it up — exactly the gap that
/// this catch-up sweep closed for `defer`, `dyn`, `omit`, `partial`,
/// `pick` — this test fails immediately rather than the regression
/// surviving into a release.
///
/// Asserts a **superset** rather than strict equality so the LSP can
/// add contextual / non-lexer keywords (e.g. snippet labels) without
/// breaking the test. The reverse direction — that the LSP doesn't
/// surface stale tokens removed from the lexer — would be useful but
/// requires tracking removed-keyword history; the existing pattern of
/// hand-editing the source on removal already catches that.
#[test]
fn lsp_keyword_completion_covers_every_lexer_keyword() {
    let state = keyword_only_state();
    let items = completion_items_for(&state);
    let lsp_keyword_labels: HashSet<&str> = items
        .iter()
        .filter(|i| i.kind == Some(CompletionItemKind::KEYWORD))
        .map(|i| i.label.as_str())
        .collect();
    let missing: Vec<&&str> = phoenix_lexer::KEYWORDS
        .iter()
        .filter(|kw| !lsp_keyword_labels.contains(*kw as &str))
        .collect();
    assert!(
        missing.is_empty(),
        "LSP keyword completion is missing entries from `phoenix_lexer::KEYWORDS`: {missing:?}. \
         Either the lexer's `KEYWORDS` const got out of sync with the LSP's keyword loop in \
         `completion_items_for`, or someone bypassed the loop. The LSP should pull from \
         `phoenix_lexer::KEYWORDS` directly so this drift is impossible.",
    );
}
