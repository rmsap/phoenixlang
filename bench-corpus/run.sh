#!/usr/bin/env bash
#
# Cross-language comparison runner for Phoenix vs Go.
#
# Builds each workload in both languages, runs each binary 5 times
# (decision C minimum), and emits `docs/perf/phoenix-vs-go.md`.
#
# Off-CI per Phase 2.7 design decision E. Refresh cadence: per-phase
# close (2.7, then 2.4, then 2.5 — decision order, also chronological).
# The script is intentionally bash + awk rather than a Rust binary —
# the work is "build A, build B, time both N times, format markdown"
# and Rust would add a build-graph dependency for no robustness win.
#
# Requirements:
#   - Phoenix workspace built (the script invokes `cargo run -p phoenix-driver`).
#   - Go 1.22+ on PATH.
#   - hyperfine on PATH (used for the timing loop — its statistics
#     are nicer than `/usr/bin/time -v`'s, and it handles warmup +
#     min/max runs cleanly).
#
# Run from the repo root:
#   bash bench-corpus/run.sh
#
# Output:
#   docs/perf/phoenix-vs-go.md  — overwritten with the latest numbers.

set -euo pipefail

# --- Toolchain checks -------------------------------------------------------

REQUIRED_GO_MINOR=22

if ! command -v go >/dev/null 2>&1; then
  echo "error: 'go' not on PATH. Install Go 1.${REQUIRED_GO_MINOR}+." >&2
  exit 1
fi

GO_VERSION_RAW="$(go version)"
# Expected: `go version go1.NN.M linux/amd64`
GO_MINOR="$(echo "$GO_VERSION_RAW" | sed -nE 's/^go version go1\.([0-9]+).*/\1/p')"
if [[ -z "$GO_MINOR" || "$GO_MINOR" -lt "$REQUIRED_GO_MINOR" ]]; then
  echo "error: Go 1.${REQUIRED_GO_MINOR}+ required (decision E); got: $GO_VERSION_RAW" >&2
  exit 1
fi

if ! command -v hyperfine >/dev/null 2>&1; then
  echo "error: 'hyperfine' not on PATH. Install via your package manager" >&2
  echo "       (apt: hyperfine, brew: hyperfine, cargo: cargo install hyperfine)." >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: 'cargo' not on PATH." >&2
  exit 1
fi

# --- Locations --------------------------------------------------------------

# Resolve paths relative to repo root regardless of where the script
# was invoked from. `realpath --relative-to` keeps the published md
# tidy, but we only need absolute paths for the build / run.
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS_DIR="$REPO_ROOT/bench-corpus"
OUT_DIR="$REPO_ROOT/docs/perf"
OUT_PATH="$OUT_DIR/phoenix-vs-go.md"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

mkdir -p "$OUT_DIR"

# Workload list. Order here = order in the published table.
WORKLOADS=(fib_recursive sort_ints hash_map_churn alloc_walk_struct)

# Timed-run count per binary. Decision C pins the minimum at 5; the
# slow workloads (sort_ints ~25 s, hash_map_churn ~100 s) benefit from
# more samples on a quiet machine — override via `RUNS=10 bash run.sh`.
RUNS="${RUNS:-5}"
WARMUP="${WARMUP:-1}"

# --- Build Phoenix driver once ----------------------------------------------

echo "Building Phoenix driver (release)..."
cargo build -p phoenix-driver --release --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
PHOENIX_DRIVER="$REPO_ROOT/target/release/phoenix"

# --- Build + time each workload --------------------------------------------

# Hyperfine JSON outputs land here so the awk renderer below can read
# both mean and stddev without re-running.
mkdir -p "$SCRATCH/results"

