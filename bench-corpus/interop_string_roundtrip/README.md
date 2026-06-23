# `interop_string_roundtrip` — full `String` round-trip across the boundary

The **marshalling-heavy** end of the Phase 2.5 interop boundary-cost bench ([`docs/perf/phoenix-interop-boundary.md`](../../docs/perf/phoenix-interop-boundary.md)). It loops 1,000,000 times calling `echo("phoenix")`, an `extern js` function that takes a `String` and returns one. Each call marshals a string **out** (Phoenix → host) and a string **back in** (host → a fresh GC-managed Phoenix string — decision F: strings are copied, never shared); `.length()` forces the inbound string to materialize so the marshalling can't be elided.

- **`interop_string_roundtrip` − `interop_pure_call`** = the headline number: the per-call cost of a string round-trip vs. a pure-WASM call (the exact comparison the Phase 2.5 exit criterion names).
- **`interop_string_roundtrip` − `interop_noop_extern`** = the per-call `String`-marshalling cost on top of the bare crossing — where the two WASM backends differ most (linear copies through its handle table / `phx_string_alloc`; WASM-GC copies through its scratch region).

Details:

- **Expected stdout:** `7000000` — `1_000_000 × len("phoenix")` (7), pinning that every round-trip both crossed out and brought a 7-byte string back.
- **Invariant:** the 1,000,000 iteration count and the 7-byte literal must stay in lockstep with the other two interop workloads, `ITERS`, and the expected-sum check in [`bench-corpus/run-interop.sh`](../run-interop.sh).
- **Not a Go-comparison workload** (see [`interop_pure_call/README.md`](../interop_pure_call/README.md)); run by `run-interop.sh`.
