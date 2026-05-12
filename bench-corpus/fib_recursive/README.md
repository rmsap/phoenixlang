# fib_recursive

Naive recursive Fibonacci at `n = 35`. Pure dispatch + arithmetic, no allocation, no GC pressure. The recursive structure dispatches `fib` ~2.7×10⁷ times, so per-call overhead dominates the wall-clock — useful for spotting inlining / function-prologue regressions in either implementation.

## Expected output

```
9227465
```

Identical across both implementations (compared byte-for-byte by [`run.sh`](../run.sh)).

## What this workload isolates

- Function call dispatch cost (no virtual / interface / dynamic calls; direct recursive call).
- Arithmetic on machine integers.
- Branch prediction on the `n < 2` base case.

What it does *not* exercise: heap allocation, GC, collections, strings, dyn dispatch, closures.

## Invariants for refactors

- Recursive structure must be preserved across both langs. A memoized or iterative rewrite would void the comparison (the point is dispatch overhead, not "compute fib(35) by any means").
- `n = 35` is locked. fib(34) takes half as long; fib(36) twice. Drift here would silently change the ratio in the published page.
