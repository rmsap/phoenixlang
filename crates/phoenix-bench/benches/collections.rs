//! Collections benchmarks: hash-map operation throughput and `List.sortBy`
//! algorithmic shape.
//!
//! Two bench groups, intentionally co-located in one file:
//!
//! - `map` — `phx_map_get_raw` / `phx_map_set_raw` / `phx_map_remove_raw`
//!   at sizes 10 / 100 / 1k / 10k. Drives the runtime's open-addressing
//!   hash table directly via `phoenix_runtime::__test_support` rather
//!   than going through compiled Phoenix code, so the numbers reflect
//!   the data-structure cost without interpreter / codegen overhead.
//!   Reports criterion's mean / median / stddev. The `set` and `remove`
//!   numbers are operation cost *plus* amortized sweep cost for the
//!   unrooted output maps each call allocates — both scale linearly in
//!   `n`, so the flat-curve shape claim below is preserved.
//!   The bench's *shape claim*: get/set/remove curves stay flat across
//!   10 → 10k (within ~2×). A linear-scan regression would surface as
//!   1000× growth.
//!
//! - `sort_by` — `phoenix_common::algorithms::merge_sort_by` over
//!   `Vec<i64>` of 100 / 1k / 10k reverse-sorted elements. Both
//!   `phoenix-ir-interp` and `phoenix-interp` call this function
//!   directly; `phoenix-cranelift` emits an inline merge sort that
//!   mirrors the same algorithm. The bench's *shape claim*: 10× input
//!   grows runtime ~13–15× (the `n log n` ratio), not 100× (the `n²`
//!   ratio). End-to-end compile-and-run timing through the codegen
//!   path arrives in PR 3 of phase 2.7 and catches any codegen-side
//!   regression that wouldn't show up here.
//!
//! # Running
//!
//! ```sh
//! cargo bench -p phoenix-bench --bench collections
//! ```
//!
//! # Baseline
//!
//! Baseline numbers will be committed to
//! `docs/perf-baselines/collections.md` at phase-2 close (see
//! `docs/phases/phase-2.md` baseline-storage task).
//!
//! FIXME(phase-2.7-close): create `docs/perf-baselines/` and remove
//! this marker. Sibling marker in `allocation.rs` must be removed in
//! the same change — `grep -rn "phase-2.7-close"` finds both.

use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use phoenix_bench::{GcStateGuard, RootedFrameGuard};
use phoenix_common::algorithms::merge_sort_by;
use phoenix_runtime::__test_support::{
    phx_map_from_pairs, phx_map_get_raw, phx_map_remove_raw, phx_map_set_raw,
};
use phoenix_runtime::gc::{
    DEFAULT_COLLECTION_THRESHOLD, phx_gc_collect, phx_gc_disable, phx_gc_enable, phx_gc_push_frame,
    phx_gc_set_root, set_collection_threshold,
};

/// Map sizes sampled by the `map` bench group.
const MAP_SIZES: &[i64] = &[10, 100, 1_000, 10_000];

/// Sort sizes sampled by the `sort_by` bench group. Phase-2.md §2.7
/// pins these explicitly so a future contributor doesn't quietly drop
/// the 10k cell (the one that makes `n²` regressions impossible to
/// hide under a flat-curve fit).
const SORT_SIZES: &[usize] = &[100, 1_000, 10_000];

/// Key / value sizes for the `map` benches. Eight bytes each so the
/// benches exercise the hot integer-key, integer-value path without
/// dragging in fat-pointer (string) tracking, which has its own
/// rooting subtleties (see `map_methods.rs` Safety notes).
const KEY_SIZE: i64 = 8;
const VAL_SIZE: i64 = 8;

/// Build a `(key, value)` byte buffer suitable for `phx_map_from_pairs`.
/// Keys are `i64` `0..n`; values are `i64` `key * 2 + 1` so a key/value
/// transposition at the bench harness shows up immediately on read-back.
/// Returns the buffer rather than writing into a caller-supplied slice
/// to keep the bench code uncluttered — the buffer is small enough
/// (~160 KB at the largest scenario) that the allocation cost outside
/// the timed region is irrelevant.
fn build_pair_buffer(n: usize) -> Vec<u8> {
    let pair_size = (KEY_SIZE + VAL_SIZE) as usize;
    let mut buf = Vec::with_capacity(n * pair_size);
    for i in 0..n as i64 {
        buf.extend_from_slice(&i.to_le_bytes());
        buf.extend_from_slice(&((i * 2) + 1).to_le_bytes());
    }
    buf
}

