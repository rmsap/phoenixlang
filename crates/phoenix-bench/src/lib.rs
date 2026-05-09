//! Benchmarks and validation tests for the Phoenix compiler pipeline.
//!
//! This crate measures the performance of each compilation stage (lex, parse,
//! sema, IR lowering, Cranelift native code generation) and both interpreters
//! (tree-walk and IR) across fixture programs of increasing complexity.  It
//! also contains integration tests that verify each fixture produces the
//! expected output.
//!
//! # Running benchmarks
//!
//! ```sh
//! cargo bench -p phoenix-bench
//! ```
//!
//! To save a baseline for later regression comparison:
//!
//! ```sh
//! cargo bench -p phoenix-bench -- --save-baseline <name>
//! ```
//!
//! To compare against a saved baseline:
//!
//! ```sh
//! cargo bench -p phoenix-bench -- --baseline <name>
//! ```
//!
//! # Running fixture validity tests
//!
//! ```sh
//! cargo test -p phoenix-bench
//! ```

#![warn(missing_docs)]

use phoenix_common::span::SourceId;
use phoenix_runtime::gc::{
    DEFAULT_COLLECTION_THRESHOLD, phx_gc_collect, phx_gc_disable, phx_gc_pop_frame,
    set_collection_threshold,
};

/// Source ID used for all benchmark fixtures (no real file backing).
pub const BENCH_SOURCE_ID: SourceId = SourceId(0);

// ---------------------------------------------------------------------------
// Fixture sources
// ---------------------------------------------------------------------------

/// Minimal fixture: recursion, if/else, arithmetic (8 lines).
pub const SMALL: &str = include_str!("../benches/fixtures/small.phx");

/// Moderate fixture: structs, enums, pattern matching (27 lines).
pub const MEDIUM: &str = include_str!("../benches/fixtures/medium.phx");

/// Mid-size fixture: structs with methods, closures, higher-order functions,
/// loops, mutable variables (~60 lines).
pub const MEDIUM_LARGE: &str = include_str!("../benches/fixtures/medium_large.phx");

/// Broad-coverage fixture: traits, generics, closures, lists, Option/Result,
/// string interpolation, loops, fizzbuzz (155 lines).
pub const LARGE: &str = include_str!("../benches/fixtures/large.phx");

/// Empty program — minimal valid fixture.
pub const EMPTY: &str = include_str!("../benches/fixtures/empty.phx");

/// Fixture that contains a deliberate parse error.
pub const PARSE_ERROR: &str = include_str!("../benches/fixtures/parse_error.phx");

/// Fixture that parses successfully but fails type checking.
pub const TYPE_ERROR: &str = include_str!("../benches/fixtures/type_error.phx");

// ---------------------------------------------------------------------------
// Shared pipeline helpers
// ---------------------------------------------------------------------------

/// Lex → parse → sema, panicking on errors.  Returns the checked program and
/// sema result so callers can feed them to an interpreter or IR lowering.
fn check_fixture(name: &str, source: &str) -> (phoenix_parser::Program, phoenix_sema::Analysis) {
    let tokens = phoenix_lexer::tokenize(source, BENCH_SOURCE_ID);
    let (program, parse_diags) = phoenix_parser::parse(&tokens);
    assert!(
        parse_diags.is_empty(),
        "{name} has parse errors: {parse_diags:?}"
    );

    let check_result = phoenix_sema::check(&program);
    assert!(
        check_result.diagnostics.is_empty(),
        "{name} has sema errors: {:?}",
        check_result.diagnostics
    );

    (program, check_result)
}

/// Compile a fixture through lex → parse → sema → IR lowering, panicking on
/// any errors.  Returns the number of functions in the resulting IR module.
pub fn compile(name: &str, source: &str) -> usize {
    let (program, check_result) = check_fixture(name, source);
    let ir_module = phoenix_ir::lower(&program, &check_result.module);
    ir_module.function_count()
}

/// Run a fixture through the full compilation pipeline and tree-walk
/// interpreter.  Returns the captured output lines.
pub fn run_tree_walk(name: &str, source: &str) -> Vec<String> {
    let (program, check_result) = check_fixture(name, source);
    phoenix_interp::run_and_capture(&program, check_result.module.lambda_captures)
        .unwrap_or_else(|e| panic!("{name} failed in tree-walk interpreter: {e:?}"))
}

