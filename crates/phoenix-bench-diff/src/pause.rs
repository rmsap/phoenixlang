//! GC pause sidecar JSON, emitted at `target/criterion-pause/pause.json`
//! by the `allocation` bench's custom pause harness.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::baseline::PauseRow;
use crate::routing::numeric_suffix;

/// Default location of the pause sidecar, relative to the workspace
/// root. The bench resolves the path via
/// `CARGO_MANIFEST_DIR/../../target` (see `allocation.rs::sidecar_path`),
/// so it always lands in the workspace `target/` directory regardless
/// of which cargo invocation triggered the bench.
pub const DEFAULT_PAUSE_SIDECAR: &str = "target/criterion-pause/pause.json";

/// Pause sidecar JSON schema mirroring the bench's emitter.
#[derive(Debug, Deserialize)]
struct PauseSidecarEntry {
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    samples: u64,
}

/// Read the pause sidecar JSON and turn its rooted-set-size keys into
/// `gc_pause/<n>` bench-ids so the rest of the tool sees a uniform id
/// space. The producer (`allocation.rs::write_pause_sidecar`)
/// serializes a `BTreeMap<usize, _>`, which lands keys as
/// integer-strings (`"1000"`, `"10000"`, …) — these are used verbatim
/// as the parameter component.
///
/// A missing file (`NotFound`) returns `Ok(vec![])` — a bench run
/// that skipped the sidecar (e.g. `cargo test --bench`) is a
/// legitimate "no pause data" case. A read failure other than
/// `NotFound`, or a JSON parse failure, is `Err(message)` so the
/// caller can promote it to exit code `2` rather than silently
/// missing pause regressions.
pub fn read_pause_sidecar(path: &Path) -> Result<Vec<PauseRow>, String> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "failed to read pause sidecar {}: {e}",
                path.display()
            ));
        }
    };
    let map = serde_json::from_slice::<BTreeMap<String, PauseSidecarEntry>>(&bytes)
        .map_err(|e| format!("failed to parse pause sidecar {}: {e}", path.display()))?;
    let mut rows: Vec<PauseRow> = map
        .into_iter()
        .map(|(key, e)| PauseRow {
            id: format!("gc_pause/{key}"),
            p50_ns: e.p50_ns,
            p95_ns: e.p95_ns,
            p99_ns: e.p99_ns,
            max_ns: e.max_ns,
            samples: e.samples,
        })
        .collect();
    // Sort by the numeric suffix so the table column order is
    // monotonic across refreshes — criterion's id sort is lexical
    // (1000 < 100 < 10000), which would jumble the baseline diff.
    rows.sort_by_key(|r| numeric_suffix(&r.id));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tmp_path;

    #[test]
    fn pause_sidecar_round_trips_keys_into_gc_pause_ids() {
        // Mirrors the on-disk shape `allocation.rs::write_pause_sidecar`
        // emits: a `BTreeMap<usize, ScenarioSummary>` serializes to
        // integer-string keys, with each value also carrying `n_rooted`
        // (serde_json ignores the extra field on read).
        let path = tmp_path("pause.json");
        fs::write(
            &path,
            r#"{
              "100000": {"n_rooted": 100000, "p50_ns": 1, "p95_ns": 2,  "p99_ns": 3,  "max_ns": 4,  "samples": 131},
              "1000":   {"n_rooted": 1000,   "p50_ns": 5, "p95_ns": 6,  "p99_ns": 7,  "max_ns": 8,  "samples": 62574},
              "10000":  {"n_rooted": 10000,  "p50_ns": 9, "p95_ns": 10, "p99_ns": 11, "max_ns": 12, "samples": 2499}
            }"#,
        )
        .unwrap();
        let rows = read_pause_sidecar(&path).unwrap();
        let _ = fs::remove_file(&path);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        // Sorted by numeric suffix, not lexically.
        assert_eq!(ids, ["gc_pause/1000", "gc_pause/10000", "gc_pause/100000"]);
    }

    #[test]
    fn pause_sidecar_missing_file_is_ok_empty() {
        let path = tmp_path("missing-sidecar.json");
        assert!(read_pause_sidecar(&path).unwrap().is_empty());
    }

    #[test]
    fn pause_sidecar_malformed_json_returns_err() {
        // A file that exists but isn't valid JSON should fail loudly
        // so the CLI can map it to exit code 2, not silently report
        // "no pause data" and miss a regression.
        let path = tmp_path("pause-malformed.json");
        fs::write(&path, "not json at all").unwrap();
        let err = read_pause_sidecar(&path).unwrap_err();
        let _ = fs::remove_file(&path);
        assert!(err.contains("failed to parse"), "got: {err}");
    }
}
