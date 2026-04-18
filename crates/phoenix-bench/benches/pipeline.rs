//! Benchmarks for each stage of the Phoenix compiler pipeline.
//!
//! Measures lex, parse, semantic analysis, IR lowering, Cranelift native
//! code generation, IR interpretation, and tree-walk interpretation across
//! fixture programs of increasing complexity.
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

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use phoenix_bench::{BENCH_SOURCE_ID, EMPTY, LARGE, MEDIUM, MEDIUM_LARGE, SMALL};

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
            b.iter(|| phoenix_ir::lower(black_box(&program), black_box(&check_result)));
        });

        // Pre-compute IR module for IR interpreter benchmark
        let ir_module = phoenix_ir::lower(&program, &check_result);

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
        let captures = check_result.lambda_captures.clone();
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
                phoenix_ir::lower(&program, &check_result)
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
                    let ir_module = phoenix_ir::lower(&program, &check_result);
                    phoenix_cranelift::compile(&ir_module)
                });
            });
        }

        group.finish();
    }
}

criterion_group!(benches, bench_pipeline);
criterion_main!(benches);