run_one() {
  local workload="$1"
  local lang="$2"        # phoenix | go
  local src_dir="$CORPUS_DIR/$workload/$lang"
  local bin_path="$SCRATCH/bin/${workload}_${lang}"
  mkdir -p "$(dirname "$bin_path")"

  case "$lang" in
    phoenix)
      "$PHOENIX_DRIVER" build "$src_dir/main.phx" -o "$bin_path" >/dev/null
      ;;
    go)
      (cd "$src_dir" && go build -o "$bin_path" .)
      ;;
    *)
      echo "internal error: unknown lang '$lang'" >&2
      exit 2
      ;;
  esac

  # Correctness gate: run once and diff stdout against expected.txt
  # (the per-workload README's "Expected output" block, kept as a
  # machine-readable file). Catches both impl drift and doc drift —
  # without this, hyperfine would happily time a program that prints
  # garbage. Done before the timing loop so a failure aborts the
  # whole refresh and no partial markdown is written.
  local out_path="$SCRATCH/out/${workload}_${lang}.txt"
  local expected="$CORPUS_DIR/$workload/expected.txt"
  mkdir -p "$(dirname "$out_path")"
  "$bin_path" > "$out_path"
  if [[ ! -f "$expected" ]]; then
    echo "error: $expected is missing; every workload must ship a gold output" >&2
    exit 1
  fi
  if ! diff -u "$expected" "$out_path" >/dev/null; then
    echo "error: $lang/$workload stdout does not match $expected:" >&2
    diff -u "$expected" "$out_path" >&2 || true
    exit 1
  fi

  # hyperfine: $WARMUP warmup + $RUNS timed runs (defaults 1 + 5, the
  # decision C minimum). Capture JSON so we can pull mean / stddev
  # from the awk renderer.
  local json="$SCRATCH/results/${workload}_${lang}.json"
  echo "  timing $lang/$workload..."
  hyperfine --warmup "$WARMUP" --runs "$RUNS" \
    --export-json "$json" \
    --command-name "$lang/$workload" \
    "$bin_path" >/dev/null
}

for w in "${WORKLOADS[@]}"; do
  echo "Workload: $w"
  run_one "$w" "phoenix"
  run_one "$w" "go"
done

# --- Render docs/perf/phoenix-vs-go.md --------------------------------------

CPU_MODEL="$(lscpu 2>/dev/null | sed -nE 's/^Model name:[[:space:]]+(.*)$/\1/p' | head -1)"
[[ -z "$CPU_MODEL" ]] && CPU_MODEL="unknown"
# Hostname is deliberately not captured — under CI the rendered doc
# would carry a meaningless GHA-runner identifier (e.g. `fv-az1234-456`)
# that adds noise without value. CPU / kernel / commit suffice for
# attribution.
KERNEL="$(uname -srm 2>/dev/null || echo unknown)"
DATE="$(date -u +%Y-%m-%d)"
PHOENIX_COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
GO_VERSION="$(echo "$GO_VERSION_RAW" | awk '{print $3}')"

# Pull mean / stddev (units = seconds) for a given hyperfine JSON.
# Schema is `{"results":[{"...","mean":X,"stddev":X,...}]}`. The
# regex anchors `"key":` at the start of its line (after whitespace)
# so it can't match a future field whose name happens to contain
# `mean` / `stddev` as a substring. Aborts loudly on a missing key
# rather than silently emitting an empty string.
#
# Brittle: relies on hyperfine emitting pretty-printed JSON (one key
# per line). If `--export-json` ever switches to compact single-line
# output, swap this for `jq -r '.results[0].mean'`.
read_metric() {
  local json="$1"
  local key="$2"
  local line
  line="$(grep -E "^[[:space:]]*\"$key\":" "$json" | head -1 || true)"
  if [[ -z "$line" ]]; then
    echo "error: '$key' not found in $json" >&2
    exit 1
  fi
  echo "$line" | sed -E 's/.*: ([0-9.eE+-]+),?.*/\1/'
}

# Format seconds as "12.345 s" or "234.5 ms" or "456 µs" — readable
# without losing precision.
fmt_secs() {
  awk -v s="$1" 'BEGIN {
    if (s >= 1)        printf "%.3f s",  s
    else if (s >= 1e-3) printf "%.2f ms", s * 1e3
    else if (s >= 1e-6) printf "%.1f µs", s * 1e6
    else                printf "%.0f ns", s * 1e9
  }'
}

