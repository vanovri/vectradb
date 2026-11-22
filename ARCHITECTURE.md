# VectraDB Systems Architecture

VectraDB is a pure-Rust, zero-shortcut, vectorized columnar OLAP SQL database engine structured as 10 strictly layered workspace crates (`M01`–`M10`) with quarantined `unsafe` memory primitives and end-to-end automated differential verification.

---

## 1. Crate Dependency Graph & `unsafe` Quarantine

```
                 ┌────────────────────┐      ┌─────────────────────┐
                 │    vectra-server   │      │     vectra-wasm     │
                 │  (PG v3 + CLI M09) │      │ (wasm-bindgen M10)  │
                 └─────────┬──────────┘      └──────────┬──────────┘
                           │                            │
                           ▼                            ▼
                 ┌────────────────────┐      ┌─────────────────────┐
                 │    vectra-tpch     │      │    vectra-oracle    │
                 │ (dbgen + 22Q M08)  │◄────►│(SLT+SQLsmith+SQLite)│
                 └─────────┬──────────┘      └──────────┬──────────┘
                           │                            │
                           └─────────────┬──────────────┘
                                         ▼
                               ┌──────────────────┐
                               │   vectra-exec    │  M06: Morsel-Driven Parallelism,
                               │  (deny unsafe)   │       Grace Hash Join, Spilling Agg/Sort,
                               └────────┬─────────┘       Segment-Tree Windows, Recursive CTEs
                                        ▼
                               ┌──────────────────┐
                               │ vectra-optimizer │  M05: HLL + Equi-Height Histograms,
                               │  (deny unsafe)   │       Rules, Moerkotte-Neumann DPccp / GOO
                               └────────┬─────────┘
                                        ▼
                               ┌──────────────────┐
                               │    vectra-sql    │  M04: Pratt Parser, Scoped Binder,
                               │  (deny unsafe)   │       Neumann-Kemper Subquery Unnesting
                               └────────┬─────────┘
                                        ▼
                               ┌──────────────────┐
                               │  vectra-storage  │  M03: 5 Column Codecs, ZoneMap + Bloom,
                               │  (deny unsafe)   │       MVCC, .vdb, Native Parquet v2 & CSV
                               └────────┬─────────┘
                                        ▼
                               ┌──────────────────┐
                               │   vectra-expr    │  M02: Kleene 64-bit Logic, Linear LIKE,
                               │ (SIMD unsafe*)   │       Quarantined AVX2/Scalar SIMD, xxHash64
                               └────────┬─────────┘
                                        ▼
                               ┌──────────────────┐
                               │   vectra-types   │  M01: 64B AlignedBuffer, 16B ByteView,
                               │ (Mem unsafe*)    │       BoundedMemoryPool, Decimal128, TupleBuffer
                               └──────────────────┘

* ONLY vectra-types (AlignedBuffer/ByteView, audited with Miri) and vectra-expr::simd
  (runtime-dispatched AVX2 with bit-identical scalar fallback) permit unsafe code.
  All other 8 crates enforce `#![deny(unsafe_code)]`.
```

---

## 2. Memory Layouts & Vectorized Primitives (`M01`–`M02`)

### 2.1 64-Byte Cache-Line Aligned Buffers & `BoundedMemoryPool`
- `AlignedBuffer` / `AlignedBufferMut` allocate strictly on 64-byte (`CACHE_LINE_ALIGN`) boundaries via `std::alloc::Layout` tracked by RAII `MemoryReservation` tokens backed by a lock-free atomic `BoundedMemoryPool`.
- Dropping any vector, `TupleBuffer`, hash directory, or sort run automatically releases its exact byte reservation; `assert_no_leaks()` verifies 0 residual bytes at query end.

### 2.2 16-Byte Umbra / Velox "German String" `ByteView` (`#[repr(C, align(16))]`)
```
Case A: Inline String (len <= 12 bytes) — 0 heap dereferences!
┌───────────────────┬───────────────────────────────────────────────────────┐
│   len: u32 (4B)   │            inline UTF-8 payload (12B, 0-pad)          │
└───────────────────┴───────────────────────────────────────────────────────┘
 0                 4                                                      16

Case B: Out-of-Line String (len > 12 bytes) — O(1) 4-byte prefix rejection!
┌───────────────────┬─────────────────┬─────────────────┬───────────────────┐
│   len: u32 (4B)   │  prefix: [u8;4] │ buffer_idx: u32 │    offset: u32    │
└───────────────────┴─────────────────┴─────────────────┴───────────────────┘
 0                 4                 8                12                  16
```

