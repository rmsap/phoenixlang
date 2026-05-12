# pipeline

Per-bench mean/median/stddev. Refresh via `phoenix-bench-diff update`.

All times in **nanoseconds**. See `README.md` for the runner spec and the refresh procedure.

| bench | parameters | mean (ns) | median (ns) | stddev (ns) | samples |
|---|---|---|---|---|---|
| empty/cranelift_compile | - | 87515.68 | 83799.05 | 11548.52 | 100 |
| empty/full_compile | - | 13455.69 | 12590.94 | 3291.27 | 100 |
| empty/full_compile_native | - | 139126.18 | 130372.68 | 27905.79 | 100 |
| empty/interp | - | 1427.41 | 1348.44 | 218.35 | 100 |
| empty/ir_interp | - | 91.34 | 87.10 | 18.11 | 100 |
| empty/ir_lower | - | 1710.19 | 1632.37 | 221.63 | 100 |
| empty/lex | - | 422.56 | 407.46 | 57.92 | 100 |
| empty/parse | - | 190.19 | 181.24 | 43.75 | 100 |
| empty/sema | - | 6355.91 | 6062.88 | 1121.41 | 100 |
| large/full_compile | - | 741883.48 | 705082.73 | 130803.14 | 100 |
| large/interp | - | 296202.58 | 284448.68 | 69042.94 | 100 |
| large/ir_lower | - | 280656.23 | 284560.63 | 67885.71 | 100 |
| large/lex | - | 119924.70 | 116773.56 | 23432.14 | 100 |
| large/parse | - | 66383.83 | 63523.15 | 12667.07 | 100 |
| large/sema | - | 322967.66 | 295623.60 | 97851.63 | 100 |
| medium/compile_and_run | - | 2207980.17 | 2196589.96 | 266240.09 | 100 |
| medium/cranelift_compile | - | 379462.01 | 368659.56 | 49898.20 | 100 |
| medium/full_compile | - | 67150.53 | 65965.28 | 8210.12 | 100 |
| medium/full_compile_native | - | 444394.96 | 425529.63 | 72197.33 | 100 |
| medium/interp | - | 12500.33 | 12035.69 | 1853.03 | 100 |
| medium/ir_interp | - | 2032.84 | 1999.36 | 242.00 | 100 |
| medium/ir_lower | - | 12903.59 | 12735.68 | 1256.06 | 100 |
| medium/lex | - | 11733.06 | 11385.54 | 2806.47 | 100 |
| medium/parse | - | 4604.47 | 4644.10 | 628.10 | 100 |
| medium/sema | - | 29660.11 | 27750.52 | 7719.97 | 100 |
| medium_large/full_compile | - | 297273.90 | 287832.13 | 64716.50 | 100 |
| medium_large/interp | - | 84552.02 | 68351.93 | 33493.67 | 100 |
| medium_large/ir_interp | - | 12428.18 | 12311.73 | 1282.05 | 100 |
| medium_large/ir_lower | - | 73988.49 | 71465.81 | 19348.51 | 100 |
| medium_large/lex | - | 25349.19 | 22915.50 | 8102.26 | 100 |
| medium_large/parse | - | 16317.96 | 15578.46 | 3709.72 | 100 |
| medium_large/sema | - | 76641.54 | 71698.68 | 17411.86 | 100 |
| small/cranelift_compile | - | 221863.68 | 215991.16 | 44676.70 | 100 |
| small/full_compile | - | 46606.75 | 45810.27 | 8844.09 | 100 |
| small/full_compile_native | - | 413913.40 | 402344.74 | 80195.39 | 100 |
| small/interp | - | 366417.66 | 360791.50 | 54858.60 | 100 |
| small/ir_interp | - | 74612.42 | 71917.38 | 9216.36 | 100 |
| small/ir_lower | - | 5915.14 | 5737.53 | 578.86 | 100 |
| small/lex | - | 2656.18 | 2561.42 | 420.03 | 100 |
| small/parse | - | 2521.22 | 2378.96 | 638.52 | 100 |
| small/sema | - | 16689.62 | 16431.71 | 1729.32 | 100 |
