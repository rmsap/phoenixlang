//! Every runnable `tests/fixtures/*.phx`
//! must produce byte-identical stdout under the four execution modes
//! that share the same source: `phoenix run`, `phoenix run-ir`,
//! `phoenix build` (native), and `phoenix build --target wasm32-linear`
//! executed under `wasmtime`. A divergence here indicates a real backend
//! bug, not a test issue.
//!
//! The harness itself — process spawning, the `wasmtime` soft-skip
//! gate, temp-bin cleanup, the divergence message — lives in
//! [`common::backend_matrix`], shared with the multi-file
//! `multi_module_matrix.rs`. This file only supplies the single-file
//! [`MatrixCfg`] conventions and the fixture list.
//!
//! The `wasm32-linear` column is **soft-skipped** when `wasmtime` isn't
//! on `$PATH` (a visible warning is printed). `PHOENIX_REQUIRE_WASMTIME=1`
//! turns the skip into a hard failure — same gating shape as the
//! [`compile_wasm_linear.rs`] integration tests and §2.3's
//! `PHOENIX_REQUIRE_VALGRIND` gate. CI sets it (see ci.yml).
//!
//! `gen_*.phx` fixtures are excluded — they exist as inputs to
//! `phoenix gen` and aren't worth exercising through the matrix.
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

use common::backend_matrix::MatrixCfg;

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
    ($name:ident, $fixture:literal) => {
        #[test]
        fn $name() {
            common::backend_matrix::assert_backend_agreement(&CFG, $fixture);
        }
    };
    ($name:ident, $fixture:literal, skip_wasm: $reason:literal) => {
        #[test]
        fn $name() {
            common::backend_matrix::assert_backend_agreement_skip_wasm(&CFG, $fixture, $reason);
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
// load/store paths, plus offsets of the `Int`s bracketing the
// 8-byte fat pointer — paths a `dyn` local/param never reaches.
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
backend_matrix_test!(matrix_collections, "collections.phx");
backend_matrix_test!(matrix_option_result, "option_result.phx");
backend_matrix_test!(matrix_defaults, "defaults.phx");
backend_matrix_test!(matrix_closures, "closures.phx");
backend_matrix_test!(
    matrix_closures_ambiguous_captures,
    "closures_ambiguous_captures.phx"
);
backend_matrix_test!(matrix_closures_over_generic, "closures_over_generic.phx");
// GC stress fixtures.
//
// **Limit of this gate:** the matrix only checks stdout equality across
// the three backends. A regression that swept `keep` (in `gc_keeps_alive`)
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
backend_matrix_test!(matrix_gc_loop_carried_ref, "gc_loop_carried_ref.phx");

// Closures returned from generic functions at *cross-width*
// instantiations (Int → 1 slot, String → 2-slot fat pointer). Now
// passes on all three backends: monomorphization clones the inner
// closure function per enclosing-generic instantiation (following
// `Op::ClosureAlloc` edges), so each specialization has concrete
// `capture_types` / return type rather than the shared `__generic`
// placeholder the Cranelift backend used to mis-size. Expected output
// is `15\nhi:there\n`.
backend_matrix_test!(
    matrix_closures_over_generic_cross_width,
    "closures_over_generic_cross_width.phx"
);