fn bench_map_ops(c: &mut Criterion) {
    let _guard = GcStateGuard;
    set_collection_threshold(DEFAULT_COLLECTION_THRESHOLD);
    phx_gc_disable();
    phx_gc_collect();

    let mut group = c.benchmark_group("map");

    for &n in MAP_SIZES {
        // Build & root the source map outside the timed region. With
        // auto-collect disabled the `phx_map_from_pairs` allocation
        // cannot trigger a sweep that would miss the not-yet-rooted
        // map; we re-enable auto-collect only after the frame is set.
        let pairs = build_pair_buffer(n as usize);
        let map = unsafe { phx_map_from_pairs(KEY_SIZE, VAL_SIZE, n, pairs.as_ptr()) };
        let frame = phx_gc_push_frame(1);
        unsafe { phx_gc_set_root(frame, 0, map) };
        let _frame_guard = RootedFrameGuard::new(frame as *mut u8);

        // get: probe the median key — always present. No allocations
        // inside the timed region, so auto-collect stays off.
        let probe_key: i64 = n / 2;
        group.bench_with_input(BenchmarkId::new("get", n), &probe_key, |b, &k| {
            b.iter(|| {
                let r = unsafe {
                    phx_map_get_raw(
                        black_box(map),
                        black_box(&k as *const i64 as *const u8),
                        KEY_SIZE,
                    )
                };
                black_box(r);
            });
        });

        // set / remove allocate fresh maps per call (Phoenix maps are
        // immutable). With auto-collect off, criterion's tight iter
        // loop would grow the heap to several gigabytes for the 10k
        // scenario. Enable auto-collect with the standard 1 MiB
        // threshold so the heap stays bounded; the rooted source map
        // survives every sweep, the unrooted output maps don't.
        phx_gc_enable();

        let novel_key: i64 = n;
        let novel_val: i64 = -1;
        group.bench_with_input(
            BenchmarkId::new("set", n),
            &(novel_key, novel_val),
            |b, &(k, v)| {
                b.iter(|| {
                    let r = unsafe {
                        phx_map_set_raw(
                            black_box(map),
                            black_box(&k as *const i64 as *const u8),
                            black_box(&v as *const i64 as *const u8),
                            KEY_SIZE,
                            VAL_SIZE,
                        )
                    };
                    black_box(r);
                });
            },
        );

        let remove_key: i64 = n / 2;
        group.bench_with_input(BenchmarkId::new("remove", n), &remove_key, |b, &k| {
            b.iter(|| {
                let r = unsafe {
                    phx_map_remove_raw(
                        black_box(map),
                        black_box(&k as *const i64 as *const u8),
                        KEY_SIZE,
                    )
                };
                black_box(r);
            });
        });

        phx_gc_disable();
        // _frame_guard drops at end-of-iteration: pops the frame and
        // collects the now-unrooted map plus any output-map garbage.
    }

    group.finish();
}

/// Build worst-case-shaped input: integers in reverse-sorted order.
/// Merge sort is non-adaptive, so worst-case complexity equals
/// best-case complexity (always `n log n` by construction); reverse
/// order is chosen to maximize the number of element moves on each
/// merge pass. Bounded by `i64::MAX` for any practical bench size.
fn build_sort_input(n: usize) -> Vec<i64> {
    (0..n).rev().map(|i| i as i64).collect()
}

fn bench_sort_by(c: &mut Criterion) {
    let mut group = c.benchmark_group("sort_by");

    for &n in SORT_SIZES {
        let input = build_sort_input(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &input, |b, items| {
            // `iter_batched` pays the `Vec::clone` cost in the setup
            // phase, which criterion does not include in the measured
            // sample. The routine measures just the sort.
            //
            // `merge_sort_by` consumes its input by value; we hand it
            // the cloned vec each iteration so subsequent iterations
            // start from the same reverse-sorted state.
            b.iter_batched(
                || items.clone(),
                |v| {
                    // `(a > b) - (a < b)` rather than `a - b` so the
                    // comparator stays correct if SORT_SIZES grows or
                    // the bench is later reused with arbitrary i64
                    // inputs (subtraction would wrap for opposite-sign
                    // operands near `i64::MAX`).
                    let r: Result<Vec<i64>, std::convert::Infallible> =
                        merge_sort_by(black_box(v), |a, b| Ok((*a > *b) as i64 - (*a < *b) as i64));
                    black_box(r)
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

criterion_group!(map_ops, bench_map_ops);
criterion_group!(sort_by, bench_sort_by);
criterion_main!(map_ops, sort_by);
