//! Coherence, within-module duplicates, builtin reservation, orphan-FuncId consumption: `impl` on imported / unknown / builtin / multiply-foreign types are rejected; within-module duplicate decls are dropped; builtin names cannot be shadowed; rejected method-bearing decls' pre-allocated FuncIds are consumed via the orphan path.
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
fn within_module_duplicate_function_emits_one_diagnostic_no_body_cascade() {
    // Two `function helper` decls in the same module. Registration
    // emits exactly one "is already defined" diagnostic for the second
    // one; body-checking the second decl would re-resolve names against
    // the first's signature and could produce cascade noise — the
    // surviving-decl guard in `check_decl_bodies` prevents that.
    let entry = entry_only(
        "function helper() -> Int { 1 }\n\
         function helper() -> Int { 2 }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    let dup_count = analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("`helper` is already defined"))
        .count();
    assert_eq!(
        dup_count, 1,
        "expected exactly one already-defined diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn self_referential_type_alias_in_non_entry_module_is_diagnosed() {
    // The direct `type A = A` self-reference is caught by the
    // syntactic guard at the top of `register_type_alias`, regardless
    // of which module owns the alias. Pin that the non-entry path is
    // wired up the same way (no silent acceptance because the alias's
    // qualified key would be `lib::A`).
    let entry = entry_only("function main() {}");
    let other = non_entry("lib", "public type A = A", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("type alias `A` refers to itself")),
        "expected a self-reference diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

// NOTE: `type_alias_cycle_walk` (the transitive cycle detector) was
// migrated to go through `lookup_type_alias` so non-entry aliases are
// followable. We don't have a regression test for it because the path
// is hard to exercise: by the time `target` reaches the walk, alias
// chains have already been substituted away by `resolve_type_expr`,
// and any unresolved name aborts to `Type::Error` before the walk
// runs. Self-reference (`type A = A`) is the only practical cycle
// path today, and it's caught by the syntactic guard above.

#[test]
fn within_module_duplicate_struct_emits_diagnostic() {
    // Two `struct Foo` decls in the same module. The first survives;
    // the second is rejected with an "is already defined" diagnostic
    // and its registration is dropped (not silently overwriting).
    let entry = entry_only(
        "struct Foo { Int x }\n\
         struct Foo { String y }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("struct `Foo` is already defined")),
        "expected duplicate-struct diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // First decl survives intact: its single field is `x: Int`.
    let foo = analysis
        .module
        .struct_info_by_name("Foo")
        .expect("Foo should still be registered");
    assert_eq!(foo.fields.len(), 1);
    assert_eq!(foo.fields[0].name, "x");
}

#[test]
fn within_module_duplicate_enum_emits_diagnostic() {
    let entry = entry_only(
        "enum E { A }\n\
         enum E { B }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("enum `E` is already defined")),
        "expected duplicate-enum diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn within_module_duplicate_trait_emits_diagnostic() {
    let entry = entry_only(
        "trait T { function f(self) -> Int }\n\
         trait T { function g(self) -> Int }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("trait `T` is already defined")),
        "expected duplicate-trait diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn within_module_duplicate_type_alias_emits_diagnostic() {
    let entry = entry_only(
        "type A = Int\n\
         type A = String\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("type alias `A` is already defined")),
        "expected duplicate-type-alias diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn impl_block_on_imported_type_is_rejected() {
    // Phoenix coherence: an `impl` block must live in the same module
    // as the type it targets. Pinning this prevents the qualified-key
    // construction in `register_impl` from silently landing methods
    // under the wrong module's namespace.
    let entry = entry_only(
        "import lib { User }\n\
         impl User { public function shout(self) -> String { \"hi\" } }\n\
         function main() {}",
    );
    let other = non_entry(
        "lib",
        "public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("cannot implement methods on type `User`")
            && d.message.contains("`lib`")),
        "expected coherence diagnostic for impl on imported type, got: {:?}",
        analysis.diagnostics
    );
}

// ── Pre-allocated FuncId consumption for rejected decls ─────────────────
//
// Pre-pass B allocates a `FuncId` for every method on every struct/enum
// AST node, including duplicates. If the registration pass rejects the
// parent (within-module duplicate, coherence-violating impl), the
// dangling `FuncId`s used to crash `build_user_and_builtin_methods`
// with a "FuncId was pre-allocated but never registered" panic. The
// orphan-key registration path now consumes those ids; these tests
// pin that the rejection paths complete without panicking and surface
// only the intended diagnostic.

#[test]
fn within_module_duplicate_struct_with_inline_methods_no_panic() {
    // Without the orphan-FuncId fix, this input would panic in
    // `build_user_and_builtin_methods`. With the fix, the duplicate is
    // diagnosed and its method's FuncId is consumed via the orphan path.
    let entry = entry_only(
        "struct Foo { Int x }\n\
         struct Foo {\n\
         String y\n\
         function bar(self) -> Int { 1 }\n\
         }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    let dup_count = analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("struct `Foo` is already defined"))
        .count();
    assert_eq!(
        dup_count, 1,
        "expected exactly one already-defined diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // First decl survives intact: single field `x: Int`, no methods.
    let foo = analysis
        .module
        .struct_info_by_name("Foo")
        .expect("Foo should still be registered");
    assert_eq!(foo.fields.len(), 1);
    assert_eq!(foo.fields[0].name, "x");
    // The duplicate's `bar` must NOT be visible on the surviving Foo —
    // it was registered under an orphan key, not the surviving Foo's.
    assert!(
        analysis.module.method_info_by_name("Foo", "bar").is_none(),
        "duplicate's `bar` must not leak onto the surviving Foo"
    );
}

#[test]
fn within_module_duplicate_enum_with_inline_methods_no_panic() {
    let entry = entry_only(
        "enum E { A }\n\
         enum E {\n\
         B\n\
         function tag(self) -> Int { 0 }\n\
         }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("enum `E` is already defined")),
        "expected duplicate-enum diagnostic, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis.module.method_info_by_name("E", "tag").is_none(),
        "duplicate's `tag` must not leak onto the surviving E"
    );
}

#[test]
fn within_module_duplicate_struct_does_not_cascade_body_diagnostics() {
    // The duplicate's inline method body would type-check with bogus
    // resolution if the surviving-decl guard in `check_decl_bodies`
    // were missing. Pin that no body-level diagnostics reference the
    // duplicate's content.
    let entry = entry_only(
        "struct Foo { Int x }\n\
         struct Foo {\n\
         function bad(self) -> Int { undefined_name }\n\
         }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    // Exactly one duplicate-struct diagnostic.
    let dup_count = analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("struct `Foo` is already defined"))
        .count();
    assert_eq!(dup_count, 1);
    // No body-level "undefined" diagnostic for the duplicate — its
    // body was skipped by the surviving-decl guard.
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| !d.message.contains("undefined_name")),
        "duplicate struct's method body must not be type-checked, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn coherence_violating_impl_methods_do_not_pollute_target_methods_table() {
    // `impl User { ... }` in the entry module on an imported `User`
    // must register the rejected methods under an orphan key — not
    // under `lib::User`, and not under `entry::User` reachable through
    // any scope-aware lookup. Verify by importing User and confirming
    // the rejected `shout` is not callable.
    let entry = entry_only(
        "import lib { User }\n\
         impl User { public function shout(self) -> String { \"hi\" } }\n\
         function main() { let u: User = User(\"a\") print(u.shout()) }",
    );
    let other = non_entry(
        "lib",
        "public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    // Coherence diagnostic is present.
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("cannot implement methods on type `User`")),
        "expected coherence diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // The rejected `shout` must not be callable on User: a specific
    // "no method `shout` on type `User`" diagnostic fires at the call
    // site, proving the method was *not* leaked into the surviving
    // methods table.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("no method `shout`") && d.message.contains("type `User`")),
        "expected `no method shout on type User` diagnostic at call site, got: {:?}",
        analysis.diagnostics
    );
    // Belt-and-braces: the resolved methods table also has no `shout`
    // entry under `User`. (`method_info_by_name` covers user methods
    // first, then builtins; neither path should find the rejected one.)
    assert!(
        analysis
            .module
            .method_info_by_name("User", "shout")
            .is_none(),
        "rejected method must not be present in the resolved methods table"
    );
}

// ── Tie-break determinism for ambiguous variant ──────────────────────────

#[test]
fn user_function_named_print_does_not_collide_with_builtin_print() {
    // `print` is dispatched via a call-site shortcut in
    // `check_expr_call.rs`, not via the function table. So a user
    // `function print(...)` lands at functions["print"] in the entry
    // module without conflicting with any pre-existing key, and no
    // "already defined" diagnostic fires. This pins that today's
    // builtin-handling for callable names is via the shortcut, not
    // via the function table.
    let entry = entry_only("function print(x: Int) -> Int { x }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| !d.message.contains("`print` is already defined")),
        "user `print` must not collide with builtin handling: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn user_function_named_some_coexists_with_option_variant() {
    // `Some` is the variant constructor of the builtin `Option<T>`
    // enum — it lives in `self.enums["Option"].variants`, not in the
    // function table. So `register_function` for a user `function Some`
    // sees no existing key at `functions["Some"]` and registers
    // without a duplicate diagnostic. This pins the (somewhat
    // surprising) shape: the enum-variant namespace and the function
    // namespace don't intersect at registration time. Variant
    // resolution at call sites happens through
    // `check_enum_variant_constructor`, which is reached only when
    // the function-table lookup misses — so the user `Some` shadows
    // the variant constructor effectively.
    let entry = entry_only("function Some(x: Int) -> Int { x }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .all(|d| !d.message.contains("`Some` is already defined")),
        "user `function Some` must not collide on the function-table key: {:?}",
        analysis.diagnostics
    );
}

// ── Construction-side field visibility ───────────────────────────────────

#[test]
fn impl_on_undeclared_type_emits_unknown_type_diagnostic() {
    // `impl Bogus { ... }` where `Bogus` is declared nowhere (not in
    // current module, not foreign, not a builtin) used to be silently
    // accepted: classify_impl_target's foreign-module scan returned
    // false, register_impl proceeded and inserted into the methods
    // table, and the surviving-decl guard in check_decl_bodies then
    // skipped the body — total silence. Pin the diagnostic so the
    // bug doesn't recur.
    let entry = entry_only(
        "impl Bogus { function shout(self) -> String { \"hi\" } }\n\
         function main() {}",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unknown type `Bogus`")),
        "expected an unknown-type diagnostic for `impl Bogus`, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn entry_module_cannot_shadow_builtin_enum() {
    let entry = entry_only("enum Option { Yes No }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Option`") && d.message.contains("reserved builtin name")
        }),
        "expected reserved-builtin diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn non_entry_module_cannot_shadow_builtin_enum() {
    let entry = entry_only("function main() {}");
    let other = non_entry("lib", "public enum Option { Yes No }", SourceId(1));
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Option`") && d.message.contains("reserved builtin name")
        }),
        "expected reserved-builtin diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cannot_shadow_builtin_with_struct() {
    let entry = entry_only("struct Result { Int x }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Result`") && d.message.contains("reserved builtin name")
        }),
        "expected reserved-builtin diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cannot_shadow_builtin_with_trait() {
    let entry = entry_only("trait Option { function f(self) -> Int }\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Option`") && d.message.contains("reserved builtin name")
        }),
        "expected reserved-builtin diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn cannot_shadow_builtin_with_type_alias() {
    let entry = entry_only("type Result = Int\nfunction main() {}");
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Result`") && d.message.contains("reserved builtin name")
        }),
        "expected reserved-builtin diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn entry_shadow_attempt_does_not_corrupt_other_modules_view_of_builtin() {
    // Pinning the leak that motivated rejecting builtin shadowing:
    // even if the entry module *attempts* to redefine `Option`, a
    // non-entry module's `Option<Int>` reference must still resolve
    // to the builtin enum (with `Some`/`None` variants), not the
    // user's two-variant attempt.
    let entry = entry_only("enum Option { Yes No }\nfunction main() {}");
    let other = non_entry(
        "lib",
        "public function tryWrap(x: Int) -> Option<Int> { Some(x) }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    // Reservation diagnostic fires for the entry's attempt.
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Option`") && d.message.contains("reserved builtin name")
        }),
        "expected reserved-builtin diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // But the non-entry module's `Option<Int>` / `Some(x)` still
    // resolves: there is no diagnostic about `Some` being undefined or
    // a wrong arity for `Option`. (If the entry's `enum Option { Yes
    // No }` had clobbered the global slot, `Some(x)` would fail to
    // resolve as a variant constructor.)
    assert!(
        !analysis.diagnostics.iter().any(|d| {
            d.message.contains("undefined")
                && (d.message.contains("Some") || d.message.contains("Option"))
        }),
        "builtin Option must remain reachable from non-entry module despite entry's attempt: {:?}",
        analysis.diagnostics
    );
}

