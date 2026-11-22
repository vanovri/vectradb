# VectraDB TPC-H Benchmark & Oracle Validation Report

> Generated automatically by `vectra-tpch-bench` from live release-mode execution.

## Environment & Run Summary

- **OS / Arch**: `windows` / `x86_64`
- **Worker Threads**: `8`
- **Differential Oracle Validation**: `22/22` queries exact match vs bundled SQLite at `SF=0.01`
- **Benchmark Scale Factor**: `SF=0.1` (865303 total rows, 599273 `lineitem` rows)
- **Ingest + Compression + ANALYZE Time**: `5482.0 ms`
- **Timed Iterations**: `1 warm-up + 3 timed`
- **Total 22-Query Median Wall Time**: `8858.38 ms`
- **22-Query Geometric Mean Latency**: `230.95 ms`

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
| Q01 Pricing Summary Report | 4 | 1277.77 | 1279.44 | 1303.43 | 2.44 MiB | 0/293 |
| Q02 Minimum Cost Supplier | 48 | 44.03 | 46.94 | 46.93 | 1.68 MiB | 0/149 |
| Q03 Shipping Priority | 10 | 102.08 | 105.88 | 105.55 | 3.31 MiB | 0/375 |
| Q04 Order Priority Checking | 5 | 113.98 | 115.51 | 119.88 | 2.36 MiB | 0/441 |
| Q05 Local Supplier Volume | 5 | 272.34 | 284.19 | 297.69 | 5.07 MiB | 0/378 |
| Q06 Forecasting Revenue Change | 1 | 73.92 | 74.10 | 74.69 | 992.0 KiB | 0/293 |
| Q07 Volume Shipping | 4 | 232.77 | 244.57 | 258.02 | 23.49 MiB | 0/378 |
| Q08 National Market Share | 2 | 142.77 | 151.40 | 148.95 | 18.82 MiB | 0/389 |
| Q09 Product Type Profit Measure | 175 | 1554.80 | 1993.92 | 1886.95 | 71.95 MiB | 0/419 |
| Q10 Returned Item Reporting | 20 | 242.71 | 250.23 | 248.13 | 27.07 MiB | 0/376 |
| Q11 Important Stock Identification | 2213 | 46.07 | 52.35 | 50.30 | 1001.6 KiB | 0/84 |
| Q12 Shipping Modes & Order Priority | 2 | 226.93 | 281.63 | 358.10 | 1.30 MiB | 0/367 |
| Q13 Customer Distribution | 36 | 401.38 | 502.42 | 633.95 | 4.00 MiB | 0/82 |
| Q14 Promotion Effect | 1 | 102.46 | 112.10 | 121.19 | 3.52 MiB | 0/303 |
| Q15 Top Supplier | 1 | 154.32 | 187.32 | 178.76 | 1.00 MiB | 0/587 |
| Q16 Parts/Supplier Relationship | 2849 | 76.14 | 90.07 | 89.02 | 1.87 MiB | 0/51 |
| Q17 Small-Quantity-Order Revenue | 1 | 447.97 | 530.10 | 504.05 | 652.8 KiB | 0/899 |
| Q18 Large Volume Customer | 100 | 1010.97 | 1052.25 | 1070.61 | 57.21 MiB | 0/668 |
| Q19 Discounted Revenue | 1 | 304.07 | 322.14 | 317.58 | 5.69 MiB | 0/303 |
| Q20 Potential Part Promotion | 40 | 191.00 | 232.37 | 255.95 | 2.09 MiB | 0/395 |
| Q21 Suppliers Who Kept Orders Waiting | 40 | 870.95 | 882.01 | 918.30 | 19.49 MiB | 0/2355 |
| Q22 Global Sales Opportunity | 7 | 64.21 | 67.44 | 66.69 | 3.45 MiB | 0/106 |
