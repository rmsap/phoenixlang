//! Bench regression detector for Phoenix.
//!
//! Reads criterion's per-bench `estimates.json` (and the pause bench's
//! JSON sidecar) and either:
//!
//! - **`diff`** — compares the latest run against the committed
//!   baselines in `docs/perf-baselines/`. Exits non-zero if any bench
//!   regressed by more than the threshold (default 20%). Used by the
//!   post-merge `bench.yml` workflow.
//! - **`update`** — overwrites the baseline files from the latest
//!   run. Run by a maintainer after an intentional perf-affecting
//!   change, then committed alongside the change.
//!
//! Per Phase 2.7 design decision A, the committed baseline format is a
//! per-bench markdown table; the diff tool parses the tables by
//! splitting on `|` (no regex), so a typo in a baseline file is a
//! parser-rejected row, not a silent drift. Per design decision B the
//! 20% slack is per-bench, not per-group.
//!
//! ## Exit codes
//!
//! - `0` — clean run, no regressions.
//! - `1` — at least one bench regressed beyond the threshold.
//! - `2` — tool/setup error (missing criterion output, baseline
//!   directory unreadable, write failure, etc.). Distinct from `1` so
//!   the CI gate can open an issue only on real regressions, not on
//!   infrastructure problems that should fail the workflow loudly
//!   instead.
//!
//! See `docs/perf-baselines/README.md` for the on-disk format.

#![warn(missing_docs)]

pub mod baseline;
pub mod commands;
pub mod criterion_walk;
pub mod pause;
pub mod routing;

#[cfg(test)]
mod test_support;
