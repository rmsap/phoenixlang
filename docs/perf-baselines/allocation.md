# allocation

Per-bench mean/median/stddev. Refresh via `phoenix-bench-diff update`.

All times in **nanoseconds**. See `README.md` for the runner spec and the refresh procedure.

| bench | parameters | mean (ns) | median (ns) | stddev (ns) | samples |
|---|---|---|---|---|---|
| alloc_throughput/string | 16 | 45.46 | 44.31 | 4.03 | 100 |
| alloc_throughput/string | 64 | 44.03 | 43.68 | 2.77 | 100 |
| alloc_throughput/string | 256 | 59.17 | 58.48 | 4.20 | 100 |
| alloc_throughput/string | 1024 | 80.91 | 78.02 | 10.72 | 100 |
| alloc_throughput/unknown | 16 | 40.74 | 39.79 | 3.59 | 100 |
| alloc_throughput/unknown | 64 | 46.20 | 44.16 | 8.08 | 100 |
| alloc_throughput/unknown | 256 | 60.72 | 59.74 | 5.21 | 100 |
| alloc_throughput/unknown | 1024 | 80.98 | 78.88 | 14.84 | 100 |
