//! Allocation-throughput and GC pause-distribution benchmarks.
//!
//! Two bench groups, intentionally co-located in one file:
//!
//! - `alloc_throughput` — `phx_gc_alloc(size, tag)` for `size ∈ {16, 64,
//!   256, 1024}` × `tag ∈ {Unknown, String}`. Auto-collect is enabled
//!   with a small threshold so the loop drives steady-state grow→sweep
//!   cycles, not first-allocation cost. The bench keeps **no rooted
//!   set**, so each triggered sweep is the empty-heap floor — the
//!   reported number is alloc-path cost + amortized empty-sweep cost,
//!   not "many live objects" GC pressure (the `gc_pause` group below
//!   covers that). Reports criterion's mean / median / stddev.
//!
//! - `gc_pause` — forces `phx_gc_collect()` against a stable rooted set
//!   of 1k / 10k / 100k objects and reports **P50 / P95 / P99 / max**.
//!   Criterion's mean/median/stddev defaults would hide tail-pause
//!   behavior, so we collect per-iteration `Duration` values via
//!   `iter_custom`, drop the leading warmup tail, compute percentiles in
//!   the bench harness, print a stdout table, and emit a JSON sidecar
//!   for the regression-diff tool.
//!
//! # Running
//!
//! ```sh
//! cargo bench -p phoenix-bench --bench allocation
//! ```
//!
//! # Baseline
//!
//! Baseline numbers will be committed to
//! `docs/perf-baselines/allocation.md` (alloc throughput) and
//! `docs/perf-baselines/pause.md` (pause distribution) at phase-2
//! close (see `docs/phases/phase-2.md` baseline-storage task).
//!
//! FIXME(phase-2.7-close): create `docs/perf-baselines/` and remove
//! this marker. Sibling marker in `collections.rs` must be removed in
//! the same change — `grep -rn "phase-2.7-close"` finds both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, fs};

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use phoenix_bench::{
    GcStateGuard, PauseStats, RootedFrameGuard, sample_counts_meet_threshold, summarize_pauses,
};
use phoenix_runtime::gc::{
    TypeTag, phx_gc_alloc, phx_gc_collect, phx_gc_disable, phx_gc_enable, phx_gc_push_frame,
    phx_gc_set_root, set_collection_threshold,
};
use serde::Serialize;

/// Object sizes (in bytes) sampled for the alloc-throughput bench.
const SIZES: &[usize] = &[16, 64, 256, 1024];

/// Tags sampled for the alloc-throughput bench. `Unknown` exercises the
/// default conservative-scan path; `String` exercises the no-scan
/// fast path.
const TAGS: &[TypeTag] = &[TypeTag::Unknown, TypeTag::String];

/// Live-object counts sampled for the pause-distribution bench.
const PAUSE_SCENARIOS: &[usize] = &[1_000, 10_000, 100_000];

/// Threshold under which auto-collect fires during the alloc-throughput
/// bench. 64 KiB is small enough that even the 16-byte cell crosses the
/// threshold within ~4k allocations — well within criterion's default
/// sample size — so the steady-state mix of "many allocs, occasional
/// sweep" shows up in the average. The default (1 MiB) would make the
/// smallest-object cells effectively never sweep, turning the bench
/// into a first-allocation measurement.
const ALLOC_BENCH_THRESHOLD: usize = 64 * 1024;

/// Minimum measurement samples per scenario for the pause sidecar to be
/// considered statistically meaningful. Below this the sidecar is
/// suppressed so a `cargo test --bench` run (which calls each
/// `iter_custom` closure once with `iters = 1`) can't clobber a real
/// baseline.
const MIN_SIDECAR_SAMPLES: usize = 30;

fn bench_alloc_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("alloc_throughput");

    // Hoist the guard before mutating global GC state so a panic in
    // `set_collection_threshold` / `phx_gc_enable` (or any future
    // setup added between here and the bench loop) still restores the
    // standard threshold + auto-collect flag for any later bench
    // group sharing the process.
    let _guard = GcStateGuard;
    set_collection_threshold(ALLOC_BENCH_THRESHOLD);
    phx_gc_enable();

    for &size in SIZES {
        for &tag in TAGS {
            let id = BenchmarkId::new(tag_name(tag), size);
            group.bench_with_input(id, &(size, tag), |b, &(size, tag)| {
                b.iter(|| {
                    let _ = black_box(phx_gc_alloc(black_box(size), black_box(tag as u32)));
                });
            });
        }
    }

    group.finish();
}

/// Stable, refactor-resistant name for a `TypeTag`. Spelled explicitly
/// so renames in the runtime's `Debug` impl don't silently rename
/// historical baseline IDs in criterion's output.
fn tag_name(tag: TypeTag) -> &'static str {
    match tag {
        TypeTag::Unknown => "unknown",
        TypeTag::String => "string",
        TypeTag::List => "list",
        TypeTag::Map => "map",
        TypeTag::Closure => "closure",
        TypeTag::Struct => "struct",
        TypeTag::Enum => "enum",
        TypeTag::Dyn => "dyn",
    }
}

/// Deterministic size mixer for the pause bench's rooted-object set.
/// Cycling 16/64/256/1024 by index gives a fixed distribution across
/// runs without pulling in a real PRNG.
fn pause_object_size(i: usize) -> usize {
    SIZES[i % SIZES.len()]
}