# Phoenix-to-Go ratio: >1 means Phoenix is slower.
fmt_ratio() {
  awk -v p="$1" -v g="$2" 'BEGIN {
    if (g <= 0) print "n/a"
    else printf "%.1fx", p / g
  }'
}

{
  cat <<'EOF'
<!-- Auto-generated by bench-corpus/run.sh — do not hand-edit;
     edits will be overwritten on the next refresh. -->
EOF
  echo "# Phoenix vs Go — cross-language comparison"
  echo
  # Underscore-wrapped markdown italics. `${DATE}_` (with braces) so
  # the trailing `_` isn't parsed as part of an identifier — without
  # braces, bash reads this as the variable `DATE_` and `set -u`
  # aborts the whole renderer.
  echo "_Published: ${DATE}_  ·  Phoenix commit: \`$PHOENIX_COMMIT\`  ·  CPU: $CPU_MODEL  ·  Kernel: $KERNEL  ·  Go: \`$GO_VERSION\`_"
  echo
  # Backticks inside this `{ ... } > $OUT_PATH` block must be
  # `\``-escaped — inside `""` bash treats `…` as command substitution.
  echo "Per [Phase 2.7 design decision E](../design-decisions.md#e-cross-language-comparison-scope-go-122-only), this page is **informational only** — Phoenix-vs-Phoenix numbers (see [\`docs/perf-baselines/\`](../perf-baselines/)) remain the gating signal for regressions. Refresh cadence: per-phase close (2.7, 2.4, 2.5 each once). Refreshed by [\`bench-corpus/run.sh\`](../../bench-corpus/run.sh)."
  echo
  # --- Snapshot caveats ---------------------------------------------------
  # All render-time deviations (Go version, host machine, CPU governor)
  # collected in one block so a reader can size up "is this snapshot
  # apples-to-apples with the previous one" without scrolling. Decision
  # E pins the canonical Go column at 1.22.x; strict match — `go1.22`
  # exactly or `go1.22.<anything>` (the bare `go1.22*` prefix would
  # also match a hypothetical `go1.220`).
  echo "## Snapshot caveats"
  echo
  echo "Render-time conditions that affect how this snapshot compares against earlier ones or against absolute targets. Distinct from the language-level [Known asymmetries](#known-asymmetries-in-the-existing-workloads) below."
  echo
  if [[ "$GO_VERSION" != "go1.22" && "$GO_VERSION" != go1.22.* ]]; then
    echo "- **Go version drift.** Rendered on \`$GO_VERSION\`; per [decision E](../design-decisions.md#e-cross-language-comparison-scope-go-122-only) the canonical pin is **Go 1.22.x**. The next CI refresh from [\`bench-corpus.yml\`](../../.github/workflows/bench-corpus.yml) will repin; the version drift isn't a regression."
  else
    echo "- **Go version.** \`$GO_VERSION\` — matches the [decision E](../design-decisions.md#e-cross-language-comparison-scope-go-122-only) canonical pin."
  fi
  echo "- **Host machine drift across snapshots.** The renderer captures CPU / kernel / Phoenix commit in the header but not hostname (a GHA-runner identifier would be noise). When the Phoenix column moves between two snapshots, check the header to confirm the underlying machine is the same before reading the delta as a Phoenix-side regression or win — see decision C for the dedicated-runner work that closes this."
  echo "- **No CPU-governor pin.** Neither WSL2 (local refreshes) nor GitHub-hosted shared-tenant VMs (CI refreshes via [\`bench-corpus.yml\`](../../.github/workflows/bench-corpus.yml)) expose \`cpufreq\` reliably; expect ±10-20 % drift across refreshes until a dedicated runner is wired up (decision C). The Phoenix/Go ratio is more stable than the absolute numbers because both columns absorb runner noise symmetrically."
  echo
  echo "## Workload results"
  echo
  echo "Each row: hyperfine mean ± stddev over $WARMUP warmup + $RUNS timed runs (decision C minimum is 5). Ratio is Phoenix mean / Go mean — values > 1× mean Phoenix is slower."
  echo
  echo "| workload | Phoenix | Go | Phoenix / Go |"
  echo "|---|---|---|---|"
  for w in "${WORKLOADS[@]}"; do
    phx_json="$SCRATCH/results/${w}_phoenix.json"
    go_json="$SCRATCH/results/${w}_go.json"
    phx_mean="$(read_metric "$phx_json" mean)"
    phx_stddev="$(read_metric "$phx_json" stddev)"
    go_mean="$(read_metric "$go_json" mean)"
    go_stddev="$(read_metric "$go_json" stddev)"
    echo "| $w | $(fmt_secs "$phx_mean") ± $(fmt_secs "$phx_stddev") | $(fmt_secs "$go_mean") ± $(fmt_secs "$go_stddev") | $(fmt_ratio "$phx_mean" "$go_mean") |"
  done
  echo
  echo "## What's not benchmarked yet"
  echo
  echo "Phoenix's pitch is a web-framework-friendly GC language; the four workloads above are compute-only and miss the actual differentiators that matter for that pitch. **Do not extrapolate from these numbers to \"Phoenix is just slower than Go.\"** The following workloads are conspicuous absences:"
  echo
  echo "- **HTTP server throughput** — req/s under load against a small handler. Phoenix has no HTTP server yet (Phase 4 stdlib)."
  echo "- **JSON encode / decode** — the bread-and-butter of any web service. Phoenix has no JSON support yet."
  echo "- **Concurrent workloads** — goroutine fan-out vs Phoenix's eventual async runtime (Phase 4.3). Phoenix is single-threaded in Phase 2."
  echo "- **String-heavy work** — header parsing, URL routing, templating. Phoenix has the primitives but not the stdlib coverage."
  echo
  echo "Each of these will get added to the corpus as the corresponding Phoenix stdlib chunk lands."
  echo
  echo "## Known asymmetries in the existing workloads"
  echo
  echo "Language-design choices that bias the Phoenix column. Distinct from render-time deviations (see [Snapshot caveats](#snapshot-caveats) above) — these would persist across machine / Go-version changes."
  echo
  echo "- **\`sort_ints\` and \`hash_map_churn\` use \`ListBuilder<T>\` / \`MapBuilder<K, V>\`** (Phase 2.7 decision F). Both workloads build their input via the transient-mutable accumulator — O(n) build + one O(n) freeze — instead of the prior O(n²) repeated-immutable-allocation shape. Pre-builder numbers (the same workloads on \`main\` before decision F landed) had Phoenix at 1900× / 6900× slower than Go on these two cells; the current ratios reflect comparable algorithmic work on both sides. Linearity / move-semantics for a future \`xs = xs.push(v)\` style is decision G and deferred to Phase 4+."
  echo "- **\`alloc_walk_struct\` doesn't measure \"1M alive concurrently\".** Phoenix has no efficient bulk-container, so each iteration's \`Point\` becomes unrooted as the next overwrites it; auto-collect reclaims periodically. The Go counterpart uses the same per-iter pattern (composite literal in a tight loop) but Go's escape analysis may stack-allocate, biasing the comparison against Phoenix. Both deviations from the literal phase-2 scope are documented per workload in [\`bench-corpus/<workload>/README.md\`](../../bench-corpus/)."
  echo
  echo "## Reproducing locally"
  echo
  echo '```sh'
  echo '# Requires: cargo, Go 1.22+, hyperfine on PATH.'
  echo 'bash bench-corpus/run.sh'
  echo '# Then inspect:'
  echo 'git diff docs/perf/phoenix-vs-go.md'
  echo '```'
} > "$OUT_PATH"

echo
echo "Wrote $OUT_PATH"
