//! Per-bench markdown baseline tables: parsing and writing.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::routing::{combine_id, split_id};

const THROUGHPUT_HEADER: &str =
    "| bench | parameters | mean (ns) | median (ns) | stddev (ns) | samples |";
const THROUGHPUT_SEPARATOR: &str = "|---|---|---|---|---|---|";
const PAUSE_HEADER: &str =
    "| bench | parameters | p50 (ns) | p95 (ns) | p99 (ns) | max (ns) | samples |";
const PAUSE_SEPARATOR: &str = "|---|---|---|---|---|---|---|";

/// One row of a throughput baseline.
#[derive(Debug, Clone)]
pub struct ThroughputRow {
    /// Criterion bench-id (`<bench>/<parameters>` recombined).
    pub id: String,
    /// Mean wall-clock per iteration, in nanoseconds.
    pub mean_ns: f64,
    /// Median wall-clock per iteration, in nanoseconds.
    pub median_ns: f64,
    /// Standard deviation, in nanoseconds.
    pub stddev_ns: f64,
    /// Per-run sample count from criterion's `sample.json`.
    pub samples: u64,
}

/// One row of the pause baseline.
#[derive(Debug, Clone)]
pub struct PauseRow {
    /// `gc_pause/<n>` bench-id, where `<n>` is the rooted-set size.
    pub id: String,
    /// 50th-percentile pause duration, in nanoseconds.
    pub p50_ns: u64,
    /// 95th-percentile pause duration, in nanoseconds.
    pub p95_ns: u64,
    /// 99th-percentile pause duration, in nanoseconds.
    pub p99_ns: u64,
    /// Maximum observed pause duration, in nanoseconds.
    pub max_ns: u64,
    /// Number of pause samples observed in this run.
    pub samples: u64,
}

/// Parse a throughput-style baseline markdown table. Skips header /
/// separator rows; ignores everything that doesn't look like a data
/// row (so prose paragraphs in the same file are harmless).
///
/// Row format:
/// `| <id> | <params> | <mean_ns> | <median_ns> | <stddev_ns> | <samples> |`
///
/// `<id>` and `<params>` are joined with `/` to recover the bench-id
/// criterion produces. A row whose numeric fields don't parse is
/// rejected — the parser refuses to silently misread a typo'd
/// baseline.
///
/// `NotFound` → `Ok(vec![])` (a baseline file that doesn't exist yet
/// is a legitimate "no committed data" state). Every other read error
/// is `Err`, so the CLI maps it to exit code `2` instead of letting an
/// unreadable baseline silently produce an empty parse — which would
/// label every bench as "new bench, no baseline" and miss real
/// regressions. Symmetric with [`crate::pause::read_pause_sidecar`].
pub fn parse_throughput_baseline(path: &Path) -> Result<Vec<ThroughputRow>, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!("failed to read baseline {}: {e}", path.display()));
        }
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            continue;
        }
        // Strip leading + trailing '|', then split on internal '|'.
        let body = trimmed.trim_start_matches('|').trim_end_matches('|');
        let cells: Vec<&str> = body.split('|').map(str::trim).collect();
        if cells[0].eq_ignore_ascii_case("bench") || cells[0].starts_with("---") {
            continue;
        }
        if cells.len() != 6 {
            // A `|`-leading row with the wrong column count is most
            // likely a hand-edit slip (stray `|` in a cell, missing
            // column). Warn so it doesn't disappear into the
            // "ignored prose" bucket.
            eprintln!(
                "warning: {} row has {} cells (expected 6), skipping: {trimmed}",
                path.display(),
                cells.len()
            );
            continue;
        }
        let id = combine_id(cells[0], cells[1]);
        let mean = cells[2].parse::<f64>();
        let median = cells[3].parse::<f64>();
        let stddev = cells[4].parse::<f64>();
        let samples = cells[5].parse::<u64>();
        match (mean, median, stddev, samples) {
            (Ok(mean_ns), Ok(median_ns), Ok(stddev_ns), Ok(samples)) => out.push(ThroughputRow {
                id,
                mean_ns,
                median_ns,
                stddev_ns,
                samples,
            }),
            _ => eprintln!(
                "warning: {} contains a malformed row, skipping: {trimmed}",
                path.display()
            ),
        }
    }
    Ok(out)
}

/// Parse the pause baseline markdown table. Row format:
/// `| <id> | <params> | <p50_ns> | <p95_ns> | <p99_ns> | <max_ns> | <samples> |`
///
/// Error policy matches [`parse_throughput_baseline`]: `NotFound` →
/// `Ok(vec![])`; every other read error → `Err`.
pub fn parse_pause_baseline(path: &Path) -> Result<Vec<PauseRow>, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "failed to read pause baseline {}: {e}",
                path.display()
            ));
        }
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            continue;
        }
        let body = trimmed.trim_start_matches('|').trim_end_matches('|');
        let cells: Vec<&str> = body.split('|').map(str::trim).collect();
        if cells[0].eq_ignore_ascii_case("bench") || cells[0].starts_with("---") {
            continue;
        }
        if cells.len() != 7 {
            eprintln!(
                "warning: {} pause row has {} cells (expected 7), skipping: {trimmed}",
                path.display(),
                cells.len()
            );
            continue;
        }
        let id = combine_id(cells[0], cells[1]);
        let p50 = cells[2].parse::<u64>();
        let p95 = cells[3].parse::<u64>();
        let p99 = cells[4].parse::<u64>();
        let max = cells[5].parse::<u64>();
        let samples = cells[6].parse::<u64>();
        match (p50, p95, p99, max, samples) {
            (Ok(p50_ns), Ok(p95_ns), Ok(p99_ns), Ok(max_ns), Ok(samples)) => out.push(PauseRow {
                id,
                p50_ns,
                p95_ns,
                p99_ns,
                max_ns,
                samples,
            }),
            _ => eprintln!(
                "warning: {} contains a malformed pause row, skipping: {trimmed}",
                path.display()
            ),
        }
    }
    Ok(out)
}

