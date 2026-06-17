#!/usr/bin/env bash
#
# Phase-close WASM-vs-native bench refresh (§Phase 2.4 decision D).
#
# Builds each corpus workload three ways — native, `wasm32-linear`,
# `wasm32-gc` — runs each N times (decision C minimum is 5), and emits
# `docs/perf/phoenix-wasm-vs-native.md`. The actionable datum is "how
# much slower is WASM than native?", which drives Phase 2.5 / Phase 4
# decisions about whether browser-served Phoenix needs a different
# optimization story. Go / tinygo are deliberately out of scope
# (decision D); the cross-language comparison lives in `run.sh`.
#
# Off-CI, same as `run.sh`. Refresh cadence: per-phase-close.
#
# Requirements:
#   - Phoenix workspace built (`cargo build -p phoenix-driver --release`).
#   - `phoenix-runtime` built for wasm32-wasip1 (the wasm32-linear merge
#     target): `cargo build -p phoenix-runtime --target wasm32-wasip1 --release`.
#   - `wasmtime` on PATH (the wasm32-gc column needs >= 24, run with
#     `-W function-references=y,gc=y`).
#   - `hyperfine` on PATH.
#
# Run from the repo root:
#   bash bench-corpus/run-wasm.sh
#   git diff docs/perf/phoenix-wasm-vs-native.md

set -euo pipefail

# --- Toolchain checks -------------------------------------------------------

for tool in cargo hyperfine wasmtime; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: '$tool' not on PATH." >&2
    exit 1
  fi
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS_DIR="$REPO_ROOT/bench-corpus"
OUT_DIR="$REPO_ROOT/docs/perf"
OUT_PATH="$OUT_DIR/phoenix-wasm-vs-native.md"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
mkdir -p "$OUT_DIR" "$SCRATCH/results" "$SCRATCH/out" "$SCRATCH/bin"

# Order here = order in the published table.
WORKLOADS=(fib_recursive sort_ints hash_map_churn alloc_walk_struct)
RUNS="${RUNS:-5}"
WARMUP="${WARMUP:-2}"
GC_FLAGS=(-W function-references=y,gc=y)

# --- Build the toolchain once ----------------------------------------------

echo "Building Phoenix driver + runtimes (release)..."
cargo build -p phoenix-driver --release --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
cargo build -p phoenix-runtime --release --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
cargo build -p phoenix-runtime --target wasm32-wasip1 --release \
  --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
PHOENIX="$REPO_ROOT/target/release/phoenix"

# --- Build + correctness-gate + time each (workload, target) ---------------

# Build a workload for one target, returning the run-command on stdout.
build_one() {
  local workload="$1" target="$2"
  local src="$CORPUS_DIR/$workload/phoenix/main.phx"
  case "$target" in
    native)
      local bin="$SCRATCH/bin/${workload}.native"
      "$PHOENIX" build "$src" -o "$bin" >/dev/null
      printf '%s' "$bin"
      ;;
    wasm32-linear)
      local mod="$SCRATCH/bin/${workload}.linear.wasm"
      "$PHOENIX" build --target wasm32-linear "$src" -o "$mod" >/dev/null
      printf 'wasmtime %s' "$mod"
      ;;
    wasm32-gc)
      local mod="$SCRATCH/bin/${workload}.gc.wasm"
      "$PHOENIX" build --target wasm32-gc "$src" -o "$mod" >/dev/null
      printf 'wasmtime %s %s' "${GC_FLAGS[*]}" "$mod"
      ;;
  esac
}

run_one() {
  local workload="$1" target="$2"
  local cmd; cmd="$(build_one "$workload" "$target")"
  # Correctness gate: stdout must equal the gold output before timing.
  local out="$SCRATCH/out/${workload}_${target}.txt"
  local expected="$CORPUS_DIR/$workload/expected.txt"
  eval "$cmd" >"$out"
  if ! diff -u "$expected" "$out" >/dev/null; then
    echo "error: $target/$workload stdout != $expected:" >&2
    diff -u "$expected" "$out" >&2 || true
    exit 1
  fi
  local json="$SCRATCH/results/${workload}_${target}.json"
  echo "  timing $target/$workload..."
  hyperfine --warmup "$WARMUP" --runs "$RUNS" \
    --export-json "$json" --command-name "$target/$workload" \
    "$cmd" >/dev/null
}

for w in "${WORKLOADS[@]}"; do
  echo "Workload: $w"
  for t in native wasm32-linear wasm32-gc; do
    run_one "$w" "$t"
  done
done

# --- Render docs/perf/phoenix-wasm-vs-native.md -----------------------------

CPU_MODEL="$(lscpu 2>/dev/null | sed -nE 's/^Model name:[[:space:]]+(.*)$/\1/p' | head -1)"
[[ -z "$CPU_MODEL" ]] && CPU_MODEL="unknown"
KERNEL="$(uname -srm 2>/dev/null || echo unknown)"
DATE="$(date -u +%Y-%m-%d)"
PHOENIX_COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
WASMTIME_VERSION="$(wasmtime --version 2>/dev/null | awk '{print $2}' || echo unknown)"

read_metric() {
  local json="$1" key="$2" line
  line="$(grep -E "^[[:space:]]*\"$key\":" "$json" | head -1 || true)"
  [[ -z "$line" ]] && { echo "error: '$key' not in $json" >&2; exit 1; }
  echo "$line" | sed -E 's/.*: ([0-9.eE+-]+),?.*/\1/'
}

