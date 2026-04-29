//! Import resolution and visibility enforcement: `import` statements bring named items into scope, private items can't be imported, wildcard imports skip privates, aliases work, single-file inputs reject `import`, and field-level visibility is enforced at access and at construction.
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
fn import_brings_public_function_into_scope() {
    let entry =
        entry_only("import lib { add }\nfunction main() { let x: Int = add(1, 2) print(x) }");
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:?}",
        analysis.diagnostics
    );
    // Positive check: the imported function is keyed under its
    // module-qualified name in the resolved tables (the import is a
    // scope entry, not a separate registration in entry's namespace).
    let info = analysis
        .module
        .function_info_by_name("lib::add")
        .expect("`add` should be registered under `lib::add`");
    assert_eq!(info.def_module, ModulePath(vec!["lib".to_string()]));
    assert!(
        analysis.module.function_info_by_name("add").is_none(),
        "imported function must not also be registered under its bare name in entry's namespace"
    );
}

#[test]
fn unimported_function_is_not_in_scope() {
    let entry = entry_only("function main() { let x: Int = add(1, 2) print(x) }");
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    // Without an import, `add` should not resolve from the entry module.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("undefined function `add`")),
        "expected undefined-function diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn importing_a_private_function_is_rejected_with_rich_diagnostic() {
    let entry = entry_only("import lib { add }\nfunction main() {}");
    let other = non_entry(
        "lib",
        "function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let private_diag = analysis
        .diagnostics
        .iter()
        .find(|d| d.message.contains("`add` is private to module `lib`"))
        .unwrap_or_else(|| {
            panic!(
                "expected private-import diagnostic, got: {:?}",
                analysis.diagnostics
            )
        });
    // Rich-shape consumer: must carry a note (definition span) and a suggestion.
    assert!(
        !private_diag.notes.is_empty(),
        "expected at least one note on the private-import diagnostic"
    );
    assert!(
        private_diag.suggestion.is_some(),
        "expected a suggestion on the private-import diagnostic"
    );
}

#[test]
fn importing_a_nonexistent_name_is_rejected() {
    let entry = entry_only("import lib { foo }\nfunction main() {}");
    let other = non_entry("lib", "public function bar() -> Int { 1 }", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`foo` is not declared in module `lib`")),
        "expected not-declared diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn aliased_import_uses_alias_in_scope() {
    let entry =
        entry_only("import lib { longName as ln }\nfunction main() { let x: Int = ln() print(x) }");
    let other = non_entry(
        "lib",
        "public function longName() -> Int { 42 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "alias call should resolve, got: {:?}",
        analysis.diagnostics
    );
    // Positive check: the registration is keyed by the *original*
    // qualified name (`lib::longName`), not by the alias. The alias
    // exists only as a scope mapping; aliasing must not create a second
    // FunctionInfo entry under `ln` or `lib::ln`.
    assert!(
        analysis
            .module
            .function_info_by_name("lib::longName")
            .is_some(),
        "original-named function should be registered under its qualified key"
    );
    assert!(
        analysis.module.function_info_by_name("ln").is_none(),
        "alias must not produce a registration under the bare alias name"
    );
    assert!(
        analysis.module.function_info_by_name("lib::ln").is_none(),
        "alias must not produce a registration under the qualified alias name"
    );
}

#[test]
fn original_name_not_in_scope_when_aliased() {
    let entry = entry_only(
        "import lib { longName as ln }\nfunction main() { let x: Int = longName() print(x) }",
    );
    let other = non_entry(
        "lib",
        "public function longName() -> Int { 42 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("undefined function `longName`")),
        "original name should not be in scope when aliased, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn wildcard_import_brings_only_public_items() {
    let entry = entry_only(
        "import lib { * }\nfunction main() { let a: Int = pub_a() let b: Int = priv_b() print(a + b) }",
    );
    let other = non_entry(
        "lib",
        "public function pub_a() -> Int { 1 }\nfunction priv_b() -> Int { 2 }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    // pub_a should resolve; priv_b should not.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("undefined function `priv_b`")),
        "expected priv_b to be undefined, got: {:?}",
        analysis.diagnostics
    );
    // pub_a should NOT trigger an undefined-function diagnostic.
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("undefined function `pub_a`")),
        "pub_a should be in scope via wildcard import, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn builtin_visibility_works_without_import() {
    // Non-entry module should be able to reference Option, Result, print
    // without an explicit import — builtins are in every module's scope.
    let entry = entry_only("function main() {}");
    let other = non_entry(
        "lib",
        "public function tryWrap(x: Int) -> Option<Int> { Some(x) }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected builtin Option to be in scope without import, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn same_function_name_in_two_modules_coexists_via_import() {
    let entry = entry_only(
        "import a { helper as helperA }\n\
         import b { helper as helperB }\n\
         function main() { let x: Int = helperA() let y: Int = helperB() print(x + y) }",
    );
    let module_a = non_entry("a", "public function helper() -> Int { 1 }", SourceId(1));
    let module_b = non_entry("b", "public function helper() -> Int { 2 }", SourceId(2));
    let analysis = check_modules(&[entry, module_a, module_b]);
    assert!(
        analysis.diagnostics.is_empty(),
        "two same-named functions imported under aliases should coexist, got: {:?}",
        analysis.diagnostics
    );
    // Both registrations land under their distinct module-qualified
    // keys. Aliases live in the importer's scope only — they do not
    // create additional FunctionInfo entries.
    let a_helper = analysis
        .module
        .function_info_by_name("a::helper")
        .expect("a's helper should be under `a::helper`");
    assert_eq!(a_helper.def_module, ModulePath(vec!["a".to_string()]));
    let b_helper = analysis
        .module
        .function_info_by_name("b::helper")
        .expect("b's helper should be under `b::helper`");
    assert_eq!(b_helper.def_module, ModulePath(vec!["b".to_string()]));
    assert_ne!(
        a_helper.func_id, b_helper.func_id,
        "two coexisting same-named functions must have distinct FuncIds"
    );
}

#[test]
fn cannot_import_from_unknown_module() {
    let entry = entry_only("import nonexistent { foo }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("cannot find module `nonexistent`")),
        "expected unknown-module diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn field_visibility_public_field_accessible_from_other_module() {
    let entry = entry_only(
        "import lib { User }\nfunction main() { let u: User = User(\"alice\") print(u.name) }",
    );
    let other = non_entry(
        "lib",
        "public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "public field on public struct should be accessible, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn field_visibility_private_field_rejected_from_other_module() {
    let entry = entry_only(
        "import lib { User }\nfunction main() { let u: User = User(\"alice\", \"hash\") print(u.password) }",
    );
    // Phoenix struct fields are newline-separated, not `;`-separated.
    let other = non_entry(
        "lib",
        "public struct User {\npublic String name\nString password\n}",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    // Structural match: the diagnostic must reference the offending
    // field name *and* signal "private". Avoid pinning the exact
    // wording — diagnostic copy may evolve and the test should keep
    // passing as long as the meaning is preserved.
    let priv_diag = analysis
        .diagnostics
        .iter()
        .find(|d| d.message.contains("`password`") && d.message.contains("private"))
        .unwrap_or_else(|| {
            panic!(
                "expected a private-field diagnostic mentioning `password`, got: {:?}",
                analysis.diagnostics
            )
        });
    assert!(
        !priv_diag.notes.is_empty(),
        "field-visibility diagnostic should carry a note pointing at the field declaration"
    );
    assert!(
        priv_diag.suggestion.is_some(),
        "field-visibility diagnostic should carry a suggestion"
    );
}

#[test]
fn own_module_can_access_own_private_field() {
    let entry = entry_only(
        "struct User {\nString name\nString password\n}\n\
         function main() { let u: User = User(\"alice\", \"hash\") print(u.password) }",
    );
    let analysis = check_modules(&[entry]);
    // The module's own private field is accessible from within the same module.
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| !d.message.contains("is private to module")),
        "own private field should be accessible, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn non_entry_module_cannot_see_unimported_entry_function() {
    // The bare-name fallback used to leak entry-module symbols into
    // every non-entry module. Pin the fix: a non-entry module without
    // any `import` of `helper` must not see entry's `helper`.
    let entry = entry_only("function helper() -> Int { 1 }\nfunction main() {}");
    let other = non_entry(
        "lib",
        "public function uses_helper() -> Int { helper() }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("undefined function `helper`")),
        "non-entry module should not see entry's `helper` without an import, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn endpoint_with_imported_struct_body_resolves() {
    // Endpoint declared in entry uses `User` brought in by an explicit
    // `import lib { User }`. Because endpoint checking now runs in the
    // body-check pass (after Phase B of module-scope construction), the
    // imported struct is reachable. Pins the fix to the bare-name probe
    // that previously lived in `check_endpoint`.
    let entry = entry_only(
        "import lib { User }\n\
         endpoint createUser: POST \"/api/users\" {\n\
         body User\n\
         response User\n\
         }\n\
         function main() {}",
    );
    let other = non_entry(
        "lib",
        "public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| !d.message.contains("unknown struct `User`")),
        "endpoint with imported struct body should resolve, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn construction_with_private_field_from_other_module_is_rejected() {
    // Symmetric to `field_visibility_private_field_rejected_from_other_module`
    // but on the *write* side: positional construction `User("a", "b")` from
    // entry on an imported `User` whose `password` is private must fail —
    // otherwise encapsulation would be a one-way mirror (read-blocked,
    // write-allowed).
    let entry = entry_only(
        "import lib { User }\n\
         function main() { let u: User = User(\"alice\", \"hash\") print(u) }",
    );
    let other = non_entry(
        "lib",
        "public struct User {\npublic String name\nString password\n}",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let priv_diag = analysis
        .diagnostics
        .iter()
        .find(|d| {
            d.message.contains("`password`")
                && d.message.contains("private")
                && d.message.contains("cannot be set")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a private-field diagnostic on construction, got: {:?}",
                analysis.diagnostics
            )
        });
    // Same rich shape as the read-side diagnostic.
    assert!(
        !priv_diag.notes.is_empty(),
        "construction-visibility diagnostic should carry a note pointing at the field declaration"
    );
    assert!(
        priv_diag.suggestion.is_some(),
        "construction-visibility diagnostic should carry a suggestion"
    );
}

#[test]
fn construction_with_only_public_fields_from_other_module_is_allowed() {
    let entry = entry_only(
        "import lib { User }\n\
         function main() { let u: User = User(\"alice\") print(u.name) }",
    );
    let other = non_entry(
        "lib",
        "public struct User {\npublic String name\n}",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "construction with only public fields should be allowed, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn own_module_can_construct_with_private_fields() {
    // Same module's construction sets its own private fields freely —
    // the gate only fires across module boundaries.
    let entry = entry_only(
        "struct User {\nString name\nString password\n}\n\
         function main() { let u: User = User(\"alice\", \"hash\") print(u.name) }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| !d.message.contains("cannot be set")),
        "same-module construction must not trigger the private-construction gate, got: {:?}",
        analysis.diagnostics
    );
}

// ── lookup_visible_enum_variant fallthrough ──────────────────────────────

#[test]
fn single_file_import_emits_diagnostic() {
    // `check_program` (single-file path) does not run Phase B of
    // module-scope construction, so `import` declarations would be
    // silently ignored. Diagnose them up front.
    use phoenix_sema::checker::check;
    let source = "import lib { foo }\nfunction main() {}";
    let tokens = phoenix_lexer::lexer::tokenize(source, SourceId(0));
    let (program, parse_errors) = phoenix_parser::parser::parse(&tokens);
    assert!(parse_errors.is_empty());
    let analysis = check(&program);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`import` is only valid in multi-module compilation")),
        "expected single-file import diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

// ── Coverage gaps surfaced in code review ────────────────────────────────

#[test]
fn field_assignment_private_field_from_other_module_is_rejected() {
    // Symmetric to the read-side and construction-side tests: writing
    // a private field via `obj.field = value` from outside the owning
    // module must fail. Without this gate, encapsulation would be a
    // one-way mirror — read-blocked, construction-blocked, but
    // write-via-mutation allowed.
    let entry = entry_only(
        "import lib { User, makeUser }\n\
         function main() { let mut u: User = makeUser(\"alice\") u.password = \"hash\" print(u) }",
    );
    let other = non_entry(
        "lib",
        "public struct User {\npublic String name\nString password\n}\n\
         public function makeUser(n: String) -> User { User(n, \"\") }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let priv_diag = analysis
        .diagnostics
        .iter()
        .find(|d| {
            d.message.contains("`password`")
                && d.message.contains("private")
                && d.message.contains("cannot be set")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a private-field diagnostic on assignment, got: {:?}",
                analysis.diagnostics
            )
        });
    assert!(
        !priv_diag.notes.is_empty(),
        "field-assignment privacy diagnostic should carry a note pointing at the field declaration"
    );
    assert!(
        priv_diag.suggestion.is_some(),
        "field-assignment privacy diagnostic should carry a suggestion"
    );
}

#[test]
fn field_assignment_public_field_from_other_module_is_allowed() {
    let entry = entry_only(
        "import lib { User, makeUser }\n\
         function main() { let mut u: User = makeUser(\"alice\") u.name = \"bob\" print(u.name) }",
    );
    let other = non_entry(
        "lib",
        "public struct User {\npublic String name\n}\n\
         public function makeUser(n: String) -> User { User(n) }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "assignment to public field should be allowed, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn import_of_cross_namespace_collision_emits_ambiguity_diagnostic() {
    // Phoenix keeps separate registration namespaces for functions /
    // structs / enums / traits / type aliases, so a target module that
    // declares `function Foo` *and* `struct Foo` is not rejected at
    // registration. Importing `Foo` from such a module must emit a
    // cross-namespace ambiguity diagnostic listing every candidate
    // (deterministic tie-break: the first-declared decl is the one
    // brought into scope) instead of crashing the compiler.
    let entry = entry_only("import lib { Foo }\nfunction main() {}");
    let other = non_entry(
        "lib",
        "public function Foo() -> Int { 1 }\npublic struct Foo { Int x }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let diag = analysis
        .diagnostics
        .iter()
        .find(|d| {
            d.message.contains("`Foo`")
                && d.message.contains("declared in multiple namespaces")
                && d.message.contains("function")
                && d.message.contains("struct")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected cross-namespace ambiguity diagnostic, got: {:?}",
                analysis.diagnostics
            )
        });
    // One note per candidate so the user can navigate to each
    // colliding declaration.
    assert!(
        diag.notes.len() >= 2,
        "expected one note per colliding decl, got: {:?}",
        diag.notes
    );
    // First-declared decl wins the tie-break and is brought into
    // scope (here: the function). Pin via the function-table key.
    assert!(
        analysis.module.function_info_by_name("lib::Foo").is_some(),
        "first-declared `Foo` (the function) must still be in scope after the ambiguity diagnostic"
    );
}

#[test]
fn field_assignment_own_module_private_field_is_allowed() {
    // Same-module mutation of a private field must not trip the
    // cross-module gate — only writes from *other* modules are blocked.
    let entry = entry_only(
        "struct User {\nString name\nString password\n}\n\
         function main() { let mut u: User = User(\"alice\", \"\") u.password = \"hash\" print(u.password) }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| !d.message.contains("cannot be set")),
        "same-module assignment must not trigger the private-field gate, got: {:?}",
        analysis.diagnostics
    );
}
