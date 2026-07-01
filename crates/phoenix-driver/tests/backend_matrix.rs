//! Every runnable `tests/fixtures/*.phx`
//! must produce byte-identical stdout under the five execution modes
//! that share the same source: `phoenix run`, `phoenix run-ir`,
//! `phoenix build` (native), `phoenix build --target wasm32-linear`
//! executed under `wasmtime`, and `phoenix build --target wasm32-gc`
//! executed under `wasmtime -W gc=y`. A divergence here indicates a
//! real backend bug, not a test issue. (The filename is historical —
//! the matrix has grown two wasm columns since.)
//!
//! The wasm32-gc column is the Phase 2.4 PR 6 progress meter: each
//! `skip_wasm_gc:` annotation names the missing feature (closures /
//! maps / `toString` / dyn / K.1 field-restriction types), and each
//! slice's exit criterion includes deleting its annotations. Skips
//! were derived empirically (2026-06-11) by building every fixture
//! with `--target wasm32-gc`.
//!
//! The harness itself — process spawning, the `wasmtime` soft-skip
//! gate, temp-bin cleanup, the divergence message — lives in
//! [`common::backend_matrix`], shared with the multi-file
//! `multi_module_matrix.rs`. This file only supplies the single-file
//! [`MatrixCfg`] conventions and the fixture list.
//!
//! Both wasm columns are **soft-skipped** when `wasmtime` isn't
//! on `$PATH` (a visible warning is printed). `PHOENIX_REQUIRE_WASMTIME=1`
//! turns the skip into a hard failure — same gating shape as the
//! [`compile_wasm_linear.rs`] integration tests and §2.3's
//! `PHOENIX_REQUIRE_VALGRIND` gate. CI sets it (see ci.yml).
//!
//! Three fixture families are excluded: `gen_*.phx` (inputs to
//! `phoenix gen`, not worth exercising through the matrix), the
//! realistic schema library that `gen_schema_fixtures.rs` guards
//! instead, and `wasm_gc_*.phx` (single-backend smoke inputs for
//! `phoenix-cranelift`'s `compile_wasm_gc.rs`; what they exercise is
//! already covered here by ordinary fixtures on the wasm32-gc column).
//! `fixture_inventory.rs` asserts every fixture is claimed by *some*
//! suite, so a newcomer missing from every list fails there rather
//! than silently going unguarded.
//!
//! One `#[test]` per fixture, generated via `backend_matrix_test!`,
//! so a failure names the diverging fixture in `cargo test` output
//! without parsing assertion text.
//!
//! Scope: only stdout is compared. Stderr divergence and warning
//! output are intentionally out of scope for this gate — different
//! backends legitimately log different progress information. Don't
//! add stderr comparison here without first confirming the goal.

mod common;

use common::matrix_harness::MatrixCfg;

fn source_rel(fixture: &str) -> String {
    format!("tests/fixtures/{fixture}")
}

fn label(fixture: &str) -> String {
    fixture.to_string()
}

fn bin_stem(fixture: &str) -> String {
    format!("phoenix_matrix_{}", fixture.trim_end_matches(".phx"))
}

static CFG: MatrixCfg = MatrixCfg {
    source_rel,
    label,
    bin_stem,
    expected_rel: None,
};

macro_rules! backend_matrix_test {
    // All five columns.
    ($name:ident, $fixture:literal) => {
        #[test]
        fn $name() {
            common::matrix_harness::assert_backend_agreement(&CFG, $fixture, None);
        }
    };
    // Skip only the wasm32-gc column — for features wasm32-linear
    // already lowers but the PR 6 wasm32-gc slices haven't reached.
    // (A both-columns `skip_wasm:` arm can be re-added if a fixture
    // ever needs an op wasm32-linear doesn't lower; no fixture does
    // today — `dyn Trait`, the original carve-out, lowers there now.)
    ($name:ident, $fixture:literal, skip_wasm_gc: $reason:literal) => {
        #[test]
        fn $name() {
            common::matrix_harness::assert_backend_agreement(&CFG, $fixture, Some($reason));
        }
    };
}

