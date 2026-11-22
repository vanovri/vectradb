# M09 Design Specification: `vectra-server` (Async PostgreSQL v3 Wire Protocol Server & Interactive CLI)

## 1. Purpose & Constitutional Guarantees

`vectra-server` exposes VectraDB over the real PostgreSQL v3 TCP binary wire protocol and an interactive REPL CLI:
1. **Zero `unsafe` code**: `#![deny(unsafe_code)]` across library and binaries.
2. **Zero `pgwire` dependency**: Implements the full PostgreSQL v3 wire protocol framing, startup handshake, authentication, Simple Query, Extended Query (`Parse`/`Bind`/`Describe`/`Execute`/`Close`/`Sync`/`Flush`), `PortalSuspended` cursor pagination, `SSLRequest`, and out-of-band `CancelRequest` from first principles over `tokio::net::TcpListener`.
3. **Production Concurrency & Isolation**: Multi-client async `tokio` tasks share `Arc<Database>` with per-connection `Session`s, MVCC transactions, cooperative `Arc<AtomicBool>` query cancellation, and graceful error recovery to `ReadyForQuery`.

## 2. Components

### 2.1 Wire Codec (`src/pgwire/codec.rs`)
- Big-endian PostgreSQL v3 message encoder & async reader.
- Exact PostgreSQL OID mapping (`BOOL=16`, `INT2=21`, `INT4=23`, `INT8=20`, `FLOAT8=701`, `NUMERIC=1700`, `DATE=1082`, `TIMESTAMP=1114`, `TEXT=25`, `BYTEA=17`).
- Exact 5-character SQLSTATE mapping via `VectraError::sqlstate()` (`42601`, `42P01`, `42703`, `22012`, `40001`, `53200`, `57014`).

### 2.2 Async Server (`src/pgwire/server.rs`)
- Tracks live backends `(pid: i32, secret_key: i32) -> Arc<AtomicBool>` for real out-of-band `CancelRequest` (`code = 80877102`).
- Supports `SSLRequest` (`'N'`), `StartupMessage`, Simple Query (`'Q'`), Extended Query (`'P'/'B'/'D'/'E'/'C'/'S'/'H'`) with `$1..$n` typed/text parameter binding and `max_rows` `PortalSuspended` streaming, plus `pg_catalog`/`information_schema.tables`/`version()`/`current_database()` compatibility.

### 2.3 Interactive CLI & Visual Inspector (`src/cli.rs`, `bin/vectra-cli.rs`, `bin/vectra-server.rs`)
- Supports `.help`, `.tables`, `.schema`, `.stats`, `.tpch <sf>`, `.mode table|csv|json`, `.timer`, `.memlimit`, `.threads`, `.quit`.
- Renders Unicode box-drawing result grids and `EXPLAIN ANALYZE` trees with wall-time bars, `Est` vs `Act` `q-error`, peak RAM, spilled bytes, and ZoneMap skip ratios.

## 3. M09 Verification Gate (`tests/m09_pgwire_concurrency.rs`)
- 32 concurrent async TCP clients executing interleaved TPC-H queries, MVCC `BEGIN/INSERT/COMMIT` writes, Extended Query `$1/$2` prepared statements with `PortalSuspended` pagination, intentional error SQLSTATE + recovery, `SSLRequest`, and real out-of-band mid-flight `CancelRequest` returning `SQLSTATE 57014`.