// ── Coherence: builtin types cannot be the receiver of `impl` ───────────

#[test]
fn cannot_impl_methods_on_builtin_enum_in_entry_module() {
    // `module_qualify(entry, "Option") == "Option"` collides with the
    // builtin's qualified key. Without an `is_builtin_name` guard at
    // the top of `register_impl`, the receiver-type lookup would hit
    // the builtin's slot and classify as `Local`, polluting the
    // builtin's methods table. Pin that the up-front guard rejects
    // this and the user-declared method does not become callable on
    // any `Option<T>` instance.
    let entry = entry_only(
        "function main() {\n\
           let o: Option<Int> = Some(42)\n\
           print(o.foo())\n\
         }\n\
         impl Option { function foo(self) -> Int { 0 } }",
    );
    let analysis = check_modules(&[entry]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Option`")
                && d.message.contains("builtin")
                && d.message.contains("reserved")
        }),
        "expected `cannot implement methods on builtin type Option` diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // The user's `foo` must not be callable on Option<T>: a specific
    // "no method `foo` on type `Option<Int>`" diagnostic fires.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("no method `foo`")),
        "rejected method must not silently become callable, got: {:?}",
        analysis.diagnostics
    );
    // And the resolved methods table has no user-defined `foo` on Option.
    assert!(
        analysis
            .module
            .method_info_by_name("Option", "foo")
            .is_none(),
        "rejected method must not be present in the resolved methods table"
    );
}

#[test]
fn cannot_impl_methods_on_builtin_enum_in_non_entry_module() {
    // Non-entry counterpart: `lib`'s `impl Option` qualifies to
    // `lib::Option` which is not in the tables, so without the
    // builtin-name guard this would fall through to `Unknown` and
    // emit the misleading "unknown type Option" diagnostic. With
    // the guard, both entry and non-entry produce the same clear
    // "cannot implement methods on builtin type" message.
    let entry = entry_only("function main() {}");
    let other = non_entry(
        "lib",
        "impl Option { function foo(self) -> Int { 0 } }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    assert!(
        analysis.diagnostics.iter().any(|d| {
            d.message.contains("`Option`")
                && d.message.contains("builtin")
                && d.message.contains("reserved")
        }),
        "expected `cannot implement methods on builtin type Option` diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // Must not also surface the misleading `unknown type` fallback.
    assert!(
        !analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unknown type `Option`")),
        "should not emit a misleading `unknown type` diagnostic, got: {:?}",
        analysis.diagnostics
    );
}

// ── Coherence: unknown trait routes through orphan path ─────────────────

#[test]
fn impl_unknown_trait_for_local_type_routes_through_orphan_path() {
    // `impl UnknownTrait for LocalType` — the type is local but the
    // trait is undeclared. Before the fix, the impl's methods landed
    // in `self.methods[LocalType]` as inherent methods *despite* the
    // unknown-trait diagnostic, because the trait-existence check ran
    // after the methods were inserted. Pin the fix: the rejected
    // method is not silently callable as inherent.
    let entry = entry_only(
        "struct Foo { Int x }\n\
         impl Bogus for Foo { function bogus(self) -> Int { 0 } }\n\
         function main() { let f: Foo = Foo(1) print(f.bogus()) }",
    );
    let analysis = check_modules(&[entry]);
    // Unknown-trait diagnostic fires.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unknown trait `Bogus`")),
        "expected unknown-trait diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // The rejected `bogus` method must not silently become callable.
    // A specific "no method `bogus` on type `Foo`" diagnostic fires at
    // the call site, proving the method was *not* inserted as an
    // inherent method on Foo.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("no method `bogus`") && d.message.contains("type `Foo`")),
        "expected `no method bogus on type Foo` diagnostic at call site, got: {:?}",
        analysis.diagnostics
    );
    // Belt-and-braces: the resolved methods table also has no `bogus`
    // entry under `Foo`.
    assert!(
        analysis
            .module
            .method_info_by_name("Foo", "bogus")
            .is_none(),
        "rejected method must not be present in the resolved methods table"
    );
}

#[test]
fn coherence_violating_trait_impl_for_imported_type_routes_through_orphan_path() {
    // Sibling of `coherence_violating_impl_methods_do_not_pollute_target_methods_table`:
    // verify that a *trait*-impl coherence violation also goes through
    // the orphan path so its method's pre-allocated FuncId is consumed
    // (otherwise build_user_and_builtin_methods would panic). The
    // coherence diagnostic still fires; the rejected method must not
    // be silently callable.
    let entry = entry_only(
        "import lib { Display, User }\n\
         impl Display for User { function show(self) -> String { \"hi\" } }\n\
         function main() { let u: User = User(\"a\") print(u.show()) }",
    );
    let other = non_entry(
        "lib",
        "public trait Display { function show(self) -> String }\n\
         public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    // Coherence diagnostic fires.
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("cannot implement methods on type `User`")),
        "expected coherence diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // The rejected `show` must not silently become callable on User —
    // pin via a specific "no method" diagnostic at the call site.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("no method `show`") && d.message.contains("type `User`")),
        "expected `no method show on type User` diagnostic, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis
            .module
            .method_info_by_name("User", "show")
            .is_none(),
        "rejected trait-impl method must not be present in the resolved methods table"
    );
}

// ── Cross-module method call on a generic receiver ──────────────────────

#[test]
fn within_module_duplicate_and_coherence_violation_in_same_program_no_panic() {
    // Combined input: entry has both a within-module duplicate
    // struct (whose duplicate decl carries an inline method) *and* a
    // coherence-violating impl on an imported type. Both rejection
    // paths route their pre-allocated FuncIds through the orphan
    // path; the combined program exercises the count-agreement
    // invariant `placeholder_fills == orphan_method_count` between
    // sema and IR. Without the fix, IR's debug_assert would fire on
    // a count mismatch.
    let entry = entry_only(
        "import lib { User }\n\
         struct Foo { Int x }\n\
         struct Foo {\n\
         String y\n\
         function dup_method(self) -> Int { 1 }\n\
         }\n\
         impl User { public function shout(self) -> String { \"hi\" } }\n\
         function main() {}",
    );
    let other = non_entry(
        "lib",
        "public struct User { public String name }",
        SourceId(1),
    );
    let analysis = check_modules(&[entry, other]);
    // Both diagnostics fire.
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("struct `Foo` is already defined")),
        "expected duplicate-struct diagnostic, got: {:?}",
        analysis.diagnostics
    );
    assert!(
        analysis.diagnostics.iter().any(|d| d
            .message
            .contains("cannot implement methods on type `User`")),
        "expected coherence diagnostic, got: {:?}",
        analysis.diagnostics
    );
    // Both rejected methods must surface as orphan-fill slots — pin
    // via `orphan_method_count >= 2` so a regression that bypasses
    // either rejection path's orphan call would surface here.
    assert!(
        analysis.module.orphan_method_count >= 2,
        "expected orphan_method_count >= 2 (one duplicate's `dup_method`, one coherence-violating `shout`), got: {}",
        analysis.module.orphan_method_count
    );
}

#[test]
fn impl_on_type_declared_in_multiple_foreign_modules_lists_all_candidates() {
    // Two foreign modules each declare `User`, neither is imported by
    // entry. An `impl User` in entry must be rejected for coherence,
    // and the diagnostic must list *both* modules deterministically
    // (sorted by dotted path) so the user can disambiguate.
    let entry = entry_only(
        "impl User { function shout(self) -> String { \"hi\" } }\n\
         function main() {}",
    );
    let module_a = non_entry(
        "alpha",
        "public struct User { public String name }",
        SourceId(1),
    );
    let module_b = non_entry("beta", "public struct User { public Int id }", SourceId(2));
    let analysis = check_modules(&[entry, module_a, module_b]);
    let diag = analysis
        .diagnostics
        .iter()
        .find(|d| {
            d.message
                .contains("cannot implement methods on type `User`")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected coherence diagnostic for impl on ambiguous foreign type, got: {:?}",
                analysis.diagnostics
            )
        });
    // Both candidates listed; deterministic alphabetical order.
    assert!(
        diag.message.contains("`alpha`") && diag.message.contains("`beta`"),
        "diagnostic must list both candidate modules, got: {}",
        diag.message
    );
    let alpha_pos = diag.message.find("`alpha`").unwrap();
    let beta_pos = diag.message.find("`beta`").unwrap();
    assert!(
        alpha_pos < beta_pos,
        "candidate modules should appear in sorted order, got: {}",
        diag.message
    );
}
