# VectraDB Engineering Progress Ledger

| Module | Crate / Path | Status | Unit & Integration Tests | Fuzz / Oracle Campaign | Key Verified Metrics |
| --- | --- | --- | --- | --- | --- |
| **M01** | `crates/vectra-types` | DONE | 7 integration/gate tests (0 fail) | 10,000 deterministic seeds + Miri (40 seeds, 0 UB) | 0 leaked bytes, 64B align, 16B `ByteView`, 0.67s release fuzz |
| **M02** | `crates/vectra-expr` | DONE | 6 integration/gate tests (0 fail) | 50,000 random AST seeds vs scalar oracle + AVX2 parity | 0.68s release fuzz, 0 leaks, linear `LIKE`, Kleene 64-bit |
| **M03** | `crates/vectra-storage` | DONE | 6 integration/gate tests (0 fail) | 1,000-seed 5-codec fuzz + CRC32C bit-flip + 8-thread MVCC | >98% block skip on 1M rows, Bloom FPR <1.5%, Parquet v2 + CSV |
| **M04** | `crates/vectra-sql` | DONE | 4 suites (1,200+ grammar/binder + 22 TPC-H) | 2,000-query AST idempotence + Neumann-Kemper unnest proof | 0 warnings, `#![deny(unsafe_code)]`, 0 DependentJoin/corr vars on all 22 TPC-H |
| **M05** | `crates/vectra-optimizer` | DONE | 3 suites (HLL/Hist, 22 TPC-H plan quality, 2..24-table join stress) | 14 join graphs (chain/star/clique/snowflake 2..24 tables) | 14.44ms total join stress (<20ms), HLL <1.8% err, Hist <4% err, build<=probe on all 22 TPC-H |
| **M06** | `crates/vectra-exec` | DONE | 6 suites (all 11 joins, 12 aggs, windows, set ops, recursive CTE, MVCC, 3MB & 32MB spill torture) | 1.5M-row (~180MB) 32MB spill torture vs 2GB RAM across all 5 workloads | FULL OUTER peak 1.47MiB, LEFT MARK 9.62MiB, 1M-grp AGG 9.66MiB, 1.5M SORT 18.05MiB, 1.5M WIN 22.95MiB; 0 leaks, 0 `.vspill` leftovers |
| **M07** | `crates/vectra-oracle` | IN PROGRESS | — | — | — |
| **M08** | `crates/vectra-tpch` | PENDING | — | — | — |
| **M09** | `crates/vectra-server` | PENDING | — | — | — |
| **M10** | `crates/vectra-wasm` & `studio/` | PENDING | — | — | — |
