# VectraDB (`vectradb`)

**VectraDB** is a from-scratch, zero-shortcut, vectorized columnar OLAP SQL engine, Cascades/DPccp cost-based query optimizer, out-of-core parallel execution runtime, async PostgreSQL v3 wire server, and WebAssembly Optimizer & Execution Studio written in pure Rust.

Zero engine dependencies (`arrow`, `parquet`, `datafusion`, `polars`, `duckdb`, `sqlparser`, `nom`, `pest`, `pgwire` are all strictly forbidden). Every layer—from 64-byte aligned memory arenas and 16-byte Umbra German Strings to LZ4/Snappy/Parquet v2 codecs, Neumann-Kemper subquery unnesting, Moerkotte-Neumann DPccp join ordering, Grace Hash Spill, and PostgreSQL v3 wire framing—is built from first principles and verified against automated differential oracles.

---

## Architecture Overview

```
SQL Query / PG v3 Wire (`vectra-server`) / Browser WASM (`vectra-wasm` + `studio/`)
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────────┐
│ M04 vectra-sql          Zero-Copy Lexer · Pratt Parser · Unique-ColId Binder    │
│                         Neumann-Kemper (BTW 2015) General Subquery Unnesting    │
├─────────────────────────────────────────────────────────────────────────────────┤
│ M05 vectra-optimizer    14-bit HyperLogLog · 64-Bucket Equi-Height Histograms   │
│                         Heuristic Rewrites (Q19 Factoring, Pushdown, Pruning)   │
│                         Moerkotte-Neumann DPccp (N<=12) + GOO Join Reordering   │
├─────────────────────────────────────────────────────────────────────────────────┤
│ M06 vectra-exec         Morsel-Driven Work-Stealing (`crossbeam-deque`)         │
│                         64-Partition Radix Grace Hash Join · Spilling HashAgg   │
│                         External k-Way Merge Sort · O(log P) SegTree Windows    │
├─────────────────────────────────────────────────────────────────────────────────┤
│ M03 vectra-storage      5 Adaptive Codecs (FOR, Delta, RLE, Dict+LZ4) + CRC32C  │
│                         ZoneMaps + 256-bit Split-Block Blooms · MVCC Snapshot   │
│                         Native Apache Parquet v2 · Double-Buffered .vdb · CSV   │
├─────────────────────────────────────────────────────────────────────────────────┤
│ M02 vectra-expr         64-Bit Kleene 3-Valued Logic · Linear-Time NFA LIKE     │
│                         Quarantined AVX2/Scalar SIMD Kernels · Vector xxHash64  │
├─────────────────────────────────────────────────────────────────────────────────┤
│ M01 vectra-types        64B AlignedBuffer · 16B German String ByteView          │
│                         Lock-Free BoundedMemoryPool · 256-Bit Decimal128        │
└─────────────────────────────────────────────────────────────────────────────────┘
                                    ▲
                                    │ Automated Ground-Truth Verification
┌───────────────────────────────────┴─────────────────────────────────────────────┐
│ M07 vectra-oracle       5,794 sqllogictest Assertions · 25,000 SQLsmith Seeds   │
│                         AST Delta-Debugging Shrinker · Isolated SQLite Oracle   │
│ M08 vectra-tpch         Pure-Rust Streaming dbgen · 22/22 Oracle Validation     │
└─────────────────────────────────────────────────────────────────────────────────┘
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for deep internal design specs and byte-level memory layouts, and [docs/design/](docs/design/) for all 10 module design documents (`M01`–`M10`).

---

## Verified Proof of Correctness & Performance

All numbers below are live measurements generated directly by the repository's release binaries and test gates (never hand-estimated).

### 1. Memory Safety & Differential Oracle Verification
| Verification Gate | Command / Harness | Verified Result |
| --- | --- | --- |
| **UB & Leak Audit (M01)** | `cargo +nightly miri test -p vectra-types` | **0 UB, 0 leaked bytes** across `AlignedBuffer`, `ByteView`, `TupleBuffer` |
| **Expression Fuzzer (M02)** | `cargo test -p vectra-expr --release` | **50,000 random AST seeds** bit-identical vs scalar oracle + AVX2 parity |
| **Storage Fuzz & MVCC (M03)** | `cargo test -p vectra-storage --release` | **1,000-seed 5-codec fuzz**, CRC32C bit-flip recovery, **>98%** ZoneMap skip on 1M rows |
| **Optimizer Stress (M05)** | `cargo test -p vectra-optimizer --release` | 14 join graphs (2–24 tables) optimized in **14.44 ms**; `build <= probe` on 22/22 TPC-H |
| **sqllogictest Suite (M07)** | `make slt` | **26 `.slt` files, 5,794 SQL assertions — 100% pass** (pure-Rust MD5) |
| **SQLsmith Differential Oracle (M07)** | `vectra-sqlsmith --seeds 25000` | **25,000 consecutive seeds** (2,500 in 4 MiB spill mode) vs SQLite — **0 mismatches, 0 panics** |
| **PG v3 Wire Concurrency (M09)** | `cargo test -p vectra-server --release` | **32 concurrent async clients**, Extended Query + `PortalSuspended`, MVCC, mid-flight `CancelRequest` (`57014`) in **0.82s** |

### 2. Out-of-Core 32 MiB Spill Torture Gate (`1,500,000` rows, ~180 MiB raw)
Measured by `crates/vectra-exec/tests/spill_torture.rs` under a hard **32.00 MiB `BoundedMemoryPool`** ceiling and verified row-for-row against unconstrained 2 GiB RAM execution (`0` leaked bytes, `0` leftover `.vspill` files):

| Out-of-Core Workload | Output Rows | Peak Pool RAM | Spilled (LZ4+CRC32C) | Equivalence vs 2 GiB |
| --- | ---: | ---: | ---: | --- |
| `FULL OUTER JOIN` (1.5M × 375K) | 1,575,301 | **1.47 MiB** | 8.15 MiB | 100% exact match |
| `LEFT MARK JOIN` (`IN` w/ NULLs) | 1,500,000 | **9.62 MiB** | 8.28 MiB | 100% exact match |
| `1,000,000`-Group `GROUP BY` (`COUNT(DISTINCT)` + `SUM(Decimal128)`) | 1,000,000 | **9.66 MiB** | 24.48 MiB | 100% exact match |
| `1.5M`-Row `ExternalSort` (`ORDER BY str DESC NULLS LAST, id ASC`) | 1,500,000 | **18.05 MiB** | 45.42 MiB | 100% sorted + exact match |
| `1.5M`-Row Sliding Window (`ROWS BETWEEN 50 PRECEDING AND 10 FOLLOWING`) | 1,500,000 | **22.95 MiB** | 38.03 MiB | 100% exact match |

### 3. TPC-H 22/22 Validation & `SF=0.1` Benchmark
Full live report: [docs/benchmarks/tpch_results.md](docs/benchmarks/tpch_results.md) (generated by `vectra-tpch-bench --sf 0.1 --validate-sf 0.01`, Windows x86_64, 8 threads, 1 warm-up + 3 timed runs).

- **Differential Validation (`SF=0.01`)**: **22/22** official queries matched bundled SQLite with **0 mismatches**.
- **Storage Compression (`SF=0.1`, 865,303 rows)**: **113.14 MiB -> 16.25 MiB (6.96x overall compression; `lineitem` 7.47x)**.
- **Query Latency (`SF=0.1`)**: **261.53 ms geometric mean**, **10.86 s** (`10857.50 ms`) total median across all 22 queries.

| Query | Rows | Median (ms) | Peak RAM | Query | Rows | Median (ms) | Peak RAM |
| --- | ---: | ---: | ---: | --- | ---: | ---: | ---: |
| Q01 Pricing Summary | 4 | 1449.22 | 2.37 MiB | Q12 Ship Modes | 2 | 244.38 | 1.30 MiB |
| Q02 Min Cost Supplier | 48 | 71.56 | 1.73 MiB | Q13 Customer Dist | 36 | 284.95 | 4.15 MiB |
| Q03 Shipping Priority | 10 | 154.49 | 3.24 MiB | Q14 Promo Effect | 1 | 92.50 | 3.52 MiB |
| Q04 Order Priority | 5 | 124.23 | 2.31 MiB | Q15 Top Supplier | 1 | 298.67 | 925.9 KiB |
| Q05 Local Supplier Vol | 5 | 410.66 | 5.07 MiB | Q16 Parts/Supplier | 2849 | 99.34 | 1.87 MiB |
| Q06 Forecast Revenue | 1 | 90.83 | 1002.0 KiB | Q17 Small Qty Order | 1 | 430.37 | 652.8 KiB |
| Q07 Volume Shipping | 4 | 503.53 | 23.39 MiB | Q18 Large Vol Cust | 100 | 2032.87 | 57.21 MiB |
| Q08 Market Share (8-Way) | 2 | 160.00 | 18.82 MiB | Q19 Discounted Rev | 1 | 294.96 | 5.68 MiB |
| Q09 Product Profit | 175 | 1934.76 | 71.95 MiB | Q20 Part Promotion | 40 | 96.95 | 2.21 MiB |
| Q10 Returned Items | 20 | 295.36 | 27.07 MiB | Q21 Kept Waiting | 40 | 1611.21 | 19.49 MiB |
| Q11 Important Stock | 2213 | 43.81 | 1001.6 KiB | Q22 Global Sales | 7 | 132.85 | 3.45 MiB |

---

## Quick Start

### Interactive SQL CLI & Visual `EXPLAIN ANALYZE`
```bash
cargo run --release -p vectra-server --bin vectra-cli -- --tpch 0.01
```
Inside the REPL:
```sql
.tables
.stats lineitem
EXPLAIN ANALYZE SELECT l_returnflag, COUNT(*), SUM(l_extendedprice) FROM lineitem GROUP BY l_returnflag;
```

### Async PostgreSQL v3 Wire Server (connect with `psql` or any PG driver)
```bash
cargo run --release -p vectra-server --bin vectra-server -- --listen 127.0.0.1:5433 --tpch 0.01
# Connect in another terminal:
psql -h 127.0.0.1 -p 5433 -U vectra vectradb
```

### WebAssembly Optimizer & Execution Studio
```bash
cargo build --release -p vectra-wasm --target wasm32-unknown-unknown
wasm-bindgen target/wasm32-unknown-unknown/release/vectra_wasm.wasm --out-dir studio/src/wasm --target web
cd studio && npm install && npm run dev
```

### Full Verification Suite
```bash
make check          # rustfmt + clippy --workspace --all-targets -D warnings
make test           # all 10 crate release unit & integration suites
make slt            # 26 .slt files (5,794 SQL assertions)
make fuzz-oracle    # SQLsmith differential oracle vs SQLite (override SEEDS=25000)
make torture-spill  # 1.5M-row 32 MiB hard-ceiling out-of-core spill gate
make tpch-bench     # 22/22 SF=0.01 validation + SF=0.1 benchmark report
make wasm-studio    # wasm32-unknown-unknown build + Vite build + real WASM Vitest suite
make verify-all     # run all gates end-to-end
```

---

## Safety & Code Hygiene Invariants
- **`#![deny(unsafe_code)]`** enforced across 8 of 10 crates (`vectra-storage`, `vectra-sql`, `vectra-optimizer`, `vectra-exec`, `vectra-oracle`, `vectra-tpch`, `vectra-server`, `vectra-wasm`).
- **`unsafe` Quarantine**: Strictly isolated to `vectra-types` (`AlignedBuffer` & `ByteView`, audited under Miri) and `vectra-expr::simd` (runtime CPU dispatch with bit-identical scalar fallback).
- **Zero Stubs / Zero Warnings**: Zero `todo!()`, `unimplemented!()`, `#[ignore]`, or compiler/clippy warnings across the entire workspace.
