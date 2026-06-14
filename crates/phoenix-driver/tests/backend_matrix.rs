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
//! Two fixture families are excluded: `gen_*.phx` (inputs to
//! `phoenix gen`, not worth exercising through the matrix) and
//! `wasm_gc_*.phx` (single-backend smoke inputs for
//! `phoenix-cranelift`'s `compile_wasm_gc.rs`; what they exercise is
//! already covered here by ordinary fixtures on the wasm32-gc column).
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
backend_matrix_test!(matrix_traits_dyn, "traits_dyn.phx", skip_wasm_gc: "`dyn Trait` is not lowered on wasm32-gc yet");
// Multi-method trait: reaches vtable slots beyond 0 and passes Int /
// String arguments through `dyn` dispatch — paths `traits_dyn.phx`
// (single self-only method) never exercises.
backend_matrix_test!(matrix_traits_dyn_multi, "traits_dyn_multi.phx", skip_wasm_gc: "`dyn Trait` is not lowered on wasm32-gc yet");
// `dyn Trait` stored in a struct field: exercises the `DynRef`
// field-storage layout (size 8 / align 4) and the two-slot field
// load/store paths, plus offsets of the `Int`s bracketing the
// 8-byte fat pointer — paths a `dyn` local/param never reaches.
backend_matrix_test!(matrix_traits_dyn_field, "traits_dyn_field.phx", skip_wasm_gc: "`dyn Trait` is not lowered on wasm32-gc yet");
// `dyn Trait` as a `List` element: exercises `DynRef` list-element
// storage (8-byte stride) and `List.sortBy` over a `dyn` element type,
// pinning that the sortBy GC key-frame tripwire admits `DynRef` (slot 1
// is a non-pointer vtable offset, so rooting slot 0 suffices).
backend_matrix_test!(matrix_traits_dyn_list, "traits_dyn_list.phx", skip_wasm_gc: "`dyn Trait` is not lowered on wasm32-gc yet");
// `dyn Trait` method return types: a `Void`-returning method (the
// `Op::DynCall` `(None, Void)` arm) and a `List<Int>`-returning method
// (a single-slot ref result rooted by the post-`DynCall` blanket
// `emit_gc_set_root`) — return shapes the other dyn fixtures, all
// `String`/`Int`, never reach.
backend_matrix_test!(matrix_traits_dyn_ret, "traits_dyn_ret.phx", skip_wasm_gc: "`dyn Trait` is not lowered on wasm32-gc yet");
// `dyn Trait` in function *return position*: `pick(...) -> dyn Shape`
// flattens its two-slot `(data_ptr, vtable_ptr)` fat pointer through the
// `call` signature's `[i32, i32]` result and the caller re-binds + roots
// it — a `DynRef` crossing a user-function boundary as a return value,
// which the other dyn fixtures (locals / params / fields / elements)
// never exercise.
backend_matrix_test!(matrix_traits_dyn_factory, "traits_dyn_factory.phx", skip_wasm_gc: "`dyn Trait` is not lowered on wasm32-gc yet");
backend_matrix_test!(matrix_collections, "collections.phx", skip_wasm_gc: "closures (`ClosureRef`) are not lowered on wasm32-gc yet (K.8 pending)");
backend_matrix_test!(matrix_option_result, "option_result.phx");
backend_matrix_test!(matrix_defaults, "defaults.phx");
// The four stdlib-enum discriminant predicates (`Result.isOk`/`isErr`,
// `Option.isSome`/`isNone`) on both variants of each enum. Already
// gated per-backend in `compile_wasm_linear.rs`; the matrix entry adds
// the cross-backend agreement check on top.
backend_matrix_test!(matrix_enum_predicates, "enum_predicates.phx", skip_wasm_gc: "stdlib-enum predicate builtins (`Result.isOk` et al.) are not lowered on wasm32-gc yet (K.7 builtin surface)");
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
backend_matrix_test!(matrix_map_hash_many_keys, "map_hash_many_keys.phx", skip_wasm_gc: "maps (`Op::MapAlloc`) are not lowered on wasm32-gc yet");
backend_matrix_test!(matrix_list_sortby_merge, "list_sortby_merge.phx", skip_wasm_gc: "closures (`ClosureRef`) are not lowered on wasm32-gc yet (K.8 pending)");
backend_matrix_test!(matrix_list_sortby_alloc_comparator, "list_sortby_alloc_comparator.phx", skip_wasm_gc: "closures (`ClosureRef`) are not lowered on wasm32-gc yet (K.8 pending)");
backend_matrix_test!(matrix_list_sortby_edge_lengths, "list_sortby_edge_lengths.phx", skip_wasm_gc: "closures (`ClosureRef`) are not lowered on wasm32-gc yet (K.8 pending)");
backend_matrix_test!(matrix_list_sortby_strings, "list_sortby_strings.phx", skip_wasm_gc: "closures (`ClosureRef`) are not lowered on wasm32-gc yet (K.8 pending)");
backend_matrix_test!(matrix_list_sortby_stable, "list_sortby_stable.phx", skip_wasm_gc: "closures (`ClosureRef`) are not lowered on wasm32-gc yet (K.8 pending)");
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

/// Tripwire: every `tests/fixtures/*.phx` outside the two excluded
/// families documented in the module header (`gen_*.phx`,
/// `wasm_gc_*.phx`) must have a `backend_matrix_test!` entry above.
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
        if !name.ends_with(".phx") || name.starts_with("gen_") || name.starts_with("wasm_gc_") {
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
