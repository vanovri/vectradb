# CLAUDE.md — Constitutional Engineering Laws for VectraDB

## 1. Mission & Architectural Identity
You are building **VectraDB**, a production-grade, zero-shortcut Vectorized Columnar OLAP SQL Engine, Cascades/DPccp Cost-Based Query Optimizer, Out-of-Core Parallel Execution Runtime, PostgreSQL v3 Wire Protocol Server, and WebAssembly Plan Inspector written in Rust (Edition 2021/2024).

Every design choice must reflect state-of-the-art analytical database internals:
- **Umbra / Velox String Layout:** 12-byte inline / 4-byte prefix + buffer handle `ByteView` ("German Strings") for cache-resident string comparisons without pointer chasing.
- **DuckDB / MonetDB Vectorized Execution:** Fixed-capacity `DataChunk` (2,048 tuples per vector), `ValidityMask` bitsets (64-bit words with hardware `popcnt`/`tzcnt`), `SelectionVector` (`u16` index indirection for late materialization), and Constant/Dictionary/Flat vector encodings.
- **Neumann & Kemper (BTW 2015) Subquery Unnesting:** Complete elimination of correlated subqueries (`DependentJoin` pushdown into `MarkJoin`, `SemiJoin`, `AntiJoin`, `SingleJoin`).
- **Leis et al. (SIGMOD 2014) Morsel-Driven Parallelism:** Push-based pipelines with work-stealing morsels and strict per-operator `MemoryReservation` integrated with disk spilling.

## 2. Strictly Banned Shortcuts & Forbidden Dependencies
1. **NO External Engine Wrapping:** Core workspace crates (`vectra-types`, `vectra-expr`, `vectra-storage`, `vectra-sql`, `vectra-optimizer`, `vectra-exec`, `vectra-tpch`, `vectra-server`, `vectra-wasm`) MUST NOT depend on `arrow`, `parquet`, `datafusion`, `polars`, `duckdb`, `sqlparser`, `nom`, `pest`, `lalrpop`, or `pgwire`. You MUST implement columnar memory layout, compression codecs, Parquet v2 reader, SQL lexer/Pratt parser, optimizer, execution operators, and PostgreSQL v3 wire framing from first principles.
2. **Allowed Low-Level Utility Crates Only:**
   - Core crates: `thiserror`, `bitflags`, `parking_lot`, `crossbeam-channel`, `crossbeam-deque`, `rayon` (or custom threadpool), `crc32c` (or hardware intrinsics), `ryu`, `itoa`, `memchr`, `serde`, `serde_json`.
   - Server/CLI (`vectra-server` only): `tokio`, `clap`, `rustyline` / `ratatui` / `crossterm`.
   - WASM (`vectra-wasm` only): `wasm-bindgen`, `js-sys`, `serde-wasm-bindgen`.
   - Differential Test Oracle (`vectra-oracle` `[dev-dependencies]` / test-only): `rusqlite` with `bundled` feature is permitted STRICTLY inside `vectra-oracle` as an external black-box differential truth oracle to battle-test `VectraDB`.
3. **ZERO Incomplete Code:** `todo!()`, `unimplemented!()`, `unreachable!("stub")`, dummy return constants, silent error swallowing (`let _ = ...`), and `#[ignore]` tests are **STRICTLY FORBIDDEN**. Every git commit must compile with zero warnings (`RUSTFLAGS="-D warnings"`).
4. **`unsafe` Quarantine Policy:**
   - `#![deny(unsafe_code)]` MUST be active at the crate root of `vectra-sql`, `vectra-optimizer`, `vectra-exec`, `vectra-oracle`, `vectra-tpch`, and `vectra-server`.
   - `unsafe` is permitted ONLY inside explicitly documented submodules of `vectra-types` (64-byte aligned buffer allocator and `ByteView` union/pointer invariants) and `vectra-expr::simd` (CPU architecture intrinsics with mandatory `is_x86_feature_detected!` / `aarch64` checks and a 100% safe scalar fallback).
   - Every `unsafe` block MUST carry a `// SAFETY:` proof comment explaining alignment, bounds, aliasing, and lifetime invariants, and MUST pass `cargo +nightly miri test` (or `cargo test` with debug pointer assertions if Miri is unavailable on SIMD intrinsics, using `cfg(miri)` scalar fallback).

## 3. Verification, Differential Oracle & Benchmark Laws
1. **Differential Truth First:** Never trust self-written assertions alone. Complex operators, SQL expressions, Kleene 3-valued `NULL` logic, window frames, and join semantics must be verified against an independent reference evaluator (`RowReferenceEngine` + `rusqlite` oracle in `M07`).
2. **Deterministic Reproducibility:** All fuzzers (`SqlSmith`, codec fuzzers, spill fuzzers) MUST accept a deterministic `u64` PRNG seed (`SplitMix64` / `ChaCha8Rng`), log the failing seed on panic, and shrink failing inputs to minimal reproducers.
3. **Hard Memory Ceiling Enforcement:** Every memory-intensive operator (Hash Join build side, Hash Aggregate groups, External Merge Sort runs, Window partitions) MUST register allocations through `MemoryPool::try_reserve(bytes)`. Exceeding quota MUST deterministically trigger out-of-core disk spilling—never OOM panic when disk spill is enabled.
4. **Honest Telemetry Law:** Never fabricate benchmark latency, throughput, compression ratios, or LOC counts in `README.md` or `BENCHMARKS.md`. Every number written to documentation must be generated directly by executing benchmark binaries and piping real artifacts into `docs/benchmarks/`.

## 4. Autonomous Progression Protocol (`PROGRESS.md`)
Maintain a root `PROGRESS.md` ledger tracking modules `M01` through `M10`. You may NOT advance from module `M(k)` to `M(k+1)` until:
1. All unit, property, fuzz, and integration gates defined for `M(k)` pass with `0` failures and `0` compiler/clippy warnings (`cargo clippy --workspace --all-targets -- -D warnings`).
2. Regression tests for all prior modules `M01..M(k-1)` still pass 100%.
3. `PROGRESS.md` is updated with real test counts, seed ranges run, and metrics, followed by a clean git commit (`feat(mXX): ...`).
