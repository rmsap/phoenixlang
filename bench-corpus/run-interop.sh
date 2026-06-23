#!/usr/bin/env bash
#
# Phase-close `extern js` interop boundary-cost bench refresh (§Phase 2.5 PR 18).
#
# Measures the per-call cost of crossing the `extern js` host-FFI boundary on
# both WASM backends, under Node (the always-on interop host — decision A). Three
# micro-workloads, each a 1,000,000-iteration loop, isolate the cost by
# subtraction:
#
#   interop_pure_call        an ordinary intra-WASM call — no boundary  (baseline)
#   interop_noop_extern      a bare extern crossing, no marshalling
#   interop_string_roundtrip an extern that round-trips a String (out + in)
#
# Per-call cost is a *difference* between two workloads' whole-process times
# divided by the iteration count: the subtraction cancels the fixed node-startup
# + glue-`instantiate` + loop-scaffold overhead the three share, leaving just the
# boundary work. That is why the absolute whole-process numbers (dominated by
# node startup) are reported only for transparency — the per-call deltas are the
# signal. The headline figure is `string_roundtrip − pure_call`: the cost of a
# string round-trip vs. a pure-WASM call (the comparison the exit criterion
# names).
#
# Native interop is out of scope here: the released `phoenix build` produces a
# standalone binary whose extern symbols are weak aborting stubs (a host shim is
# linked only by the in-tree native interop tests), so there is no CLI path to a
# runnable native-interop binary to time. The browser-relevant cost is the JS
# boundary anyway — which is exactly what this measures. Native correctness is
# covered by the five-backend matrix, not perf.
#
# Off-CI, same as run.sh / run-wasm.sh. Refresh cadence: per-phase-close.
#
# Requirements:
#   - Phoenix workspace built (`cargo build -p phoenix-driver --release`).
#   - `phoenix-runtime` built for wasm32-wasip1 (the wasm32-linear merge target):
#     `cargo build -p phoenix-runtime --target wasm32-wasip1 --release`.
#   - `node` on PATH (with WasmGC support — node >= 22; the gc glue uses it).
#   - `hyperfine` on PATH.
#
# Run from the repo root:
#   bash bench-corpus/run-interop.sh
#   git diff docs/perf/phoenix-interop-boundary.md

set -euo pipefail

# --- Toolchain checks -------------------------------------------------------

for tool in cargo node hyperfine; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: '$tool' not on PATH." >&2
    exit 1
  fi
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS_DIR="$REPO_ROOT/bench-corpus"
OUT_DIR="$REPO_ROOT/docs/perf"
OUT_PATH="$OUT_DIR/phoenix-interop-boundary.md"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
mkdir -p "$OUT_DIR" "$SCRATCH/results" "$SCRATCH/out"

# Order here = order in the published tables.
WORKLOADS=(interop_pure_call interop_noop_extern interop_string_roundtrip)
TARGETS=(wasm32-linear wasm32-gc)
# Decision C fixes 5 as the minimum timed-run count for a published refresh, and
# the rendered doc hard-codes that "minimum is 5" claim. RUNS stays overridable
# upward (a noisy host may want more), but is clamped at the floor so a
# `RUNS=3` run can't publish numbers whose own method line contradicts them.
RUNS="${RUNS:-5}"
if (( RUNS < 5 )); then
  echo "note: RUNS=$RUNS is below the decision-C floor; clamping to 5." >&2
  RUNS=5
