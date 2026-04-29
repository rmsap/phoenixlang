//! Foundation tests for the multi-module sema entry point: single-file behavior is preserved when only the entry module is supplied; `def_module` is stamped onto each registered `*Info`; `function main()` in a non-entry module is rejected; cross-module name coexistence under per-module mangling.
//!
//! Sibling of `check_modules_foundation.rs`, `check_modules_imports.rs`,
//! `check_modules_callable.rs`, `check_modules_coherence.rs` — split out from
//! a single 1969-line `check_modules.rs` so each topical group lives in
//! its own test binary.

mod common;

use common::{entry_only, non_entry};
use phoenix_common::module_path::ModulePath;
use phoenix_common::span::SourceId;
use phoenix_sema::checker::check_modules;

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
fn cross_module_function_names_coexist() {
    let entry = entry_only("function helper() -> Int { 1 }\nfunction main() {}");
    let other = non_entry(
        "helpers",
        "public function helper() -> Int { 2 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("is already defined")),
        "expected no within-module duplicate diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // Both registrations land under distinct qualified keys.
    let entry_helper = analysis
        .module
        .function_info_by_name("helper")
        .expect("entry's helper should be registered under its bare name");
    assert_eq!(entry_helper.def_module, ModulePath::entry());
    let other_helper = analysis
        .module
        .function_info_by_name("helpers::helper")
        .expect("non-entry helper should be registered under its qualified name");
    assert_eq!(
        other_helper.def_module,
        ModulePath(vec!["helpers".to_string()])
    );
}

#[test]
fn cross_module_struct_names_coexist() {
    // Both the entry's `User` and `models::User` exist with distinct
    // shapes. The entry uses its own `User`, and `models` uses its own
    // `User` internally — both should type-check against their *own*
    // field set without collision. Touches the qualified-key
    // registration path end-to-end (not just absence of diagnostics).
    let entry = entry_only(
        "struct User { Int id }\n\
         function main() { let u: User = User(42) print(u.id) }",
    );
    let other = non_entry(
        "models",
        "public struct User { String name }\n\
         public function makeUser(n: String) -> User { User(n) }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:?}",
        analysis.diagnostics
    );
    // Entry's User has `id`; models::User has `name`. Both coexist.
    let entry_user = analysis
        .module
        .struct_info_by_name("User")
        .expect("entry's User should be registered under its bare name");
    assert_eq!(entry_user.def_module, ModulePath::entry());
    assert_eq!(entry_user.fields.len(), 1);
    assert_eq!(entry_user.fields[0].name, "id");
}

#[test]
fn cross_module_enum_names_coexist() {
    let entry = entry_only("enum Color { Red Green }\nfunction main() {}");
    let other = non_entry("palette", "public enum Color { Blue Yellow }", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_trait_names_coexist() {
    let entry = entry_only("trait Display { function show(self) -> String }\nfunction main() {}");
    let other = non_entry(
        "ui",
        "public trait Display { function render(self) -> String }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_type_alias_names_coexist() {
    let entry = entry_only("type UserId = Int\nfunction main() {}");
    let other = non_entry("models", "public type UserId = String", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn no_diagnostics_when_names_differ() {
    let entry = entry_only("function entryHelper() -> Int { 1 }\nfunction main() {}");
    let other = non_entry(
        "helpers",
        "public function libHelper() -> Int { 2 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:?}",
        analysis.diagnostics
    );
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
    // Non-entry functions are registered under their qualified key.
    let info = analysis
        .module
        .function_info_by_name("helpers::libHelper")
        .expect("libHelper should be registered under its qualified name");
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
