//! Walks criterion's per-bench output directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

/// Criterion `estimates.json` schema (only the fields we need).
#[derive(Debug, Deserialize)]
pub struct Estimates {
    /// Mean estimate (in nanoseconds for time benchmarks).
    pub mean: PointEstimate,
    /// Median estimate (in nanoseconds for time benchmarks).
    pub median: PointEstimate,
    /// Standard-deviation estimate.
    pub std_dev: PointEstimate,
}

/// One point estimate from a criterion `estimates.json` file.
#[derive(Debug, Deserialize)]
pub struct PointEstimate {
    /// The point value of the estimate (`mean`, `median`, …).
    pub point_estimate: f64,
}

/// Bundle of per-bench data pulled from criterion's `new/` directory.
#[derive(Debug)]
pub struct CriterionResult {
    /// Parsed `estimates.json` for this bench run.
    pub estimates: Estimates,
    /// Length of `sample.json`'s `iters` array — criterion's actual
    /// sample count for the run, not the configured default.
    pub samples: u64,
}

/// Criterion `sample.json` schema (only the field we need).
#[derive(Debug, Deserialize)]
struct CriterionSample {
    iters: Vec<f64>,
}

/// Walk `target/criterion/` and collect `(bench-id, CriterionResult)`
/// pairs. Bench-id is the path under the root with `/new/estimates.json`
/// stripped — e.g. `target/criterion/map/get/100/new/estimates.json`
/// becomes `map/get/100`. The sibling `sample.json` is read to recover
/// criterion's actual sample count.
///
/// Returns `Err` if any `estimates.json` fails to read or parse, or if
/// any directory under `root` is unreadable. A corrupted bench result
/// must surface as exit `2` rather than look indistinguishable from
/// "bench not run" — see the pause sidecar's contract in
/// [`crate::pause::read_pause_sidecar`].
pub fn walk_criterion(root: &Path) -> Result<BTreeMap<String, CriterionResult>, String> {
    let mut out = BTreeMap::new();
    if !root.exists() {
        return Ok(out);
    }
    walk_dir(root, root, &mut out)?;
    Ok(out)
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, CriterionResult>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("failed to read directory {}: {e}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("failed to read entry under {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            // Criterion drops HTML summaries under `report/` (both at
            // the criterion root and per-group). They have no
            // `new/estimates.json`, so recursing into them is wasted
            // I/O — skip explicitly.
            if path.file_name().is_some_and(|n| n == "report") {
                continue;
            }
            walk_dir(root, &path, out)?;
        } else if path.ends_with("new/estimates.json") {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            // rel is `<id>/new/estimates.json`; trim the trailing two
            // components to recover the bench id.
            let Some(id) = rel
                .parent()
                .and_then(Path::parent)
                .map(|p| p.to_string_lossy().to_string())
            else {
                continue;
            };
            let bytes =
                fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            let estimates: Estimates = serde_json::from_slice(&bytes)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
            let sample_path = path.with_file_name("sample.json");
            let samples = read_sample_count(&sample_path);
            out.insert(id, CriterionResult { estimates, samples });
        }
    }
    Ok(())
}

/// Read criterion's `sample.json` and return the length of its `iters`
/// array (the per-run sample count). Returns 0 if the file is missing
/// or unparseable — the cell will be `0`, which is obviously wrong
/// rather than silently lying with the historical default of 100.
pub fn read_sample_count(path: &Path) -> u64 {
    let Ok(bytes) = fs::read(path) else {
        return 0;
    };
    serde_json::from_slice::<CriterionSample>(&bytes)
        .map(|s| s.iters.len() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tmp_path;

    #[test]
    fn read_sample_count_returns_iters_length() {
        let path = tmp_path("sample.json");
        fs::write(
            &path,
            r#"{"iters": [1.0, 2.0, 3.0, 4.0, 5.0], "times": []}"#,
        )
        .unwrap();
        let n = read_sample_count(&path);
        let _ = fs::remove_file(&path);
        assert_eq!(n, 5);
    }

    #[test]
    fn read_sample_count_missing_file_is_zero() {
        let path = tmp_path("missing-sample.json");
        assert_eq!(read_sample_count(&path), 0);
    }

    #[test]
    fn read_sample_count_unparseable_is_zero() {
        let path = tmp_path("broken-sample.json");
        fs::write(&path, "not json").unwrap();
        let n = read_sample_count(&path);
        let _ = fs::remove_file(&path);
        assert_eq!(n, 0);
    }

    #[test]
    fn walk_criterion_picks_up_estimates_and_samples() {
        // Build a synthetic tree:
        //   <root>/map/get/100/new/estimates.json
        //   <root>/map/get/100/new/sample.json
        let root = tmp_path("criterion-walk-root");
        let bench_dir = root.join("map").join("get").join("100").join("new");
        fs::create_dir_all(&bench_dir).unwrap();
        fs::write(
            bench_dir.join("estimates.json"),
            r#"{
              "mean":    {"point_estimate": 23.5},
              "median":  {"point_estimate": 22.8},
              "std_dev": {"point_estimate": 4.17}
            }"#,
        )
        .unwrap();
        fs::write(
            bench_dir.join("sample.json"),
            r#"{"iters": [1,2,3,4,5,6,7,8,9,10], "times": []}"#,
        )
        .unwrap();

        let results = walk_criterion(&root).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(results.len(), 1);
        let r = results.get("map/get/100").expect("bench id missing");
        assert!((r.estimates.mean.point_estimate - 23.5).abs() < 1e-9);
        assert_eq!(r.samples, 10);
    }

    #[test]
    fn walk_criterion_empty_root_is_ok_empty() {
        // An existing but empty criterion directory (no bench runs yet)
        // returns an empty map, not an error — `run_diff`/`run_update`
        // then translate "empty" into a setup error, but the walker
        // itself must not conflate "no benches" with "I/O failure".
        let root = tmp_path("criterion-walk-empty");
        fs::create_dir_all(&root).unwrap();
        let results = walk_criterion(&root).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert!(results.is_empty());
    }

    #[test]
    fn walk_criterion_returns_err_on_malformed_estimates() {
        // A corrupted estimates.json must surface as an error (exit 2),
        // not silently disappear into a "bench not run" gap that lets a
        // regression slip past the post-merge diff.
        let root = tmp_path("criterion-walk-malformed");
        let bench_dir = root.join("map").join("get").join("100").join("new");
        fs::create_dir_all(&bench_dir).unwrap();
        fs::write(bench_dir.join("estimates.json"), "not json").unwrap();

        let err = walk_criterion(&root).unwrap_err();
        let _ = fs::remove_dir_all(&root);
        assert!(err.contains("failed to parse"), "got: {err}");
    }
}
