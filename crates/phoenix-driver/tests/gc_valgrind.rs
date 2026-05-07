//! Verifies that compiled Phoenix programs terminate **leak-clean**
//! under valgrind: zero "definitely lost", "indirectly lost", and
//! "possibly lost" bytes after each fixture runs.
//!
//! Companion to `gc_bounded_memory.rs` — that test catches "GC stopped
//! reclaiming during execution" via `RLIMIT_AS`; this one catches "GC
//! reclaims during execution but leaks at termination" via valgrind on
//! the compiled binary's exit. The two together pin both ends of the
//! GC's lifetime contract.
//!
//! Tracks against the **definitely / indirectly /
//! possibly** lost categories. The "still reachable" category may
//! contain process-lifetime allocations (e.g. Rust's stdout buffer)
//! but is bounded — we assert it stays under a small cap so a
//! regression that accumulated heap headers in the static would still
//! surface.
//!
//! Linux-only. Other platforms either lack valgrind or have it as an
//! out-of-tree port; CI is expected to run Linux for this gate.
//! Skipped with a `println!` when `valgrind` is not on `$PATH` so dev
//! machines without it are not blocked, *unless*
//! `PHOENIX_REQUIRE_VALGRIND` is set — CI sets that variable so a
//! missing valgrind is a hard failure instead of a silent bypass.

#![cfg(target_os = "linux")]

mod common;

use common::compiled_fixtures::build_fixture;
use std::process::Command;

