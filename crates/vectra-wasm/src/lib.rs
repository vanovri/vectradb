#![deny(unsafe_code)]

use serde::Serialize;
use std::sync::Arc;
use vectra_exec::{format_explain_analyze, Database, ExecutionConfig, ExecutionMetricsSnapshot};
use vectra_optimizer::{PhysicalOp, PhysicalPlan};
use vectra_sql::LogicalPlan;
use vectra_storage::{load_csv_chunks, read_parquet_bytes, CsvOptions, TableStorageSummary};
use vectra_types::{BoundedMemoryPool, ByteView, LogicalType, ScalarValue, StringHeapBuilder};
use wasm_bindgen::prelude::*;

#[derive(Debug, Serialize)]
pub struct DagNodeDto {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub category: String,
    pub estimated_rows: Option<f64>,
    pub actual_rows: Option<u64>,
    pub q_error: Option<f64>,
    pub elapsed_ms: Option<f64>,
    pub peak_memory_bytes: Option<usize>,
    pub spilled_bytes: Option<u64>,
    pub zonemap_skipped: Option<u64>,
    pub zonemap_total: Option<u64>,
    pub children: Vec<DagNodeDto>,
}

#[derive(Debug, Serialize)]
pub struct ColumnDto {
    pub name: String,
    pub data_type: String,
}

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub sqlstate: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct StagesDto {
    pub bound_logical: DagNodeDto,
    pub unnested_logical: DagNodeDto,
    pub optimized_logical: DagNodeDto,
    pub physical: DagNodeDto,
}

#[derive(Debug, Serialize)]
pub struct ExecuteResponseDto {
    pub ok: bool,
    pub command_tag: String,
    pub columns: Vec<ColumnDto>,
    pub rows: Vec<Vec<Option<String>>>,
    pub total_rows: usize,
    pub truncated: bool,
    pub peak_memory_bytes: usize,
    pub total_spilled_bytes: u64,
    pub explain_analyze: Option<String>,
    pub stages: Option<StagesDto>,
    pub error: Option<ErrorDto>,
}

