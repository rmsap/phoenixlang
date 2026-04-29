//! Cross-module callable surfaces: methods, trait bounds, enum variants, generic receivers; imported types in registration-time signature positions; known limitations (imported type aliases, imported `dyn Trait`).
//!
//! Sibling of `check_modules_foundation.rs`, `check_modules_imports.rs`,
//! `check_modules_callable.rs`, `check_modules_coherence.rs` — split out from
//! a single 1969-line `check_modules.rs` so each topical group lives in
//! its own test binary.

mod common;

use common::{entry_only, non_entry};
use phoenix_common::span::SourceId;
use phoenix_sema::checker::check_modules;

#[test]
fn cross_module_method_call_via_import_resolves() {
    let entry = entry_only(
        "import lib { Greeter }\n\
         function main() { let g: Greeter = Greeter() print(g.greet()) }",
    );
    let other = non_entry(
        "lib",
        "public struct Greeter {}\n\
         impl Greeter { function greet(self) -> String { \"hi\" } }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "cross-module method call should resolve, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_method_call_in_owning_module_resolves() {
    // Pin the bug fix that motivated `lookup_methods`: a non-entry
    // module's *own* method-call site needs the receiver type-name
    // qualified to find the methods table entry. With the old bare-name
    // probe this regressed to "no method on type" inside `lib`.
    let entry = entry_only("function main() {}");
    let other = non_entry(
        "lib",
        "public struct Greeter {}\n\
         impl Greeter { function greet(self) -> String { \"hi\" } }\n\
         public function shout() -> String { let g: Greeter = Greeter() g.greet() }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "non-entry-module own method call should resolve, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_trait_bound_satisfied_via_import() {
    let entry = entry_only(
        "import lib { Display, Greeter }\n\
         function show<T: Display>(item: T) -> String { item.toString() }\n\
         function main() { let g: Greeter = Greeter() print(show(g)) }",
    );
    let other = non_entry(
        "lib",
        "public trait Display { function toString(self) -> String }\n\
         public struct Greeter {}\n\
         impl Display for Greeter { function toString(self) -> String { \"greet\" } }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "trait bound on imported types should be satisfied, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_trait_bound_unsatisfied_diagnoses() {
    let entry = entry_only(
        "import lib { Display, Empty }\n\
         function show<T: Display>(item: T) -> String { item.toString() }\n\
         function main() { let e: Empty = Empty() print(show(e)) }",
    );
    let other = non_entry(
        "lib",
        "public trait Display { function toString(self) -> String }\n\
         public struct Empty {}",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("does not implement trait `Display`")),
        "expected unsatisfied-trait-bound diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cross_module_enum_variant_constructor_resolves_via_import() {
    let entry = entry_only(
        "import palette { Color }\n\
         function main() { let c: Color = Red() print(c) }",
    );
    let other = non_entry(
        "palette",
        "public enum Color { Red Green Blue }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "imported enum's variant constructor should resolve, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn unimported_enum_variant_does_not_resolve() {
    let entry = entry_only("function main() { let c: Color = Red() print(c) }");
    let other = non_entry(
        "palette",
        "public enum Color { Red Green Blue }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    // Without `import palette { Color }`, neither `Color` (the type
    // annotation) nor `Red()` (the variant constructor) should resolve
    // — the enum is not visible in entry's scope.
    //
    // For `Red()`: anchor the assertion on the *callsite name* + a
    // negative ("undefined" / "unknown" / "not declared") so the test
    // keeps passing if today's "undefined type or variant" wording
    // ever specializes to either "undefined variant" or "unknown
    // type" alone — the meaning ("Red doesn't resolve from here") is
    // what we're pinning, not the joint phrasing.
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Red`")
                && (d.message.contains("undefined")
                    || d.message.contains("unknown")
                    || d.message.contains("not declared"))
        }),
        "expected a diagnostic indicating `Red` does not resolve, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unknown type `Color`")),
        "expected an unknown-type diagnostic for `Color`, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn ambiguous_variant_across_imported_enums_diagnoses() {
    // Two imported enums both have a `Red` variant. The variant
    // constructor `Red()` is ambiguous and `lookup_visible_enum_variant`
    // should emit a diagnostic listing both candidates.
    let entry = entry_only(
        "import a { Color }\n\
         import b { Tint }\n\
         function main() { let c: Color = Red() print(c) }",
    );
    let module_a = non_entry("a", "public enum Color { Red Green }", SourceId(1));
    let module_b = non_entry("b", "public enum Tint { Red Blue }", SourceId(2));
    let analysis = check_modules(&[entry, module_a, module_b]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("variant `Red` is ambiguous")
                && d.message.contains("Color")
                && d.message.contains("Tint")),
        "expected ambiguous-variant diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn ambiguous_variant_resolves_to_alphabetically_first_local_name() {
    // Both `a::Color` and `b::Tint` define `Red`. The resolution
    // function picks the alphabetically-first *local* name (the alias
    // visible in scope). Here `Color` < `Tint`, so the variant
    // constructor's resulting type binding is for `Color`'s shape.
    // Pin via a downstream type-check effect: assigning the result to
    // `Color` should succeed (no type-mismatch diagnostic), proving
    // the tie-break picked `Color` and not `Tint`.
    let entry = entry_only(
        "import a { Color }\n\
         import b { Tint }\n\
         function main() { let c: Color = Red() print(c) }",
    );
    let module_a = non_entry("a", "public enum Color { Red Green }", SourceId(1));
    let module_b = non_entry("b", "public enum Tint { Red Blue }", SourceId(2));
    let analysis = check_modules(&[entry, module_a, module_b]);
    // Ambiguity diagnostic is present.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("variant `Red` is ambiguous")),
        "expected ambiguity diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // Type-mismatch diagnostic *must not* fire for `let c: Color = Red()`,
    // because the tie-break picked `Color`. If the tie-break flipped
    // to `Tint`, we would expect a type-mismatch diagnostic comparing
    // `Tint` and `Color`.
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| !(d.message.contains("Tint")
                && d.message.contains("Color")
                && d.message.contains("expected"))),
        "tie-break should pick `Color` (alphabetically first), got: {:?}",
        analysis.diagnostics
    );
}

// ── Builtin shadowing in entry module ────────────────────────────────────

#[test]
fn calling_a_struct_name_does_not_resolve_as_an_enum_variant() {
    // `Foo()` where `Foo` is a struct (not a variant) must take the
    // struct-literal path, not the enum-variant lookup path. Pin the
    // contract: `lookup_visible_enum_variant` returns None for a name
    // that's in scope as something else, and `check_struct_literal`
    // handles construction without an "ambiguous variant"/"unknown
    // variant" diagnostic.
    let entry = entry_only(
        "struct Foo { Int x }\n\
         function main() { let f: Foo = Foo(42) print(f.x) }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.is_empty(),
        "struct construction via `Foo()` must not be misrouted through enum-variant lookup, got: {:?}",
        analysis.diagnostics
    );
}

// ── Single-file `import` is diagnosed ────────────────────────────────────

#[test]
fn same_enum_imported_under_two_aliases_is_not_ambiguous() {
    // Importing the same enum twice (once unaliased, once aliased)
    // both map to the same qualified key (`palette::Color`).
    // collect_visible_enum_variant_matches dedupes by qualified key so
    // a single underlying enum yields a single match — `Red()` is
    // unambiguous and no ambiguity diagnostic should fire.
    let entry = entry_only(
        "import palette { Color }\n\
         import palette { Color as Hue }\n\
         function main() { let c: Color = Red() print(c) }",
    );
    let other = non_entry(
        "palette",
        "public enum Color { Red Green Blue }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("variant `Red` is ambiguous")),
        "same enum under two aliases must not be ambiguous, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis.diagnostics.is_empty(),
        "no other diagnostics expected, got: {:?}",
        analysis.diagnostics
    );
}

// ── Builtin name reservation ─────────────────────────────────────────────
//
// Builtin names (`Option`, `Result`, …) are reserved in every module —
// entry and non-entry alike. Letting the entry module shadow a builtin
// would mutate the global symbol-table slot the builtin lives in, and
// every other module's scope would silently resolve the bare name to
// the user's replacement. Allowing it in non-entry would be misleading
// even though the qualified keys don't collide. So: reject every kind
// of decl whose name matches a builtin, with a clear message.

#[test]
fn cross_module_method_call_on_generic_receiver_resolves_with_bindings() {
    // `Container<T>` is declared in `lib` with an inherent method that
    // returns `T`. The entry imports it, builds `Container<Int>`, and
    // calls the method. Exercises `lookup_methods` *plus* the type-
    // parameter-bindings merge for a cross-module receiver — the most
    // likely place for a regression once a future change touches
    // either path.
    let entry = entry_only(
        "import lib { Container }\n\
         function main() { let c: Container<Int> = Container(7) let v: Int = c.get() print(v) }",
    );
    let other = non_entry(
        "lib",
        "public struct Container<T> { public T value }\n\
         impl Container { function get(self) -> T { self.value } }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "cross-module generic method call should resolve with bindings, got: {:?}",
        analysis.diagnostics
    );
}

// ── Imported types in registration-time positions ────────────────────────
//
// Phase B of module-scope construction runs *before* the registration
// pass, so `resolve_type_expr` (called from each `register_*` while
// resolving parameter / return / field types) sees imported names.
// These tests pin that imported struct/enum types work as parameter and
// return types in the importer's function signatures *without*
// requiring a local type alias as a workaround.

#[test]
fn imported_struct_works_in_function_signature() {
    let entry = entry_only(
        "import lib { User }\n\
         function process(u: User) -> User { u }\n\
         function main() { let u: User = User(\"a\") let v: User = process(u) print(v.name) }",
    );
    let other = non_entry(
        "lib",
        "public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "imported struct should resolve in function signature, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn imported_enum_works_in_function_signature() {
    let entry = entry_only(
        "import palette { Color }\n\
         function render(c: Color) -> Color { c }\n\
         function main() { let c: Color = Red() let r: Color = render(c) print(r) }",
    );
    let other = non_entry(
        "palette",
        "public enum Color { Red Green Blue }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "imported enum should resolve in function signature, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn imported_generic_struct_works_in_function_signature() {
    let entry = entry_only(
        "import lib { Container }\n\
         function unwrap(c: Container<Int>) -> Int { c.get() }\n\
         function main() { let c: Container<Int> = Container(7) print(unwrap(c)) }",
    );
    let other = non_entry(
        "lib",
        "public struct Container<T> { public T value }\n\
         impl Container { function get(self) -> T { self.value } }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "imported generic struct should resolve in function signature, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn imported_struct_works_via_alias_in_signature() {
    let entry = entry_only(
        "import lib { User as Account }\n\
         function process(a: Account) -> Account { a }\n\
         function main() { let a: Account = Account(\"x\") print(process(a).name) }",
    );
    let other = non_entry(
        "lib",
        "public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "imported struct under alias should resolve in function signature, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn imported_struct_used_as_struct_field_type_resolves() {
    let entry = entry_only(
        "import lib { Inner }\n\
         struct Outer { Inner inner }\n\
         function main() { let i: Inner = Inner(1) let o: Outer = Outer(i) print(o.inner.x) }",
    );
    let other = non_entry("lib", "public struct Inner { public Int x }", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "imported struct should resolve as a field type in another struct, got: {:?}",
        analysis.diagnostics
    );
}

// ── Known limitations: imported type aliases / `dyn` traits ─────────────
//
// Pre-registration is struct/enum-only. Imported type aliases used at
// registration time can't be resolved (their `target` would need an
// already-resolved `Type`); imported `dyn ImportedTrait` would need a
// pre-registered trait whose `object_safety_error` has been computed
// before any use site. Both fall back to the same "unknown type" /
// "unknown trait" diagnostic that fires for same-module forward
// references. These tests pin that fall-back so future work that
// removes the limitation is detectable.

#[test]
fn imported_type_alias_in_function_signature_is_a_known_limitation() {
    let entry = entry_only(
        "import lib { UserId }\n\
         function process(id: UserId) -> UserId { id }\n\
         function main() {}",
    );
    let other = non_entry("lib", "public type UserId = Int", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    // Today this fails with "unknown type `UserId`". When a topological
    // type-alias pre-resolution pass lands, this test should flip to
    // expecting clean diagnostics — at which point the limitation
    // comments in `pre_register_type_names` should be revisited.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unknown type `UserId`")),
        "imported type alias in signature is a known limitation; expected fall-through \
         `unknown type` diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn imported_dyn_trait_in_function_signature_is_a_known_limitation() {
    let entry = entry_only(
        "import lib { Display }\n\
         function process(d: dyn Display) -> String { d.show() }\n\
         function main() {}",
    );
    let other = non_entry(
        "lib",
        "public trait Display { function show(self) -> String }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unknown trait `Display`")),
        "imported `dyn ImportedTrait` is a known limitation; expected `unknown trait` \
         diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

// ── find_foreign_definition_modules: deterministic ambiguity ────────────

#[test]
fn non_entry_user_method_lookup_uses_qualified_key() {
    // `method_index` (and therefore `method_info_by_name`) is keyed by
    // the receiver type's *qualified* name for non-entry types. Pin the
    // contract: a method on `lib::Greeter` is reachable as
    // `method_info_by_name("lib::Greeter", "greet")`, and the bare-name
    // probe `method_info_by_name("Greeter", "greet")` does *not* hit a
    // user method (only built-ins are reachable by bare name).
    let entry = entry_only(
        "import lib { Greeter }\n\
         function main() { let g: Greeter = Greeter() print(g.greet()) }",
    );
    let other = non_entry(
        "lib",
        "public struct Greeter {}\n\
         impl Greeter { function greet(self) -> String { \"hi\" } }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.is_empty(),
        "expected no diagnostics, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis
            .module
            .method_info_by_name("lib::Greeter", "greet")
            .is_some(),
        "method on non-entry user type should be reachable via qualified-key lookup"
    );
    assert!(
        analysis
            .module
            .method_info_by_name("Greeter", "greet")
            .is_none(),
        "bare-name lookup must not resolve a non-entry user method (only builtins are bare-keyed)"
    );
}
