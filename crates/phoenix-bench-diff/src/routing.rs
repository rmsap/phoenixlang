//! Routes criterion bench-ids to the baseline file that owns them.

/// Per-bench mapping from baseline file name to the criterion-ID
/// prefixes it owns. A criterion bench whose full ID starts with one
/// of the prefixes routes to the matching baseline file.
///
/// Lookup is first-match-in-declaration-order via
/// [`find_baseline_for`]. Today there is no cross-file prefix overlap
/// (`medium/` and `medium_large/` both route to `pipeline.md`, and
/// `"medium_large/x"` does not start with `"medium/"` because `_` is
/// not `/`), so first-match and longest-match are equivalent. If a
/// future routing rule introduces real cross-file overlap, replace
/// [`find_baseline_for`] with a longest-prefix scan.
///
/// This table is also the source-of-truth for "which baseline files
/// exist": `commands::run_diff` iterates it to find files to read.
/// A baseline file added without an entry here will be invisible to
/// `diff` even if it lives in the baseline directory.
pub const BASELINE_ROUTING: &[(&str, &[&str])] = &[
    ("allocation.md", &["alloc_throughput/"]),
    ("collections.md", &["map/", "sort_by/"]),
    // `gc_pause/*` is intentionally absent from criterion routing:
    // pause percentiles live in the JSON sidecar produced by the
    // allocation bench, not in criterion's mean/median output. See
    // `crate::pause`.
    (
        "pipeline.md",
        &["empty/", "small/", "medium/", "medium_large/", "large/"],
    ),
];

/// Filename inside the baseline directory that owns the pause stats.
/// Treated specially because the source-of-truth is a JSON sidecar
/// (see `phoenix-bench/benches/allocation.rs::write_pause_sidecar`),
/// not criterion's own estimates.
pub const PAUSE_BASELINE: &str = "pause.md";

/// Map a criterion bench-id to the baseline filename that owns it.
/// `None` if no prefix matches. See [`BASELINE_ROUTING`] for the
/// resolution rule.
pub fn find_baseline_for(id: &str) -> Option<&'static str> {
    BASELINE_ROUTING
        .iter()
        .find(|(_, prefixes)| prefixes.iter().any(|p| id.starts_with(p)))
        .map(|(file, _)| *file)
}

/// Re-assemble a criterion bench-id from the `bench` cell and
/// `parameters` cell of a baseline row. When `parameters` is `-` or
/// empty, the id is the bench cell alone; otherwise the two are joined
/// with `/`.
pub fn combine_id(bench: &str, parameters: &str) -> String {
    if parameters.is_empty() || parameters == "-" {
        bench.to_string()
    } else {
        format!("{bench}/{parameters}")
    }
}

/// Split a criterion bench-id back into the `(bench, parameters)`
/// cells the baseline format wants. The last `/`-delimited segment is
/// taken as the parameters cell when it's purely numeric (matches the
/// IDs criterion emits for `BenchmarkId::new`/`from_parameter`);
/// otherwise the whole id is the bench cell with `-` for parameters.
pub fn split_id(id: &str) -> (String, String) {
    if let Some(idx) = id.rfind('/') {
        let (head, tail) = id.split_at(idx);
        let tail = &tail[1..]; // skip the '/'
        if tail.chars().all(|c| c.is_ascii_digit()) {
            return (head.to_string(), tail.to_string());
        }
    }
    (id.to_string(), "-".to_string())
}

/// Parse a numeric suffix from a `/`-delimited bench id. `u64::MAX` is
/// the fallback when the trailing segment is not purely numeric — it
/// sorts non-numeric tails to the end of a numeric sort.
pub fn numeric_suffix(id: &str) -> u64 {
    id.rsplit('/')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_id_handles_numeric_param() {
        assert_eq!(split_id("map/get/100"), ("map/get".into(), "100".into()));
        assert_eq!(
            split_id("alloc_throughput/unknown/16"),
            ("alloc_throughput/unknown".into(), "16".into())
        );
    }

    #[test]
    fn split_id_falls_back_for_non_numeric() {
        assert_eq!(split_id("medium/lex"), ("medium/lex".into(), "-".into()));
        assert_eq!(split_id("empty/parse"), ("empty/parse".into(), "-".into()));
    }

    #[test]
    fn combine_id_treats_empty_parameters_like_dash() {
        // The baseline format uses `-` for parameter-less benches, but
        // `combine_id` accepts an empty parameters cell too so a
        // hand-edited baseline (e.g. an accidentally blank cell) still
        // recovers a valid id rather than producing `"empty/parse/"`.
        assert_eq!(combine_id("empty/parse", ""), "empty/parse");
        assert_eq!(combine_id("empty/parse", "-"), "empty/parse");
    }

    #[test]
    fn combine_id_round_trips_split_id() {
        for id in [
            "map/get/100",
            "sort_by/10000",
            "alloc_throughput/string/64",
            "medium/lex",
            "empty/parse",
        ] {
            let (bench, params) = split_id(id);
            assert_eq!(
                combine_id(&bench, &params),
                id,
                "round-trip mismatch for {id}"
            );
        }
    }

    #[test]
    fn find_baseline_routes_known_prefixes() {
        assert_eq!(
            find_baseline_for("alloc_throughput/unknown/16"),
            Some("allocation.md")
        );
        assert_eq!(find_baseline_for("map/get/100"), Some("collections.md"));
        assert_eq!(find_baseline_for("sort_by/1000"), Some("collections.md"));
        assert_eq!(find_baseline_for("medium/lex"), Some("pipeline.md"));
        assert_eq!(find_baseline_for("large/full_compile"), Some("pipeline.md"));
        // `gc_pause/*` is handled out-of-band via the sidecar, not by
        // BASELINE_ROUTING — its absence here is intentional.
        assert_eq!(find_baseline_for("gc_pause/1000"), None);
        assert_eq!(find_baseline_for("foo/bar"), None);
    }

    #[test]
    fn numeric_suffix_sorts_pause_scenarios_correctly() {
        let ids = ["gc_pause/100000", "gc_pause/1000", "gc_pause/10000"];
        let mut sorted: Vec<_> = ids.to_vec();
        sorted.sort_by_key(|i| numeric_suffix(i));
        assert_eq!(
            sorted,
            ["gc_pause/1000", "gc_pause/10000", "gc_pause/100000"]
        );
    }
}
