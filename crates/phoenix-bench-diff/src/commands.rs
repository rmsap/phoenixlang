//! `diff` and `update` subcommand entry points.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use crate::baseline::{
    PauseRow, ThroughputRow, parse_pause_baseline, parse_throughput_baseline, write_pause_baseline,
    write_throughput_baseline,
};
use crate::criterion_walk::walk_criterion;
use crate::pause::read_pause_sidecar;
use crate::routing::{
    BASELINE_ROUTING, PAUSE_BASELINE, find_baseline_for, numeric_suffix, split_id,
};

/// Default regression threshold: a bench whose new mean (or any pause
/// percentile) exceeds the baseline by more than 20% is flagged.
pub const DEFAULT_REGRESSION_THRESHOLD: f64 = 0.20;

// ---------------------------------------------------------------------------
// `update`
// ---------------------------------------------------------------------------

/// Refresh the committed baselines from the latest bench run. Returns
/// `Err(message)` for setup/I-O failures so [`cmd_update`] can map them
/// to exit code `2` without a stack panic.
pub fn run_update(
    baseline_dir: &Path,
    criterion_dir: &Path,
    pause_sidecar: &Path,
) -> Result<(), String> {
    let criterion = walk_criterion(criterion_dir)?;
    if criterion.is_empty() {
        return Err(format!(
            "no criterion output found under {} — run `cargo bench` first",
            criterion_dir.display()
        ));
    }

    fs::create_dir_all(baseline_dir).map_err(|e| {
        format!(
            "could not create baseline directory {}: {e}",
            baseline_dir.display()
        )
    })?;

    // Group throughput rows by destination file.
    let mut grouped: BTreeMap<&'static str, Vec<ThroughputRow>> = BTreeMap::new();
    let mut unrouted: Vec<String> = Vec::new();
    for (id, result) in &criterion {
        // `gc_pause/*` is handled by the pause-sidecar branch below;
        // criterion's own mean/median for that group is intentionally
        // discarded (see `BASELINE_ROUTING` comment).
        if id.starts_with("gc_pause/") {
            continue;
        }
        let Some(file) = find_baseline_for(id) else {
            unrouted.push(id.clone());
            continue;
        };
        grouped.entry(file).or_default().push(ThroughputRow {
            id: id.clone(),
            mean_ns: result.estimates.mean.point_estimate,
            median_ns: result.estimates.median.point_estimate,
            stddev_ns: result.estimates.std_dev.point_estimate,
            samples: result.samples,
        });
    }

    for (file, rows) in &mut grouped {
        // Sort by (bench-prefix, numeric-param) so the baseline reads
        // in scenario order (16 / 64 / 256 / 1024) instead of lexical
        // (16 / 1024 / 256 / 64). Falls back to lex ordering when the
        // trailing segment isn't numeric.
        rows.sort_by(|a, b| {
            let (a_bench, _) = split_id(&a.id);
            let (b_bench, _) = split_id(&b.id);
            a_bench
                .cmp(&b_bench)
                .then_with(|| numeric_suffix(&a.id).cmp(&numeric_suffix(&b.id)))
                .then_with(|| a.id.cmp(&b.id))
        });
        let path = baseline_dir.join(file);
        write_throughput_baseline(&path, rows)
            .map_err(|e| format!("could not write baseline {}: {e}", path.display()))?;
        eprintln!("wrote {} ({} rows)", path.display(), rows.len());
    }

    // Pause is handled separately from the JSON sidecar. A malformed
    // sidecar bubbles up as a setup error (exit 2); a missing file is
    // a legitimate "no pause data" case and falls through to the warn
    // branch below.
    let pause_rows = read_pause_sidecar(pause_sidecar)?;
    if !pause_rows.is_empty() {
        let path = baseline_dir.join(PAUSE_BASELINE);
        write_pause_baseline(&path, &pause_rows)
            .map_err(|e| format!("could not write pause baseline {}: {e}", path.display()))?;
        eprintln!("wrote {} ({} rows)", path.display(), pause_rows.len());
    } else {
        eprintln!(
            "warning: pause sidecar at {} not found or empty; {} not refreshed",
            pause_sidecar.display(),
            PAUSE_BASELINE
        );
    }

    if !unrouted.is_empty() {
        eprintln!(
            "warning: {} bench ids matched no baseline routing rule \
             (add a prefix to BASELINE_ROUTING in phoenix-bench-diff): {:?}",
            unrouted.len(),
            unrouted
        );
    }

    Ok(())
}