/// Write a throughput baseline file from a list of rows. Errors
/// propagate to the caller; `cmd_update` maps them to exit code `2`
/// per the contract documented at the crate root.
pub fn write_throughput_baseline(path: &Path, rows: &[ThroughputRow]) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("baseline");
    writeln!(
        f,
        "# {}\n\n\
         Per-bench mean/median/stddev. Refresh via `phoenix-bench-diff update`.\n\n\
         All times in **nanoseconds**. See `README.md` for the runner spec and \
         the refresh procedure.\n",
        stem
    )?;
    writeln!(f, "{}", THROUGHPUT_HEADER)?;
    writeln!(f, "{}", THROUGHPUT_SEPARATOR)?;
    for row in rows {
        let (bench, params) = split_id(&row.id);
        writeln!(
            f,
            "| {bench} | {params} | {:.2} | {:.2} | {:.2} | {} |",
            row.mean_ns, row.median_ns, row.stddev_ns, row.samples
        )?;
    }
    Ok(())
}

/// Write the pause baseline file from a list of rows. Errors
/// propagate to the caller.
pub fn write_pause_baseline(path: &Path, rows: &[PauseRow]) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    writeln!(
        f,
        "# pause\n\n\
         GC pause distribution: P50 / P95 / P99 / max per rooted-object \
         scenario.  Refresh via `phoenix-bench-diff update`.\n\n\
         All times in **nanoseconds**. See `README.md` for the runner spec.\n"
    )?;
    writeln!(f, "{}", PAUSE_HEADER)?;
    writeln!(f, "{}", PAUSE_SEPARATOR)?;
    for row in rows {
        let (bench, params) = split_id(&row.id);
        writeln!(
            f,
            "| {bench} | {params} | {} | {} | {} | {} | {} |",
            row.p50_ns, row.p95_ns, row.p99_ns, row.max_ns, row.samples
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tmp_path;

    #[test]
    fn throughput_baseline_round_trips() {
        let rows = vec![
            ThroughputRow {
                id: "map/get/100".into(),
                mean_ns: 23.51,
                median_ns: 22.80,
                stddev_ns: 4.17,
                samples: 100,
            },
            ThroughputRow {
                id: "empty/parse".into(),
                mean_ns: 190.19,
                median_ns: 181.24,
                stddev_ns: 43.75,
                samples: 100,
            },
        ];
        let path = tmp_path("throughput.md");
        write_throughput_baseline(&path, &rows).unwrap();
        let parsed = parse_throughput_baseline(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(parsed.len(), rows.len());
        for (orig, got) in rows.iter().zip(parsed.iter()) {
            assert_eq!(orig.id, got.id);
            assert!((orig.mean_ns - got.mean_ns).abs() < 1e-9);
            assert!((orig.median_ns - got.median_ns).abs() < 1e-9);
            assert!((orig.stddev_ns - got.stddev_ns).abs() < 1e-9);
            assert_eq!(orig.samples, got.samples);
        }
    }

    #[test]
    fn pause_baseline_round_trips() {
        let rows = vec![PauseRow {
            id: "gc_pause/1000".into(),
            p50_ns: 83619,
            p95_ns: 152080,
            p99_ns: 344697,
            max_ns: 9800048,
            samples: 62574,
        }];
        let path = tmp_path("pause.md");
        write_pause_baseline(&path, &rows).unwrap();
        let parsed = parse_pause_baseline(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(parsed.len(), 1);
        let got = &parsed[0];
        let orig = &rows[0];
        assert_eq!(orig.id, got.id);
        assert_eq!(orig.p50_ns, got.p50_ns);
        assert_eq!(orig.p95_ns, got.p95_ns);
        assert_eq!(orig.p99_ns, got.p99_ns);
        assert_eq!(orig.max_ns, got.max_ns);
        assert_eq!(orig.samples, got.samples);
    }

    #[test]
    fn parse_baseline_skips_malformed_rows_without_aborting() {
        let path = tmp_path("malformed.md");
        fs::write(
            &path,
            "| bench | parameters | mean (ns) | median (ns) | stddev (ns) | samples |\n\
             |---|---|---|---|---|---|\n\
             | map/get | 100 | not-a-number | 22.80 | 4.17 | 100 |\n\
             | map/get | 1000 | 16.64 | 15.64 | 2.72 | 100 |\n",
        )
        .unwrap();
        let parsed = parse_throughput_baseline(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "map/get/1000");
    }

    #[test]
    fn missing_baseline_file_is_silent_empty() {
        let path = tmp_path("does-not-exist.md");
        assert!(parse_throughput_baseline(&path).unwrap().is_empty());
        assert!(parse_pause_baseline(&path).unwrap().is_empty());
    }

    #[test]
    fn unreadable_baseline_file_propagates_error() {
        // A baseline file that exists but cannot be read (here: a
        // directory at the file path, which `read_to_string` rejects
        // with an error distinct from `NotFound`) must surface as
        // `Err` so the CLI maps it to exit code 2. Previously this
        // path silently returned an empty parse, which would make
        // every committed bench look "new" and miss every regression.
        let path = tmp_path("baseline-as-dir");
        fs::create_dir_all(&path).unwrap();
        let err = parse_throughput_baseline(&path).unwrap_err();
        assert!(err.contains("failed to read baseline"), "got: {err}");
        let err = parse_pause_baseline(&path).unwrap_err();
        let _ = fs::remove_dir_all(&path);
        assert!(err.contains("failed to read pause baseline"), "got: {err}");
    }
}
