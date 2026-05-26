#!/usr/bin/env bash
#
# Reproducible keystroke-to-diagnostics latency measurement for phoenix-lsp.
#
# Drives a real LSP session over stdio: opens a fixture, then fires a burst
# of `textDocument/didChange` edits and measures the server-side `analyze`
# span (full lex -> parse -> module-resolve -> type-check; no incremental
# caching) emitted on stderr by the tracing JSON subscriber. Reports the
# latency distribution after dropping the cold first sample.
#
# Pinned method (change any of these and the numbers move):
#   - release build
#   - default fixture: crates/phoenix-bench/benches/fixtures/large.phx (~4.5 KB)
#   - 100 warm edits, cold first sample dropped
#   - metric: server-side `analyze` span duration (not editor round-trip)
#
# Numbers are machine-relative (CPU / OS). Re-run a few times to gauge
# run-to-run spread before quoting a figure.
#
# Usage:
#   crates/phoenix-lsp/scripts/latency.sh [FIXTURE] [EDITS] [RUNS]
#
# Examples:
#   crates/phoenix-lsp/scripts/latency.sh                 # default fixture, 1 run
#   crates/phoenix-lsp/scripts/latency.sh path/to/file.phx 200 5
#
# Env:
#   SKIP_BUILD=1   skip `cargo build --release` (use an existing binary)
#
set -euo pipefail

ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
FIXTURE="${1:-$ROOT/crates/phoenix-bench/benches/fixtures/large.phx}"
EDITS="${2:-100}"
RUNS="${3:-1}"
BIN="$ROOT/target/release/phoenix-lsp"

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  cargo build --release -p phoenix-lsp --manifest-path "$ROOT/Cargo.toml" >&2
fi

[[ -x "$BIN" ]] || { echo "missing binary: $BIN (run without SKIP_BUILD=1)" >&2; exit 1; }
[[ -f "$FIXTURE" ]] || { echo "missing fixture: $FIXTURE" >&2; exit 1; }

echo "fixture: $FIXTURE ($(wc -c <"$FIXTURE") bytes, $(wc -l <"$FIXTURE") lines)  edits=$EDITS  runs=$RUNS"
printf "%6s %8s %8s %8s %8s %8s\n" run p50_ms p95_ms p99_ms max_ms warmN

for r in $(seq 1 "$RUNS"); do
  PHOENIX_LSP_LOG=info python3 - "$BIN" "$FIXTURE" "$EDITS" "$r" <<'PY'
import sys, json, subprocess, threading, time, pathlib

binary, fixture, edits, run = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
text = pathlib.Path(fixture).read_text()
uri = "file://" + str(pathlib.Path(fixture).resolve())

proc = subprocess.Popen(
    [binary],
    stdin=subprocess.PIPE,
    stdout=subprocess.DEVNULL,   # discard LSP protocol replies
    stderr=subprocess.PIPE,      # tracing JSON lands here
)

# Drain stderr concurrently: the trace volume exceeds the pipe buffer,
# so not reading would deadlock the server mid-run.
err_lines = []
def drain():
    for line in proc.stderr:
        err_lines.append(line.decode("utf-8", "replace"))
t = threading.Thread(target=drain, daemon=True)
t.start()

def send(obj):
    body = json.dumps(obj).encode("utf-8")
    proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    proc.stdin.flush()

send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {}}})
send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
send({"jsonrpc": "2.0", "method": "textDocument/didOpen",
      "params": {"textDocument": {"uri": uri, "languageId": "phoenix", "version": 1, "text": text}}})
# Each edit appends a distinct run of trailing newlines so the buffer
# genuinely changes (no accidental no-op dedup) while staying valid.
for i in range(edits):
    send({"jsonrpc": "2.0", "method": "textDocument/didChange",
          "params": {"textDocument": {"uri": uri, "version": i + 2},
                     "contentChanges": [{"text": text + ("\n" * (i + 1))}]}})
    time.sleep(0.01)
time.sleep(0.8)  # let the last edits' analyze spans close before exit
send({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": None})
send({"jsonrpc": "2.0", "method": "exit", "params": None})
proc.stdin.close()
proc.wait(timeout=30)
t.join(timeout=5)

def to_ms(s):
    s = s.strip()
    for suf, scale in (("ms", 1.0), ("µs", 1e-3), ("us", 1e-3), ("ns", 1e-6), ("s", 1e3)):
        if s.endswith(suf):
            return float(s[: -len(suf)]) * scale
    return float(s)

samples = []
for line in err_lines:
    try:
        rec = json.loads(line)
    except ValueError:
        continue
    f = rec.get("fields", {})
    if f.get("message") == "close" and rec.get("span", {}).get("name") == "analyze":
        samples.append(to_ms(f["time.busy"]))

if len(samples) < 2:
    sys.exit(f"only {len(samples)} analyze samples captured — check the build/fixture")

warm = sorted(samples[1:])  # drop the cold first analyze
n = len(warm)
def pct(p):
    return warm[min(n - 1, int(p / 100 * n))]
print(f"{run:>6} {pct(50):8.2f} {pct(95):8.2f} {pct(99):8.2f} {warm[-1]:8.2f} {n:8}")
PY
done
