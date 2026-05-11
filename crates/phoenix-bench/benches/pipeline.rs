//! Benchmarks for each stage of the Phoenix compiler pipeline.
//!
//! Measures lex, parse, semantic analysis, IR lowering, Cranelift native
//! code generation, IR interpretation, tree-walk interpretation, and
//! end-to-end compile-and-run wall-clock across fixture programs of
//! increasing complexity. The `compile_and_run` group is the only one
//! that times the *compiled binary's* runtime — every other group
//! stops at IR or earlier, so a regression that shows up only after
//! Cranelift codegen + linking + execution would slip past them. See
//! `phoenix_bench::compile_and_link` / `phoenix_bench::time_run` for
//! the harness and [`COMPILE_AND_RUN_FIXTURES`] for the gating rules.
//!
//! **`compile_and_run` resolution caveat.** Subprocess spawn and the
//! harness's 1 ms poll cap together impose a measurement floor of a
//! few milliseconds. Treat <5% changes in this group as noise — it is
//! designed to catch cumulative regressions across codegen + runtime
//! + execution, not sub-ms tuning wins.
//!
//! Sibling benches: `allocation` (`phx_gc_alloc` throughput + GC
//! pause distribution); `collections` (Map ops + `List.sortBy`
//! algorithmic shape).
//!
//! Baseline numbers will be committed to
//! `docs/perf-baselines/pipeline.md` at phase-2 close (see
//! `docs/phases/phase-2.md` baseline-storage task).
//!
//! FIXME(phase-2.7-close): create `docs/perf-baselines/` and remove
//! this marker.
//!
//! # Running
//!
//! ```sh
//! cargo bench -p phoenix-bench
//! ```
//!
//! Benchmarks are compiled in release mode by default (`cargo bench` implies
//! `--release`). Do not confuse `cargo test -p phoenix-bench` (which runs the
//! fixture validity tests) with `cargo bench` (which runs these measurements).
//!
//! # Regression tracking
//!
//! Save a baseline after a known-good state:
//!
//! ```sh
//! cargo bench -p phoenix-bench -- --save-baseline main
//! ```
//!
//! Then compare after changes:
//!
//! ```sh
//! cargo bench -p phoenix-bench -- --baseline main
//! ```
//!
//! Criterion will report percentage changes and flag regressions.

use std::sync::LazyLock;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{BatchSize, BenchmarkGroup, Criterion, black_box, criterion_group, criterion_main};
use phoenix_bench::{
    BENCH_SOURCE_ID, EMPTY, LARGE, MEDIUM, MEDIUM_LARGE, SMALL, compile_and_link, probe_native,
    time_run,
};

/// Snapshot of the strict-mode env var, taken once so the gate is
/// stable across an entire `cargo bench` run. `=="1"` (not `.is_ok()`)
/// to avoid the `=0` / `=""` footgun.
static STRICT_COMPILE_AND_RUN: LazyLock<bool> =
    LazyLock::new(|| std::env::var("PHOENIX_BENCH_REQUIRE_COMPILE_AND_RUN").as_deref() == Ok("1"));

/// Fixtures that run end-to-end today. `empty` / `small` are excluded
/// because subprocess spawn dwarfs their sub-ms wall-clock.
const COMPILE_AND_RUN_FIXTURES: &[&str] = &["medium"];

/// Fixtures auto-enabled once the matching Cranelift codegen gap
/// closes. Listed up front so the bench doesn't pay the full compile
/// pipeline only to see the same failure each run. Mirrors the
/// `#[ignore]`d `*_native` tests in `tests/fixture_validity.rs`;
/// move entries to [`COMPILE_AND_RUN_FIXTURES`] when lifting the
/// `#[ignore]`.
const KNOWN_BLOCKED_FIXTURES: &[(&str, &str)] = &[
    (
        "medium_large",
        "phoenix-cranelift: print() of list<i64> not yet lowered",
    ),
    (
        "large",
        "phoenix-cranelift: string methods used by describe() not yet lowered",
    ),
];