### 2.3 Prefix-Free Binary Sort Keys (`row_layout.rs`)
- `encode_composite_sort_key` transforms any multi-column `ScalarValue` tuple across all 12 types and all 4 `ASC/DESC × NULLS FIRST/LAST` orders into a normalized byte string whose lexicographical `memcmp` equals exact SQL comparison order (including `-0.0 == +0.0`, canonical `NaN`, 128-bit `Decimal128`, and embedded `\0` bytes via `0x00 -> [0x00, 0x01]`, terminator `[0x00, 0x00]`).

### 2.4 Expression Engine (`vectra-expr`)
- 64-bit word Kleene 3-valued logic (`valid_word`, `data_word`) operating on 64 rows per CPU instruction.
- Short-circuit `SelectionVector` narrowing for `CASE WHEN` and `COALESCE`.
- Linear-time $O(N \cdot M)$ `CompiledLike` (`Exact`, `Prefix`, `Suffix`, `Contains` via `memchr::memmem`, and two-pointer NFA for general `%`/`_` patterns — immune to catastrophic backtracking).
- Quarantined AVX2 SIMD comparison & arithmetic kernels with runtime CPU feature detection (`is_x86_feature_detected!("avx2")`) and bit-identical scalar/Miri fallback.

---

## 3. Columnar Storage, Codecs, Indexing & MVCC (`M03`)

- **Hierarchy**: `Table` -> `RowGroup` (65,536 rows, Split-Block 256-bit Bloom Filters + ZoneMaps) -> `SubBlock` (2,048 rows = `VECTOR_CAPACITY`, per-column `ColumnPage` + ZoneMaps).
- **Adaptive Page Codecs** (all protected by CRC32C checksums):
  1. `Uncompressed`
  2. `FrameOfReferenceBitPack` (unsigned delta from min packed to exact bit width)
  3. `DeltaBinaryPacked` (first-order delta encoding for sorted timestamps/IDs)
  4. `Rle` (run-length encoding for low-cardinality flags/statuses)
  5. `DictionaryLz4` (dictionary IDs + pure-Rust LZ4 block compression)
- **MVCC Snapshot Isolation**: Lock-free visibility checks (`insert_txn`, per-row `deletions` map) with Write-Write conflict detection (`SQLSTATE 40001`).
- **Persistence & Interop**:
  - `.vdb` double-buffered ping-pong superblock format with CRC32C bit-flip resilience.
  - Pure-Rust Apache Parquet v2 reader/writer (Compact Thrift metadata, `DATA_PAGE_V2`, `PLAIN`/`RLE_DICTIONARY`/`RLE`, `UNCOMPRESSED`/`SNAPPY`/`LZ4_RAW`, RowGroup statistics pruning).
  - RFC-4180 CSV parser with automatic schema inference.

---

## 4. SQL Compiler, Neumann-Kemper Unnesting & DPccp Optimizer (`M04`–`M05`)

