# hash_map_churn

100k inserts followed by 200k lookups (every key in `0..200000` probed exactly once → half hit on `0..100000`, half miss on `100000..200000`).

## Expected output

```
100000
34999650000
```

- Line 1: `m.length()` after all 100k inserts.
- Line 2: Σ `(k * 7)` for `k = 0..99999` = `7 * 99999 * 100000 / 2` = `34_999_650_000`. Misses contribute 0 (Phoenix's `unwrapOr(0)` and Go's zero-value default agree). Byte-for-byte equal across both implementations.

## What this workload isolates

- Hash function quality (poor hashing → long probe chains → quadratic-ish growth) — exercised by the 200k lookups against the 100k-entry table.
- Lookup hit + miss paths separately: half the probes terminate `Vacant`, half `Found`.

## Build phase (builder side)

Phoenix's `Map<K, V>` is immutable — every `m.set(k, v)` allocates a fresh table and copies the previous body. Phase 2.7 decision F (PR 7) ships `MapBuilder<K, V>` as the transient-mutable accumulator: `Map.builder()` → `.set()` (append a `(key, value)` pair, O(1) amortized via 2× capacity doubling in `phx_map_builder_set`) → `.freeze()` (one O(n) hash build via `phx_map_from_pairs`, last-wins dedup on duplicate keys).

The Phoenix program uses the builder pattern:

```phoenix
let mb: MapBuilder<Int, Int> = Map.builder()
for i in 0..n { mb.set(i, i * 7) }
let m: Map<Int, Int> = mb.freeze()
```

Total build cost is O(n). With the builder in place, the comparison ratio in [`docs/perf/phoenix-vs-go.md`](../../docs/perf/phoenix-vs-go.md) reflects comparable algorithmic work on both sides; Go's `map[K]V` insert is amortized O(1) but the Phoenix `.set() → .freeze()` total is also O(n).

## Frozen map's resize behavior (lookup side)

Independent of the builder: once `.freeze()` lands the map in its final form, the post-freeze `Map<Int, Int>` sits at the size the open-addressing table's 70 % load-factor threshold picks for 100k entries. The bucket-count progression (8 → 16 → … → 262144 buckets per `phoenix-runtime::map_methods::buckets_for`) is what the lookup phase exercises across the 200k probes — half hitting the final-size table, half missing through to `Vacant`. The build phase does **not** cross every threshold incrementally any more — the freeze step is a one-shot hash build at the final size.

## Invariants for refactors

- `n = 100000` is locked (decision E).
- Probe range `0..2n` with stride 1 is locked: half-hit / half-miss split is what exercises both `ProbeResult` branches.
- Sum check (`34_999_650_000`) is the only correctness fingerprint; preserve it through any rewrite or the cross-language byte-for-byte stdout match breaks.