/// Thin CLI wrapper around [`run_update`].
pub fn cmd_update(baseline_dir: &Path, criterion_dir: &Path, pause_sidecar: &Path) -> ExitCode {
    match run_update(baseline_dir, criterion_dir, pause_sidecar) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}

// ---------------------------------------------------------------------------
// `diff`
// ---------------------------------------------------------------------------

/// Structured result of a diff run. Tests inspect this directly;
/// [`cmd_diff`] prints it and converts to an `ExitCode`.
#[derive(Debug, Default)]
pub struct DiffReport {
    /// One-line-per-comparison summary, in insertion order. Callers
    /// may want to sort before printing.
    pub summary: Vec<String>,
    /// Subset of summary lines flagged as regressions (above the
    /// threshold).
    pub regressions: Vec<String>,
}

/// Build a [`DiffReport`] without printing. Returns `Err(message)` for
/// setup failures that the CLI should surface as exit code `2`.
pub fn run_diff(
    baseline_dir: &Path,
    criterion_dir: &Path,
    pause_sidecar: &Path,
    threshold: f64,
) -> Result<DiffReport, String> {
    let criterion = walk_criterion(criterion_dir)?;
    if criterion.is_empty() {
        return Err(format!(
            "no criterion output found under {} — run `cargo bench` first",
            criterion_dir.display()
        ));
    }

    let mut report = DiffReport::default();

    // Build baseline maps grouped by file. A baseline file that's
    // missing on disk is treated as "no committed data yet" — the
    // tool reports new benches but doesn't flag them as regressions.
    // A baseline file that exists but is unreadable surfaces as an
    // `Err` from `parse_throughput_baseline` and lands here as exit
    // code 2 via `cmd_diff`'s wrapper.
    //
    // We iterate `BASELINE_ROUTING` rather than `read_dir(baseline_dir)`
    // so the routing table stays the single source of truth for which
    // throughput baseline files exist. A new baseline file added
    // without an entry here will be invisible to `diff`; see
    // `routing.rs`. (`pause.md` is out-of-band — it's keyed off
    // `PAUSE_BASELINE` and read separately below, since the source of
    // truth for pause numbers is the JSON sidecar, not criterion's
    // estimates.)
    let mut throughput_baseline: BTreeMap<String, ThroughputRow> = BTreeMap::new();
    for (file, _) in BASELINE_ROUTING {
        let path = baseline_dir.join(file);
        for row in parse_throughput_baseline(&path)? {
            throughput_baseline.insert(row.id.clone(), row);
        }
    }

    // Diff each criterion result against its baseline.
    for (id, result) in &criterion {
        // `gc_pause/*` is tracked via the sidecar; criterion's own
        // mean/median for that group is intentionally discarded (see
        // `BASELINE_ROUTING` comment + `run_update`'s skip).
        if id.starts_with("gc_pause/") {
            continue;
        }
        let new_mean = result.estimates.mean.point_estimate;
        match throughput_baseline.get(id) {
            Some(base) => {
                let ratio = safe_ratio(new_mean, base.mean_ns);
                let base_mean = base.mean_ns;
                let pct = ratio * 100.0;
                let line = format!("{id}: {base_mean:.2}ns → {new_mean:.2}ns ({pct:+.1}%)");
                push_comparison(&mut report, line, ratio > threshold);
            }
            None => report
                .summary
                .push(format!("{id}: new bench, no baseline ({new_mean:.2}ns)")),
        }
    }

    // Pause diff: per-percentile.
    let pause_baseline: BTreeMap<String, PauseRow> =
        parse_pause_baseline(&baseline_dir.join(PAUSE_BASELINE))?
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
    let pause_rows = read_pause_sidecar(pause_sidecar)?;
    if pause_rows.is_empty() && !pause_baseline.is_empty() {
        eprintln!(
            "warning: pause sidecar {} produced no rows but {} has committed data — \
             pause regressions will NOT be detected this run",
            pause_sidecar.display(),
            PAUSE_BASELINE
        );
    }
    for new in pause_rows {
        match pause_baseline.get(&new.id) {
            Some(base) => {
                for (label, base_v, new_v) in [
                    ("p50", base.p50_ns, new.p50_ns),
                    ("p95", base.p95_ns, new.p95_ns),
                    ("p99", base.p99_ns, new.p99_ns),
                    ("max", base.max_ns, new.max_ns),
                ] {
                    let ratio = safe_ratio(new_v as f64, base_v as f64);
                    let pct = ratio * 100.0;
                    let id = &new.id;
                    let line = format!("{id}/{label}: {base_v}ns → {new_v}ns ({pct:+.1}%)");
                    push_comparison(&mut report, line, ratio > threshold);
                }
            }
            None => report.summary.push(format!(
                "{}: new pause scenario, no baseline (p99={}ns)",
                new.id, new.p99_ns
            )),
        }
    }

    Ok(report)
}