fn bench_pipeline(c: &mut Criterion) {
    for (name, source) in [
        ("empty", EMPTY),
        ("small", SMALL),
        ("medium", MEDIUM),
        ("medium_large", MEDIUM_LARGE),
        ("large", LARGE),
    ] {
        let mut group = c.benchmark_group(name);

        // Stage 1: Lexing
        group.bench_function("lex", |b| {
            b.iter(|| phoenix_lexer::tokenize(black_box(source), BENCH_SOURCE_ID));
        });

        // Pre-compute tokens for parse benchmark
        let tokens = phoenix_lexer::tokenize(source, BENCH_SOURCE_ID);

        // Stage 2: Parsing
        group.bench_function("parse", |b| {
            b.iter(|| phoenix_parser::parse(black_box(&tokens)));
        });

        // Pre-compute AST for subsequent benchmarks
        let (program, parse_diags) = phoenix_parser::parse(&tokens);
        assert!(
            parse_diags.is_empty(),
            "{name} has parse errors: {parse_diags:?}"
        );

        // Stage 3: Semantic analysis
        group.bench_function("sema", |b| {
            b.iter(|| phoenix_sema::check(black_box(&program)));
        });

        // Pre-compute check result for IR and interp benchmarks
        let check_result = phoenix_sema::check(&program);
        assert!(
            check_result.diagnostics.is_empty(),
            "{name} has sema errors: {:?}",
            check_result.diagnostics
        );

        // Stage 4: IR lowering
        group.bench_function("ir_lower", |b| {
            b.iter(|| phoenix_ir::lower(black_box(&program), black_box(&check_result.module)));
        });

        // Pre-compute IR module for IR interpreter benchmark
        let ir_module = phoenix_ir::lower(&program, &check_result.module);

        // Stage 5: Cranelift native code generation.
        // Like the IR interpreter bench, probe support at setup time and
        // skip fixtures the Cranelift backend doesn't yet handle.
        let cranelift_ok = phoenix_cranelift::compile(&ir_module).is_ok();
        if cranelift_ok {
            group.bench_function("cranelift_compile", |b| {
                b.iter(|| {
                    let _ = phoenix_cranelift::compile(black_box(&ir_module));
                });
            });
        }

        // Stage 6: IR interpretation
        // Run the interpreter once at setup time to determine whether it
        // supports this fixture.  The extra setup run is not measured by
        // Criterion and is negligible compared to the benchmark iterations.
        if phoenix_ir_interp::run_and_capture(&ir_module).is_ok() {
            group.bench_function("ir_interp", |b| {
                b.iter(|| {
                    let _ = phoenix_ir_interp::run_and_capture(black_box(&ir_module));
                });
            });
        }

        // Stage 7: Tree-walk interpretation
        // Uses iter_batched so that the captures.clone() setup cost is excluded
        // from the measurement (run_and_capture takes ownership of the map).
        let captures = check_result.module.lambda_captures.clone();
        group.bench_function("interp", |b| {
            b.iter_batched(
                || captures.clone(),
                |caps| {
                    let _ = phoenix_interp::run_and_capture(black_box(&program), caps);
                },
                BatchSize::SmallInput,
            );
        });

        // Full pipeline: source string through IR lowering.
        // Named "full_compile" because it covers the compilation stages
        // (lex -> parse -> sema -> IR) but excludes interpretation, which is
        // a runtime concern rather than a compilation step.
        //
        // Assertions are omitted here — the setup code above already validates
        // that each fixture compiles cleanly, and the fixture_validity tests
        // provide the authoritative correctness checks.
        group.bench_function("full_compile", |b| {
            b.iter(|| {
                let tokens = phoenix_lexer::tokenize(black_box(source), BENCH_SOURCE_ID);
                let (program, _) = phoenix_parser::parse(&tokens);
                let check_result = phoenix_sema::check(&program);
                phoenix_ir::lower(&program, &check_result.module)
            });
        });

        // Full native pipeline: source string through Cranelift object emission.
        // Extends full_compile with the Cranelift backend stage so we can
        // measure end-to-end native-build latency, not just frontend-to-IR.
        if cranelift_ok {
            group.bench_function("full_compile_native", |b| {
                b.iter(|| {
                    let tokens = phoenix_lexer::tokenize(black_box(source), BENCH_SOURCE_ID);
                    let (program, _) = phoenix_parser::parse(&tokens);
                    let check_result = phoenix_sema::check(&program);
                    let ir_module = phoenix_ir::lower(&program, &check_result.module);
                    phoenix_cranelift::compile(&ir_module)
                });
            });
        }

        register_compile_and_run(&mut group, name, source);

        group.finish();
    }
}

/// Register the `compile_and_run` bench for `name`/`source` when every
/// precondition holds; otherwise emit a skip on stderr (and panic in
/// strict mode for everything except deliberate exclusions).
///
/// Times only the spawn + run + teardown of the linked binary;
/// compile + link is amortized at setup via `compile_and_link`'s
/// per-process cache. Gating order: KNOWN_BLOCKED → not-in-fixtures →
/// compile_and_link → probe_native. The intentional-exclusion skip
/// is the only one that doesn't honor strict mode — fixtures excluded
/// for "subprocess spawn dominates" are a deliberate design choice,
/// not an unmet codegen gap.
fn register_compile_and_run(group: &mut BenchmarkGroup<'_, WallTime>, name: &str, source: &str) {
    // Skip for an unmet condition: honors strict mode.
    let skip = |reason: String| {
        if *STRICT_COMPILE_AND_RUN {
            panic!("PHOENIX_BENCH_REQUIRE_COMPILE_AND_RUN=1 set: {reason}");
        }
        eprintln!("{reason}");
    };

    if let Some((_, reason)) = KNOWN_BLOCKED_FIXTURES.iter().find(|(n, _)| *n == name) {
        skip(format!(
            "skipping {name}/compile_and_run: known-blocked ({reason}) — \
             move into COMPILE_AND_RUN_FIXTURES when the gap closes"
        ));
        return;
    }
    if !COMPILE_AND_RUN_FIXTURES.contains(&name) {
        // Deliberate design exclusion — never escalate under strict mode.
        eprintln!(
            "skipping {name}/compile_and_run: subprocess-spawn floor dominates sub-ms \
             fixtures (intentional, not a blocked codegen gap)"
        );
        return;
    }

    let exe = match compile_and_link(name, source) {
        Ok(exe) => exe,
        Err(e) => {
            skip(format!("skipping {name}/compile_and_run: {e}"));
            return;
        }
    };
    if !probe_native(&exe) {
        skip(format!(
            "skipping {name}/compile_and_run: linked binary exited non-zero on probe"
        ));
        return;
    }
    group.bench_function("compile_and_run", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += time_run(black_box(&exe));
            }
            total
        });
    });
}

criterion_group!(benches, bench_pipeline);
criterion_main!(benches);