fi
WARMUP="${WARMUP:-2}"
# The loop count compiled into every workload's phoenix/main.phx (a literal —
# Phoenix programs take no runtime input, so it cannot be injected at build
# time). ITERS is the per-call divisor and the doc's "loops N times" figure, NOT
# a tunable knob: changing it here without editing all three sources in lockstep
# would divide the whole-process delta by the wrong count and silently publish
# bad per-call numbers (the correctness gate wouldn't catch it — expected.txt
# tracks the source, not ITERS). The guard below enforces the lockstep the
# workload READMEs document.
readonly ITERS=1000000
for w in "${WORKLOADS[@]}"; do
  # Format-sensitive on purpose: it matches the exact `i < <digits>` the
  # workloads are written with. An empty capture therefore means the loop was
  # reformatted (spacing changed, or a `1_000_000`-style underscore literal),
  # not that the count is absent — call that out so the failure is actionable.
  src_iters="$(grep -oE 'i < [0-9]+' "$CORPUS_DIR/$w/phoenix/main.phx" \
    | grep -oE '[0-9]+' || true)"
  if [[ -z "$src_iters" ]]; then
    echo "error: $w/main.phx: no 'i < <count>' loop matched — the bound was" \
         "reformatted (spacing or an underscore literal). Restore the literal" \
         "form or update this guard's regex." >&2
    exit 1
  fi
  if [[ "$src_iters" != "$ITERS" ]]; then
    echo "error: $w/main.phx loops '$src_iters' but ITERS=$ITERS —" \
         "per-call math would be wrong. Update them in lockstep." >&2
    exit 1
  fi
done

# The Node driver: instantiate the glue with the workload's host stub, run, and
# print the program's stdout. Lives beside the built app.wasm / app.js / host.mjs
# so every import is a sibling. (Same shape as the interop test driver in
# crates/phoenix-driver/tests/common/interop.rs.)
read -r -d '' DRIVER_MJS <<'EOF' || true
import { readFile } from "node:fs/promises";
import { instantiate } from "./app.js";
import { host as makeHost } from "./host.mjs";

const wasm = await readFile(new URL("./app.wasm", import.meta.url));
let out = "";
// `emit` is the host's output-ordering channel (ctx.emit in the real interop
// driver). None of the three interop_* host stubs use it — their `print` output
// arrives via writeStdout — but it is still passed so this driver is
// functionally identical to crates/phoenix-driver/tests/common/interop.rs's
// harness (same imports, instantiate shape, and output buffering; only these
// explanatory comments differ), which the subtraction relies on for a matched
// fixed overhead.
const emit = (t) => { out += t; };
const { run } = await instantiate({
  wasm,
  host: makeHost({ emit }),
  writeStdout: (t, _fd) => { out += t; },
});
run();
process.stdout.write(out);
EOF

# --- Build the toolchain once ----------------------------------------------

echo "Building Phoenix driver + wasm runtime (release)..."
cargo build -p phoenix-driver --release --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
cargo build -p phoenix-runtime --target wasm32-wasip1 --release \
  --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
PHOENIX="$REPO_ROOT/target/release/phoenix"

# --- Build + correctness-gate + time each (workload, target) ---------------

