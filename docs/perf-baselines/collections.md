# collections

Per-bench mean/median/stddev. Refresh via `phoenix-bench-diff update`.

All times in **nanoseconds**. See `README.md` for the runner spec and the refresh procedure.

| bench | parameters | mean (ns) | median (ns) | stddev (ns) | samples |
|---|---|---|---|---|---|
| map/get | 10 | 53.15 | 52.35 | 15.93 | 100 |
| map/get | 100 | 23.51 | 22.80 | 4.17 | 100 |
| map/get | 1000 | 16.64 | 15.64 | 2.72 | 100 |
| map/get | 10000 | 22.97 | 21.93 | 3.69 | 100 |
| map/remove | 10 | 360.83 | 331.14 | 116.96 | 100 |
| map/remove | 100 | 461.52 | 447.52 | 76.92 | 100 |
| map/remove | 1000 | 3050.32 | 2906.45 | 443.63 | 100 |
| map/remove | 10000 | 32111.67 | 30693.22 | 5169.18 | 100 |
| map/set | 10 | 326.19 | 295.95 | 104.08 | 100 |
| map/set | 100 | 488.27 | 485.58 | 84.77 | 100 |
| map/set | 1000 | 2530.83 | 2483.65 | 282.82 | 100 |
| map/set | 10000 | 29307.16 | 27668.43 | 4997.74 | 100 |
| sort_by | 100 | 890.57 | 868.58 | 115.90 | 100 |
| sort_by | 1000 | 12151.49 | 11621.28 | 1932.54 | 100 |
| sort_by | 10000 | 139708.37 | 131303.47 | 27052.15 | 100 |
