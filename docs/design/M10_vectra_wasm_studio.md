# M10 Design Specification: `vectra-wasm` & WebAssembly Optimizer & Execution Studio

## 1. Purpose & Constitutional Guarantees

`vectra-wasm` compiles the entire VectraDB engine (M01–M06 + M08 `dbgen` with `default-features = false`) to `wasm32-unknown-unknown` with **zero `unsafe` code** (`#![deny(unsafe_code)]`) and powers `studio/`, a live browser-native Optimizer & Execution Studio.

## 2. WASM Bridge API (`crates/vectra-wasm/src/lib.rs`)

`#[wasm_bindgen] pub struct VectraWasmDb`:
- `new(memory_limit_mb: u32)`: initializes an isolated in-browser VectraDB instance with configurable `BoundedMemoryPool` ceiling.
- `set_memory_limit_mb(mb: u32)`: live memory ceiling control (2 MB – 256 MB) triggering in-browser compressed page spill on tight budgets.
- `load_tpch(sf: f64) -> String`: streams synthetic TPC-H tables directly into WASM columnar storage and runs `ANALYZE`.
- `import_csv(table_name, csv_text) -> String` / `import_parquet(table_name, bytes) -> String`: zero-backend browser data ingestion using native M03 parsers.
- `execute_json(sql) -> String`: returns structured JSON with result rows, SQLSTATE errors, formatted `EXPLAIN ANALYZE`, runtime metrics (`peak_memory_bytes`, `total_spilled_bytes`), and all 4 compilation DAG stages (`bound_logical`, `unnested_logical`, `optimized_logical`, `physical` annotated with live `actual_rows`, `q_error`, `spilled_bytes`, `zonemap_skipped`).
- `inspect_storage_layout(table_name) -> String`: returns `TableStorageSummary` + **16-byte Umbra German String `ByteView` X-Ray** hex breakdown distinguishing `<=12B` inline values from `>12B` heap-referenced prefix values.

## 3. Studio Workbench (`studio/`)

- Built with TypeScript + Vite + Vitest.
- Features:
  - SQL editor with curated presets (TPC-H Q01/Q02/Q08/Q19/Q21, Low-RAM Spill demo, German String `ByteView` demo) and live `2 MB – 256 MB` memory ceiling slider.
  - Custom pure-TS hierarchical SVG 4-Stage Plan DAG Explorer (`Bound -> Unnested -> Optimized -> Executed`) with row-volume edge scaling and `q-error > 10x` warning badges.
  - Paginated Results Grid, `EXPLAIN ANALYZE` terminal view, Columnar Codec/ZoneMap Inspector, and 16-byte `ByteView` Hex X-Ray visualizer.
  - Drag-and-drop CSV/Parquet file ingestion into live WASM tables.
- Automated `npm test` (Vitest) loads the real compiled `vectra_wasm_bg.wasm` bundle and verifies TPC-H execution, 4-stage DAG output, CSV import, `ByteView` hex X-Ray, and 2 MB spill telemetry.
