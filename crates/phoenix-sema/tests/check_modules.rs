//! Integration tests for the multi-module sema entry point
//! [`phoenix_sema::checker::check_modules`].
//!
//! These tests exercise the Phase 2.6 foundation: that single-file behavior
//! is preserved when only the entry module is supplied, that `def_module`
//! is stamped onto each registered `*Info`, and that `function main()` in a
//! non-entry module is rejected with a single, well-shaped diagnostic.

use std::path::PathBuf;

use phoenix_common::module_path::ModulePath;
use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_modules::ResolvedSourceModule;
use phoenix_parser::parser;
use phoenix_sema::checker::check_modules;

fn parse(source: &str, source_id: SourceId) -> phoenix_parser::ast::Program {
    let tokens = tokenize(source, source_id);
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    program
}

fn entry_only(source: &str) -> ResolvedSourceModule {
    ResolvedSourceModule {
        module_path: ModulePath::entry(),
        source_id: SourceId(0),
        program: parse(source, SourceId(0)),
        is_entry: true,
        file_path: PathBuf::from("<test>"),
    }
}

fn non_entry(name: &str, source: &str, source_id: SourceId) -> ResolvedSourceModule {
    ResolvedSourceModule {
        module_path: ModulePath(vec![name.to_string()]),
        source_id,
        program: parse(source, source_id),
        is_entry: false,
        file_path: PathBuf::from(format!("<test:{}>", name)),
    }
}

#[test]
fn single_entry_module_round_trips_like_check() {
    let entry = entry_only("function main() { let x: Int = 42 print(x) }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "diagnostics: {:?}",
        analysis.diagnostics
    );
    // Function table includes `main`.
    assert!(analysis.module.function_id("main").is_some());
}

#[test]
fn entry_module_function_def_module_is_entry() {
    let entry = entry_only("function helper() -> Int { 1 }\nfunction main() { }");
    let analysis = check_modules(&[entry]);
    let helper = analysis
        .module
        .function_info_by_name("helper")
        .expect("helper should be registered");
    assert_eq!(helper.def_module, ModulePath::entry());
}

#[test]
fn main_in_non_entry_module_is_rejected() {
    let entry = entry_only("function main() { print(\"hi\") }");
    let other = non_entry("other", "function main() { print(\"oops\") }", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`main` may only be declared in the entry module")),
        "expected main-in-non-entry diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn non_entry_module_with_no_main_is_accepted() {
    let entry = entry_only("function main() { }");
    let other = non_entry(
        "helpers",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.iter().all(|d| !d
            .message
            .contains("may only be declared in the entry module")),
        "expected no main-in-non-entry diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn public_function_carries_visibility_into_function_info() {
    use phoenix_parser::ast::Visibility;
    let entry = entry_only("public function exported(a: Int) -> Int { a }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    let info = analysis
        .module
        .function_info_by_name("exported")
        .expect("exported should be registered");
    assert_eq!(info.visibility, Visibility::Public);
}

#[test]
fn private_default_visibility_on_function_info() {
    use phoenix_parser::ast::Visibility;
    let entry = entry_only("function helper() -> Int { 1 }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    let info = analysis
        .module
        .function_info_by_name("helper")
        .expect("helper should be registered");
    assert_eq!(info.visibility, Visibility::Private);
}

#[test]
fn cross_module_function_name_collision_diagnosed() {
    let entry = entry_only("function helper() -> Int { 1 }\nfunction main() {}");
    let other = non_entry(
        "helpers",
        "public function helper() -> Int { 2 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("function `helper` is declared in modules")),
        "expected cross-module collision diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_struct_name_collision_diagnosed() {
    let entry = entry_only("struct User { Int id }\nfunction main() {}");
    let other = non_entry("models", "public struct User { String name }", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("struct `User` is declared in modules")),
        "expected cross-module struct collision diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_enum_name_collision_diagnosed() {
    let entry = entry_only("enum Color { Red Green }\nfunction main() {}");
    let other = non_entry("palette", "public enum Color { Blue Yellow }", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("enum `Color` is declared in modules")),
        "expected cross-module enum collision diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn no_collision_when_names_differ() {
    let entry = entry_only("function entryHelper() -> Int { 1 }\nfunction main() {}");
    let other = non_entry(
        "helpers",
        "public function libHelper() -> Int { 2 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("is declared in modules")),
        "expected no cross-module collision diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_trait_name_collision_diagnosed() {
    let entry = entry_only("trait Display { function show(self) -> String }\nfunction main() {}");
    let other = non_entry(
        "ui",
        "public trait Display { function render(self) -> String }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("trait `Display` is declared in modules")),
        "expected cross-module trait collision diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_type_alias_name_collision_diagnosed() {
    let entry = entry_only("type UserId = Int\nfunction main() {}");
    let other = non_entry("models", "public type UserId = String", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("type alias `UserId` is declared in modules")),
        "expected cross-module type-alias collision diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_collision_keeps_first_function_info() {
    // First-write-wins: the entry's `helper` must remain in the function
    // table even after the colliding non-entry `helper` is registered.
    let entry = entry_only("function helper() -> Int { 1 }\nfunction main() {}");
    let other = non_entry(
        "helpers",
        "public function helper() -> Int { 2 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let info = analysis
        .module
        .function_info_by_name("helper")
        .expect("entry's helper should still be in the function table");
    assert_eq!(info.def_module, ModulePath::entry());
}

#[test]
fn cross_module_collision_keeps_first_struct_fields() {
    // The entry's `User` struct has one field `id: Int`. The colliding
    // non-entry `User` has `name: String`. After registration, the
    // surviving StructInfo must be the entry's — not a partial merge,
    // not the non-entry version.
    let entry = entry_only("struct User { Int id }\nfunction main() {}");
    let other = non_entry("models", "public struct User { String name }", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    let info = analysis
        .module
        .struct_info_by_name("User")
        .expect("User struct should be registered");
    assert_eq!(info.def_module, ModulePath::entry());
    assert_eq!(info.fields.len(), 1);
    assert_eq!(info.fields[0].name, "id");
}

#[test]
fn non_entry_function_carries_def_module() {
    use phoenix_parser::ast::Visibility;
    let entry = entry_only("function main() {}");
    let other = non_entry(
        "helpers",
        "public function libHelper() -> Int { 2 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let info = analysis
        .module
        .function_info_by_name("libHelper")
        .expect("libHelper should be registered");
    assert_eq!(info.def_module, ModulePath(vec!["helpers".to_string()]));
    assert_eq!(info.visibility, Visibility::Public);
}

// NOTE: a corresponding `non_entry_struct_carries_def_module` test would
// be tempting to add here, but `build_structs` in `resolved.rs` currently
// iterates only the entry program's declarations, so non-entry-module
// structs do not land in `Analysis::module.structs`. That aggregation is
// part of the Phase 2.6 follow-up. The function-level test above covers
// the def_module stamping path because `build_functions` already
// aggregates cross-module via `checker.functions.drain()`.
