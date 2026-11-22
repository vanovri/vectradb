# VectraDB Engineering Progress Ledger

| Module | Crate / Path | Status | Unit & Integration Tests | Fuzz / Oracle Campaign | Key Verified Metrics |
| --- | --- | --- | --- | --- | --- |
| **M01** | `crates/vectra-types` | DONE | 7 integration/gate tests (0 fail) | 10,000 deterministic seeds + Miri (40 seeds, 0 UB) | 0 leaked bytes, 64B align, 16B `ByteView`, 0.67s release fuzz |
| **M02** | `crates/vectra-expr` | DONE | 6 integration/gate tests (0 fail) | 50,000 random AST seeds vs scalar oracle + AVX2 parity | 0.68s release fuzz, 0 leaks, linear `LIKE`, Kleene 64-bit |
| **M03** | `crates/vectra-storage` | DONE | 6 integration/gate tests (0 fail) | 1,000-seed 5-codec fuzz + CRC32C bit-flip + 8-thread MVCC | >98% block skip on 1M rows, Bloom FPR <1.5%, Parquet v2 + CSV |
| **M04** | `crates/vectra-sql` | IN PROGRESS | — | — | — |
| **M05** | `crates/vectra-optimizer` | PENDING | — | — | — |
| **M06** | `crates/vectra-exec` | PENDING | — | — | — |
| **M07** | `crates/vectra-oracle` | PENDING | — | — | — |
| **M08** | `crates/vectra-tpch` | PENDING | — | — | — |
| **M09** | `crates/vectra-server` | PENDING | — | — | — |
| **M10** | `crates/vectra-wasm` & `studio/` | PENDING | — | — | — |