fn bench_pause_distribution(c: &mut Criterion) {
    phx_gc_disable();
    phx_gc_collect();

    let mut group = c.benchmark_group("gc_pause");
    let mut summaries: Vec<ScenarioSummary> = Vec::with_capacity(PAUSE_SCENARIOS.len());

    for &n_rooted in PAUSE_SCENARIOS {
        let frame = phx_gc_push_frame(n_rooted);
        let _guard = RootedFrameGuard::new(frame as *mut u8);
        for i in 0..n_rooted {
            let p = phx_gc_alloc(pause_object_size(i), TypeTag::Unknown as u32);
            unsafe { phx_gc_set_root(frame, i, p) };
        }

        // `iter_custom` is invoked during *both* criterion's warmup and
        // measurement phases, with no API to tell them apart. We push
        // one Duration per inner iteration into `raw_samples` here; the
        // chronological warmup prefix is discarded later by
        // `summarize_pauses` (see `WARMUP_DROP_DENOMINATOR` in
        // phoenix-bench's lib.rs). That trim is what makes this single
        // accumulating Vec correct rather than measurement-only.
        let mut raw_samples: Vec<Duration> = Vec::new();
        group.bench_function(BenchmarkId::from_parameter(n_rooted), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let t = Instant::now();
                    phx_gc_collect();
                    let d = t.elapsed();
                    raw_samples.push(d);
                    total += d;
                }
                total
            });
        });

        summaries.push(ScenarioSummary::from_raw(n_rooted, raw_samples));
        // `_guard` drops here: pops the frame and runs a final collect
        // so the next scenario starts from a clean heap.
    }

    group.finish();

    print_pause_table(&summaries);
    write_pause_sidecar(&summaries);
}

/// Saturating `Duration → u64` nanosecond conversion. `Duration::as_nanos`
/// returns `u128`, so durations longer than ~584 years would overflow a
/// plain `as u64` truncation. Saturation keeps the JSON sidecar and
/// stdout table well-formed under any input rather than printing a
/// silently-wrapped value.
fn duration_ns_u64(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Steady-state pause statistics for a single scenario. `samples` is
/// the count after warmup is dropped — i.e. what the percentiles are
/// computed over (see [`summarize_pauses`]). `n_rooted` is serialized
/// so each value is self-describing: a downstream JSON parser that
/// reorders object keys (the spec doesn't promise key ordering) still
/// recovers the scenario from the value alone.
#[derive(Serialize)]
struct ScenarioSummary {
    n_rooted: usize,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    samples: usize,
}

impl ScenarioSummary {
    fn from_raw(n_rooted: usize, raw: Vec<Duration>) -> Self {
        let PauseStats {
            p50,
            p95,
            p99,
            max,
            samples,
        } = summarize_pauses(raw);
        Self {
            n_rooted,
            p50_ns: duration_ns_u64(p50),
            p95_ns: duration_ns_u64(p95),
            p99_ns: duration_ns_u64(p99),
            max_ns: duration_ns_u64(max),
            samples,
        }
    }
}

fn print_pause_table(summaries: &[ScenarioSummary]) {
    println!();
    println!("== GC pause distribution ==");
    println!(
        "{:>10}  {:>14}  {:>14}  {:>14}  {:>14}  {:>8}",
        "rooted", "P50", "P95", "P99", "max", "samples"
    );
    for s in summaries {
        println!(
            "{:>10}  {:>14?}  {:>14?}  {:>14?}  {:>14?}  {:>8}",
            s.n_rooted,
            Duration::from_nanos(s.p50_ns),
            Duration::from_nanos(s.p95_ns),
            Duration::from_nanos(s.p99_ns),
            Duration::from_nanos(s.max_ns),
            s.samples
        );
    }
    println!();
}

/// Sidecar path. Lives at `target/criterion-pause/pause.json` —
/// deliberately a sibling of criterion's own `target/criterion/` rather
/// than nested under it, so a future `cargo bench` flag that wipes
/// criterion's output (`--save-baseline`, `cargo clean`-equivalent
/// hooks) cannot incidentally take the pause sidecar with it. The
/// regression-diff tool consumes this exact path.
fn sidecar_path() -> PathBuf {
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target_dir.join("criterion-pause").join("pause.json")
}

fn write_pause_sidecar(summaries: &[ScenarioSummary]) {
    if !sample_counts_meet_threshold(summaries.iter().map(|s| s.samples), MIN_SIDECAR_SAMPLES) {
        let min_samples = summaries.iter().map(|s| s.samples).min().unwrap_or(0);
        eprintln!(
            "[allocation bench] skipping pause sidecar: {min_samples} samples < {MIN_SIDECAR_SAMPLES} \
             (likely a `cargo test --bench` run, not `cargo bench`)"
        );
        return;
    }

    let path = sidecar_path();
    let Some(dir) = path.parent() else {
        return;
    };
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("[allocation bench] could not create {}: {e}", dir.display());
        return;
    }

    // BTreeMap<usize, _> iterates in numeric order, so the JSON output
    // lists scenarios as 1000, 10000, 100000 (not the lexical 10000,
    // 100000, 1000 a stringly-keyed map would produce). Each value
    // also carries `n_rooted`, so consumers that re-sort keys are
    // still safe.
    let payload: BTreeMap<usize, &ScenarioSummary> =
        summaries.iter().map(|s| (s.n_rooted, s)).collect();

    let file = match fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[allocation bench] could not open {}: {e}", path.display());
            return;
        }
    };
    if let Err(e) = serde_json::to_writer_pretty(file, &payload) {
        eprintln!("[allocation bench] could not write {}: {e}", path.display());
    }
}

criterion_group!(throughput, bench_alloc_throughput);
criterion_group!(pause, bench_pause_distribution);
// Order matters: `throughput` lowers the global collection threshold
// and enables auto-collect, then its `GcStateGuard` restores the
// defaults at end-of-function. `pause` then runs against a clean
// global state. Reordering or interleaving these groups would have
// `pause` observe `throughput`'s tuned threshold.
criterion_main!(throughput, pause);
