# `bench-corpus/` — cross-language comparison workloads

Paired Phoenix and Go programs for the off-CI comparison documented in [`docs/perf/phoenix-vs-go.md`](../docs/perf/phoenix-vs-go.md). Scope locked by [Phase 2.7 design decision E](../docs/design-decisions.md#e-cross-language-comparison-scope-go-122-only):

- **One comparator: Go 1.22+.** Java / .NET / TypeScript / Rust explicitly considered and declined — see decision E for the reasoning.
- **Four workloads:** `fib_recursive`, `sort_ints`, `hash_map_churn`, `alloc_walk_struct`. New workloads must update [`docs/design-decisions.md`](../docs/design-decisions.md#e-cross-language-comparison-scope-go-122-only) — the locked-scope wording prevents drive-by additions.
- **Off-CI, informational only.** Refreshed at each phase close (2.7, then 2.4, then 2.5 — listed in decision order, which is also their chronological order in the roadmap). Not a regression gate — Phoenix-vs-Phoenix numbers in [`docs/perf-baselines/`](../docs/perf-baselines/) remain the gating signal.

## Layout

```
bench-corpus/
├── README.md             ← this file
├── run.sh                ← runner; writes docs/perf/phoenix-vs-go.md
├── <workload>/
│   ├── README.md         ← workload invariants + expected output (human-readable)
│   ├── expected.txt      ← gold stdout the runner diffs both impls against
│   ├── phoenix/main.phx
│   └── go/main.go
```

Each workload's `README.md` documents its expected stdout, what it isolates, and any language-specific asymmetries baked into the comparison. The `expected.txt` file is the machine-readable form of the same expected stdout — `run.sh` diffs both the Phoenix and Go binary's output against it before timing, so any drift in either implementation or in the documented gold aborts the refresh.

## Toolchain requirements

- **Phoenix** — the workspace is the source of truth. `run.sh` invokes `cargo build -p phoenix-driver --release` and uses the resulting `target/release/phoenix build` to compile each `main.phx`.
- **Go 1.22+** — `slices.Sort` (added in 1.21) is used in `sort_ints`; pinning at 1.22 leaves a one-minor-version cushion.
- **[hyperfine](https://github.com/sharkdp/hyperfine)** — `run.sh` defers timing to hyperfine because its statistics output is more useful than `/usr/bin/time -v`'s and it handles warmup + min-runs cleanly. Install:
  - apt: `apt install hyperfine`
  - brew: `brew install hyperfine`
  - cargo: `cargo install hyperfine`
- **bash + awk + sed** — `run.sh` is bash (uses `[[`, arrays, `sed -nE`). No Python / Rust dependency for the runner itself.

## Refresh procedure

```sh
# From repo root.
bash bench-corpus/run.sh
git diff docs/perf/phoenix-vs-go.md
# Commit if the deltas look sane.
```

The script aborts loudly if any toolchain is missing or out-of-version; a partial refresh is never written.

## Why this is off-CI

Two reasons (both decision E):

1. **Cost.** The Phoenix `sort_ints` and `hash_map_churn` workloads currently take ~25 s and ~100 s respectively due to the immutable-container build cost (call-out in each workload's `README.md`). Running these on every PR would add minutes to CI; the comparison is informational, so the cost isn't justified.
2. **Signal/noise.** GitHub-hosted runners have wildly variable noise neighbors. The Phoenix-vs-Phoenix gate (`bench.yml`, decision B) tolerates this via the 20% per-bench threshold. The cross-language ratio is more sensitive to runner drift — a Go binary unaffected by a noisy neighbor while a Phoenix binary is would invert the ratio temporarily. Phase-close refresh on a quiet machine gives a cleaner number.

## Adding a workload

Adding a fifth workload requires:

1. A design-decisions.md amendment (decision E pins the four). The reviewer's bar should be "is the new workload predictive of a Phoenix UX decision a user would make today?" — not "is it interesting".
2. Both `phoenix/main.phx` and `go/main.go` matching the existing structure: deterministic input, byte-for-byte equal stdout, recorded expected output in the workload's `README.md` *and* in `expected.txt` (the runner enforces the latter).
3. An entry in `WORKLOADS` in [`run.sh`](run.sh) — order there fixes the order in the published table.
