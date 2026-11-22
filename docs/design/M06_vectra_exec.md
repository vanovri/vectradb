# M06 Design Specification: `vectra-exec` (Morsel-Driven Parallel Vectorized & Out-of-Core Execution Engine)

## 1. Purpose & Architectural Invariants

`vectra-exec` executes compiled `PhysicalPlan` trees from `vectra-optimizer` over `vectra-storage`'s MVCC compressed columnar tables, strictly adhering to `#![deny(unsafe_code)]`, hard `BoundedMemoryPool` byte ceilings, out-of-core LZ4 + CRC32C disk spilling, and cooperative query cancellation.

Key invariants:
1. **Zero `unsafe` code**: `#![deny(unsafe_code)]` crate-wide.
2. **Hard Memory Ceiling**: All operator buffers (`TupleBuffer`, hash directories, sort runs, aggregation tables, window partitions) reserve bytes from `BoundedMemoryPool` via RAII `MemoryReservation`. Under a 32 MB limit processing 1.5M-row (~180 MB) inputs, operators transition transparently to out-of-core disk spill and maintain `pool.peak_bytes() <= limit` with `0` leaked bytes on completion.
3. **Zero Leaked Spill Files**: Every `.vspill` temporary file is owned by an RAII `SpillFile` guard that deletes the file on `Drop`, guaranteeing 0 leftover files on normal completion, error, panic, or `CancelRequest`. Under `#[cfg(target_arch = "wasm32")]`, `SpillFile` seamlessly switches to a compressed in-memory page backend so `vectra-wasm` compiles cleanly without host filesystem syscalls.
4. **Streaming Vectorized Output**: Operators yield 2,048-row `DataChunk`s via a pull-based `ChunkStream` (`Box<dyn Iterator<Item = Result<DataChunk>> + Send>`), preventing output materialization blowups during streaming scans, joins, merges, and PG wire transfer.
5. **Per-Operator Live Telemetry**: Every `PhysicalPlan::plan_id` records `input_rows`, `output_rows`, `chunks_processed`, `elapsed_nanos`, `peak_memory_bytes`, `spilled_bytes`, and `zonemap_blocks_skipped` into `ExecutionMetrics` for `EXPLAIN ANALYZE` and M10 WASM Plan Explorer.

---

## 2. Core Components

### 2.1 Morsel-Driven Work-Stealing Scheduler (`morsel.rs`)
- Inspired by Leis et al. (SIGMOD 2014).
- Dispatches work morsels (RowGroup indices for `ColumnarScan`, radix partition IDs for parallel partition processing) into a `crossbeam_deque::Injector` consumed by `N` scoped worker threads (`std::thread::scope`) with local `Worker`/`Stealer` deques.
- Checks `ctx.is_cancelled()` at every morsel boundary; under `wasm32` or `threads == 1`, executes synchronously on the caller thread without spawning OS threads.

### 2.2 Out-of-Core Spill Manager (`spill.rs`)
- Page format per spilled block:
  - `[magic: b"VSPL" (4B)][uncompressed_len: u32][compressed_len: u32][crc32c: u32][lz4_payload...]`
- `SpillWriter` buffers up to 64 KB per spill block (or 16 KB per partition fan-out buffer so 64 partitions stay under ~1.2 MB RAM), LZ4-compresses via `vectra_storage::lz4_compress_block`, appends `crc32c::crc32c`, writes to `.vspill`, and increments `SpillManager::total_spilled_bytes` + operator `spilled_bytes`.
- `SpillReader` streams one compressed block at a time, verifying magic and CRC32C on every read.
- `SpillFile::drop` unlinks the file and decrements `active_files`.

### 2.3 Radix-Partitioned Grace Hash Join (`ops/hash_join.rs`)
- Supports all 11 `PhysicalJoinType`s: `Inner`, `LeftOuter`, `RightOuter`, `FullOuter`, `Cross`, `LeftSemi`, `LeftAnti`, `RightSemi`, `RightAnti`, `LeftMark` (exact 3-valued SQL `IN`/`NOT IN`/`EXISTS`/`ANY`/`ALL` NULL semantics via `probe_pair`), and `Single` (enforces `<= 1` right match per left row).
- Open-addressed hash directory with **16-bit salt tags** (`u16` high hash bits packed alongside chain head index) minimizing false cache-line probes into `TupleBuffer`.
- **Adaptive In-Memory -> Out-of-Core Transition**:
  - Builds in memory while reserved bytes stay within `build_budget` (~40% of `pool.limit_bytes()`).
  - On budget overflow, partitions already-buffered build tuples plus remaining build stream and full probe stream into `NUM_PARTITIONS = 64` radix partitions using top 6 bits of `xxHash64` key hash (recursive sub-partitioning with shifted bits up to depth 4 handles skewed partitions).
  - Non-null-safe NULL keys (`null_safe_keys[k] == false`) are routed to an unmatched fast path for Outer/Anti/Mark joins without ever false-matching.
  - Streams each partition `p in 0..64` independently (build small partition `p` -> stream probe partition `p` -> emit unmatched build rows for `RightOuter`/`FullOuter`/`RightAnti`/`RightSemi` -> drop partition `p` immediately).
- `NestedLoopJoin`: handles non-equi/cross joins and accelerates uncorrelated `LeftMark` with `probe_pair` using an $O(N + M)$ hash set tracking `right_is_empty` and `right_has_null`.