/// `(new - base) / base`, with a `max(1.0)` floor on the denominator so
/// a baseline of zero (legitimate for sub-nanosecond counters or a
/// freshly-zeroed pause percentile) does not produce a NaN/Inf ratio.
/// Negative outputs represent improvements and never cross the
/// regression threshold.
///
/// Side effect of the `1.0` floor: a `0ns → Nns` transition reports as
/// `+N%` (since the denominator is 1), so a previously-zeroed
/// percentile rising into the tens or hundreds of nanoseconds will
/// trip the 20% threshold. This is intentional — a pause-percentile
/// that flipped from "always zero" to "consistently nonzero" is a real
/// signal we want surfaced, even though the percentage is somewhat
/// arbitrary in the zero-baseline case.
fn safe_ratio(new: f64, base: f64) -> f64 {
    let denom = if base > 0.0 { base } else { 1.0 };
    (new - base) / denom
}

/// Push a comparison line onto the report, marking it as a regression
/// when `is_regression` is set. The plain line is preserved for the
/// regressions list (which feeds the CI alert body); the summary line
/// gets the human-readable `** REGRESSION **` marker appended.
fn push_comparison(report: &mut DiffReport, line: String, is_regression: bool) {
    if is_regression {
        report.regressions.push(line.clone());
        report.summary.push(format!("{line} ** REGRESSION **"));
    } else {
        report.summary.push(line);
    }
}