1. **Frontend (`vectra-sql`)**: Hand-written zero-copy `Span` lexer + caret diagnostics, Pratt expression + full analytical SQL parser, globally unique `ColId`-based `LogicalPlan` IR, LUB type coercion lattice, and **Neumann & Kemper (BTW 2015) General Subquery Unnesting** with COUNT-bug protection (`LeftOuterJoin` + `COALESCE(count, 0)`), `LeftMark` 3-valued `IN`/`NOT IN`/`OR EXISTS` preservation, and CrossJoin domain minimization.
2. **Statistics (`vectra-optimizer::stats`)**: 14-bit `HyperLogLog` (linear counting + merge) + 64-bucket `EquiHeightHistogram` populated via `ANALYZE`.
3. **Rewrite Pipeline (`rules.rs`)**: Constant folding (`DATE +/- INTERVAL`), disjunctive common-factor extraction (turning TPC-H Q19's 3-way `OR` into an Equi `HashJoin`), `LeftMark -> LeftSemi/LeftAnti`, null-rejecting Outer-to-Inner simplification, predicate pushdown to `Scan::pushed_filters` + M03 `PruningPredicate`, top-down `ColId` projection pruning, and `Limit + Sort -> TopN` fusion.
4. **Join Reordering (`join_order.rs`)**: Exact Moerkotte-Neumann **DPccp** (`N <= 12`, pair budget 8,000, zero tree cloning) with Greedy Operator Ordering (`GOO`) fallback for large graphs (`N > 12`), followed by single-pass bottom-up physical selection enforcing the build-side invariant (`right.estimated_rows <= left.estimated_rows` on symmetric joins).

---

## 5. Morsel-Driven Parallelism & Out-of-Core Execution (`M06`)

- **Morsel Scheduler (`morsel.rs`)**: Work-stealing `crossbeam_deque::{Injector, Worker}` scheduler dispatching RowGroup morsels across scoped worker threads with cooperative `Arc<AtomicBool>` cancellation checks.
- **Spill Manager (`spill.rs`)**: LZ4-compressed, CRC32C-verified `.vspill` blocks with RAII file deletion on `Drop` (and compressed in-memory page backend on `wasm32`).
- **Operators**:
  - **Radix-Partitioned Grace Hash Join**: 16-bit salt-tagged open-addressed hash directory coalescing equal 64-bit hashes into chains; dynamically transitions to 64-partition radix disk spill (with recursive skew re-partitioning) whenever build RAM approaches the `BoundedMemoryPool` ceiling; covers all 11 join modes including $O(N + M)$ spill-capable uncorrelated `LeftMark`.
  - **Two-Phase Spilling Hash Aggregate**: Supports all 12 aggregates, `DISTINCT`, `FILTER`, `ROLLUP`/`CUBE`/`GROUPING SETS`, Welford/Chan parallel variance, and `HyperLogLog` `APPROX_COUNT_DISTINCT`; flushes partial states into 64 hash partitions and merges partition-by-partition.
  - **External k-Way Merge Sort & TopN**: Prefix-free binary sort keys + compressed sorted runs + streaming binary-heap tournament merge; $O(N \log K)$ bounded max-heap `TopN`.
  - **Window Functions**: Out-of-core partition streaming via `ExternalSort` + iterative bottom-up **$O(\log P)$ Segment Tree** over monoid `AggState`s for arbitrary sliding `ROWS`/`RANGE` frames.
  - **Recursive CTEs**: Semi-naive fixpoint evaluation with `max_recursive_iterations` guard.

---

## 6. Verification Oracles, Wire Server & WASM Studio (`M07`–`M10`)

- **Differential Verification (`vectra-oracle`)**: Pure-Rust RFC 1321 MD5 `sqllogictest` runner (26 `.slt` suites, 5,794 assertions), type-aware `SQLsmith` fuzzer comparing VectraDB (10% in 4 MiB forced-spill mode) against bundled SQLite (25,000 consecutive seeds, 0 mismatches), and AST delta-debugging shrinker.
- **TPC-H Suite (`vectra-tpch`)**: Pure-Rust streaming `dbgen` + all 22 official SQL queries validated 22/22 against SQLite at `SF=0.01` and benchmarked at `SF=0.1`.
- **PostgreSQL v3 Server & CLI (`vectra-server`)**: From-scratch async `tokio` TCP server supporting `SSLRequest`, `Startup`, Simple Query, Extended Query (`Parse`/`Bind`/`Describe`/`Execute`/`Sync` + `PortalSuspended`), exact PG Type OIDs & SQLSTATEs, and out-of-band `CancelRequest` (`57014`).
- **WASM Studio (`vectra-wasm`, `studio/`)**: Browser-native TypeScript + Vite workbench compiling the engine to `wasm32-unknown-unknown` with an interactive 4-Stage SVG Plan DAG Explorer, live `2–256 MiB` spill slider, CSV/Parquet drag-and-drop import, and 16-byte `ByteView` hex X-Ray.
