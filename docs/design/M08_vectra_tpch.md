# M08 Design Specification: `vectra-tpch` (Pure-Rust TPC-H `dbgen`, 22/22 Oracle Validation & Benchmark Suite)

## 1. Purpose & Constitutional Guarantees

`vectra-tpch` provides a zero-external-tool, pure-Rust TPC-H benchmark and validation suite:
1. **Zero `unsafe` code**: `#![deny(unsafe_code)]` crate-wide.
2. **Strict `rusqlite` Isolation**: `vectra-tpch` NEVER depends on `rusqlite`. Differential validation delegates to `vectra-oracle::SqliteOracle` behind the default `oracle` Cargo feature, allowing `vectra-wasm` (M10) to depend on `vectra-tpch` with `default-features = false` to run `load_tpch(sf)` directly inside the browser!
3. **Honest Telemetry**: `vectra-tpch-bench` runs real timed iterations and generates `docs/benchmarks/tpch_results.md` + `docs/benchmarks/tpch_results.json` directly from measured execution latencies, row counts, `BoundedMemoryPool::peak_bytes()`, ZoneMap pruning stats, and storage compression ratios.

## 2. Components

### 2.1 Deterministic Streaming `dbgen` (`schema.rs`, `dbgen.rs`)
- Generates all 8 official TPC-H tables (`region`, `nation`, `supplier`, `part`, `partsupp`, `customer`, `orders`, `lineitem`) at arbitrary scale factors (`SF = 0.005`, `0.01`, `0.1`, `1.0`) using deterministic `SplitMix64`.
- Authentic TPC-H vocabulary & distributions guarantee non-empty, meaningful results across all 22 queries (e.g., `EUROPE`/`%BRASS`/size 15 for Q2, `ASIA` 1994 for Q5, `FRANCE`/`GERMANY` 1995-96 for Q7, `BRAZIL`/`ECONOMY ANODIZED STEEL` for Q8, `%green%` for Q9, `special requests` for Q13, `PROMO%` for Q14, `Customer...Complaints` for Q16, `Brand#12/23/34` + `SM/MED/LG` containers for Q19, `forest%`/`CANADA` for Q20, `SAUDI ARABIA` multi-supplier late orders for Q21, and country phone codes + zero-order customers for Q22).
- Streams directly in 2,048-row `DataChunk` batches into `vectra-exec::Database` and optionally exports `.parquet` files or populates `SqliteOracle` with identical rows for bit-accurate validation.

### 2.2 All 22 Official Queries (`queries/q01.sql`..`q22.sql`, `queries.rs`)
- Embeds all 22 standard ANSI SQL TPC-H queries (`Q1`–`Q22`) for VectraDB execution alongside exact date-translated SQLite reference translations (`DATE '...' +/- INTERVAL` -> ISO date literal, `EXTRACT(YEAR FROM d)` -> `CAST(substr(d, 1, 4) AS INTEGER)`, `SUBSTRING(x FROM 1 FOR 2)` -> `substr(x, 1, 2)`).

### 2.3 Validation & Benchmark Harness (`validation.rs`, `bin/vectra-tpch-bench.rs`, `tests/m08_tpch_validation.rs`)
- `validate_all_22_against_sqlite(sf)`: validates 22/22 queries at `SF=0.01` against `SqliteOracle`, verifying exact row counts, exact ordering, numerical equivalence (within floating-point tolerance against SQLite's IEEE-754 REAL vs VectraDB's exact `Decimal128`), and 0 leaked bytes in `BoundedMemoryPool`.
- `vectra-tpch-bench --sf 0.1 --validate-sf 0.01 --output docs/benchmarks/tpch_results.md`: validates all 22/22 queries at `SF=0.01`, then executes warm-up + 3 timed runs at `SF=0.1` (~866K rows, ~600K `lineitem` rows), writing authentic benchmark tables & telemetry to `docs/benchmarks/tpch_results.md`.
