export interface DagNodeDto {
  id: string;
  label: string;
  detail: string;
  category: string;
  estimated_rows: number | null;
  actual_rows: number | null;
  q_error: number | null;
  elapsed_ms: number | null;
  peak_memory_bytes: number | null;
  spilled_bytes: number | null;
  zonemap_skipped: number | null;
  zonemap_total: number | null;
  children: DagNodeDto[];
}

export interface StagesDto {
  bound_logical: DagNodeDto;
  unnested_logical: DagNodeDto;
  optimized_logical: DagNodeDto;
  physical: DagNodeDto;
}

export interface ColumnDto {
  name: string;
  data_type: string;
}

export interface ErrorDto {
  sqlstate: string;
  message: string;
}

export interface ExecuteResponseDto {
  ok: boolean;
  command_tag: string;
  columns: ColumnDto[];
  rows: (string | null)[][];
  total_rows: number;
  truncated: boolean;
  peak_memory_bytes: number;
  total_spilled_bytes: number;
  explain_analyze: string | null;
  stages: StagesDto | null;
  error: ErrorDto | null;
}

export interface ColumnStorageSummary {
  name: string;
  logical_type: string;
  dominant_codec: string;
  uncompressed_bytes: number;
  compressed_bytes: number;
  compression_ratio: number;
  null_count: number;
  min_value: string | null;
  max_value: string | null;
  bloom_bytes: number;
}

export interface TableStorageSummary {
  table_name: string;
  total_rows: number;
  row_groups: number;
  sub_blocks: number;
  columns: ColumnStorageSummary[];
}

export interface ByteViewXRayDto {
  column: string;
  value: string;
  byte_len: number;
  is_inline: boolean;
  prefix_ascii: string;
  bytes_hex: string[];
}

export interface StorageInspectDto {
  ok: boolean;
  summary: TableStorageSummary | null;
  byte_views: ByteViewXRayDto[];
  error: string | null;
}

export interface LoadResponseDto {
  ok: boolean;
  message: string;
  tables: string[];
  error: string | null;
}
