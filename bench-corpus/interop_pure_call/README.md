# `interop_pure_call` — boundary-cost baseline (pure-WASM call)

The **baseline** of the Phase 2.5 interop boundary-cost bench ([`docs/perf/phoenix-interop-boundary.md`](../../docs/perf/phoenix-interop-boundary.md)). The timed loop runs 1,000,000 ordinary Phoenix function calls (`work(i)`) — no host crossing, no marshalling. It declares one extern (`marker`) and calls it **once** before the loop, purely so the build emits the `.js` glue and runs through the identical Node-driver harness as the other two workloads; that single crossing is negligible against a million pure calls, and keeping the harness identical is what makes the subtraction valid.

Its whole-process time is the fixed cost shared by all three interop workloads — node startup, glue `instantiate`, and the loop scaffold. Subtracting it from `interop_noop_extern` / `interop_string_roundtrip` cancels that shared cost and isolates the boundary-crossing / string-marshalling cost. (V8 may inline `work`, so this column is the honest "calls that never leave WASM are ~free" floor, not a per-call cost in its own right.)

- **Expected stdout:** `500000500000` — the sum `1 + 2 + … + 1_000_000` (`work(i) = i + 1`), pinning the loop ran fully.
- **Invariant:** the 1,000,000 iteration count must stay in lockstep with the other two interop workloads and `ITERS` in [`bench-corpus/run-interop.sh`](../run-interop.sh); the published per-call figures divide the whole-process delta by it.
- **Not a Go-comparison workload.** Unlike the four cross-language workloads (decision E), the `interop_*` family has no Go pair — `extern js` is a Phoenix-only boundary. It's run by `run-interop.sh`, not `run.sh` / `run-wasm.sh`.
