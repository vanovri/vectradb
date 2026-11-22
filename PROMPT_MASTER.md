# VectraDB Master Autonomous Execution Specification (M01–M10)

This repository implements **VectraDB**: a zero-shortcut Vectorized Columnar OLAP SQL Engine, Cascades/DPccp Cost-Based Query Optimizer, Out-of-Core Parallel Execution Runtime, PostgreSQL v3 Wire Protocol Server, and WebAssembly Optimizer & Execution Studio in Rust.

See `CLAUDE.md` for constitutional engineering rules, `PROGRESS.md` for verified module status, and `docs/design/MXX_*.md` for detailed per-module invariants.

## Modules
- **M01 (`crates/vectra-types`)**: 64B Cache-Aligned Buffers, `BoundedMemoryPool`, Umbra German Strings (`ByteView`), `ValidityMask`, `SelectionVector`, `Vector` encodings, `DataChunk`, `TupleBuffer`, normalized binary sort keys.
- **M02 (`crates/vectra-expr`)**: Late-materialization Expression VM, Kleene 3-valued logic, SIMD kernels (`AVX2`/`NEON`/Scalar), built-in scalar functions, vectorized `xxHash64`.
- **M03 (`crates/vectra-storage`)**: Columnar RowGroups, ZoneMaps, Split-Block Bloom Filters, 5 compression codecs (`Uncompressed`, `FoR+BitPack`, `DeltaBinaryPacked`, `RLE`, `Dictionary+LZ4`), `.vdb` double-buffered persistence with CRC32C, MVCC Snapshot Isolation, native Parquet v2 reader/writer & RFC-4180 CSV loader.
- **M04 (`crates/vectra-sql`)**: Zero-copy Lexer, Pratt SQL Parser, caret span diagnostics, deterministic AST Pretty-Printer, Semantic Binder + LUB Type Coercion Lattice, Neumann-Kemper (BTW 2015) Arbitrary Subquery Unnesting.
- **M05 (`crates/vectra-optimizer`)**: HyperLogLog++, Equi-Height Histograms, algebraic & join-simplification rewrite passes, Predicate Pushdown, Projection Pruning, `DPccp`/`DPsize` + `GOO`/`IDP` Bushy Join Order Optimizer & Physical Cost Model.
- **M06 (`crates/vectra-exec`)**: Morsel-driven parallel pipeline scheduler, `SpillManager`, Radix Grace Hash Join with spill, Two-Phase Hash Aggregation with Grouping Sets & spill, External k-Way Merge Sort, Top-N heap, Segment-Tree Sliding Window operator, Recursive CTE executor.
- **M07 (`crates/vectra-oracle`)**: `sqllogictest` runner (5,000+ assertions), type-aware `SQLsmith` differential fuzzer (25,000 seeds vs bundled SQLite), automated AST delta-debugging shrinker.
- **M08 (`crates/vectra-tpch`)**: Deterministic native TPC-H `dbgen`, full Q1–Q22 execution, `CountingAllocator` telemetry, differential oracle validation & `SF=0.1` benchmark artifacts.
- **M09 (`crates/vectra-server`)**: From-scratch async PostgreSQL v3 wire server (`pgwire`) + interactive `vectra-cli` REPL with visual `EXPLAIN ANALYZE` `q-error` tree renderer.
- **M10 (`crates/vectra-wasm` & `studio/`)**: `wasm32-unknown-unknown` engine bridge + TypeScript/Vite interactive Plan DAG & ByteView X-Ray Studio + root `Makefile` proof suite.
