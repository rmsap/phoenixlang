# pause

GC pause distribution: P50 / P95 / P99 / max per rooted-object scenario.  Refresh via `phoenix-bench-diff update`.

All times in **nanoseconds**. See `README.md` for the runner spec.

**2026-05-12 refresh.** Numbers below dropped 45–77 % from the previous snapshot, but **the headline delta is mostly environmental quiescence, not a code-induced win** — don't over-extrapolate. PR 6 threaded typed `TypeTag` values through `phx_list_alloc`, `phx_map_alloc`, the closure-env allocator, and struct/enum allocators, but the mark phase still uses conservative interior scanning (`heap.rs` `scans_interior` short-circuits only on `String`), and the pause bench's own allocations are tagged `TypeTag::Unknown` (see [`crates/phoenix-bench/benches/allocation.rs`](../../crates/phoenix-bench/benches/allocation.rs)), so the scan workload was *not* reduced by the code change. Typed-allocator threading provides the substrate for trace tables (GC subordinate decision C); the pause-time win those would unlock is queued behind their implementation, not surfaced here.

Two independent signatures back the "environmental" attribution:

1. `alloc_throughput/string/16` stddev fell from ~44 ns to ~4 ns (see [`allocation.md`](allocation.md)) — a much tighter distribution at the same workload.
2. The `100000` row's P99 was ~99 % of max in the prior snapshot (58.4 ms P99 / 59.2 ms max) and is ~73 % of max now (27.4 ms P99 / 37.5 ms max). That's not just lower noise; it's the disappearance of a sporadic stall tail. A code change that reduced *mean* scan cost would not selectively flatten the long tail like that — system-noise quiescence does.

Future reruns that show *worse* numbers should compare against this 2026-05-12 snapshot, when judging whether the regression is real or noise-attributable.


| bench | parameters | p50 (ns) | p95 (ns) | p99 (ns) | max (ns) | samples |
|---|---|---|---|---|---|---|
| gc_pause | 1000 | 28284 | 50205 | 79902 | 7098842 | 149388 |
| gc_pause | 10000 | 391636 | 870599 | 1428691 | 11541551 | 14633 |
| gc_pause | 100000 | 18761659 | 22293748 | 27367112 | 37488598 | 342 |