#[derive(Debug, Serialize)]
pub struct ByteViewXRayDto {
    pub column: String,
    pub value: String,
    pub byte_len: u32,
    pub is_inline: bool,
    pub prefix_ascii: String,
    pub bytes_hex: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct StorageInspectDto {
    pub ok: bool,
    pub summary: Option<TableStorageSummary>,
    pub byte_views: Vec<ByteViewXRayDto>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LoadResponseDto {
    pub ok: bool,
    pub message: String,
    pub tables: Vec<String>,
    pub error: Option<String>,
}

/// WebAssembly entrypoint wrapping VectraDB's full SQL, Optimizer, Storage & Execution stack.
#[wasm_bindgen]
pub struct VectraWasmDb {
    db: Arc<Database>,
    memory_limit_bytes: usize,
}

#[wasm_bindgen]
impl VectraWasmDb {
    #[wasm_bindgen(constructor)]
    pub fn new(memory_limit_mb: u32) -> Self {
        let bytes = (memory_limit_mb.clamp(2, 512) as usize) * 1024 * 1024;
        let cfg = ExecutionConfig {
            memory_limit_bytes: bytes,
            threads: 1,
            ..ExecutionConfig::default()
        };
        Self {
            db: Arc::new(Database::with_config(cfg)),
            memory_limit_bytes: bytes,
        }
    }

    pub fn set_memory_limit_mb(&mut self, mb: u32) {
        self.memory_limit_bytes = (mb.clamp(2, 512) as usize) * 1024 * 1024;
    }

    pub fn memory_limit_mb(&self) -> u32 {
        (self.memory_limit_bytes / (1024 * 1024)) as u32
    }

    pub fn list_tables_json(&self) -> String {
        serde_json::to_string(&self.db.catalog().list_tables()).unwrap_or_else(|_| "[]".into())
    }

    pub fn load_tpch(&mut self, sf: f64) -> String {
        let clamped_sf = sf.clamp(0.001, 0.05);
        match vectra_tpch::load_tpch_into_database(&self.db, clamped_sf) {
            Ok(sum) => serde_json::to_string(&LoadResponseDto {
                ok: true,
                message: format!(
                    "Loaded TPC-H SF={clamped_sf} ({} rows, {} lineitem)",
                    sum.total_rows(),
                    sum.lineitem_rows
                ),
                tables: self.db.catalog().list_tables(),
                error: None,
            })
            .unwrap(),
            Err(e) => serde_json::to_string(&LoadResponseDto {
                ok: false,
                message: String::new(),
                tables: self.db.catalog().list_tables(),
                error: Some(e.to_string()),
            })
            .unwrap(),
        }
    }

    pub fn import_csv(&mut self, table_name: &str, csv_text: &str) -> String {
        let pool = BoundedMemoryPool::unbounded().as_dyn();
        let opts = CsvOptions::default();
        let res = (|| -> vectra_types::Result<usize> {
            let (schema, chunks) = load_csv_chunks(csv_text, None, &opts, &pool)?;
            let _ = self.db.catalog().drop_table(table_name, true);
            let tbl = self.db.catalog().create_table(table_name, schema, false)?;
            let snap = self.db.catalog().begin_txn();
            let mut total = 0usize;
            for ch in &chunks {
                total += tbl.insert_chunk(ch, snap, &pool)?;
            }
            self.db.catalog().commit_txn(snap.txn_id)?;
            self.db.analyze_table(Some(table_name))?;
            Ok(total)
        })();
        match res {
            Ok(rows) => serde_json::to_string(&LoadResponseDto {
                ok: true,
                message: format!("Imported {rows} rows into `{table_name}` from CSV"),
                tables: self.db.catalog().list_tables(),
                error: None,
            })
            .unwrap(),
            Err(e) => serde_json::to_string(&LoadResponseDto {
                ok: false,
                message: String::new(),
                tables: self.db.catalog().list_tables(),
                error: Some(e.to_string()),
            })
            .unwrap(),
        }
    }

    pub fn import_parquet(&mut self, table_name: &str, bytes: &[u8]) -> String {
        let pool = BoundedMemoryPool::unbounded().as_dyn();
        let res = (|| -> vectra_types::Result<usize> {
            let (schema, chunks, _stats) = read_parquet_bytes(bytes, None, None, &pool)?;
            let _ = self.db.catalog().drop_table(table_name, true);
            let tbl = self.db.catalog().create_table(table_name, schema, false)?;
            let snap = self.db.catalog().begin_txn();
            let mut total = 0usize;
            for ch in &chunks {
                total += tbl.insert_chunk(ch, snap, &pool)?;
            }
            self.db.catalog().commit_txn(snap.txn_id)?;
            self.db.analyze_table(Some(table_name))?;
            Ok(total)
        })();
        match res {
            Ok(rows) => serde_json::to_string(&LoadResponseDto {
                ok: true,
                message: format!("Imported {rows} rows into `{table_name}` from Parquet"),
                tables: self.db.catalog().list_tables(),
                error: None,
            })
            .unwrap(),
            Err(e) => serde_json::to_string(&LoadResponseDto {
                ok: false,
                message: String::new(),
                tables: self.db.catalog().list_tables(),
                error: Some(e.to_string()),
            })
            .unwrap(),
        }
    }

    pub fn execute_json(&mut self, sql: &str) -> String {
        let mut sess = self.db.new_session();
        sess.set_memory_limit(self.memory_limit_bytes);
        sess.set_threads(1);

        match sess.execute_sql(sql) {
            Ok(qr) => {
                let columns: Vec<ColumnDto> = qr
                    .schema
                    .fields
                    .iter()
                    .map(|f| ColumnDto {
                        name: f.name.clone(),
                        data_type: f.logical_type.to_string(),
                    })
                    .collect();
                let total_rows = qr.total_rows();
                const MAX_UI_ROWS: usize = 500;
                let mut rows = Vec::with_capacity(total_rows.min(MAX_UI_ROWS));
                for ch in &qr.chunks {
                    for r in 0..ch.len() {
                        if rows.len() >= MAX_UI_ROWS {
                            break;
                        }
                        let row = ch
                            .get_row(r)
                            .into_iter()
                            .map(|v| {
                                if v.is_null() {
                                    None
                                } else {
                                    Some(v.to_display_string())
                                }
                            })
                            .collect();
                        rows.push(row);
                    }
                    if rows.len() >= MAX_UI_ROWS {
                        break;
                    }
                }

                let metrics_snap = qr.metrics.clone().unwrap_or_default();
                let explain_analyze = qr
                    .bundle
                    .as_ref()
                    .map(|b| format_explain_analyze(&b.physical, &metrics_snap));
                let stages = qr.bundle.as_ref().map(|b| {
                    let mut seq = 1usize;
                    StagesDto {
                        bound_logical: logical_to_dag(&b.bound_logical, "B", &mut seq),
                        unnested_logical: logical_to_dag(&b.unnested_logical, "U", &mut seq),
                        optimized_logical: logical_to_dag(&b.optimized_logical, "O", &mut seq),
                        physical: physical_to_dag(&b.physical, &metrics_snap),
                    }
                });

                serde_json::to_string(&ExecuteResponseDto {
                    ok: true,
                    command_tag: qr.command_tag,
                    columns,
                    rows,
                    total_rows,
                    truncated: total_rows > MAX_UI_ROWS,
                    peak_memory_bytes: metrics_snap.peak_memory_bytes,
                    total_spilled_bytes: metrics_snap.total_spilled_bytes,
                    explain_analyze,
                    stages,
                    error: None,
                })
                .unwrap()
            }
            Err(e) => serde_json::to_string(&ExecuteResponseDto {
                ok: false,
                command_tag: String::new(),
                columns: Vec::new(),
                rows: Vec::new(),
                total_rows: 0,
                truncated: false,
                peak_memory_bytes: 0,
                total_spilled_bytes: 0,
                explain_analyze: None,
                stages: None,
                error: Some(ErrorDto {
                    sqlstate: e.sqlstate().to_string(),
                    message: e.to_string(),
                }),
            })
            .unwrap(),
        }
    }

    pub fn inspect_storage_layout(&self, table_name: &str) -> String {
        let snap = self.db.catalog().begin_txn();
        let res = (|| -> vectra_types::Result<(TableStorageSummary, Vec<ByteViewXRayDto>)> {
            let summary = self.db.catalog().inspect_table(table_name, snap)?;
            let table = self.db.catalog().get_table(table_name)?;
            let pool = BoundedMemoryPool::unbounded().as_dyn();

            // Sample up to 16 Utf8 values to build live 16-byte Umbra German String ByteView X-Ray!
            let utf8_cols: Vec<(usize, String)> = table
                .schema()
                .fields
                .iter()
                .enumerate()
                .filter(|(_, f)| f.logical_type == LogicalType::Utf8)
                .map(|(i, f)| (i, f.name.clone()))
                .collect();

            let mut xrays = Vec::new();
            if !utf8_cols.is_empty() {
                let proj: Vec<usize> = utf8_cols.iter().map(|(i, _)| *i).collect();
                let (chunks, _) = table.scan(
                    Some(&proj),
                    None,
                    self.db.catalog().transaction_manager(),
                    snap,
                    &pool,
                )?;
                let mut heap = StringHeapBuilder::new(&pool);
                'outer: for ch in &chunks {
                    for r in 0..ch.len().min(8) {
                        for (local_c, (_, col_name)) in utf8_cols.iter().enumerate() {
                            if xrays.len() >= 12 {
                                break 'outer;
                            }
                            if let ScalarValue::Utf8(s) = ch.columns[local_c].get_scalar(r) {
                                let bv = heap.append(s.as_bytes())?;
                                xrays.push(encode_byte_view_xray(col_name, &s, bv));
                            }
                        }
                    }
                }
            }

            Ok((summary, xrays))
        })();
        let _ = self.db.catalog().commit_txn(snap.txn_id);

        match res {
            Ok((summary, byte_views)) => serde_json::to_string(&StorageInspectDto {
                ok: true,
                summary: Some(summary),
                byte_views,
                error: None,
            })
            .unwrap(),
            Err(e) => serde_json::to_string(&StorageInspectDto {
                ok: false,
                summary: None,
                byte_views: Vec::new(),
                error: Some(e.to_string()),
            })
            .unwrap(),
        }
    }
}

fn encode_byte_view_xray(column: &str, value: &str, bv: ByteView) -> ByteViewXRayDto {
    let raw = bv.raw_bytes();
    let prefix_bytes = &raw[4..8];
    let prefix_ascii: String = prefix_bytes
        .iter()
        .take((bv.len as usize).min(4))
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    ByteViewXRayDto {
        column: column.to_string(),
        value: value.to_string(),
        byte_len: bv.len,
        is_inline: bv.is_inline(),
        prefix_ascii,
        bytes_hex: raw.iter().map(|b| format!("{b:02X}")).collect(),
    }
}

fn logical_to_dag(plan: &LogicalPlan, prefix: &str, seq: &mut usize) -> DagNodeDto {
    let id = format!("{prefix}{}", *seq);
    *seq += 1;

    let (label, detail, category, children): (&str, String, &str, Vec<&LogicalPlan>) = match plan {
        LogicalPlan::EmptyRelation {
            produce_one_row, ..
        } => (
            "Empty",
            format!("one_row={produce_one_row}"),
            "scan",
            vec![],
        ),
        LogicalPlan::Values { rows, .. } => {
            ("Values", format!("{} rows", rows.len()), "scan", vec![])
        }
        LogicalPlan::Scan {
            table_name,
            columns,
            pushed_filters,
            ..
        } => (
            "Scan",
            format!(
                "{table_name} ({} cols, {} pushed)",
                columns.len(),
                pushed_filters.len()
            ),
            "scan",
            vec![],
        ),
        LogicalPlan::TableFunc { func, .. } => ("TableFunc", format!("{func:?}"), "scan", vec![]),
        LogicalPlan::CteScan { cte_name, .. } => ("CteScan", cte_name.clone(), "scan", vec![]),
        LogicalPlan::Filter { input, .. } => ("Filter", "predicate".into(), "filter", vec![input]),
        LogicalPlan::Project { input, exprs } => (
            "Project",
            format!("{} exprs", exprs.len()),
            "project",
            vec![input],
        ),
        LogicalPlan::Join {
            left,
            right,
            join_type,
            ..
        } => ("Join", format!("{join_type:?}"), "join", vec![left, right]),
        LogicalPlan::DependentJoin {
            left,
            right,
            join_type,
            correlated_cols,
            ..
        } => (
            "DependentJoin",
            format!("{join_type:?} (corr={})", correlated_cols.len()),
            "dependent",
            vec![left, right],
        ),
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            ..
        } => (
            "Aggregate",
            format!("keys={}, aggs={}", group_by.len(), aggregates.len()),
            "agg",
            vec![input],
        ),
        LogicalPlan::Window { input, windows } => (
            "Window",
            format!("{} funcs", windows.len()),
            "window",
            vec![input],
        ),
        LogicalPlan::Sort { input, order_by } => (
            "Sort",
            format!("{} keys", order_by.len()),
            "sort",
            vec![input],
        ),
        LogicalPlan::TopN {
            input,
            limit,
            offset,
            ..
        } => (
            "TopN",
            format!("limit={limit}, off={offset}"),
            "sort",
            vec![input],
        ),
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => (
            "Limit",
            format!("limit={limit:?}, off={offset}"),
            "filter",
            vec![input],
        ),
        LogicalPlan::Distinct { input, .. } => {
            ("Distinct", "hash dedup".into(), "agg", vec![input])
        }
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
            ..
        } => (
            "SetOp",
            format!("{op:?} all={all}"),
            "join",
            vec![left, right],
        ),
        LogicalPlan::RecursiveCte {
            name,
            anchor,
            recursive,
            consumer,
            ..
        } => (
            "RecursiveCte",
            name.clone(),
            "join",
            vec![anchor, recursive, consumer],
        ),
    };

    let child_dtos = children
        .into_iter()
        .map(|c| logical_to_dag(c, prefix, seq))
        .collect();

    DagNodeDto {
        id,
        label: label.to_string(),
        detail,
        category: category.to_string(),
        estimated_rows: None,
        actual_rows: None,
        q_error: None,
        elapsed_ms: None,
        peak_memory_bytes: None,
        spilled_bytes: None,
        zonemap_skipped: None,
        zonemap_total: None,
        children: child_dtos,
    }
}