backend_matrix_test!(matrix_hello, "hello.phx");
backend_matrix_test!(matrix_fibonacci, "fibonacci.phx");
backend_matrix_test!(matrix_fizzbuzz, "fizzbuzz.phx");
backend_matrix_test!(matrix_features, "features.phx");
backend_matrix_test!(matrix_generics, "generics.phx");
backend_matrix_test!(matrix_traits_static, "traits_static.phx");
backend_matrix_test!(matrix_traits_dyn, "traits_dyn.phx");
// Multi-method trait: reaches vtable slots beyond 0 and passes Int /
// String arguments through `dyn` dispatch — paths `traits_dyn.phx`
// (single self-only method) never exercises.
backend_matrix_test!(matrix_traits_dyn_multi, "traits_dyn_multi.phx");
// `dyn Trait` stored in a struct field: exercises the `DynRef`
// field-storage layout (size 8 / align 4) and the two-slot field
// load/store paths on wasm32-linear, plus offsets of the `Int`s
// bracketing the field — paths a `dyn` local/param never reaches. On
// wasm32-gc the `dyn` field is one `(ref null $dyn_Shape)` slot, lowered
// by the K.11 reference-typed-struct-fields work (reserve struct indices
// early, define bodies late) — the `dyn` ABI itself needed no change.
backend_matrix_test!(matrix_traits_dyn_field, "traits_dyn_field.phx");
// `dyn Trait` as a `List` element: exercises `DynRef` list-element
// storage (8-byte stride) and `List.sortBy` over a `dyn` element type,
// pinning that the sortBy GC key-frame tripwire admits `DynRef` (slot 1
// is a non-pointer vtable offset, so rooting slot 0 suffices).
backend_matrix_test!(matrix_traits_dyn_list, "traits_dyn_list.phx");
// `dyn Trait` method return types: a `Void`-returning method (the
// `Op::DynCall` `(None, Void)` arm) and a `List<Int>`-returning method
// (a single-slot ref result rooted by the post-`DynCall` blanket
// `emit_gc_set_root`) — return shapes the other dyn fixtures, all
// `String`/`Int`, never reach.
backend_matrix_test!(matrix_traits_dyn_ret, "traits_dyn_ret.phx");
// `dyn Trait` in function *return position*: `pick(...) -> dyn Shape`
// flattens its two-slot `(data_ptr, vtable_ptr)` fat pointer through the
// `call` signature's `[i32, i32]` result and the caller re-binds + roots
// it — a `DynRef` crossing a user-function boundary as a return value,
// which the other dyn fixtures (locals / params / fields / elements)
// never exercise.
backend_matrix_test!(matrix_traits_dyn_factory, "traits_dyn_factory.phx");
// Reference-typed struct fields (§Phase 2.4 K.11) — one fixture per
// field kind so a divergence names the kind. Each holds a managed
// reference (`(ref null $T)` on wasm32-gc, a heap pointer on the linear
// backends) whose target type is reserved before, and defined after,
// the surrounding enum / list / map / closure / struct types.
backend_matrix_test!(
    matrix_struct_nested_struct_field,
    "struct_nested_struct_field.phx"
);
backend_matrix_test!(matrix_struct_list_field, "struct_list_field.phx");
backend_matrix_test!(matrix_struct_enum_field, "struct_enum_field.phx");
backend_matrix_test!(matrix_struct_map_field, "struct_map_field.phx");
backend_matrix_test!(matrix_struct_closure_field, "struct_closure_field.phx");
// Self-referential struct (`Node { next: Option<Node> }`) — the genuine
// cycle the K.11 reserve/define split exists for: the struct field's
// target type (`$enum_Option_Node`) has a *higher* type index than the
// struct itself, a forward reference legal only inside the rec group.
backend_matrix_test!(matrix_struct_recursive_field, "struct_recursive_field.phx");
backend_matrix_test!(matrix_collections, "collections.phx");
// `ListBuilder<T>` / `MapBuilder<K,V>` (Phase 2.7 decision F) across all
// five backends — the gap the Phase 2.4 close-out bench refresh exposed
// (builders were compiled-backend-only; both interpreters and wasm32-gc
// `MapBuilder` were added here). Pins push/set loops, freeze, and the
// last-wins / first-position dedup. Small by design (interpreter
// `Map.get` is O(n)).
backend_matrix_test!(matrix_builders, "builders.phx");
// List query methods: `first`/`last`/`find` (→ `Option<T>`) and
// `any`/`all` (short-circuiting → `Bool`), with empty-list cases and
// printing predicates on `find`/`any`/`all` that pin identical
// short-circuit evaluation order across all five backends — each stops
// at a decisive element that is neither first nor last, so both
// "no short-circuit" and "stops too early" regressions surface in the
// printed prefix (wasm32-gc lowering added 2026-06-15).
backend_matrix_test!(matrix_list_query_methods, "list_query_methods.phx");
backend_matrix_test!(matrix_option_result, "option_result.phx");
// Explicit turbofish type args on a generic method (`b.wrap<Int>(42)`),
// threaded through monomorphization — two instantiations, both backends.
backend_matrix_test!(matrix_method_turbofish, "method_turbofish.phx");
// `json.encode` of scalars + a nested struct. The synthesized
// per-type encoders are ordinary IR, so the four backends agree
// byte-for-byte. wasm32-gc is skipped: its `phx_json_escape_str` port (a
// hand-written byte-loop string transform) lands in a follow-up slice.
backend_matrix_test!(
    matrix_json_encode_struct,
    "json_encode_struct.phx",
    skip_wasm_gc: "json.encode string escaping not yet ported to wasm32-gc (Phase 4.6 follow-up)"
);
// `json.encode` of `Option` (null / passthrough) and enums (adjacently
// tagged), plus a struct mixing both — exercises the discriminant-branch
// synthesis. wasm32-gc skipped (shares the escape-port deferral).
backend_matrix_test!(
    matrix_json_encode_enum,
    "json_encode_enum.phx",
    skip_wasm_gc: "json.encode string escaping not yet ported to wasm32-gc (Phase 4.6 follow-up)"
);
// `json.encode` of `List<T>` (array) and `Map<String, V>` (object),
// including empty collections and nesting — exercises the block-param
// accumulator loop synthesis. wasm32-gc skipped (escape-port deferral).
backend_matrix_test!(
    matrix_json_encode_collections,
    "json_encode_collections.phx",
    skip_wasm_gc: "json.encode string escaping not yet ported to wasm32-gc (Phase 4.6 follow-up)"
);
// The pre-registered builtin `JsonError` enum: usable with no
// import as a param type, constructed by bare variant name, matched on,
// and as the `Err` arm of `Result<T, JsonError>` (the `json.decode` shape).
backend_matrix_test!(matrix_json_error_enum, "json_error_enum.phx");
// The Option/Result combinators the original `option_result.phx` slice
// deferred on wasm32-gc — `mapErr` / `orElse` / `okOr` / `unwrapOrElse`
// / `ok` / `err`, each on both variants (lowering added 2026-06-15).
// Single `Result<Int,String>` instantiation by design (a second sibling
// would trip the orthogonal K.4 partial-generic enum-key limitation).
backend_matrix_test!(
    matrix_option_result_combinators,
    "option_result_combinators.phx"
);
// Expected-type inference for phantom-parameter enum constructors
// (§Phase 2.4 K.12): `Ok(x)` / `None` pinned from context at the implicit
// return / call-arg + nested-constructor / list-element / if-match-arm
// boundaries. Deliberately *two* `Result` instantiations, so an unpinned
// `Result<Int, __generic>` would be ambiguous on wasm32-gc — exactly the
// divergence the inference fix and the IR verifier invariant prevent.
backend_matrix_test!(
    matrix_partial_generic_enum_inference,
    "partial_generic_enum_inference.phx"
);
// The companion to the case above: collection literals annotated with a
// *type parameter* (`let xs: List<T> = []`) inside a generic function.
// The leaf-refinement guard leaves these unpinned (the annotation still
// carries a type var), deferring resolution to monomorphization — the
// empty container lowers to an inert `List<__generic>` the verifier
// tolerates. Each generic is instantiated at two concrete types, so a
// `__generic` that escaped mono would diverge on wasm32-gc. See K.12.
backend_matrix_test!(
    matrix_generic_annotated_empty_collections,
    "generic_annotated_empty_collections.phx"
);
backend_matrix_test!(matrix_defaults, "defaults.phx");
// The four stdlib-enum discriminant predicates (`Result.isOk`/`isErr`,
// `Option.isSome`/`isNone`) on both variants of each enum. Lowered on
// wasm32-gc by `option_result.rs::translate_is_variant` (`i32.eqz` for
// the positive variant, `i32.eq` against the discriminant for the
// negative), the same surface `option_result.phx` already exercises —
// so all five backends agree.
backend_matrix_test!(matrix_enum_predicates, "enum_predicates.phx");
backend_matrix_test!(matrix_closures, "closures.phx");
backend_matrix_test!(
    matrix_closures_ambiguous_captures,
    "closures_ambiguous_captures.phx"
);
backend_matrix_test!(matrix_closures_over_generic, "closures_over_generic.phx");
// GC stress fixtures.
//
// **Limit of this gate:** the matrix only checks stdout equality across
// its columns. A regression that swept `keep` (in `gc_keeps_alive`)
// or `acc` (in `gc_loop_carried_ref`) mid-loop would either crash the
// compiled binary or print garbage — caught here by exit status or an
// `unwrap` failure on `from_utf8`. But a regression that *didn't* crash
// and that produced the right number anyway (e.g. a sweep that left the
// payload bytes intact because the allocator hadn't reused them yet)
// would slip through.
//
// `alloc_loop.phx` has its own dedicated address-space-limited regression
// in `crates/phoenix-driver/tests/gc_bounded_memory.rs`; the other two
// rely on the matrix here. Future hardening: run the matrix under
// `MALLOC_PERTURB_=255` (Linux) or equivalent so use-after-free reads
// produce visibly wrong bytes; or build a sibling bounded-memory test
// per fixture. Tracked as a Phase 2.7 follow-up rather than a 2.3 gate
// because both options are infrastructure work, not GC correctness work.
backend_matrix_test!(matrix_alloc_loop, "alloc_loop.phx");
backend_matrix_test!(matrix_defer_basic, "defer_basic.phx");
backend_matrix_test!(matrix_defer_explicit_return, "defer_explicit_return.phx");
backend_matrix_test!(matrix_map_hash_many_keys, "map_hash_many_keys.phx");
backend_matrix_test!(matrix_map_duplicate_keys, "map_duplicate_keys.phx");
backend_matrix_test!(matrix_map_float_keys, "map_float_keys.phx");
backend_matrix_test!(matrix_map_bool_keys, "map_bool_keys.phx");
backend_matrix_test!(matrix_list_sortby_merge, "list_sortby_merge.phx");
backend_matrix_test!(
    matrix_list_sortby_alloc_comparator,
    "list_sortby_alloc_comparator.phx"
);
backend_matrix_test!(
    matrix_list_sortby_edge_lengths,
    "list_sortby_edge_lengths.phx"
);
backend_matrix_test!(matrix_list_sortby_strings, "list_sortby_strings.phx");
backend_matrix_test!(matrix_list_sortby_stable, "list_sortby_stable.phx");
backend_matrix_test!(matrix_defer_lazy_capture, "defer_lazy_capture.phx");
backend_matrix_test!(matrix_defer_method, "defer_method.phx");
backend_matrix_test!(matrix_defer_heap, "defer_heap.phx");
backend_matrix_test!(matrix_defer_closure, "defer_closure.phx");
backend_matrix_test!(matrix_defer_try, "defer_try.phx");
backend_matrix_test!(matrix_defer_multiple_returns, "defer_multiple_returns.phx");
backend_matrix_test!(
    matrix_defer_shadowed_at_return,
    "defer_shadowed_at_return.phx"
);
backend_matrix_test!(
    matrix_defer_nested_function_frames,
    "defer_nested_function_frames.phx"
);
backend_matrix_test!(matrix_gc_keeps_alive, "gc_keeps_alive.phx");
backend_matrix_test!(matrix_gc_loop_carried_ref, "gc_loop_carried_ref.phx", skip_wasm_gc: "output is correct but 50k growing-string concats take minutes under wasmtime's GC — host-VM GC throughput, not a codegen gap (verified 2026-06-11 at reduced iteration counts); loop-carried-ref rooting stays covered on wasm32-gc via gc_loop_carried_ref_small.phx");
// Reduced-iteration sibling (2000 concats, ~2 MB cumulative — still
// past the 1-MB auto-collect threshold) that runs all five columns.
// Unlike the feature skips above, the full fixture's wasm32-gc
// opt-out is a *performance* skip that no PR 6 slice will ever
// delete, so without this sibling the loop-carried-ref rooting path
// would stay permanently unexercised on wasm32-gc.
backend_matrix_test!(
    matrix_gc_loop_carried_ref_small,
    "gc_loop_carried_ref_small.phx"
);

