# pause

GC pause distribution: P50 / P95 / P99 / max per rooted-object scenario.  Refresh via `phoenix-bench-diff update`.

All times in **nanoseconds**. See `README.md` for the runner spec.

| bench | parameters | p50 (ns) | p95 (ns) | p99 (ns) | max (ns) | samples |
|---|---|---|---|---|---|---|
| gc_pause | 1000 | 83619 | 152080 | 344697 | 9800048 | 62574 |
| gc_pause | 10000 | 1764631 | 3205496 | 4519479 | 12121144 | 2499 |
| gc_pause | 100000 | 34344164 | 50836489 | 58426296 | 59185918 | 131 |
