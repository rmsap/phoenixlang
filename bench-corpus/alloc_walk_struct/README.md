# alloc_walk_struct

Allocate 1M small `Point`s, read both fields, drop. Exercises the allocator hot path + the GC sweep cadence in Phoenix.

## Deviation from phase-2.md scope wording

The phase doc reads "allocate 1M small structs, walk them once, drop" — implying all 1M alive concurrently. Phoenix's only ordered container today (`List<T>`) builds in O(n²), so a 1M-element list build would dwarf the alloc workload itself. The bench instead allocates one `Point` per iteration, reads both fields into a running sum, and lets auto-collect reclaim it before the next allocation lands. That measures alloc-fast-path + GC sweep cadence rather than "many live at once". The Phoenix and Go programs use the same per-iter pattern so the ratio is apples-to-apples.

The 1M-alive variant is a candidate for re-introduction once Phoenix gains a constant-build list or a transient mutable phase. See [`phoenix-vs-go.md`](../../docs/perf/phoenix-vs-go.md) for the page-level call-out.

## Expected output

```
1000000000000
```

Σ `(i + (i + 1))` = Σ `(2i + 1)` for `i = 0..n−1` = `n*(n−1) + n` = `n²`. For `n = 1_000_000` that's `10¹² = 1_000_000_000_000`. Recorded in [`expected.txt`](expected.txt) for the runner's correctness check.

## What this workload isolates

- **Phoenix:** `phx_gc_alloc(sizeof(Point), TypeTag::Unknown)` per iter; periodic auto-collect sweep when the byte-allocated counter crosses `DEFAULT_COLLECTION_THRESHOLD` (1 MiB). The `Point` body is 16 bytes, so a sweep fires every ~65k iterations.
- **Go:** `Point{X, Y}` composite literal. Escape analysis may stack-allocate; the published page documents this asymmetry.

## Invariants for refactors

- `n = 1_000_000` is locked (decision E).
- `Point` shape (two `Int` / `int64` fields) is locked: changing it changes the per-alloc size and breaks the cross-run comparison.
- Both langs read both fields. Reading only one would let an optimizer dead-code-eliminate the field write — measuring nothing useful.