// Closures returned from generic functions at *cross-width*
// instantiations (Int → 1 slot, String → 2-slot fat pointer). Now
// passes on every column except the skipped wasm32-gc one:
// monomorphization clones the inner
// closure function per enclosing-generic instantiation (following
// `Op::ClosureAlloc` edges), so each specialization has concrete
// `capture_types` / return type rather than the shared `__generic`
// placeholder the Cranelift backend used to mis-size. Expected output
// is `15\nhi:there\n`.
backend_matrix_test!(
    matrix_closures_over_generic_cross_width,
    "closures_over_generic_cross_width.phx"
);

/// The realistic schema library — inputs to `phoenix gen`/`phoenix check`,
/// not runnable stdout-producing programs, so they're outside the matrix.
/// Guarded by `gen_schema_fixtures.rs` instead (its `check_*` tests). Keep
/// in sync with that suite's fixture list.
const SCHEMA_LIBRARY: &[&str] = &[
    "payments.phx",
    "multitenant_saas.phx",
    "webhooks.phx",
    "file_storage.phx",
    "social.phx",
    "internal_admin.phx",
];

/// Tripwire: every `tests/fixtures/*.phx` outside the three excluded
/// families documented in the module header (`gen_*.phx`, the realistic
/// schema library guarded by `gen_schema_fixtures.rs`, and `wasm_gc_*.phx`)
/// must have a `backend_matrix_test!` entry above.
/// The registered set is checked by scanning this file's own source
/// for the quoted fixture name, so a fixture dropped into the
/// directory without an entry fails here instead of silently getting
/// zero matrix coverage.
#[test]
fn every_fixture_has_a_matrix_entry() {
    let src = include_str!("backend_matrix.rs");
    let dir = common::compiled_fixtures::workspace_root().join("tests/fixtures");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_file() {
            continue;
        }
        let name = entry.file_name().into_string().unwrap();
        if !name.ends_with(".phx")
            || name.starts_with("gen_")
            || name.starts_with("wasm_gc_")
            || SCHEMA_LIBRARY.contains(&name.as_str())
        {
            continue;
        }
        if !src.contains(&format!("\"{name}\"")) {
            missing.push(name);
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "fixtures without a backend_matrix_test! entry — add one, or extend the \
         excluded families documented in the module header: {missing:?}"
    );
}
