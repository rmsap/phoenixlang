//! Import resolution and visibility enforcement: `import` statements bring named items into scope, private items can't be imported, wildcard imports skip privates, aliases work, single-file inputs reject `import`, and field-level visibility is enforced at access and at construction.
//!
//! Sibling of `check_modules_foundation.rs`, `check_modules_imports.rs`,
//! `check_modules_callable.rs`, `check_modules_coherence.rs` — split out from
//! a single 1969-line `check_modules.rs` so each topical group lives in
//! its own test binary.

mod common;

use common::{entry_only, non_entry, non_entry_dotted};
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
fn namespace_import_from_unknown_module_diagnoses() {
    // The namespace form (`import a.b`, no braces) flows through the same
    // in-resolution module-existence check as the named/wildcard forms: an
    // unresolvable target must still produce a "cannot find module"
    // diagnostic (here via the defense-in-depth arm in `resolve_imports`,
    // since no file resolver runs in this harness), not be silently accepted
    // by the no-op `Namespace` resolution arm.
    let entry = entry_only("import nonexistent\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("cannot find module `nonexistent`")),
        "expected unknown-module diagnostic for namespace import, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn namespace_import_does_not_pollute_visible_symbols() {
    // A namespace import binds into the module's *namespaces* map, kept
    // separate from `visible_symbols`. Pin that it does not (a) error or
    // (b) leak the target's public names, nor the namespace name itself,
    // into the importer's value/type scope — qualified access goes
    // through `ns.func(...)` dispatch, not a bare-name lookup.
    let entry = entry_only("import lib\nfunction main() {}");
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "valid namespace import must not error, got: {:?}",
        analysis.diagnostics
    );
    let entry_scope = analysis
        .module
        .module_scopes
        .get(&ModulePath::entry())
        .expect("entry module scope must exist");
    assert!(
        !entry_scope.contains_key("add"),
        "namespace import must not pull `add` into visible_symbols (keys: {:?})",
        entry_scope.keys().collect::<Vec<_>>()
    );
    assert!(
        !entry_scope.contains_key("lib"),
        "namespace name `lib` must not enter visible_symbols (keys: {:?})",
        entry_scope.keys().collect::<Vec<_>>()
    );
}

