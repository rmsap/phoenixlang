# `interop_noop_extern` — bare boundary crossing (no marshalling)

The **mid-point** of the Phase 2.5 interop boundary-cost bench ([`docs/perf/phoenix-interop-boundary.md`](../../docs/perf/phoenix-interop-boundary.md)). It loops 1,000,000 times calling `nop()`, an `extern js` function that takes and returns nothing. Each call is a real WASM→JS import crossing (the engine cannot prove a host import is side-effect-free, so it can't be hoisted or elided) but marshals no values.

- **`interop_noop_extern` − `interop_pure_call`** = the *bare* per-call boundary-crossing cost (the glue thunk + the import call), independent of any value marshalling.
- **`interop_string_roundtrip` − `interop_noop_extern`** = the per-call cost of marshalling a `String` out and back, on top of the crossing.

Details:

- **Expected stdout:** `1000000` — the loop counter, pinning that all 1,000,000 crossings happened.
- **Invariant:** the 1,000,000 iteration count must stay in lockstep with the other two interop workloads and `ITERS` in [`bench-corpus/run-interop.sh`](../run-interop.sh).
- **Not a Go-comparison workload** (see [`interop_pure_call/README.md`](../interop_pure_call/README.md)); run by `run-interop.sh`.
