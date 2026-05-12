# Phoenix performance baselines

Per [Phase 2.7 design decision A](../design-decisions.md#a-baseline-storage-strategy-manual-snapshot-in-docsperf-baselines), the committed snapshot of Phoenix's benchmark numbers lives here as per-bench markdown tables. The post-merge [`.github/workflows/bench.yml`](../../.github/workflows/bench.yml) diff runs against these files via `phoenix-bench-diff`, so a maintainer who cuts a regression can read off both the offending bench and the prior steady-state number from one place.

## Files

| file | benches | columns |
|---|---|---|
| [`allocation.md`](allocation.md) | `phx_gc_alloc` throughput (4 sizes × 2 tags) | mean / median / stddev / samples |
| [`pause.md`](pause.md) | GC pause distribution (1k / 10k / 100k rooted) | P50 / P95 / P99 / max / samples |
| [`collections.md`](collections.md) | `Map.{get,set,remove}` × 4 sizes; `List.sortBy` × 3 sizes | mean / median / stddev / samples |
| [`pipeline.md`](pipeline.md) | Per-stage compile times + `compile_and_run` × 5 fixtures | mean / median / stddev / samples |

Aggregate columns per [design decision D](../design-decisions.md#d-aggregate-choice): throughput benches keep criterion's mean/median/stddev defaults; pause benches keep P50/P95/P99/max so worst-case stalls aren't averaged away.

## Format

Every row is one bench cell:

```
| <bench-prefix> | <parameters> | <mean (ns)> | <median (ns)> | <stddev (ns)> | <samples> |
```

The `<bench-prefix>` + `<parameters>` cells round-trip to the criterion ID format (`<group>/<function>/<param>`). A parameter-less bench writes `-` in the parameters cell.

Pause rows substitute `P50 / P95 / P99 / max` for `mean / median / stddev`.

All times are in **nanoseconds**, no unit suffixes — so the diff tool's parser can be a plain `split('|')` + `.parse::<f64>()` with no regex.

## Runner spec

Numbers are sensitive to host environment. The current baseline was captured on:

- **CPU:** AMD Ryzen 7 7735HS (16 threads), x86_64
- **OS / kernel:** WSL2 (Linux 6.6.87.2-microsoft-standard-WSL2), Ubuntu 22.04
- **glibc:** 2.35
- **rustc:** 1.94.1 (e408947bf, 2026-03-25), release profile
- **criterion:** 0.5.1, html_reports feature on
- **CPU governor:** default (WSL2 doesn't expose `cpufreq`); single-threaded runs per [decision C](../design-decisions.md#c-calibration-and-runner-constraints)

A refresh from the post-merge CI runner will replace these. Per [decision C](../design-decisions.md#c-calibration-and-runner-constraints) the runner spec is committed alongside the numbers so a future "the numbers got worse" investigation can rule out runner drift before chasing a real regression.

## Refresh procedure

Run after an intentional perf-affecting change, or at phase close:

```sh
# 1. Quiesce the machine (close browsers / IDEs / Slack). On Linux,
#    `sudo cpupower frequency-set -g performance` if you have it.
# 2. Build the runtime so `compile_and_run` benches can link.
cargo build -p phoenix-runtime --release
# 3. Run the bench suite. `--warm-up-time` / `--measurement-time` left at
#    criterion defaults unless variance is unworkable per decision C.
cargo bench -p phoenix-bench
# 4. Refresh the committed snapshot.
cargo run -p phoenix-bench-diff --release -- update
# 5. Inspect the diff before committing.
git diff -- docs/perf-baselines/
```

## Regression-detection contract

`phoenix-bench-diff diff` distinguishes three outcomes via exit code:

- `0` — clean run.
- `1` — at least one throughput bench's mean or one pause bench's P50/P95/P99/max regressed by more than 20% relative to the committed baseline ([design decision B](../design-decisions.md#b-ci-gating-policy-post-merge-on-main)). Slack applies **per-bench, not per-group** so an improvement in one cell can't hide a regression in another.
- `2` — tool / setup error (no criterion output, baseline directory unreadable). The CI workflow only files a regression issue on exit `1`, so infrastructure failures fail the workflow loudly instead of generating a misleading "regression" issue.

A bench that exists in this run but not in the committed baseline ("new bench, no baseline") prints to the summary but does *not* fail the diff — refreshing the baseline is a separate action.

### Known-noisy benches

- `gc_pause/100000` keeps a low sample count (~130 measured pauses per run) because each iteration touches 100k rooted objects. P99 / max for this row is essentially one data point and *will* flake at the 20% threshold even on a well-behaved runner. Until decision C's CPU-governor pin is in place on the CI host, treat alerts on the 100k row as advisory; once `BENCH_ENFORCE=1` flips, this row may need its own per-bench slack.

## Noise-floor gate

Enforcement is OFF by default. There is **no automatic counter** — a maintainer must manually edit `BENCH_ENFORCE` to `"1"` in `bench.yml`'s env block to turn alerts on. Until that flip, every run records numbers and prints the diff in the job log but does **not** open issues. The convention is to wait until ~3 post-merge runs have been observed so the runner's variance band is visible before alerts start firing.

The fallback in [decision C](../design-decisions.md#c-calibration-and-runner-constraints) — drop to informational-only if the runner keeps flaking — is the escape hatch if 20% slack still produces noisy alerts after the noise floor is established.