#[test]
fn field_visibility_public_field_accessible_from_other_module() {
    let entry = entry_only(
        "import lib { User }\nfunction main() { let u: User = User(\"alice\") print(u.name) }",
    );
    let other = non_entry(
        "lib",
        "public struct User { public name: String }",
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
        "public struct User {\npublic name: String\npassword: String\n}",
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
        "struct User {\nname: String\npassword: String\n}\n\
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
        "public struct User { public name: String }",
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
        "public struct User {\npublic name: String\npassword: String\n}",
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
        "public struct User {\npublic name: String\n}",
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
        "struct User {\nname: String\npassword: String\n}\n\
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
        "public struct User {\npublic name: String\npassword: String\n}\n\
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
        "public struct User {\npublic name: String\n}\n\
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
        "public function Foo() -> Int { 1 }\npublic struct Foo { x: Int }",
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
        "struct User {\nname: String\npassword: String\n}\n\
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

// ── namespace imports (`import lib` → `lib.func(...)`) ──────────────

#[test]
fn namespace_import_calls_public_function() {
    let entry = entry_only("import lib\nfunction main() { let x: Int = lib.add(1, 2) print(x) }");
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
    // The resolved call target is recorded under the qualified key for IR.
    assert!(
        analysis
            .module
            .namespace_call_targets
            .values()
            .any(|t| t == "lib::add"),
        "expected namespace_call_targets to record `lib::add`, got: {:?}",
        analysis.module.namespace_call_targets
    );
}

#[test]
fn namespace_import_aliased_calls_public_function() {
    let entry = entry_only(
        "import lib as helpers\nfunction main() { let x: Int = helpers.add(1, 2) print(x) }",
    );
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
    // The recorded target is keyed by module path, not the alias: aliasing
    // is purely local, so IR still resolves `helpers.add` to `lib::add`.
    assert!(
        analysis
            .module
            .namespace_call_targets
            .values()
            .any(|t| t == "lib::add"),
        "expected the aliased call to record `lib::add`, got: {:?}",
        analysis.module.namespace_call_targets
    );
}

#[test]
fn namespace_call_arity_mismatch_is_rejected() {
    // Arity diagnostics flow through the shared call machinery
    // (`check_call_with_info`, fed the arg slices directly — no synthetic
    // `CallExpr`); pin that the namespace path reaches them.
    let entry = entry_only("import lib\nfunction main() { lib.add(1, 2, 3) }");
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("takes 2 argument(s), got 3")),
        "expected an arity diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn namespace_call_with_turbofish_is_rejected() {
    // A namespaced free-function call infers its type args from the
    // arguments; an explicit turbofish is meaningless and is rejected (the
    // `json.*` intrinsic path, which will define its own type-arg semantics
    // and is handled separately and not affected here).
    let entry = entry_only("import lib\nfunction main() { lib.id<Int>(1) }");
    let other = non_entry(
        "lib",
        "public function id(x: Int) -> Int { x }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`lib.id` does not take type arguments")),
        "expected a turbofish-rejection diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn namespace_call_to_private_function_is_rejected() {
    let entry = entry_only("import lib\nfunction main() { lib.add(1, 2) }");
    let other = non_entry(
        "lib",
        "function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("is private to module `lib`")),
        "expected private-access diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn namespace_call_to_unknown_function_is_rejected() {
    let entry = entry_only("import lib\nfunction main() { lib.missing(1) }");
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("module `lib` has no function `missing`")),
        "expected unknown-function diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn namespace_call_type_checks_arguments() {
    let entry =
        entry_only("import lib\nfunction main() { let x: Int = lib.add(1, \"two\") print(x) }");
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("expected `Int` but got `String`")),
        "expected an argument-type diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn local_binding_shadows_namespace() {
    // A `let lib = ...` shadows the namespace: `lib.add(...)` then dispatches
    // on the Int value, which has no such method.
    let entry = entry_only(
        "import lib\nfunction main() { let lib: Int = 5 let y: Int = lib.add(1, 2) print(y) }",
    );
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("no method `add` on type `Int`")),
        "expected the shadowing local to route to the value path, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn intrinsic_json_namespace_encode_typechecks() {
    // `import json` binds the intrinsic namespace (no source file needed)
    // and `json.encode(value)` type-checks to `String`.
    let entry =
        entry_only("import json\nfunction main() { let s: String = json.encode(5) print(s) }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "json.encode should type-check via the bound namespace, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn intrinsic_json_namespace_aliased_binds() {
    // `import json as j` binds the intrinsic under the alias, exactly like
    // a user-module namespace; the alias dispatches to the same intrinsic.
    let entry =
        entry_only("import json as j\nfunction main() { let s: String = j.encode(5) print(s) }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "json.encode via the alias should type-check, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_scalar_typechecks_to_result() {
    // `json.decode<Int>(s)` type-checks to `Result<Int, JsonError>`.
    let entry = entry_only(
        "import json\n\
         function main() {\n  \
           let r: Result<Int, JsonError> = json.decode<Int>(\"5\")\n  \
           match r { Ok(v) -> print(toString(v)) Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "json.decode<Int> should type-check to Result<Int, JsonError>, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_via_aliased_namespace_binds_and_records() {
    // `import json as j`: the alias dispatches to the same intrinsic (like
    // the encode aliasing test above), and the decode site is recorded for
    // IR decoder synthesis exactly as with the bare name.
    let entry = entry_only(
        "import json as j\n\
         function main() {\n  \
           let r: Result<Int, JsonError> = j.decode<Int>(\"5\")\n  \
           match r { Ok(v) -> print(toString(v)) Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "j.decode<Int> via the alias should type-check, got: {:?}",
        analysis.diagnostics
    );
    assert_eq!(
        analysis.module.json_decode_types.len(),
        1,
        "the aliased decode call must be recorded as a decode site"
    );
}

#[test]
fn json_decode_without_type_argument_is_an_error() {
    // Without an explicit turbofish, `json.decode` cannot resolve `T`.
    let entry = entry_only("import json\nfunction main() { json.decode(\"x\") }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("requires an explicit type argument")),
        "expected the missing-type-argument diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_with_wrong_argument_count_is_an_error() {
    // `json.decode` takes exactly one argument; zero (or two) is rejected
    // even though the turbofish resolves `T`.
    let entry = entry_only("import json\nfunction main() { json.decode<Int>() }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("json.decode() takes 1 argument")),
        "expected the argument-count diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_with_two_arguments_is_an_error() {
    // The other side of the argument-count check: too many arguments,
    // with recovery type-checking still applied to each of them.
    let entry = entry_only("import json\nfunction main() { json.decode<Int>(\"a\", \"b\") }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("json.decode() takes 1 argument, got 2")),
        "expected the argument-count diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_with_non_string_argument_is_an_error() {
    // A non-`String` argument is rejected — and, crucially, the call must
    // NOT be recorded as a decode site: IR lowering and the interpreters
    // index `args[0]` unconditionally for recorded sites, so a malformed
    // call slipping through would be a downstream panic, not a diagnostic.
    let entry = entry_only("import json\nfunction main() { json.decode<Int>(5) }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("expects a `String` argument")),
        "expected the argument-type diagnostic, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis.module.json_decode_types.is_empty(),
        "a malformed json.decode call must not be recorded as a decode site"
    );
}

#[test]
fn json_decode_with_multiple_type_arguments_is_an_error() {
    // A single turbofish type argument names `T`; more is rejected.
    let entry = entry_only("import json\nfunction main() { json.decode<Int, Bool>(\"5\") }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("json.decode takes a single type argument")),
        "expected the too-many-type-arguments diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_of_struct_typechecks() {
    // A non-generic struct (with a nested struct field) is a decodable target.
    let entry = entry_only(
        "import json\n\
         struct P { x: Int }\n\
         struct Q { p: P name: String }\n\
         function main() {\n  \
           let r: Result<Q, JsonError> = json.decode<Q>(\"x\")\n  \
           match r { Ok(v) -> print(v.name) Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "struct decode should type-check, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_of_generic_struct_is_an_error() {
    // A generic struct instantiation is not a decodable target yet.
    let entry = entry_only(
        "import json\n\
         struct Box<T> { value: T }\n\
         function main() {\n  \
           let r: Result<Box<Int>, JsonError> = json.decode<Box<Int>>(\"x\")\n  \
           match r { Ok(v) -> print(\"ok\") Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`json.decode` does not support")),
        "expected an unsupported-type diagnostic for json.decode of a generic struct, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_of_struct_with_undecodable_field_is_an_error() {
    // The gate recurses into fields: a struct wrapping a `List<Int>` is
    // rejected, and the diagnostic names the offending field type, not the
    // struct.
    let entry = entry_only(
        "import json\n\
         struct S { xs: List<Int> }\n\
         function main() {\n  \
           let r: Result<S, JsonError> = json.decode<S>(\"x\")\n  \
           match r { Ok(v) -> print(\"ok\") Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("does not support `List<Int>`")),
        "expected the diagnostic to name the undecodable field type `List<Int>`, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_of_struct_with_enum_field_is_an_error() {
    // The gate's `Named` arm requires a struct: a field whose type resolves
    // to an enum is rejected (enum decode lands in a later slice), and the
    // diagnostic names the enum, not the struct.
    let entry = entry_only(
        "import json\n\
         enum Color { Red  Green }\n\
         struct S { c: Color }\n\
         function main() {\n  \
           let r: Result<S, JsonError> = json.decode<S>(\"x\")\n  \
           match r { Ok(v) -> print(\"ok\") Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("does not support `Color`")),
        "expected the diagnostic to name the enum field type `Color`, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_diagnostic_names_an_imported_type_with_its_qualifier() {
    // The gate prints *canonical* names: for an imported type that means the
    // module qualifier stays in the diagnostic, so same-named types from
    // different modules remain distinguishable. (An entry-module type's
    // canonical spelling is bare — see
    // `json_decode_of_struct_with_enum_field_is_an_error`.)
    let entry = entry_only(
        "import json\n\
         import models { Color }\n\
         struct S { c: Color }\n\
         function main() {\n  \
           let r: Result<S, JsonError> = json.decode<S>(\"x\")\n  \
           match r { Ok(v) -> print(\"ok\") Err(e) -> print(\"err\") }\n\
         }",
    );
    let models = non_entry("models", "public enum Color { Red  Green }", SourceId(1));
    let analysis = check_modules(&[entry, models]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("does not support `models::Color`")),
        "expected the diagnostic to keep the imported enum's qualifier, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_imported_generic_struct_is_rejected_as_spelled() {
    // Encode-side sibling of the test above, pinning the one *reachable*
    // imported-and-unsupported encode shape: a generic struct instantiation.
    // Unlike `Type::Named` payloads (always canonical, qualifier included),
    // a `Type::Generic` payload keeps the source spelling — so the
    // diagnostic says `Box<Int>`, as written at the call site. If generic
    // payloads are ever canonicalized too, update the expected spelling to
    // `models::Box<Int>`. (The gate's other qualified-name arms — a bare
    // `Type::Named` naming a generic struct or enum — are defensive:
    // resolution errors out before producing such a type.)
    let entry = entry_only(
        "import json\n\
         import models { Box }\n\
         function main() {\n  \
           let b: Box<Int> = Box(1)\n  \
           print(json.encode(b))\n\
         }",
    );
    let models = non_entry(
        "models",
        "public struct Box<T> { public value: T }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, models]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`json.encode` does not support `Box<Int>`")),
        "expected an unsupported-type diagnostic for the imported generic \
         struct instantiation, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_of_imported_struct_with_private_field_is_rejected() {
    // `json.decode` constructs the struct, so decoding a foreign struct
    // with a private field would bypass the construction-privacy gate
    // (see design-decisions.md §Phase 4.6 JSON serialization).
    let entry = entry_only(
        "import json\n\
         import models { Vault }\n\
         function main() {\n  \
           let r: Result<Vault, JsonError> = json.decode<Vault>(\"x\")\n  \
           match r { Ok(v) -> print(\"ok\") Err(e) -> print(\"err\") }\n\
         }",
    );
    let models = non_entry(
        "models",
        "public struct Vault { secret: String }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, models]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message
                .contains("`json.decode` cannot construct struct `models::Vault`")
                && d.message.contains("`secret`")
                && d.message.contains("private to module `models`")
        }),
        "expected a batched privacy diagnostic naming the struct, field, and \
         module, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_reaching_a_private_field_struct_through_a_field_is_rejected() {
    // The privacy walk is transitive: the offending struct sits behind a
    // field of an entry-module struct, and the diagnostic names the nested
    // struct (where the private field lives), not the decode target.
    let entry = entry_only(
        "import json\n\
         import models { Vault }\n\
         struct Wrapper { v: Vault  tag: String }\n\
         function main() {\n  \
           let r: Result<Wrapper, JsonError> = json.decode<Wrapper>(\"x\")\n  \
           match r { Ok(v) -> print(\"ok\") Err(e) -> print(\"err\") }\n\
         }",
    );
    let models = non_entry(
        "models",
        "public struct Vault { secret: String }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, models]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`json.decode` cannot construct struct `models::Vault`")),
        "expected the privacy diagnostic to name the nested foreign struct, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_of_own_module_struct_with_private_field_typechecks() {
    // Privacy is a cross-module boundary, exactly as for reads/writes/
    // construction: the defining module may decode its own private fields.
    let entry = entry_only(
        "import json\n\
         struct Vault { secret: String }\n\
         function main() {\n  \
           let r: Result<Vault, JsonError> = json.decode<Vault>(\"x\")\n  \
           match r { Ok(v) -> print(v.secret) Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "same-module decode of a private-field struct should type-check, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_imported_struct_with_private_field_is_rejected() {
    // Encode-side sibling: serializing a foreign struct would expose field
    // values the caller cannot read directly.
    let entry = entry_only(
        "import json\n\
         import models { Vault }\n\
         function show(v: Vault) -> String { json.encode(v) }\n\
         function main() { print(\"x\") }",
    );
    let models = non_entry(
        "models",
        "public struct Vault { secret: String }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, models]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message
                .contains("`json.encode` cannot serialize struct `models::Vault`")
                && d.message.contains("private to module `models`")
        }),
        "expected an encode-privacy diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_reaching_a_private_field_struct_through_an_enum_is_rejected() {
    // The walk recurses through enum variant payloads too (enums are
    // encodable), so a private-field struct can't hide inside one.
    let entry = entry_only(
        "import json\n\
         import models { Holder }\n\
         function show(h: Holder) -> String { json.encode(h) }\n\
         function main() { print(\"x\") }",
    );
    let models = non_entry(
        "models",
        "public struct Vault { secret: String }\n\
         public enum Holder { Empty  Full(Vault) }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, models]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`json.encode` cannot serialize struct `models::Vault`")),
        "expected the enum payload's foreign struct to be rejected, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_generic_container_of_private_field_struct_is_rejected() {
    // The walk recurses through generic *arguments* (`List<T>` / `Option<T>`
    // / `Map<K, V>` are encodable containers), so a private-field struct
    // can't hide inside one. Pins the `Type::Generic` arm of the privacy
    // walk, which the direct / struct-field / enum-payload tests don't reach.
    let entry = entry_only(
        "import json\n\
         import models { Vault }\n\
         function showAll(vs: List<Vault>) -> String { json.encode(vs) }\n\
         function showMaybe(v: Option<Vault>) -> String { json.encode(v) }\n\
         function main() { print(\"x\") }",
    );
    let models = non_entry(
        "models",
        "public struct Vault { secret: String }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, models]);
    let privacy_errors = analysis
        .diagnostics
        .iter()
        .filter(|d| {
            d.message
                .contains("`json.encode` cannot serialize struct `models::Vault`")
        })
        .count();
    assert_eq!(
        privacy_errors, 2,
        "expected both container call sites (List and Option) to be rejected, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_own_module_struct_with_private_field_typechecks() {
    // Same-module encode of private fields is fine, matching field reads.
    let entry = entry_only(
        "import json\n\
         struct Vault { secret: String }\n\
         function main() { print(json.encode(Vault(\"s\"))) }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "same-module encode of a private-field struct should type-check, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_of_mutually_recursive_structs_terminates() {
    // The gate's cycle guard: a struct cycle (A → B → A) must terminate
    // rather than recurse forever, and — mirroring the encode-side gate —
    // the back-edge itself is treated as decodable, so the walk's verdict
    // comes from the remaining (scalar) fields.
    let entry = entry_only(
        "import json\n\
         struct A { b: B x: Int }\n\
         struct B { a: A }\n\
         function main() {\n  \
           let r: Result<A, JsonError> = json.decode<A>(\"x\")\n  \
           match r { Ok(v) -> print(\"ok\") Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "the cycle guard should accept a struct cycle, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_decode_of_unsupported_type_is_a_clear_error() {
    // This slice decodes scalars + structs; a `List` target is rejected.
    let entry = entry_only(
        "import json\n\
         function main() {\n  \
           let r: Result<List<Int>, JsonError> = json.decode<List<Int>>(\"x\")\n  \
           match r { Ok(v) -> print(\"ok\") Err(e) -> print(\"err\") }\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`json.decode` does not support")),
        "expected an unsupported-type diagnostic for json.decode of a List, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_list_and_string_map_typechecks() {
    // `List<T>` and `Map<String, V>` are encodable.
    let entry = entry_only(
        "import json\n\
         function main() {\n  \
           print(json.encode([1, 2, 3]))\n  \
           let m: Map<String, Int> = {\"x\": 1}\n  \
           print(json.encode(m))\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "List and Map<String,_> encode should type-check, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_nonstring_key_map_is_unsupported() {
    // Non-`String`-key maps (which serialize as `[k, v]` pairs) are deferred,
    // so they are rejected with a "does not support" diagnostic.
    let entry = entry_only(
        "import json\n\
         function main() {\n  \
           let m: Map<Int, Int> = {1: 2}\n  \
           print(json.encode(m))\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`json.encode` does not support")),
        "expected an unsupported-type diagnostic for a non-String-key Map, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_option_and_enum_typechecks() {
    // `Option<T>` and non-generic enums (unit + payload variants) are
    // encodable.
    let entry = entry_only(
        "import json\n\
         enum Status {\n  Active\n  Pending(Int)\n}\n\
         function main() {\n  \
           print(json.encode(Pending(3)))\n  \
           let o: Option<Int> = None\n  \
           print(json.encode(o))\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "Option and non-generic enum encode should type-check, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_generic_enum_is_unsupported() {
    // Generic enums other than `Option` (here `Result`) are not yet
    // encodable and are rejected with a clear diagnostic.
    let entry = entry_only(
        "import json\n\
         function main() {\n  \
           let r: Result<Int, String> = Ok(1)\n  \
           print(json.encode(r))\n\
         }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("`json.encode` does not support")),
        "expected an unsupported-type diagnostic for json.encode of a Result, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_enum_with_unsupported_variant_field_names_the_field_type() {
    // `unsupported_json_encode_type` recurses into enum *variant* field types:
    // an enum whose own shape is fine but that carries an unsupported payload
    // (a non-`String`-key `Map`) must be rejected, and the diagnostic must
    // name the variant field's type (`Map`), not the enum — proving the walk
    // descended into the variant.
    let entry = entry_only(concat!(
        "import json\n",
        "enum Tagged { Plain Boxed(Map<Int, Int>) }\n",
        "function main() { print(json.encode(Boxed({1: 2}))) }",
    ));
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`json.encode` does not support") && d.message.contains("Map")
        }),
        "expected an unsupported-variant-field diagnostic naming `Map`, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_empty_enum_is_supported_and_does_not_crash() {
    // An uninhabited enum (zero variants) is vacuously encodable: every
    // variant's fields must be encodable, and there are none. Sema must accept
    // it (no diagnostic) rather than reject it as unsupported. (The IR-side
    // regression guard — that encoder synthesis doesn't underflow on the empty
    // variant ladder — lives in phoenix-ir's
    // `json_encode_of_empty_enum_synthesizes_without_panicking`.)
    let entry = entry_only(concat!(
        "import json\n",
        "enum Empty {}\n",
        "function unused(v: Empty) { print(json.encode(v)) }\n",
        "function main() { print(\"ok\") }",
    ));
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "empty enum should be vacuously encodable, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_recursive_option_self_type_typechecks() {
    // A struct closed through `Option<Self>` is encodable: the encodability
    // walk must terminate on the back-edge (the `visiting` guard returns
    // `None`) rather than recurse forever, and accept the type. This locks in
    // the back-edge guard's reasoning — such a type IS constructible and
    // reaches a live `json.encode` (the synthesized encoder is itself
    // recursive and terminates on the finite runtime value).
    let entry = entry_only(concat!(
        "import json\n",
        "struct Node { value: Int  next: Option<Node> }\n",
        "function main() { print(json.encode(Node(1, None))) }",
    ));
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "recursive Option<Self> struct should be encodable, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_of_struct_with_unsupported_field_names_the_field_type() {
    // `unsupported_json_encode_type` recurses into struct fields: a struct
    // whose own shape is fine but that carries an unsupported field (a
    // non-`String`-key `Map`) must be rejected, and the diagnostic must name
    // the *field's* type (`Map`), not the struct — proving the walk descended
    // into the field.
    let entry = entry_only(concat!(
        "import json\n",
        "struct Bag { items: Map<Int, Int> }\n",
        "function main() { print(json.encode(Bag({1: 2}))) }",
    ));
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`json.encode` does not support") && d.message.contains("Map")
        }),
        "expected an unsupported-field diagnostic naming `Map`, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_rejects_type_arguments() {
    // `json.encode` is not generic: a turbofish must be rejected with a
    // dedicated diagnostic (not silently accepted).
    let entry = entry_only("import json\nfunction main() { print(json.encode<Int>(5)) }");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`json.encode` does not take type arguments")),
        "expected a type-argument-rejection diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn json_encode_rejects_wrong_argument_count() {
    // `json.encode` takes exactly one argument; zero or many must report
    // the arity error.
    let zero = entry_only("import json\nfunction main() { print(json.encode()) }");
    let zero_analysis = check_modules(&[zero]);
    assert!(
        zero_analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("json.encode() takes 1 argument, got 0")),
        "expected an arity diagnostic for zero args, got: {:?}",
        zero_analysis.diagnostics
    );

    let many = entry_only("import json\nfunction main() { print(json.encode(1, 2)) }");
    let many_analysis = check_modules(&[many]);
    assert!(
        many_analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("json.encode() takes 1 argument, got 2")),
        "expected an arity diagnostic for two args, got: {:?}",
        many_analysis.diagnostics
    );
}

#[test]
fn destructuring_import_of_json_is_rejected() {
    let entry = entry_only("import json { encode }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("destructuring import of `json` members is not available")),
        "expected the json-destructuring diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn wildcard_import_of_json_is_rejected() {
    // The wildcard form of an intrinsic is rejected with the same guidance
    // as the named form — only `import json` (namespace) is supported.
    let entry = entry_only("import json { * }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("destructuring import of `json` members is not available")),
        "expected the json-wildcard diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

// ── duplicate imports (same local name, any import forms) ───────────

#[test]
fn duplicate_namespace_import_is_rejected() {
    // `import a.user` + `import b.user` both bind the last segment `user`;
    // the second is rejected (first-import-wins) rather than silently
    // shadowing the first.
    let entry = entry_only("import a.user\nimport b.user\nfunction main() {}");
    let a_user = non_entry_dotted("a.user", "public function f() -> Int { 0 }", SourceId(1));
    let b_user = non_entry_dotted("b.user", "public function f() -> Int { 0 }", SourceId(2));
    let analysis = check_modules(&[entry, a_user, b_user]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`user` is already imported in this module")),
        "expected a duplicate-import diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn duplicate_named_import_is_rejected() {
    // `import a { foo }` + `import b { foo }` both bind `foo` — rejected.
    let entry = entry_only("import a { foo }\nimport b { foo }\nfunction main() {}");
    let a = non_entry("a", "public function foo() -> Int { 0 }", SourceId(1));
    let b = non_entry("b", "public function foo() -> Int { 0 }", SourceId(2));
    let analysis = check_modules(&[entry, a, b]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`foo` is already imported in this module")),
        "expected a duplicate-import diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn duplicate_import_across_kinds_is_rejected() {
    // A named import and a namespace import that bind the same local name
    // collide just like two of the same kind: the tracking is form-agnostic.
    let entry = entry_only("import a { foo }\nimport b as foo\nfunction main() {}");
    let a = non_entry("a", "public function foo() -> Int { 0 }", SourceId(1));
    let b = non_entry("b", "public function bar() -> Int { 0 }", SourceId(2));
    let analysis = check_modules(&[entry, a, b]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`foo` is already imported in this module")),
        "expected a cross-kind duplicate-import diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn duplicate_import_diagnostic_is_rich() {
    // The diagnostic carries a note (the first import's span) and a
    // suggestion (`as`), mirroring the private-import diagnostic shape.
    let entry = entry_only("import a { foo }\nimport b { foo }\nfunction main() {}");
    let a = non_entry("a", "public function foo() -> Int { 0 }", SourceId(1));
    let b = non_entry("b", "public function foo() -> Int { 0 }", SourceId(2));
    let analysis = check_modules(&[entry, a, b]);
    let dup = analysis
        .diagnostics
        .iter()
        .find(|d| {
            d.message
                .contains("`foo` is already imported in this module")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a duplicate-import diagnostic, got: {:?}",
                analysis.diagnostics
            )
        });
    assert!(
        !dup.notes.is_empty(),
        "expected a note pointing at the first import"
    );
    assert!(
        dup.suggestion.is_some(),
        "expected an `as`-rename suggestion"
    );
}

#[test]
fn namespace_call_diagnostic_uses_source_form() {
    // Diagnostics for a namespace call name the callee in source form
    // (`helpers.add`, honoring the alias) — the internal `::`-qualified
    // key (`lib::add`) must never leak into a user-facing message.
    let entry = entry_only("import lib as helpers\nfunction main() { helpers.add(1, 2, 3) }");
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    let arity = analysis
        .diagnostics
        .iter()
        .find(|d| d.message.contains("takes 2 argument(s), got 3"))
        .unwrap_or_else(|| {
            panic!(
                "expected an arity diagnostic, got: {:?}",
                analysis.diagnostics
            )
        });
    assert!(
        arity.message.contains("`helpers.add`"),
        "diagnostic should name the source form `helpers.add`, got: {:?}",
        arity.message
    );
    assert!(
        !arity.message.contains("::"),
        "diagnostic must not leak the internal `::`-qualified key, got: {:?}",
        arity.message
    );
}

#[test]
fn namespace_call_error_path_surfaces_arg_errors() {
    // An unknown-function namespace call still type-checks its arguments,
    // so an error *inside* an argument (here an undefined variable) is not
    // swallowed by the missing-callee diagnostic.
    let entry = entry_only("import lib\nfunction main() { lib.missing(nope) }");
    let other = non_entry(
        "lib",
        "public function add(a: Int, b: Int) -> Int { a + b }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("module `lib` has no function `missing`")),
        "expected the missing-function diagnostic, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("undefined variable `nope`")),
        "expected the nested undefined-variable diagnostic to survive, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn duplicate_wildcard_import_reports_once() {
    // Two wildcard imports of modules that share several public names
    // collide on every shared name, but the duplicate is reported once
    // per import statement — not once per colliding name.
    let entry = entry_only("import a { * }\nimport b { * }\nfunction main() {}");
    let a = non_entry(
        "a",
        "public function foo() -> Int { 0 }\n\
         public function bar() -> Int { 0 }\n\
         public function baz() -> Int { 0 }",
        SourceId(1),
    );
    let b = non_entry(
        "b",
        "public function foo() -> Int { 0 }\n\
         public function bar() -> Int { 0 }\n\
         public function baz() -> Int { 0 }",
        SourceId(2),
    );
    let analysis = check_modules(&[entry, a, b]);
    let dups = analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("is already imported in this module"))
        .count();
    assert_eq!(
        dups, 1,
        "expected exactly one duplicate-import diagnostic for the wildcard, got {}: {:?}",
        dups, analysis.diagnostics
    );
}

#[test]
fn aliasing_resolves_duplicate_namespace_import() {
    // The `as` escape hatch: rebinding the second import to a distinct
    // name clears the collision, and both namespaces stay callable.
    let entry = entry_only(
        "import a.user\nimport b.user as admin\n\
         function main() { let x: Int = user.f() let y: Int = admin.f() print(x) print(y) }",
    );
    let a_user = non_entry_dotted("a.user", "public function f() -> Int { 0 }", SourceId(1));
    let b_user = non_entry_dotted("b.user", "public function f() -> Int { 0 }", SourceId(2));
    let analysis = check_modules(&[entry, a_user, b_user]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics once aliased, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn intrinsic_json_cannot_be_shadowed_by_source_module() {
    // `import json` reserves the intrinsic namespace: even when a source
    // module literally named `json` is present, the import binds the
    // compiler intrinsic (resolver skips file resolution for it), not the
    // file. The source module's `encode` returns `Int`; the intrinsic's
    // returns `String`. Binding `let s: String = json.encode(5)` cleanly
    // proves the intrinsic won — if the source module had won, this would
    // be a `String`-vs-`Int` type error.
    let entry =
        entry_only("import json\nfunction main() { let s: String = json.encode(5) print(s) }");
    let json_module = non_entry(
        "json",
        "public function encode(x: Int) -> Int { x }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, json_module]);
    assert!(
        analysis.diagnostics.is_empty(),
        "intrinsic `json.encode` (-> String) must win over a same-named source \
         module's `encode` (-> Int), got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn duplicate_intrinsic_import_is_rejected() {
    // Two `import json` statements bind the same local name `json`; the
    // intrinsic form funnels through the same duplicate-import check as
    // user-module imports, so the second is rejected.
    let entry = entry_only("import json\nimport json\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("`json` is already imported in this module")),
        "expected a duplicate-import diagnostic for the repeated intrinsic, got: {:?}",
        analysis.diagnostics
    );
}
