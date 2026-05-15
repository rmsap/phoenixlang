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

## Build phase

Phoenix's `List<T>` is immutable — `xs.push(v)` allocates a fresh list and copies the previous body, so a 100k-element build via repeated `push` is O(n²). Phase 2.7 decision F (PR 7) ships `ListBuilder<T>` as the transient-mutable accumulator: `List.builder()` → `.push()` (O(1) amortized) → `.freeze()` (O(n) memcpy into a fresh `List<T>`). Total build cost is O(n).

The Phoenix program uses the builder pattern explicitly:

```phoenix
let b: ListBuilder<Int> = List.builder()
for i in 0..n { b.push(lcg_next(...)) }
let xs: List<Int> = b.freeze()
```

The sort itself is O(n log n) and matches Go's complexity. With the builder in place, the published ratio in [`docs/perf/phoenix-vs-go.md`](../../docs/perf/phoenix-vs-go.md) reflects "Phoenix's build + sort" against "Go's build + sort" with neither side stacked by an immutability tax.

## Invariants for refactors

- `n = 100000` is locked (decision E).
- LCG constants are locked across both implementations — changing either side desyncs the input sequence.
- The sort must be stable for the `equal-element preserves left-hand position` contract `phoenix-cranelift::translate_list_sortby` enforces. Phoenix's merge sort is stable; Go's `slices.Sort` is not, but with distinct LCG outputs there are no ties in practice.
