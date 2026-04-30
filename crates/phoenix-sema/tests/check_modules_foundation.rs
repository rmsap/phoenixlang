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

#[test]
fn single_file_module_scopes_map_each_name_to_itself() {
    // Pin the doc-claim on `ResolvedModule::module_scopes`: in a
    // single-file program, the entry-keyed scope contains every
    // user-defined name *and* every builtin as `name → name`. The IR's
    // `qualify` fast path (which short-circuits to `Cow::Borrowed`
    // when the scope-resolved key equals the input) relies on this.
    // If a future change starts qualifying entry-module names against
    // a non-empty prefix, the scope entries would diverge from the
    // input and every IR call site would start allocating.
    //
    // We assert the invariant over *every* entry in the scope rather
    // than a hard-coded literal list, so a new builtin (e.g. a future
    // `Iterator`) is automatically covered without touching this test.
    // The hard-coded `expected` set still pins the user-defined names
    // and the today's builtins as a smoke check that the scope isn't
    // empty for some pathological reason.
    let entry = entry_only(
        "struct User { String name }\n\
         enum Color { Red Green Blue }\n\
         function helper() -> Int { 1 }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "diagnostics: {:?}",
        analysis.diagnostics,
    );
    let entry_scope = analysis
        .module
        .module_scopes
        .get(&ModulePath::entry())
        .expect("entry module scope must exist for a single-file program");

    // Every entry maps to itself — covers user-defined names plus
    // whatever builtins this build registered, today and tomorrow.
    for (local, mapped) in entry_scope {
        assert_eq!(
            local, mapped,
            "entry-module `{}` must map to itself in scope (got `{}`)",
            local, mapped,
        );
    }

    // Smoke check: the user-defined names and today's builtins must
    // be present. If a future builtin is added, no edit is required
    // here — the loop above already covers it.
    for name in ["User", "Color", "helper", "main", "Option", "Result"] {
        assert!(
            entry_scope.contains_key(name),
            "`{}` missing from entry scope (scope keys: {:?})",
            name,
            entry_scope.keys().collect::<Vec<_>>(),
        );
    }
}

#[test]
fn non_entry_struct_carries_def_module() {
    use phoenix_parser::ast::Visibility;
    let entry = entry_only("function main() {}");
    let other = non_entry(
        "models",
        "public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let info = analysis
        .module
        .struct_info_by_name("models::User")
        .expect("non-entry struct should be drained into Analysis::module.structs");
    assert_eq!(info.def_module, ModulePath(vec!["models".to_string()]));
    assert_eq!(info.visibility, Visibility::Public);
}

#[test]
fn non_entry_drain_is_lexical_so_id_allocation_is_deterministic() {
    // Pin the doc-claim on `drain_remaining_into`: non-entry structs
    // are appended to `module.structs` in lexical order of their
    // qualified key, so id allocation is deterministic across runs
    // (HashMap iteration is not). Two non-entry modules `a` and `z`,
    // each contributing one struct — `a::Foo` must come before
    // `z::Bar` even though the dependency-ordering / parse-ordering
    // of the modules tells us nothing about which would land first.
    let entry = entry_only("function main() {}");
    let mod_z = non_entry("z", "public struct Bar { public Int x }", SourceId(1));
    let mod_a = non_entry("a", "public struct Foo { public Int x }", SourceId(2));
    let analysis = check_modules(&[entry, mod_z, mod_a]);
    let foo_id = analysis
        .module
        .struct_id("a::Foo")
        .expect("a::Foo must be drained");
    let bar_id = analysis
        .module
        .struct_id("z::Bar")
        .expect("z::Bar must be drained");
    assert!(
        foo_id.index() < bar_id.index(),
        "a::Foo (id {}) must precede z::Bar (id {}) in lexical drain order",
        foo_id.index(),
        bar_id.index(),
    );
}

#[test]
fn non_entry_drain_is_lexical_within_a_module_too() {
    // The cross-module test above only pins inter-module ordering.
    // Pin the *intra-module* contract too: two structs declared in
    // the same non-entry module land in lexical order of their
    // qualified key, regardless of AST source order. This is what
    // `drain_remaining_into`'s `sort_by(|a, b| a.0.cmp(&b.0))` yields,
    // and a future change that switches to AST-order drain (to
    // preserve single-file allocation order) would re-order these
    // two ids — tripping this assert before any downstream id-lookup
    // call site silently consumes the new order.
    let entry = entry_only("function main() {}");
    // Source-order: `Zebra` declared *before* `Aardvark`. The lexical
    // order of qualified keys (`models::Aardvark`, `models::Zebra`)
    // is the opposite, so a regression that drained AST-order would
    // give `Zebra` the lower id.
    let other = non_entry(
        "models",
        "public struct Zebra { public Int x }\n\
         public struct Aardvark { public Int x }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let aardvark_id = analysis
        .module
        .struct_id("models::Aardvark")
        .expect("models::Aardvark must be drained");
    let zebra_id = analysis
        .module
        .struct_id("models::Zebra")
        .expect("models::Zebra must be drained");
    assert!(
        aardvark_id.index() < zebra_id.index(),
        "models::Aardvark (id {}) must precede models::Zebra (id {}) in lexical drain order, \
         even though `Zebra` appears first in the source",
        aardvark_id.index(),
        zebra_id.index(),
    );
}
