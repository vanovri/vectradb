# M03 Design Document: `vectra-storage`

## 1. Objectives & Architectural Guarantees
`vectra-storage` implements VectraDB's compressed columnar storage engine (`#![deny(unsafe_code)]`), block-level ZoneMap and Split-Block Bloom Filter pruning, double-buffered `.vdb` persistence with hardware CRC32C validation, HyPer/DuckDB-style MVCC Snapshot Isolation with Write-Write conflict detection, RFC-4180 CSV ingestion, and a from-scratch native Apache Parquet v1/v2 reader & writer.

## 2. On-Disk & In-Memory Layouts

### 2.1 `ColumnPage` 32-Byte Header
Every encoded sub-block (up to `2,048` rows) serializes as:
- Bytes `0..4`: `[codec_id: u8, type_tag: u8, dec_precision: u8, dec_scale: i8]`
- Bytes `4..8`: `row_count: u32` (LE)
- Bytes `8..12`: `uncompressed_len: u32` (LE)
- Bytes `12..16`: `compressed_len: u32` (LE)
- Bytes `16..20`: `null_count: u32` (LE)
- Bytes `20..24`: `validity_compressed_len: u32` (LE)
- Bytes `24..28`: `crc32c: u32` (LE; computed over bytes `[0..24]` + compressed validity + compressed payload)
- Bytes `28..32`: `flags/reserved: u32`

### 2.2 Compression Codecs & Automatic `CodecSelector`
1. `Uncompressed (0)`: raw little-endian values (or `[len: u32, bytes...]` for varlen).
2. `FrameOfReferenceBitPack (1)`: stores `min_val: i64` + `bit_width: u8` (`0..=64`), packing `(val - min_val) as u64` tightly across bit boundaries.
3. `DeltaBinaryPacked (2)`: stores `first_val: i64`, computes zigzag-encoded deltas `d_i = val_i - val_{i-1}`, then applies FoR bit-packing over deltas — ideal for monotonic timestamps and IDs.
4. `RleCodec (3)`: run-length encodes repeated fixed values or variable strings as `(run_len: u32, value)`.
5. `DictionaryLz4Codec (4)`: builds a deduplicated per-page value dictionary, bit-packs dictionary codes, and compresses dictionary payload via pure-Rust standard LZ4 block format.
- `CodecSelector` trials applicable codecs per column type during RowGroup ingestion and persists the smallest verified encoding.

### 2.3 ZoneMaps & 256-Bit Split-Block Bloom Filters
- Each RowGroup (`65,536` rows) contains up to 32 sub-blocks (`2,048` rows each).
- Every RowGroup and sub-block maintains a `ZoneMap { min, max, null_count, row_count }`.
- Every RowGroup column builds a cache-line-aligned 256-bit **Split-Block Bloom Filter** (8 × `u32` lanes per block with Parquet standard salt multipliers) sized at 10 bits/key (<1.5% FPR).
- `PruningPredicate::evaluate` returns `AlwaysFalse | MaybeTrue | AlwaysTrue`, skipping page decompression completely on `AlwaysFalse`.

### 2.4 MVCC Snapshot Isolation & `.vdb` Double-Buffered Persistence
- `TransactionManager` issues monotonically increasing `TxnId` and `Timestamp` values.
- RowGroups record `(insert_txn, insert_commit_ts)` and per-RowGroup versioned deletion entries `row_offset -> (delete_txn, delete_commit_ts)`.
- Concurrent readers execute lock-free under Snapshot Isolation (SI); concurrent writers attempting to delete/update a row modified by another uncommitted or newer committed transaction abort cleanly with `VectraError::TransactionConflict`.
- `.vdb` file layout: 8-byte magic `VECTRADB`, version, dual alternating 64-byte Superblocks (generation counter + CRC32C) enabling crash-safe atomic ping-pong commits via `sync_all()`.

### 2.5 Native Apache Parquet v1/v2 Reader & Writer
- Implements Thrift Compact Protocol (`TCompactProtocol`) serialization/deserialization, Parquet RLE/Bit-Packing Hybrid coding, `PLAIN`, `RLE_DICTIONARY`/`PLAIN_DICTIONARY`, `DATA_PAGE` (v1) & `DATA_PAGE_V2`, and `UNCOMPRESSED` / `SNAPPY` / `LZ4_RAW` compression codecs from scratch with RowGroup min/max statistics pruning.
