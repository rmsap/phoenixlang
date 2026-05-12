//! Shared fixture helpers for the crate's inline test modules. Only
//! compiled under `cfg(test)`; not part of the public surface.

#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique temp path under the system temp dir. Does not create the path.
pub fn tmp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("phoenix-bench-diff-{nonce}-{name}"));
    p
}

/// [`tmp_path`] + `create_dir_all`.
pub fn tmp_dir(name: &str) -> PathBuf {
    let p = tmp_path(name);
    fs::create_dir_all(&p).unwrap();
    p
}

/// Write a synthetic criterion `estimates.json` + `sample.json` pair
/// under `<criterion_dir>/<id>/new/`. Matches the on-disk layout that
/// [`crate::criterion_walk::walk_criterion`] expects.
pub fn write_synthetic_bench(
    criterion_dir: &Path,
    id: &str,
    mean: f64,
    median: f64,
    std_dev: f64,
    sample_count: usize,
) {
    let bench_dir = criterion_dir.join(id).join("new");
    fs::create_dir_all(&bench_dir).unwrap();
    fs::write(
        bench_dir.join("estimates.json"),
        format!(
            r#"{{
              "mean":    {{"point_estimate": {mean}}},
              "median":  {{"point_estimate": {median}}},
              "std_dev": {{"point_estimate": {std_dev}}}
            }}"#
        ),
    )
    .unwrap();
    let iters: Vec<String> = (0..sample_count).map(|i| (i + 1).to_string()).collect();
    fs::write(
        bench_dir.join("sample.json"),
        format!(r#"{{"iters": [{}], "times": []}}"#, iters.join(",")),
    )
    .unwrap();
}