# Build a workload for one target into its own dir (app.wasm + app.js glue +
# host.mjs + driver.mjs), and echo the run-command on stdout.
build_one() {
  local workload="$1" target="$2"
  local src="$CORPUS_DIR/$workload/phoenix/main.phx"
  local d="$SCRATCH/$workload.$target"
  mkdir -p "$d"
  "$PHOENIX" build --target "$target" "$src" -o "$d/app.wasm" >/dev/null
  if [[ ! -f "$d/app.js" ]]; then
    echo "error: $target build of $workload produced no .js glue sidecar" >&2
    exit 1
  fi
  cp "$CORPUS_DIR/$workload/host.mjs" "$d/host.mjs"
  printf '%s' "$DRIVER_MJS" > "$d/driver.mjs"
  printf 'node %s' "$d/driver.mjs"
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
  for t in "${TARGETS[@]}"; do
    run_one "$w" "$t"
  done
done

# --- Render docs/perf/phoenix-interop-boundary.md ---------------------------

CPU_MODEL="$(lscpu 2>/dev/null | sed -nE 's/^Model name:[[:space:]]+(.*)$/\1/p' | head -1)"
[[ -z "$CPU_MODEL" ]] && CPU_MODEL="unknown"
KERNEL="$(uname -srm 2>/dev/null || echo unknown)"
DATE="$(date -u +%Y-%m-%d)"
PHOENIX_COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
NODE_VERSION="$(node --version 2>/dev/null || echo unknown)"

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

# Per-call cost = (mean_a − mean_b) / ITERS, formatted. A clamp at 0 guards the
# rare case where run-to-run noise makes a near-equal pair come out slightly
# negative (e.g. pure vs. noop on a noisy host) — a negative per-call cost is
# meaningless, so report it as the floor.
per_call() {
  local a="$1" b="$2" d
  d="$(awk -v a="$a" -v b="$b" -v n="$ITERS" 'BEGIN { v=(a-b)/n; if (v<0) v=0; printf "%.12f", v }')"
  fmt_secs "$d"
}

# Pull the means we need for the per-call table.
pure_l="$(read_metric "$SCRATCH/results/interop_pure_call_wasm32-linear.json" mean)"
pure_g="$(read_metric "$SCRATCH/results/interop_pure_call_wasm32-gc.json" mean)"
noop_l="$(read_metric "$SCRATCH/results/interop_noop_extern_wasm32-linear.json" mean)"
noop_g="$(read_metric "$SCRATCH/results/interop_noop_extern_wasm32-gc.json" mean)"
str_l="$(read_metric "$SCRATCH/results/interop_string_roundtrip_wasm32-linear.json" mean)"
str_g="$(read_metric "$SCRATCH/results/interop_string_roundtrip_wasm32-gc.json" mean)"

# Friendly labels for the whole-process table (keep the dir name discoverable).
declare -A LABEL=(
  [interop_pure_call]="pure-WASM call (baseline, no boundary)"
  [interop_noop_extern]="bare extern crossing (no marshalling)"
  [interop_string_roundtrip]="String round-trip (out + in)"
)

{
  echo "<!-- GENERATED by bench-corpus/run-interop.sh — do not edit by hand. -->"
  echo
  echo "# Phoenix \`extern js\` interop — boundary cost"
  echo
  echo "How expensive is it to cross the \`extern js\` host-FFI boundary, and how"
  echo "much of that is value marshalling? This is the actionable datum from the"
  echo "Phase 2.5 close: it tells us whether a browser-served Phoenix program that"
  echo "leans on host/DOM calls pays a per-call tax worth optimizing. Measured on"
  echo "**both WASM backends** under **Node** (the always-on interop host —"
  echo "[decision A](../design-decisions.md#a-host-set--gating-node-always-on-browser-gated));"
  echo "the boundary is the same JS crossing in a browser). Native interop is out"
  echo "of scope (the released \`phoenix build\` links no host shim, so there is no"
  echo "CLI path to a runnable native-interop binary; the matrix covers native"
  echo "*correctness*, not perf)."
  echo
  echo "## Snapshot"
  echo
  echo "- **Date (UTC):** $DATE — Phoenix \`$PHOENIX_COMMIT\`"
  echo "- **CPU:** $CPU_MODEL"
  echo "- **Kernel:** $KERNEL"
  echo "- **Host:** \`node $NODE_VERSION\` (WasmGC native; the gc glue needs node >= 22)"
  echo "- **Method:** \`hyperfine\` mean ± stddev, $WARMUP warmup + $RUNS timed runs per workload (decision C minimum is 5). Each workload loops $(printf "%'d" "$ITERS" 2>/dev/null || echo "$ITERS") times. Per-call cost is a *difference* of whole-process means divided by the iteration count — the subtraction cancels the fixed node-startup + glue-\`instantiate\` overhead the three workloads share."
  echo
  echo "## Per-call boundary cost (the signal)"
  echo
  echo "Each figure is \`(meanₐ − meanᵦ) / $(printf "%'d" "$ITERS" 2>/dev/null || echo "$ITERS")\`, isolating one layer of the crossing:"
  echo
  echo "| per-call cost | wasm32-linear | wasm32-gc |"
  echo "|---|---|---|"
  echo "| bare boundary crossing (noop − pure) | $(per_call "$noop_l" "$pure_l") | $(per_call "$noop_g" "$pure_g") |"
  echo "| **String round-trip vs. pure call (string − pure)** | **$(per_call "$str_l" "$pure_l")** | **$(per_call "$str_g" "$pure_g")** |"
  echo "| ↳ String marshalling only (string − noop) | $(per_call "$str_l" "$noop_l") | $(per_call "$str_g" "$noop_g") |"
  echo
  echo "The bold row is the headline the exit criterion names — the per-call cost"
  echo "of a \`String\` round-trip across the boundary vs. a call that never leaves"
  echo "WASM. It decomposes into the bare crossing (row 1) plus the marshalling"
  echo "(row 3); marshalling is where the two backends' \`String\` strategies differ"
  echo "(linear copies via its handle table + \`phx_string_alloc\`; WASM-GC copies"
  echo "via its scratch region)."
  echo
  echo "## Whole-process times (for transparency)"
  echo
  echo "These are dominated by a fixed node-startup + \`instantiate\` cost (tens of"
  echo "ms) paid once per process — which is exactly what the per-call table above"
  echo "subtracts out. They are not a per-call cost; they show the raw measurements"
  echo "the deltas come from."
  echo
  echo "| workload | wasm32-linear | wasm32-gc |"
  echo "|---|---|---|"
  for w in "${WORKLOADS[@]}"; do
    lmean="$(read_metric "$SCRATCH/results/${w}_wasm32-linear.json" mean)"
    gmean="$(read_metric "$SCRATCH/results/${w}_wasm32-gc.json" mean)"
    lsd="$(read_metric "$SCRATCH/results/${w}_wasm32-linear.json" stddev)"
    gsd="$(read_metric "$SCRATCH/results/${w}_wasm32-gc.json" stddev)"
    echo "| ${LABEL[$w]} | $(fmt_secs "$lmean") ± $(fmt_secs "$lsd") | $(fmt_secs "$gmean") ± $(fmt_secs "$gsd") |"
  done
  echo
  echo "## Reading the numbers"
  echo
  echo "- **Per-call, not whole-program.** These are micro-costs of a single"
  echo "  boundary crossing. A real program crosses the boundary a handful of"
  echo "  times per frame / request, not a million times in a tight loop, so the"
  echo "  boundary is rarely the bottleneck — this bench exists to *know* the cost,"
  echo "  not because it is currently a problem."
  echo "- **The baseline may be inlined.** V8 can inline the intra-WASM \`work\` call"
  echo "  in \`interop_pure_call\`, so that column is closer to a \"calls that stay in"
  echo "  WASM are ~free\" floor than a true call cost. That makes the boundary"
  echo "  deltas (which subtract it) a slight *over*-estimate of the crossing's"
  echo "  marginal cost — the honest direction for a cost we want to bound."
  echo "- **The bare-crossing row is the noisiest.** \`interop_noop_extern\`'s loop"
  echo "  body does one fewer add than \`interop_pure_call\`'s (no accumulator), so"
  echo "  \`noop − pure\` is a low-single-digit-ns figure dominated by run-to-run"
  echo "  noise — read it as \"the bare crossing is nearly free,\" not a precise"
  echo "  cost. The headline (\`string − pure\`) is unaffected: both loops do the"
  echo "  same \`acc = acc + <call>(…)\` work, so only the called function differs."
  echo "- **The host stubs do nothing.** \`nop\` returns immediately and \`echo\`"
  echo "  hands its argument straight back, so the measured cost is the glue +"
  echo "  engine crossing + marshalling, not host work."
  echo "- **String allocation is real.** Each \`String\` round-trip allocates a fresh"
  echo "  GC-managed Phoenix string (decision F: copied, never shared), so the"
  echo "  marshalling row includes per-call allocation + GC pressure — representative"
  echo "  of real string-returning host APIs."
  echo
  echo "## Reproducing locally"
  echo
  echo '```sh'
  echo "# Requires: cargo, node (>= 22), hyperfine on PATH."
  echo "cargo build -p phoenix-runtime --target wasm32-wasip1 --release"
  echo "bash bench-corpus/run-interop.sh"
  echo "git diff docs/perf/phoenix-interop-boundary.md"
  echo '```'
} >"$OUT_PATH"

echo
echo "Wrote $OUT_PATH"
