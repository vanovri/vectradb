/* tslint:disable */
/* eslint-disable */
/**
 * WebAssembly entrypoint wrapping VectraDB's full SQL, Optimizer, Storage & Execution stack.
 */
export class VectraWasmDb {
  free(): void;
  import_csv(table_name: string, csv_text: string): string;
  execute_json(sql: string): string;
  import_parquet(table_name: string, bytes: Uint8Array): string;
  memory_limit_mb(): number;
  list_tables_json(): string;
  set_memory_limit_mb(mb: number): void;
  inspect_storage_layout(table_name: string): string;
  constructor(memory_limit_mb: number);
  load_tpch(sf: number): string;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
  readonly memory: WebAssembly.Memory;
  readonly __wbg_vectrawasmdb_free: (a: number, b: number) => void;
  readonly vectrawasmdb_execute_json: (a: number, b: number, c: number) => [number, number];
  readonly vectrawasmdb_import_csv: (a: number, b: number, c: number, d: number, e: number) => [number, number];
  readonly vectrawasmdb_import_parquet: (a: number, b: number, c: number, d: number, e: number) => [number, number];
  readonly vectrawasmdb_inspect_storage_layout: (a: number, b: number, c: number) => [number, number];
  readonly vectrawasmdb_list_tables_json: (a: number) => [number, number];
  readonly vectrawasmdb_load_tpch: (a: number, b: number) => [number, number];
  readonly vectrawasmdb_memory_limit_mb: (a: number) => number;
  readonly vectrawasmdb_new: (a: number) => number;
  readonly vectrawasmdb_set_memory_limit_mb: (a: number, b: number) => void;
  readonly __wbindgen_export_0: WebAssembly.Table;
  readonly __wbindgen_malloc: (a: number, b: number) => number;
  readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
  readonly __wbindgen_free: (a: number, b: number, c: number) => void;
  readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;
/**
* Instantiates the given `module`, which can either be bytes or
* a precompiled `WebAssembly.Module`.
*
* @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
*
* @returns {InitOutput}
*/
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
* If `module_or_path` is {RequestInfo} or {URL}, makes a request and
* for everything else, calls `WebAssembly.instantiate` directly.
*
* @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
*
* @returns {Promise<InitOutput>}
*/
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
