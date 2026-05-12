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

- Hash function quality (poor hashing → long probe chains → quadratic-ish growth).
- Resize / rehash cadence: 100k inserts cross every Phoenix resize threshold (8 → 16 → … → 262144 buckets per `phoenix-runtime::map_methods::buckets_for`).
- Lookup hit + miss paths separately: half the probes terminate `Vacant`, half `Found`.

## Phoenix-specific gap call-out

Phoenix's `Map<K, V>` is immutable. Every `m.set(k, v)` allocates a fresh table and copies the previous body, so building the 100k-entry map is **O(n²)** in Phoenix and is the dominant wall-clock cost. Go's `map[K]V` is mutable + amortized O(1) per insert. The comparison ratio in the published page therefore reflects build + read together; a transient mutable-build path is a stdlib gap.

## Invariants for refactors

- `n = 100000` is locked (decision E).
- Probe range `0..2n` with stride 1 is locked: half-hit / half-miss split is what exercises both `ProbeResult` branches.
- Sum check (`34_999_650_000`) is the only correctness fingerprint; preserve it through any rewrite or the cross-language byte-for-byte stdout match breaks.