### 2.4 Two-Phase Spilling Hash Aggregate (`ops/hash_agg.rs`)
- Supports global aggregates (`group_by.is_empty()`), dense-integer `perfect_hash_hint`, `GROUPING SETS`/`ROLLUP`/`CUBE` + `__grouping_id`, `FILTER (WHERE ...)`, `DISTINCT` modifiers, and all 12 `AggregateFunc`s:
  - `CountStar`, `Count`, `Sum`, `Avg`, `Min`, `Max`, `StddevSamp`, `StddevPop` (Welford online algorithm + Chan parallel merge), `BoolAnd`, `BoolOr`, `StringAgg`, `ApproxCountDistinct` (`vectra_optimizer::HyperLogLog`).
- **Partitioned Partial-State Spill**:
  - Tracks live hash-table + distinct-set bytes via `MemoryReservation`.
  - When reserved bytes exceed `agg_budget` (~35% of pool limit), flushes all accumulated `(group_key, PartialAggState[])` entries into 64 hash-partitioned `SpillFile`s and clears the in-memory table, continuing ingestion with pre-aggregation.
  - Final merge streams partition `p = 0..64`: loads partial states for partition `p`, merges duplicate groups (including `DistinctSet` union, Chan `Welford` merge, `HyperLogLog::merge`), finalizes, yields 2,048-row chunks, and frees partition `p` before `p + 1`.

### 2.5 External k-Way Merge Sort & TopN (`ops/sort.rs`)
- Encodes keys using `vectra_types::encode_composite_sort_key` (exact bytewise total order across all 12 types and 4 `ASC/DESC × NULLS FIRST/LAST` combinations).
- Buffers rows up to `sort_budget` (~38% of pool limit), sorts in memory, and flushes compressed sorted runs (`SpillFile`) whenever exceeded.
- Streams sorted output lazily via a binary-heap k-way tournament merge (with multi-pass 32-way consolidation if run count exceeds 64).
- `PhysicalTopN` maintains a bounded binary max-heap of size `limit + offset` in $O(N \log K)$ time.

### 2.6 Vectorized Window Functions & $O(\log N)$ Segment-Tree Frames (`ops/window.rs`)
- Reuses `ExternalSort` to partition and order input out-of-core by `(partition_by..., order_by...)`, loading only one partition at a time to honor tight memory limits.
- Supports `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`, `LEAD`, `LAG`, `FIRST_VALUE`, `LAST_VALUE`, and all window aggregates (`SUM`, `COUNT`, `AVG`, `MIN`, `MAX`, `BOOL_AND`, `BOOL_OR`, `STDDEV_SAMP`, `STDDEV_POP`, `STRING_AGG`).
- **Segment-Tree Sliding Frame Evaluator**: builds an iterative bottom-up segment tree of monoid aggregate states per partition ($O(P)$ space/build, $O(\log P)$ per-row frame query) covering both `ROWS` and `RANGE` (peer-group & numeric-offset) frames, avoiding $O(P^2)$ worst-case blowups.

### 2.7 Set Operations, Recursive CTEs & High-Level `Database`/`Session` Facade (`ops/set_op.rs`, `engine.rs`)
- `Distinct`, `SetOp` (`UNION`/`INTERSECT`/`EXCEPT` with `ALL`/distinct and SQL `IS NOT DISTINCT FROM` null equality), and semi-naive fixpoint `RecursiveCte` with `max_recursive_iterations` cycle protection.
- `Database` & `Session` facade executes full SQL (`SELECT`, DDL, MVCC `INSERT`/`UPDATE`/`DELETE`, `COPY`, `ANALYZE`, `BEGIN`/`COMMIT`/`ROLLBACK`, `EXPLAIN [ANALYZE|LOGICAL|UNNESTED|PHYSICAL|JSON]`, `SET`/`SHOW`, parameterized `$1..$n` queries) and exposes streaming execution for low-RAM workloads and M09 PG wire server.

---

## 3. M06 Verification Gate (`tests/spill_torture.rs`)

1. **Out-of-Core 32 MB Spill Torture Test** on 1,500,000 rows (~180 MB raw):
   - `FULL OUTER JOIN`
   - `LEFT MARK JOIN` (`IN` / `NOT IN` with NULLs)
   - 1,000,000-group `GROUP BY` with `COUNT(DISTINCT)` + `SUM(Decimal128)`
   - 1,500,000-row `ORDER BY str DESC NULLS LAST, id ASC`
   - Sliding-frame `SUM(...) OVER (PARTITION BY dept ORDER BY ts ROWS BETWEEN 50 PRECEDING AND 10 FOLLOWING)`
   - Asserts: `spilled_bytes > 0`, `pool.peak_bytes() <= 32 MB`, `0` leaked bytes, `0` leftover `.vspill` files, and 100% row-for-row equivalence against unconstrained 2 GB RAM execution.
2. **Operator Correctness & Feature Suite**:
   - All 11 join modes (including `Single` >1 row error & `LeftMark` 3-valued NULL truth table), all 12 aggregates + `GROUPING SETS`/`ROLLUP`/`CUBE`, all window functions + `RANGE`/`ROWS` frames, `UNION`/`INTERSECT`/`EXCEPT`, recursive CTE graph traversal + cycle limit, MVCC `BEGIN`/`INSERT`/`UPDATE`/`DELETE`/`COMMIT`/`ROLLBACK`, `COPY` CSV/Parquet roundtrip, and mid-flight cooperative `CancelRequest`.