/// `true` if `valgrind --version` runs successfully. Used to skip the
/// gate on dev machines without valgrind installed instead of
/// hard-failing — CI environments install valgrind explicitly and
/// set `PHOENIX_REQUIRE_VALGRIND` to opt out of the silent skip.
fn valgrind_available() -> bool {
    Command::new("valgrind")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// CI sets this env var to turn the "valgrind not on PATH" silent skip
/// into a hard failure, so a misconfigured CI image cannot bypass the
/// gate. Local dev leaves it unset so the test self-skips when
/// valgrind isn't installed.
fn valgrind_required() -> bool {
    std::env::var_os("PHOENIX_REQUIRE_VALGRIND").is_some()
}

/// Default cap on "still reachable" bytes after a fixture exits. The
/// only contributors today are Rust's stdout buffer (~1024 bytes) and
/// any process-lifetime allocations from the GC's `OnceLock`. 2 KiB
/// is ~2× the measured baseline — tight enough to catch a regression
/// that accumulates ~1 KiB of headers in the static, loose enough to
/// absorb stdout-buffer drift.
///
/// **Calibration risk.** A future libc / Rust runtime change could
/// shift the baseline. If that happens, raise the cap with a doc
/// note (or set the env override below in CI) — do not silently
/// relax the bound.
///
/// Local debugging escape hatch:
/// `PHOENIX_GC_VALGRIND_REACHABLE_CAP_BYTES=8192 cargo test ...`
/// overrides the cap without editing this file. Same pattern as
/// `PHOENIX_GC_RLIMIT_BYTES` in `gc_bounded_memory.rs`.
const DEFAULT_STILL_REACHABLE_CAP_BYTES: u64 = 2 * 1024;

fn still_reachable_cap_bytes() -> u64 {
    match std::env::var("PHOENIX_GC_VALGRIND_REACHABLE_CAP_BYTES") {
        Ok(s) => s.parse().unwrap_or_else(|e| {
            panic!(
                "PHOENIX_GC_VALGRIND_REACHABLE_CAP_BYTES must be a u64 byte \
                 count (no suffix); got {s:?}: {e}"
            )
        }),
        Err(_) => DEFAULT_STILL_REACHABLE_CAP_BYTES,
    }
}

/// Build `fixture`, run it under valgrind, and assert each leak
/// category is at or under its bound. Returns early with a `println!`
/// skip message if valgrind isn't on `$PATH` and
/// `PHOENIX_REQUIRE_VALGRIND` is unset.
fn assert_fixture_leak_clean(fixture: &str) {
    if !valgrind_available() {
        let msg = format!(
            "valgrind not on PATH; skipping {fixture} valgrind gate. CI \
             must set PHOENIX_REQUIRE_VALGRIND=1 to turn this into a hard \
             failure."
        );
        if valgrind_required() {
            panic!("{msg}");
        }
        // Cargo's default harness captures both stdout and stderr, so
        // this only surfaces under `--nocapture`. The real safety net
        // is `PHOENIX_REQUIRE_VALGRIND` — a CI image without valgrind
        // hard-fails rather than silently passing.
        println!("{msg}");
        return;
    }
    let bin = build_fixture(fixture, "phoenix_gc_vg");

    // `--error-exitcode=99` makes memcheck *errors* (UAF, OOB, etc.)
    // fail the run with a distinguishable code, but NOT plain leaks —
    // leaks are reported in stderr without changing the exit code.
    // The success/non-zero check below treats any non-zero code the
    // same; the `99` is documentation of intent, not a value the test
    // matches against.
    //
    // `--num-callers=20` widens valgrind's stacktrace depth. It only
    // matters in the failure-stderr embedded in panics — green runs
    // pay nothing — and 4 frames is too shallow to identify the
    // offending GC site if a regression hits.
    let output = Command::new("valgrind")
        .args([
            "--leak-check=full",
            "--show-leak-kinds=definite,indirect,possible",
            "--error-exitcode=99",
            "--track-origins=no",
            "--num-callers=20",
        ])
        .arg(&bin.0)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn valgrind: {e}"));

    let stderr = String::from_utf8_lossy(&output.stderr);

    // First: the program itself must have exited cleanly (so we know
    // the GC did not abort under valgrind's tighter heap discipline).
    if !output.status.success() {
        panic!(
            "valgrind run of {fixture} exited non-zero (code {:?}); \
             valgrind likely reported a memcheck error.\n  stderr:\n{}",
            output.status.code(),
            stderr
        );
    }

    // Second: assert each leak category is within its bound. valgrind
    // always emits all four lines in the LEAK SUMMARY block; if the
    // format changes, `parse_leak_summary` panics rather than
    // returning a false-zero.
    let summary = parse_leak_summary(&stderr);
    let cap = still_reachable_cap_bytes();

    assert_eq!(
        summary.definitely_lost, 0,
        "{fixture} leaked {} definitely-lost bytes — GC is no longer \
         leak-clean at process exit. valgrind stderr:\n{stderr}",
        summary.definitely_lost
    );
    assert_eq!(
        summary.indirectly_lost, 0,
        "{fixture} leaked {} indirectly-lost bytes (a definitely-lost \
         block holds pointers to these). valgrind stderr:\n{stderr}",
        summary.indirectly_lost
    );
    assert_eq!(
        summary.possibly_lost, 0,
        "{fixture} leaked {} possibly-lost bytes — valgrind found \
         pointer-like words into mid-block addresses. Likely a GC \
         root was lost mid-collection or the heap stored interior \
         pointers without a header. valgrind stderr:\n{stderr}",
        summary.possibly_lost
    );
    assert!(
        summary.still_reachable <= cap,
        "{fixture} left {} bytes still-reachable at exit (cap is {cap}). \
         The cap exists because process-lifetime allocations (Rust \
         stdout buffer, GC singleton) are unavoidable, but a growing \
         still-reachable pool means the GC is accumulating headers \
         in the static heap. valgrind stderr:\n{stderr}",
        summary.still_reachable,
    );
}

#[test]
fn alloc_loop_terminates_leak_clean_under_valgrind() {
    assert_fixture_leak_clean("alloc_loop.phx");
}

/// Catches GC root-tracking bugs in the loop-carried-ref shape that
/// `alloc_loop.phx` doesn't exercise: a ref-typed value carried
/// across a loop back-edge as a block parameter, with the
/// auto-collect threshold tripping many times while it's the only
/// live root chain. If a regression broke `emit_block_param_roots`
/// *and* still produced the right stdout, the matrix would miss it;
/// this gate ensures the surviving pointers also leave no leak at
/// termination.
#[test]
fn gc_loop_carried_ref_terminates_leak_clean_under_valgrind() {
    assert_fixture_leak_clean("gc_loop_carried_ref.phx");
}

/// Smokes `defer`-cleanup leak-cleanliness. The fixture is tiny (no
/// allocation in the body), so this is a fast pass over the defer
/// state machine — it will fail if a future change to the deferred-
/// callback machinery (lazy-capture closures, multi-defer LIFO
/// teardown) leaks the captured state at exit.
#[test]
fn defer_basic_terminates_leak_clean_under_valgrind() {
    assert_fixture_leak_clean("defer_basic.phx");
}

/// Companion to `gc_keeps_alive_stays_under_address_space_limit` in
/// `gc_bounded_memory.rs`: that test catches "GC stops reclaiming
/// during execution" via `RLIMIT_AS`; this one catches the other end
/// of the contract — that the long-lived `keep` reference and the
/// many-times-collected throwaway allocations both leave the heap
/// leak-clean at exit.
#[test]
fn gc_keeps_alive_terminates_leak_clean_under_valgrind() {
    assert_fixture_leak_clean("gc_keeps_alive.phx");
}

/// Parsed leak-summary byte counts. All four categories are always
/// emitted by `--leak-check=full`; we read them all and let callers
/// assert per-category bounds.
struct LeakSummary {
    definitely_lost: u64,
    indirectly_lost: u64,
    possibly_lost: u64,
    still_reachable: u64,
}

/// Pull the four leak-summary byte counts out of valgrind's stderr.
///
/// Anchors the parse to the substring after `LEAK SUMMARY:` so a
/// future verbose mode that prints `<category>: ` in a per-leak
/// detail block above the summary cannot match first.
///
/// When *every* heap block was freed before exit, valgrind suppresses
/// the `LEAK SUMMARY:` block entirely and emits the sentinel `"All
/// heap blocks were freed -- no leaks are possible"` instead. Treat
/// that as all-zero — it's a stricter outcome than what the cap
/// allows, not a parse failure.
fn parse_leak_summary(stderr: &str) -> LeakSummary {
    if stderr.contains("All heap blocks were freed -- no leaks are possible") {
        return LeakSummary {
            definitely_lost: 0,
            indirectly_lost: 0,
            possibly_lost: 0,
            still_reachable: 0,
        };
    }
    let summary_start = stderr.find("LEAK SUMMARY:").unwrap_or_else(|| {
        panic!(
            "valgrind output did not contain a `LEAK SUMMARY:` block \
             nor the `no leaks are possible` sentinel. stderr:\n{stderr}"
        )
    });
    let summary = &stderr[summary_start..];
    LeakSummary {
        definitely_lost: parse_leak_line(summary, "definitely lost"),
        indirectly_lost: parse_leak_line(summary, "indirectly lost"),
        possibly_lost: parse_leak_line(summary, "possibly lost"),
        still_reachable: parse_leak_line(summary, "still reachable"),
    }
}

/// Pull the byte count out of one of valgrind's leak-summary lines.
///
/// Lines look like `==123==    definitely lost: 1,234 bytes in 5 blocks`.
/// We strip thousands separators and parse the first integer after
/// `<category>: `. A missing line, an empty token, or a non-numeric
/// token all panic — silent fallbacks would mask a real regression.
fn parse_leak_line(summary: &str, category: &str) -> u64 {
    let needle = format!("{category}: ");
    for line in summary.lines() {
        let Some(idx) = line.find(&needle) else {
            continue;
        };
        let rest = &line[idx + needle.len()..];
        let Some(token) = rest.split_whitespace().next() else {
            panic!(
                "leak-summary line for `{category}` had no byte-count \
                 token after the category label: {line:?}"
            );
        };
        let cleaned: String = token.chars().filter(|c| *c != ',').collect();
        return cleaned.parse::<u64>().unwrap_or_else(|e| {
            panic!(
                "could not parse byte count from {line:?} \
                 (token = {token:?}, cleaned = {cleaned:?}): {e}"
            )
        });
    }
    panic!(
        "valgrind output did not contain a `{category}:` line within \
         the LEAK SUMMARY block — the format may have changed. \
         summary:\n{summary}"
    );
}