/// Thin CLI wrapper around [`run_diff`]. Exit codes:
///
/// - `0` — clean run.
/// - `1` — at least one regression detected.
/// - `2` — setup failure (no criterion output, etc.).
pub fn cmd_diff(
    baseline_dir: &Path,
    criterion_dir: &Path,
    pause_sidecar: &Path,
    threshold: f64,
) -> ExitCode {
    let report = match run_diff(baseline_dir, criterion_dir, pause_sidecar, threshold) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let mut summary = report.summary;
    summary.sort();
    for line in &summary {
        println!("{line}");
    }

    // Regression list goes to stdout so a single `> bench-diff.txt`
    // redirect in CI captures both the summary and the alert block —
    // the failure-issue body would otherwise be missing the actionable
    // part. Both blocks are sorted alphabetically so the two views
    // present rows in the same order.
    let mut regressions = report.regressions;
    regressions.sort();
    if regressions.is_empty() {
        println!("\nno regressions above {:.0}% threshold", threshold * 100.0);
        ExitCode::SUCCESS
    } else {
        println!(
            "\n{} regression(s) above {:.0}% threshold:",
            regressions.len(),
            threshold * 100.0
        );
        for r in &regressions {
            println!("  - {r}");
        }
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::write_throughput_baseline;
    use crate::test_support::{tmp_dir, write_synthetic_bench};

    #[test]
    fn run_diff_reports_no_regressions_when_within_threshold() {
        let baseline_dir = tmp_dir("diff-clean-baseline");
        let criterion_dir = tmp_dir("diff-clean-criterion");
        let sidecar = baseline_dir.join("nonexistent-pause.json");

        // Baseline: map/get/100 at 100ns.
        write_throughput_baseline(
            &baseline_dir.join("collections.md"),
            &[ThroughputRow {
                id: "map/get/100".into(),
                mean_ns: 100.0,
                median_ns: 100.0,
                stddev_ns: 1.0,
                samples: 100,
            }],
        )
        .unwrap();
        // New run: 110ns (10% slower — under the 20% threshold).
        write_synthetic_bench(&criterion_dir, "map/get/100", 110.0, 110.0, 1.0, 100);

        let report = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        assert!(
            report.regressions.is_empty(),
            "expected no regressions, got: {:?}",
            report.regressions
        );
        assert_eq!(report.summary.len(), 1);
    }

    #[test]
    fn run_diff_flags_regression_above_threshold() {
        let baseline_dir = tmp_dir("diff-reg-baseline");
        let criterion_dir = tmp_dir("diff-reg-criterion");
        let sidecar = baseline_dir.join("nonexistent-pause.json");

        write_throughput_baseline(
            &baseline_dir.join("collections.md"),
            &[ThroughputRow {
                id: "map/get/100".into(),
                mean_ns: 100.0,
                median_ns: 100.0,
                stddev_ns: 1.0,
                samples: 100,
            }],
        )
        .unwrap();
        // 150ns → 50% slower → regression.
        write_synthetic_bench(&criterion_dir, "map/get/100", 150.0, 150.0, 1.0, 100);

        let report = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        assert_eq!(report.regressions.len(), 1);
        assert!(report.regressions[0].contains("map/get/100"));
    }

    #[test]
    fn run_diff_skips_gc_pause_in_criterion_branch() {
        // `gc_pause/*` in the criterion tree must be ignored by the
        // throughput diff — its data is supposed to come from the
        // sidecar, not from criterion's mean. Without a sidecar, the
        // report should contain zero entries for the gc_pause id.
        let baseline_dir = tmp_dir("diff-pause-skip-baseline");
        let criterion_dir = tmp_dir("diff-pause-skip-criterion");
        let sidecar = baseline_dir.join("nonexistent-pause.json");

        write_synthetic_bench(&criterion_dir, "gc_pause/1000", 9999.0, 9999.0, 1.0, 100);
        // Include a real bench too so walk_criterion doesn't return
        // an empty map (which would short-circuit run_diff).
        write_synthetic_bench(&criterion_dir, "map/get/100", 50.0, 50.0, 1.0, 100);

        let report = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        assert!(
            !report.summary.iter().any(|s| s.starts_with("gc_pause/")),
            "criterion's gc_pause/* output must be ignored by run_diff; got: {:?}",
            report.summary
        );
    }

    #[test]
    fn run_diff_reports_new_bench_when_baseline_missing_row() {
        // A criterion run that emits a bench id absent from the
        // baseline must produce a "new bench, no baseline" summary
        // line and NOT count as a regression.
        let baseline_dir = tmp_dir("diff-newbench-baseline");
        let criterion_dir = tmp_dir("diff-newbench-criterion");
        let sidecar = baseline_dir.join("nonexistent.json");

        // Baseline has map/get/100 but not map/get/1000.
        write_throughput_baseline(
            &baseline_dir.join("collections.md"),
            &[ThroughputRow {
                id: "map/get/100".into(),
                mean_ns: 100.0,
                median_ns: 100.0,
                stddev_ns: 1.0,
                samples: 100,
            }],
        )
        .unwrap();
        write_synthetic_bench(&criterion_dir, "map/get/100", 100.0, 100.0, 1.0, 100);
        write_synthetic_bench(&criterion_dir, "map/get/1000", 250.0, 250.0, 1.0, 100);

        let report = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        assert!(
            report.regressions.is_empty(),
            "new bench must not be flagged as a regression: {:?}",
            report.regressions
        );
        let new_bench_line = report
            .summary
            .iter()
            .find(|s| s.starts_with("map/get/1000:"))
            .expect("missing summary line for new bench");
        assert!(
            new_bench_line.contains("new bench, no baseline"),
            "got: {new_bench_line}"
        );
    }

    #[test]
    fn run_diff_returns_setup_error_when_criterion_empty() {
        let baseline_dir = tmp_dir("diff-empty-baseline");
        let criterion_dir = tmp_dir("diff-empty-criterion");
        let sidecar = baseline_dir.join("nonexistent.json");

        let err = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap_err();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);
        assert!(
            err.contains("no criterion output"),
            "expected setup-error message, got: {err}"
        );
    }

    #[test]
    fn run_diff_flags_pause_percentile_regression() {
        // Pause percentiles live in the sidecar, not in criterion's
        // mean. Verify the per-percentile loop in `run_diff` flags a
        // p95 jump above the threshold.
        let baseline_dir = tmp_dir("diff-pause-reg-baseline");
        let criterion_dir = tmp_dir("diff-pause-reg-criterion");
        let sidecar = baseline_dir.join("pause.json");

        // Non-pause bench so walk_criterion isn't empty.
        write_synthetic_bench(&criterion_dir, "map/get/100", 50.0, 50.0, 1.0, 100);

        write_pause_baseline(
            &baseline_dir.join(PAUSE_BASELINE),
            &[PauseRow {
                id: "gc_pause/1000".into(),
                p50_ns: 100,
                p95_ns: 200,
                p99_ns: 300,
                max_ns: 400,
                samples: 100,
            }],
        )
        .unwrap();

        // p95 goes 200 → 300 (+50%) — over the 20% threshold.
        fs::write(
            &sidecar,
            r#"{"1000":{"n_rooted":1000,"p50_ns":100,"p95_ns":300,"p99_ns":300,"max_ns":400,"samples":100}}"#,
        )
        .unwrap();

        let report = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        assert_eq!(report.regressions.len(), 1, "{:?}", report.regressions);
        assert!(
            report.regressions[0].contains("gc_pause/1000/p95"),
            "got: {}",
            report.regressions[0]
        );
    }

    #[test]
    fn run_update_warns_on_unrouted_bench_but_still_writes_routed_ones() {
        // A bench id matching no BASELINE_ROUTING prefix must not
        // appear in any baseline file, but routed benches in the same
        // run still write through.
        let baseline_dir = tmp_dir("update-unrouted-baseline");
        let criterion_dir = tmp_dir("update-unrouted-criterion");
        let sidecar = baseline_dir.join("nonexistent.json");

        write_synthetic_bench(&criterion_dir, "mystery_bench/foo", 50.0, 50.0, 1.0, 100);
        write_synthetic_bench(&criterion_dir, "map/get/100", 50.0, 50.0, 1.0, 100);

        run_update(&baseline_dir, &criterion_dir, &sidecar).unwrap();
        let collections = fs::read_to_string(baseline_dir.join("collections.md")).unwrap();
        let allocation_exists = baseline_dir.join("allocation.md").exists();
        let pipeline_exists = baseline_dir.join("pipeline.md").exists();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        assert!(collections.contains("map/get"));
        assert!(
            !collections.contains("mystery_bench"),
            "unrouted bench must not be written into any baseline file"
        );
        assert!(
            !allocation_exists && !pipeline_exists,
            "unrouted bench must not create stray baseline files"
        );
    }

    #[test]
    fn run_diff_returns_setup_error_on_malformed_pause_sidecar() {
        // A sidecar that exists but doesn't parse must surface as a
        // setup error (exit 2), not silently produce zero rows and
        // miss a real regression.
        let baseline_dir = tmp_dir("diff-pause-bad-baseline");
        let criterion_dir = tmp_dir("diff-pause-bad-criterion");
        let sidecar = baseline_dir.join("pause.json");

        write_synthetic_bench(&criterion_dir, "map/get/100", 50.0, 50.0, 1.0, 100);
        fs::create_dir_all(&baseline_dir).unwrap();
        fs::write(&sidecar, "not json").unwrap();

        let err = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap_err();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);
        assert!(err.contains("failed to parse"), "got: {err}");
    }

    #[test]
    fn run_update_skips_gc_pause_when_writing_throughput_baselines() {
        let baseline_dir = tmp_dir("update-pause-skip-baseline");
        let criterion_dir = tmp_dir("update-pause-skip-criterion");
        let sidecar = baseline_dir.join("nonexistent-pause.json");

        write_synthetic_bench(&criterion_dir, "gc_pause/1000", 9999.0, 9999.0, 1.0, 100);
        write_synthetic_bench(&criterion_dir, "map/get/100", 50.0, 50.0, 1.0, 100);

        run_update(&baseline_dir, &criterion_dir, &sidecar).unwrap();
        let collections = fs::read_to_string(baseline_dir.join("collections.md")).unwrap();
        let pause_path = baseline_dir.join(PAUSE_BASELINE);
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        assert!(collections.contains("map/get"));
        assert!(
            !collections.contains("gc_pause"),
            "gc_pause/* must not appear in throughput baselines"
        );
        assert!(
            !pause_path.exists(),
            "pause.md must not be written when sidecar is missing"
        );
    }

    #[test]
    fn run_update_writes_rows_in_numeric_param_order() {
        // `run_update`'s sort uses `(bench-prefix, numeric_suffix)` so
        // baseline rows appear in scenario order (10, 100, 1000, 10000)
        // instead of lexical (10, 100, 1000, 10000 happens to coincide
        // for power-of-10 strings; use 16 / 64 / 256 / 1024 which is
        // lexically wrong: 1024 < 16 < 256 < 64). If this sort
        // regresses, the baseline diff becomes harder to scan and any
        // future tooling that assumes scenario-monotonic rows breaks
        // silently — pin it with a test.
        let baseline_dir = tmp_dir("update-order-baseline");
        let criterion_dir = tmp_dir("update-order-criterion");
        let sidecar = baseline_dir.join("nonexistent.json");

        // Insert in non-monotonic order; expect monotonic on read.
        write_synthetic_bench(
            &criterion_dir,
            "alloc_throughput/string/1024",
            5.0,
            5.0,
            1.0,
            100,
        );
        write_synthetic_bench(
            &criterion_dir,
            "alloc_throughput/string/16",
            1.0,
            1.0,
            1.0,
            100,
        );
        write_synthetic_bench(
            &criterion_dir,
            "alloc_throughput/string/256",
            4.0,
            4.0,
            1.0,
            100,
        );
        write_synthetic_bench(
            &criterion_dir,
            "alloc_throughput/string/64",
            2.0,
            2.0,
            1.0,
            100,
        );

        run_update(&baseline_dir, &criterion_dir, &sidecar).unwrap();
        let allocation = fs::read_to_string(baseline_dir.join("allocation.md")).unwrap();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        // Find the offsets of each parameter row in the rendered file.
        let i16 = allocation.find("| 16 |").expect("missing 16 row");
        let i64 = allocation.find("| 64 |").expect("missing 64 row");
        let i256 = allocation.find("| 256 |").expect("missing 256 row");
        let i1024 = allocation.find("| 1024 |").expect("missing 1024 row");
        assert!(
            i16 < i64 && i64 < i256 && i256 < i1024,
            "rows not in numeric order: 16@{i16} 64@{i64} 256@{i256} 1024@{i1024}\nfile:\n{allocation}"
        );
    }

    #[test]
    fn run_diff_reports_new_pause_scenario_when_baseline_missing_row() {
        // Symmetric to `run_diff_reports_new_bench_when_baseline_missing_row`
        // but for the pause-sidecar branch: a sidecar scenario absent
        // from `pause.md` must produce a "new pause scenario, no
        // baseline" summary line and NOT count as a regression.
        let baseline_dir = tmp_dir("diff-new-pause-baseline");
        let criterion_dir = tmp_dir("diff-new-pause-criterion");
        let sidecar = baseline_dir.join("pause.json");

        // Non-pause bench so walk_criterion isn't empty.
        write_synthetic_bench(&criterion_dir, "map/get/100", 50.0, 50.0, 1.0, 100);

        // Baseline has 1000 but not 10000.
        write_pause_baseline(
            &baseline_dir.join(PAUSE_BASELINE),
            &[PauseRow {
                id: "gc_pause/1000".into(),
                p50_ns: 100,
                p95_ns: 200,
                p99_ns: 300,
                max_ns: 400,
                samples: 100,
            }],
        )
        .unwrap();

        // Sidecar emits 1000 (matched) + 10000 (new).
        fs::write(
            &sidecar,
            r#"{
              "1000":  {"n_rooted": 1000,  "p50_ns": 100, "p95_ns": 200, "p99_ns": 300, "max_ns": 400, "samples": 100},
              "10000": {"n_rooted": 10000, "p50_ns": 700, "p95_ns": 800, "p99_ns": 900, "max_ns": 999, "samples": 50}
            }"#,
        )
        .unwrap();

        let report = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);

        assert!(
            report.regressions.is_empty(),
            "new pause scenario must not be flagged as a regression: {:?}",
            report.regressions
        );
        let new_scenario_line = report
            .summary
            .iter()
            .find(|s| s.starts_with("gc_pause/10000:"))
            .expect("missing summary line for new pause scenario");
        assert!(
            new_scenario_line.contains("new pause scenario, no baseline"),
            "got: {new_scenario_line}"
        );
        assert!(
            new_scenario_line.contains("p99=900"),
            "summary should surface p99 of the new scenario: {new_scenario_line}"
        );
    }

    #[test]
    fn run_diff_returns_setup_error_when_baseline_unreadable() {
        // A baseline file that exists but cannot be read (here: a
        // directory at the file path) must surface as exit code 2,
        // not silently report every existing bench as "new" and miss
        // regressions. Mirrors the malformed-pause-sidecar test.
        let baseline_dir = tmp_dir("diff-bad-baseline");
        let criterion_dir = tmp_dir("diff-bad-criterion");
        let sidecar = baseline_dir.join("nonexistent.json");

        write_synthetic_bench(&criterion_dir, "map/get/100", 50.0, 50.0, 1.0, 100);
        // Place a *directory* where `collections.md` should be — the
        // routing iteration will try to read it as a file and fail.
        fs::create_dir_all(baseline_dir.join("collections.md")).unwrap();

        let err = run_diff(&baseline_dir, &criterion_dir, &sidecar, 0.20).unwrap_err();
        let _ = fs::remove_dir_all(&baseline_dir);
        let _ = fs::remove_dir_all(&criterion_dir);
        assert!(
            err.contains("failed to read baseline"),
            "expected propagated baseline-read error, got: {err}"
        );
    }
}
