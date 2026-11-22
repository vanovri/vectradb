# M07 Design Specification: `vectra-oracle` (Differential `sqllogictest`, Type-Aware `SQLsmith` Fuzzer, & AST Shrinker)

## 1. Purpose & Constitutional Isolation

`vectra-oracle` serves as VectraDB's automated ground-truth differential verification oracle:
1. **Strict `rusqlite` Quarantine**: Bundled SQLite (`rusqlite = { workspace = true }` with `bundled`) is allowed ONLY inside `crates/vectra-oracle`. All downstream crates (`vectra-tpch` dev-tests) import `vectra_oracle::SqliteOracle` without ever depending on `rusqlite` directly.
2. **Zero `unsafe` code**: `#![deny(unsafe_code)]`.
3. **Zero external crypto/parser crates**: Implements standard RFC 1321 MD5 (`md5.rs`) and full `sqllogictest` grammar (`slt.rs`) from scratch in pure Rust.

## 2. Architecture

### 2.1 `SqliteOracle` (`sqlite_oracle.rs`)
- Wraps `rusqlite::Connection::open_in_memory()` with `PRAGMA case_sensitive_like = ON;`.
- Executes DDL/DML and queries, returning canonical `OracleValue` (`Null`, `Int(i64)`, `Float(f64)`, `Text(String)`).
- Maps VectraDB `ScalarValue` (`Boolean(b)` -> `Int(0/1)` when comparing against SQLite's integer booleans, `Decimal128` -> `Float` / exact decimal string, `Date32`/`TimestampMicros` -> ISO text) and compares ordered or unordered row multisets with configurable floating-point tolerance (`1e-5`).

### 2.2 Standard `sqllogictest` Runner (`slt.rs`, `md5.rs`)
- Parses and executes `.slt` files supporting `statement ok`, `statement error [pattern]`, `query <I|R|T|B>+ [nosort|rowsort|valuesort] [label]`, literal result lines, `<N> values hashing to <md5>` (RFC 1321 MD5 over `\n`-terminated formatted values), `halt`, and comments.
- Houses **26 `.slt` test suites** in `crates/vectra-oracle/tests/slt/` totaling **>= 5,000 SQL assertions**.

### 2.3 Type-Aware Stochastic `SQLsmith` Fuzzer (`sqlsmith.rs`, `bin/vectra-sqlsmith.rs`)
- Deterministic `SplitMix64` generator building type-checked SQL queries over 6 multi-type tables (`t0..t5` containing NULLs, duplicates, negatives, edge-case strings, booleans, floats) loaded identically in both VectraDB and SQLite.
- Covers:
  - 1-to-6-way joins (`INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`)
  - Kleene 3-valued boolean trees (`AND`, `OR`, `NOT`, `IS [NOT] NULL`, `BETWEEN`, `IN`, `NOT IN`, `LIKE`)
  - `CASE WHEN`, `COALESCE`, `NULLIF`, scalar math & string functions
  - Correlated & uncorrelated `EXISTS`, `NOT EXISTS`, `IN (SELECT ...)`, `NOT IN (SELECT ...)`, scalar subqueries
  - `GROUP BY` + `HAVING` (`COUNT(*)`, `COUNT(x)`, `COUNT(DISTINCT x)`, `SUM`, `AVG`, `MIN`, `MAX`)
  - Window functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `SUM/COUNT/MIN/MAX OVER (PARTITION BY ... ORDER BY ... ROWS BETWEEN ...)`)
  - Set operations (`UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`)
  - `ORDER BY ... ASC/DESC NULLS FIRST/LAST` + `LIMIT` / `OFFSET`
- **Forced-Spill Stress**: 10% of seeds (`seed % 10 == 0`) additionally execute VectraDB under a hard **4 MB `BoundedMemoryPool`** ceiling to prove out-of-core spill paths never diverge from SQLite!

### 2.4 Automated AST Delta-Debugging Shrinker (`shrinker.rs`)
- Parses failing SQL into `vectra_sql::Statement`, iteratively applies AST reduction candidates (removing `WHERE`/`HAVING` conjuncts, dropping joins, pruning `SELECT` items, collapsing `CASE`/`OR`/`AND` branches, dropping `ORDER BY`/`LIMIT`, simplifying set operations) while checking that the differential mismatch still reproduces.
- Emits minimal `.slt` reproducer scripts into `artifacts/failures/seed_<N>.slt`.

## 3. M07 Verification Gates
1. `tests/slt_suite.rs` (`make slt`): >= 25 `.slt` files and >= 5,000 assertions pass 100%.
2. `vectra-sqlsmith --seeds 25000` (`make fuzz-oracle`): **25,000 consecutive seeds** (0..25,000) pass with **0 mismatches** and **0 panics** (including 2,500 seeds in 4 MB forced-spill mode).
3. Shrinker verification test confirming reduction of a multi-clause query to a minimal `.slt` reproducer.