fmt_secs() {
  awk -v s="$1" 'BEGIN {
    if (s >= 1)         printf "%.3f s",  s
    else if (s >= 1e-3) printf "%.2f ms", s * 1e3
    else if (s >= 1e-6) printf "%.1f µs", s * 1e6
    else                printf "%.0f ns", s * 1e9
  }'
}

# WASM-to-native ratio: >1 means WASM is slower.
fmt_ratio() {
  awk -v w="$1" -v n="$2" 'BEGIN { if (n <= 0) print "n/a"; else printf "%.1fx", w / n }'
}

{
  echo "<!-- GENERATED by bench-corpus/run-wasm.sh — do not edit by hand. -->"
  echo
  echo "# Phoenix WASM vs native"
  echo
  echo "How much slower is a Phoenix program compiled to WebAssembly than the"
  echo "same program compiled to a native binary? This is the actionable datum"
  echo "from the Phase 2.4 close ([decision D](../design-decisions.md#d-phase-close-bench-refresh-scope-wasm-vs-native-phoenix-only)):"
  echo "it tells us whether browser-served Phoenix needs a different optimization"
  echo "story than native. Go / tinygo-as-WASM are deliberately **out of scope**"
  echo "(a \"Phoenix vs Go-in-WASM\" column would mislead — tinygo is not Go); the"
  echo "cross-language comparison lives in [phoenix-vs-go.md](phoenix-vs-go.md)."
  echo
  echo "## Snapshot"
  echo
  echo "- **Date (UTC):** $DATE — Phoenix \`$PHOENIX_COMMIT\`"
  echo "- **CPU:** $CPU_MODEL"
  echo "- **Kernel:** $KERNEL"
  echo "- **Runtime:** \`wasmtime $WASMTIME_VERSION\` (wasm32-gc run with \`-W function-references=y,gc=y\`)"
  echo "- **Method:** \`hyperfine\` mean ± stddev, $WARMUP warmup + $RUNS timed runs per binary (decision C minimum is 5). Each row times the *whole process* — for the WASM columns that includes \`wasmtime\` startup + JIT, which is the honest end-to-end cost of running the workload as WASM."
  echo
  echo "## Workload results"
  echo
  echo "| workload | native | wasm32-linear | wasm32-gc | linear / native | gc / native |"
  echo "|---|---|---|---|---|---|"
  for w in "${WORKLOADS[@]}"; do
    nmean="$(read_metric "$SCRATCH/results/${w}_native.json" mean)"
    lmean="$(read_metric "$SCRATCH/results/${w}_wasm32-linear.json" mean)"
    gmean="$(read_metric "$SCRATCH/results/${w}_wasm32-gc.json" mean)"
    nsd="$(read_metric "$SCRATCH/results/${w}_native.json" stddev)"
    lsd="$(read_metric "$SCRATCH/results/${w}_wasm32-linear.json" stddev)"
    gsd="$(read_metric "$SCRATCH/results/${w}_wasm32-gc.json" stddev)"
    echo "| $w | $(fmt_secs "$nmean") ± $(fmt_secs "$nsd") | $(fmt_secs "$lmean") ± $(fmt_secs "$lsd") | $(fmt_secs "$gmean") ± $(fmt_secs "$gsd") | $(fmt_ratio "$lmean" "$nmean") | $(fmt_ratio "$gmean" "$nmean") |"
  done
  echo
  echo "## Reading the numbers"
  echo
  echo "- The four workloads are compute / allocation / map-churn / sort — the"
  echo "  same corpus as the cross-language doc. They're compute-only and miss"
  echo "  the web-framework workloads that are Phoenix's actual pitch (Phase 4)."
  echo "- The WASM columns include a fixed \`wasmtime\` startup + JIT-compile cost"
  echo "  (single-digit to low-tens of ms) paid once per process. On these"
  echo "  ~50-100 ms workloads that startup is a visible slice of the ratio, so"
  echo "  the slowdown is **smaller** for longer-running programs than the table"
  echo "  suggests. A steady-state (in-process, startup-excluded) measurement is"
  echo "  the right refinement when a workload's absolute runtime starts to matter."
  echo "- Two algorithmic gaps the earlier WASM backends carried — surfaced by"
  echo "  this very refresh, since the corpus is the first thing to exercise"
  echo "  collections at 100k scale — were closed during the Phase 2.4 close:"
  echo "  - **\`List.sortBy\`** now uses the same O(n log n) bottom-up merge sort"
  echo "    on all backends; earlier WASM builds shipped an O(n²) insertion"
  echo "    sort, making \`sort_ints\` (100k elements) ~1000× slower."
  echo "  - **\`wasm32-gc\` \`Map\`** gained an open-addressing hash *index* over"
  echo "    its still-insertion-ordered key/value arrays ([K.9](../design-decisions.md#k9-wasm32-gc-mapkv-ordered-association-over-parallel-arrays-not-a-hash-table)),"
  echo "    making \`get\` / \`contains\` and construction O(1); the prior ordered"
  echo "    array's O(n) linear scan made \`hash_map_churn\` (200k lookups over a"
  echo "    100k-entry map) ~380× slower. (\`wasm32-linear\` was already O(1) — it"
  echo "    merges the native runtime's hash table.) Output is byte-identical"
  echo "    across all backends; only the lookup speed changed."
  echo
  echo "## Reproducing locally"
  echo
  echo '```sh'
  echo "# Requires: cargo, wasmtime (>= 24), hyperfine on PATH."
  echo "cargo build -p phoenix-runtime --target wasm32-wasip1 --release"
  echo "bash bench-corpus/run-wasm.sh"
  echo "git diff docs/perf/phoenix-wasm-vs-native.md"
  echo '```'
} >"$OUT_PATH"

echo
echo "Wrote $OUT_PATH"