/// Run a fixture through the full compilation pipeline and IR interpreter.
/// Returns the captured output lines.
pub fn run_ir(name: &str, source: &str) -> Vec<String> {
    let (program, check_result) = check_fixture(name, source);
    let ir_module = phoenix_ir::lower(&program, &check_result.module);
    phoenix_ir_interp::run_and_capture(&ir_module)
        .unwrap_or_else(|e| panic!("{name} failed in IR interpreter: {e:?}"))
}

/// Assert that a fixture source has parse errors (does not reach sema).
pub fn assert_parse_error(name: &str, source: &str) {
    let tokens = phoenix_lexer::tokenize(source, BENCH_SOURCE_ID);
    let (_program, parse_diags) = phoenix_parser::parse(&tokens);
    assert!(
        !parse_diags.is_empty(),
        "{name} was expected to have parse errors but parsed successfully"
    );
}

/// Returns the value at the nearest-rank percentile `q ∈ [0, 1]` of a
/// **sorted** slice of [`Duration`]s. `q = 0` returns the first element;
/// `q = 1` returns the last. Empty input returns [`Duration::ZERO`]. The
/// chosen index is `round((len - 1) * q)` (nearest-rank, no
/// interpolation). Out-of-range, infinite, or `NaN` `q` is handled
/// without panicking via the saturating `f64 as usize` cast plus an
/// `idx.min(len - 1)` clamp: `q ≥ 1` and `+∞` return the last element;
/// `q ≤ 0`, `-∞`, and `NaN` return the first.
///
/// Used by the `allocation` bench's pause-distribution group; lives in
/// the library so it can be unit-tested.
pub fn percentile(sorted: &[std::time::Duration], q: f64) -> std::time::Duration {
    if sorted.is_empty() {
        return std::time::Duration::ZERO;
    }
    // Saturating `f64 as usize` cast (stable since Rust 1.45): negative
    // and `NaN` map to 0, `+∞` to `usize::MAX`. Combined with the
    // `idx.min(len - 1)` clamp below, this is what makes the panic-free
    // out-of-range `q` handling load-bearing — see
    // `percentile_handles_negative_and_nan_without_panicking`.
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Fraction of leading samples discarded as warmup before computing
/// pause percentiles. Criterion's `iter_custom` is invoked during both
/// warmup and measurement and exposes no signal to distinguish them, so
/// [`summarize_pauses`] drops the first 1/`WARMUP_DROP_DENOMINATOR`
/// samples (20%) **by collection order**, not by sorted rank.
///
/// 20% is an empirical heuristic, not a tight bound: each
/// `iter_custom` call iterates `iters` times (a value criterion picks
/// adaptively per phase), so the warmup share of total raw samples
/// isn't pinned to any fixed fraction. The trim is intended to cover
/// warmup outliers (cold caches, first-collection setup) for the
/// scenarios in the `allocation` bench; revisit if those distributions
/// change shape.
pub const WARMUP_DROP_DENOMINATOR: usize = 5;

/// Steady-state pause statistics produced by [`summarize_pauses`].
#[derive(Debug, Clone, Copy)]
pub struct PauseStats {
    /// Median pause (50th percentile, nearest-rank).
    pub p50: std::time::Duration,
    /// 95th-percentile pause (nearest-rank).
    pub p95: std::time::Duration,
    /// 99th-percentile pause (nearest-rank). Quality depends on
    /// post-trim sample count: with ~80 steady-state samples this is
    /// `sorted[78]` — closer to a P97 in classical terms than a true
    /// P99. Useful for trend tracking across runs; raise criterion's
    /// `sample_size` if a tighter tail estimate is needed.
    pub p99: std::time::Duration,
    /// Largest measured pause after warmup is dropped.
    pub max: std::time::Duration,
    /// Sample count after warmup is dropped — i.e. the slice over
    /// which the percentiles above are computed.
    pub samples: usize,
}

/// Drop the first 1/[`WARMUP_DROP_DENOMINATOR`] samples chronologically
/// (warmup), then sort the remainder and compute P50/P95/P99/max.
///
/// The trim happens **before** sorting on purpose: warmup samples
/// (cold caches, first-collection setup) are typically *slower* than
/// steady state and would land near the top of a sorted distribution.
/// A sort-then-drop pipeline would discard the fastest samples and
/// keep the warmup outliers, biasing every percentile upward.
pub fn summarize_pauses(mut raw: Vec<std::time::Duration>) -> PauseStats {
    let drop = raw.len() / WARMUP_DROP_DENOMINATOR;
    raw.drain(0..drop);
    raw.sort();
    PauseStats {
        p50: percentile(&raw, 0.50),
        p95: percentile(&raw, 0.95),
        p99: percentile(&raw, 0.99),
        max: raw.last().copied().unwrap_or_default(),
        samples: raw.len(),
    }
}

/// Returns `true` only when **every** scenario's sample count clears
/// `min_required`. Empty input returns `false` so a caller iterating an
/// empty summary list cannot accidentally write a baseline sidecar with
/// no data.
///
/// Used by the `allocation` bench's pause sidecar to refuse overwriting
/// a real baseline from a `cargo test --bench` run (which calls each
/// `iter_custom` closure once with `iters = 1`, producing single-sample
/// summaries). Lives in the library because `#[cfg(test)]` modules in a
/// `harness = false` bench file are never executed — criterion's
/// `criterion_main!` replaces libtest's test discovery.
pub fn sample_counts_meet_threshold(
    sample_counts: impl IntoIterator<Item = usize>,
    min_required: usize,
) -> bool {
    sample_counts
        .into_iter()
        .min()
        .is_some_and(|m| m >= min_required)
}

/// RAII guard that restores the global GC to its standard idle state
/// (auto-collect off, default collection threshold, empty heap) when
/// dropped. Hoist before mutating GC state so a panic in setup or
/// inside a bench iteration still resets globals for any later bench
/// group sharing the process.
pub struct GcStateGuard;

impl Drop for GcStateGuard {
    fn drop(&mut self) {
        phx_gc_disable();
        phx_gc_collect();
        set_collection_threshold(DEFAULT_COLLECTION_THRESHOLD);
    }
}

/// RAII pop of a shadow-stack frame plus a final `phx_gc_collect()` so
/// a panic mid-bench can't leak a rooted set into a later scenario.
///
/// The frame field is typed as `*mut u8` rather than `*mut Frame`
/// because the `phoenix_runtime::gc::shadow_stack` module is
/// `pub(crate)` — the `Frame` struct itself is `pub` within the
/// module, but the module's visibility makes the type unnameable from
/// outside the runtime crate. The cast round-trips through the
/// raw-pointer representation, which is well-defined.
pub struct RootedFrameGuard {
    frame: *mut u8,
}

impl RootedFrameGuard {
    /// Wrap a frame pointer returned by `phx_gc_push_frame`. The
    /// pointer is stored as `*mut u8` for the visibility reason above;
    /// callers pass it through as-is from the runtime API.
    pub fn new(frame: *mut u8) -> Self {
        Self { frame }
    }
}

impl Drop for RootedFrameGuard {
    fn drop(&mut self) {
        unsafe { phx_gc_pop_frame(self.frame as *mut _) };
        phx_gc_collect();
    }
}

/// Assert that a fixture source parses successfully but fails type checking.
pub fn assert_type_error(name: &str, source: &str) {
    let tokens = phoenix_lexer::tokenize(source, BENCH_SOURCE_ID);
    let (program, parse_diags) = phoenix_parser::parse(&tokens);
    assert!(
        parse_diags.is_empty(),
        "{name} was expected to parse cleanly but had errors: {parse_diags:?}"
    );

    let check_result = phoenix_sema::check(&program);
    assert!(
        !check_result.diagnostics.is_empty(),
        "{name} was expected to have type errors but passed sema cleanly"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ds(ns: u64) -> Duration {
        Duration::from_nanos(ns)
    }

    #[test]
    fn percentile_empty_returns_zero() {
        assert_eq!(percentile(&[], 0.5), Duration::ZERO);
        assert_eq!(percentile(&[], 0.0), Duration::ZERO);
        assert_eq!(percentile(&[], 1.0), Duration::ZERO);
    }

    #[test]
    fn percentile_single_element() {
        let s = [ds(42)];
        assert_eq!(percentile(&s, 0.0), ds(42));
        assert_eq!(percentile(&s, 0.5), ds(42));
        assert_eq!(percentile(&s, 1.0), ds(42));
    }

    #[test]
    fn percentile_endpoints_pick_first_and_last() {
        let s: Vec<Duration> = (1u64..=100).map(ds).collect();
        assert_eq!(percentile(&s, 0.0), ds(1));
        assert_eq!(percentile(&s, 1.0), ds(100));
    }

    #[test]
    fn percentile_nearest_rank_rounding() {
        let s: Vec<Duration> = (1u64..=100).map(ds).collect();
        // q = 0.99: idx = round(99 * 0.99) = 98 -> sorted[98] = 99.
        assert_eq!(percentile(&s, 0.99), ds(99));
        // q = 0.95: idx = round(99 * 0.95) = 94 -> sorted[94] = 95.
        assert_eq!(percentile(&s, 0.95), ds(95));
        // q = 0.50: idx = round(99 * 0.50) = round(49.5) = 50 (half
        // away from zero) -> sorted[50] = 51.
        assert_eq!(percentile(&s, 0.50), ds(51));
    }

    #[test]
    fn percentile_clamps_above_one() {
        let s = [ds(1), ds(2), ds(3)];
        assert_eq!(percentile(&s, 1.5), ds(3));
    }

    #[test]
    fn percentile_handles_negative_and_nan_without_panicking() {
        let s = [ds(10), ds(20), ds(30)];
        // Saturating `f64 as usize` cast: negative and NaN both map to 0.
        assert_eq!(percentile(&s, -1.0), ds(10));
        assert_eq!(percentile(&s, f64::NEG_INFINITY), ds(10));
        assert_eq!(percentile(&s, f64::NAN), ds(10));
        assert_eq!(percentile(&s, f64::INFINITY), ds(30));
    }

    #[test]
    fn summarize_pauses_empty_returns_all_zero() {
        let s = summarize_pauses(Vec::new());
        assert_eq!(s.samples, 0);
        assert_eq!(s.p50, Duration::ZERO);
        assert_eq!(s.p95, Duration::ZERO);
        assert_eq!(s.p99, Duration::ZERO);
        assert_eq!(s.max, Duration::ZERO);
    }

    #[test]
    fn summarize_pauses_drops_leading_warmup_not_fastest_samples() {
        // 100 samples in collection order: first 20 are slow warmup
        // (1s each), remaining 80 are fast steady state (1..=80 ns).
        // Correct behavior drops the first 20 chronologically, leaving
        // the steady-state distribution. A sort-then-drop bug would
        // discard the fastest 20 ns and keep the warmup outliers,
        // pushing `max` to 1 s and dragging every percentile upward.
        let mut raw = Vec::with_capacity(100);
        for _ in 0..20 {
            raw.push(Duration::from_secs(1));
        }
        for n in 1u64..=80 {
            raw.push(Duration::from_nanos(n));
        }

        let s = summarize_pauses(raw);

        assert_eq!(s.samples, 80);
        assert_eq!(
            s.max,
            Duration::from_nanos(80),
            "warmup outlier leaked into measured distribution"
        );
        // Sorted steady-state values are 1..=80 ns; nearest-rank P50 is
        // round(79 * 0.5) = round(39.5) = 40 (half away from zero) ->
        // sorted[40] = 41 ns.
        assert_eq!(s.p50, Duration::from_nanos(41));
    }

    #[test]
    fn summarize_pauses_under_warmup_threshold_keeps_everything() {
        // With < WARMUP_DROP_DENOMINATOR samples, `len / D` is 0 — the
        // trim is a no-op and every sample survives. This guards
        // tiny-sample paths (e.g. a single `cargo test --bench` call
        // through `iter_custom`) from accidentally producing an empty
        // measured slice.
        let raw: Vec<Duration> = (1u64..=4).map(ds).collect();
        let s = summarize_pauses(raw);
        assert_eq!(s.samples, 4);
        assert_eq!(s.max, ds(4));
    }

    #[test]
    fn sample_counts_meet_threshold_requires_every_scenario_to_pass() {
        // A `cargo test --bench` invocation drives `iter_custom` once
        // with `iters = 1`, producing single-sample summaries. The
        // gate must refuse to overwrite a real baseline even if other
        // scenarios in the same run cleared the threshold.
        assert!(!sample_counts_meet_threshold([35usize, 25], 30));
        assert!(sample_counts_meet_threshold([30usize, 100, 31], 30));
        assert!(sample_counts_meet_threshold([30usize], 30));
    }

    #[test]
    fn sample_counts_meet_threshold_rejects_empty_input() {
        // No scenarios → no data to write → must not produce a sidecar.
        assert!(!sample_counts_meet_threshold(
            std::iter::empty::<usize>(),
            30
        ));
    }

    #[test]
    fn sample_counts_meet_threshold_zero_threshold_always_passes_nonempty() {
        // Degenerate threshold = 0: any non-empty input qualifies; an
        // empty iterator still fails because there's nothing to write.
        assert!(sample_counts_meet_threshold([0usize], 0));
        assert!(!sample_counts_meet_threshold(
            std::iter::empty::<usize>(),
            0
        ));
    }
}
