# VectraDB TPC-H Benchmark & Oracle Validation Report

> Generated automatically by `vectra-tpch-bench` from live release-mode execution.

## Environment & Run Summary

- **OS / Arch**: `windows` / `x86_64`
- **Worker Threads**: `8`
- **Differential Oracle Validation**: `22/22` queries exact match vs bundled SQLite at `SF=0.01`
- **Benchmark Scale Factor**: `SF=0.1` (865303 total rows, 599273 `lineitem` rows)
- **Ingest + Compression + ANALYZE Time**: `5971.0 ms`
- **Timed Iterations**: `1 warm-up + 3 timed`
- **Total 22-Query Median Wall Time**: `10857.50 ms`
- **22-Query Geometric Mean Latency**: `261.53 ms`

## Columnar Storage & Compression Telemetry

| Table | Rows | Uncompressed | Compressed | Compression Ratio |
| --- | ---: | ---: | ---: | ---: |
| `region` | 5 | 307 B | 354 B | 0.87x |
| `nation` | 25 | 1.4 KiB | 714 B | 2.00x |
| `supplier` | 1000 | 110.3 KiB | 32.4 KiB | 3.41x |
| `part` | 20000 | 2.62 MiB | 461.9 KiB | 5.81x |
| `partsupp` | 80000 | 4.22 MiB | 1.10 MiB | 3.85x |
| `customer` | 15000 | 1.72 MiB | 478.2 KiB | 3.69x |
| `orders` | 150000 | 15.00 MiB | 2.23 MiB | 6.73x |
| `lineitem` | 599273 | 89.47 MiB | 11.97 MiB | 7.47x |
| **Total** | **865303** | **113.14 MiB** | **16.25 MiB** | **6.96x** |

## Query Latency & Runtime Telemetry (22/22 Queries)

| Query | Rows | Min (ms) | Median (ms) | Mean (ms) | Peak Pool RAM | ZoneMap Pruned |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Q01 Pricing Summary Report | 4 | 1309.25 | 1449.22 | 1433.41 | 2.37 MiB | 0/293 |
| Q02 Minimum Cost Supplier | 48 | 64.68 | 71.56 | 69.39 | 1.73 MiB | 0/149 |
| Q03 Shipping Priority | 10 | 138.66 | 154.49 | 149.70 | 3.24 MiB | 0/375 |
| Q04 Order Priority Checking | 5 | 119.61 | 124.23 | 124.67 | 2.31 MiB | 0/441 |
| Q05 Local Supplier Volume | 5 | 287.94 | 410.66 | 371.70 | 5.07 MiB | 0/378 |
| Q06 Forecasting Revenue Change | 1 | 89.92 | 90.83 | 90.98 | 1002.0 KiB | 0/293 |
| Q07 Volume Shipping | 4 | 308.05 | 503.53 | 572.77 | 23.39 MiB | 0/378 |
| Q08 National Market Share | 2 | 145.56 | 160.00 | 164.87 | 18.82 MiB | 0/389 |
| Q09 Product Type Profit Measure | 175 | 1780.01 | 1934.76 | 1894.10 | 71.95 MiB | 0/419 |
| Q10 Returned Item Reporting | 20 | 287.16 | 295.36 | 348.72 | 27.07 MiB | 0/376 |
| Q11 Important Stock Identification | 2213 | 43.10 | 43.81 | 46.93 | 1001.6 KiB | 0/84 |
| Q12 Shipping Modes & Order Priority | 2 | 213.41 | 244.38 | 247.09 | 1.30 MiB | 0/367 |
| Q13 Customer Distribution | 36 | 228.50 | 284.95 | 289.38 | 4.15 MiB | 0/82 |
| Q14 Promotion Effect | 1 | 91.02 | 92.50 | 100.16 | 3.52 MiB | 0/303 |
| Q15 Top Supplier | 1 | 224.34 | 298.67 | 297.03 | 925.9 KiB | 0/587 |
| Q16 Parts/Supplier Relationship | 2849 | 90.73 | 99.34 | 101.69 | 1.87 MiB | 0/51 |
| Q17 Small-Quantity-Order Revenue | 1 | 382.40 | 430.37 | 417.18 | 652.8 KiB | 0/899 |
| Q18 Large Volume Customer | 100 | 1754.18 | 2032.87 | 2531.72 | 57.21 MiB | 0/668 |
| Q19 Discounted Revenue | 1 | 280.35 | 294.96 | 295.40 | 5.68 MiB | 0/303 |
| Q20 Potential Part Promotion | 40 | 92.48 | 96.95 | 96.71 | 2.21 MiB | 0/395 |
| Q21 Suppliers Who Kept Orders Waiting | 40 | 1601.06 | 1611.21 | 1804.67 | 19.49 MiB | 0/2355 |
| Q22 Global Sales Opportunity | 7 | 125.86 | 132.85 | 140.52 | 3.45 MiB | 0/106 |
