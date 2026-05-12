# sort_ints

Sort 100k pseudo-random `i64` values. Phoenix uses `List.sortBy` (bottom-up merge sort from `phoenix_common::algorithms`); Go uses `slices.Sort` (pattern-defeating quicksort).

## Input generation

Both programs run the same linear-congruential generator seeded at 1:

```
state ← (state * 1664525 + 1013904223) mod 2^31 - 1
```

This is Knuth's MMIX-style LCG, modified to wrap on 2³¹−1 so the result stays within Phoenix's signed `Int`. Same constants in both langs → same input sequence → byte-for-byte equal stdout.

## Expected output

```
7765
1072048287
2147479522
100000
```

First, middle, and last elements of the sorted list, plus the length.

## Phoenix-specific gap call-out

Phoenix's `List<T>` is immutable. Every `xs.push(v)` allocates a fresh list and copies the previous body, so building the 100k-element input is **O(n²)** in Phoenix — and dominates the workload's wall-clock. The sort itself is O(n log n) and matches Go's complexity, but the comparison ratio published in [`docs/perf/phoenix-vs-go.md`](../../docs/perf/phoenix-vs-go.md) reflects "Phoenix's full build + sort" against "Go's full build + sort". A constant-build list (or a transient mutable phase) is a stdlib gap; revisit this workload when that lands.

## Invariants for refactors

- `n = 100000` is locked (decision E).
- LCG constants are locked across both implementations — changing either side desyncs the input sequence.
- The sort must be stable for the `equal-element preserves left-hand position` contract `phoenix-cranelift::translate_list_sortby` enforces. Phoenix's merge sort is stable; Go's `slices.Sort` is not, but with distinct LCG outputs there are no ties in practice.