fn physical_to_dag(plan: &PhysicalPlan, metrics: &ExecutionMetricsSnapshot) -> DagNodeDto {
    let m = metrics.operators.get(&plan.plan_id);
    let (detail, category) = match &plan.op {
        PhysicalOp::Scan {
            table_name,
            projection,
            pruning_predicate,
            ..
        } => (
            format!(
                "{table_name} ({} cols, zm={})",
                projection.len(),
                pruning_predicate.is_some()
            ),
            "scan",
        ),
        PhysicalOp::HashJoin {
            join_type,
            left_keys,
            ..
        } => (format!("{join_type:?} ({} keys)", left_keys.len()), "join"),
        PhysicalOp::NestedLoopJoin { join_type, .. } => (format!("{join_type:?}"), "join"),
        PhysicalOp::HashAggregate {
            group_by,
            aggregates,
            ..
        } => (
            format!("keys={}, aggs={}", group_by.len(), aggregates.len()),
            "agg",
        ),
        PhysicalOp::ExternalSort { order_by, .. } => (format!("{} keys", order_by.len()), "sort"),
        PhysicalOp::TopN { limit, offset, .. } => (format!("limit={limit}, off={offset}"), "sort"),
        PhysicalOp::Window { windows, .. } => (format!("{} funcs", windows.len()), "window"),
        PhysicalOp::Filter { .. } | PhysicalOp::Limit { .. } => ("streaming".into(), "filter"),
        PhysicalOp::Project { exprs, .. } => (format!("{} cols", exprs.len()), "project"),
        _ => (plan.op_name().to_string(), "scan"),
    };

    let children = plan
        .children()
        .into_iter()
        .map(|c| physical_to_dag(c, metrics))
        .collect();

    DagNodeDto {
        id: format!("P{}", plan.plan_id),
        label: plan.op_name().to_string(),
        detail,
        category: category.to_string(),
        estimated_rows: Some(plan.estimated_rows),
        actual_rows: m.map(|x| x.output_rows),
        q_error: m.map(|x| x.q_error()),
        elapsed_ms: m.map(|x| (x.elapsed_nanos as f64) / 1_000_000.0),
        peak_memory_bytes: m.map(|x| x.peak_memory_bytes),
        spilled_bytes: m.map(|x| x.spilled_bytes),
        zonemap_skipped: m.map(|x| x.zonemap_blocks_skipped),
        zonemap_total: m.map(|x| x.zonemap_blocks_total),
        children,
    }
}
