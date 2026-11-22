use crate::context::ExecutionContext;
use crate::morsel::execute_morsels;
use crate::stream::{chunk_from_scalar_rows, ChunkStream};
use std::collections::VecDeque;
use std::sync::Arc;
use vectra_expr::{eval_expr, eval_filter, PhysicalExpr};
use vectra_sql::TableFunctionKind;
#[cfg(not(target_arch = "wasm32"))]
use vectra_storage::{load_csv_chunks, read_parquet_bytes, CsvOptions};
use vectra_storage::{PruningPredicate, RowGroup, Table};
use vectra_types::{
    DataChunk, FlatVector, Result, ScalarValue, Schema, Vector, VectraError, VECTOR_CAPACITY,
};

/// Executes `PhysicalOp::Empty`.
pub fn exec_empty(
    produce_one_row: bool,
    schema: &Schema,
    ctx: &ExecutionContext,
) -> Result<ChunkStream> {
    if !produce_one_row {
        return Ok(Box::new(std::iter::empty()));
    }
    let types = schema.logical_types();
    let chunk = if types.is_empty() {
        DataChunk::new(Vec::new(), 1)
    } else {
        let row = vec![ScalarValue::Null; types.len()];
        chunk_from_scalar_rows(&types, &[row], &ctx.pool)?
    };
    Ok(Box::new(std::iter::once(Ok(chunk))))
}

/// Executes `PhysicalOp::Values`.
pub fn exec_values(
    rows: &[Vec<PhysicalExpr>],
    schema: &Schema,
    ctx: &ExecutionContext,
) -> Result<ChunkStream> {
    if rows.is_empty() {
        return Ok(Box::new(std::iter::empty()));
    }
    let types = schema.logical_types();
    let one_row = DataChunk::new(Vec::new(), 1);
    let mut scalar_rows = Vec::with_capacity(rows.len());
    for expr_row in rows {
        let mut r = Vec::with_capacity(expr_row.len());
        for expr in expr_row {
            let v = eval_expr(expr, &one_row, &ctx.pool)?;
            r.push(v.get_scalar(0));
        }
        scalar_rows.push(r);
    }
    let mut chunks = Vec::new();
    let mut off = 0;
    while off < scalar_rows.len() {
        let take = (scalar_rows.len() - off).min(VECTOR_CAPACITY);
        chunks.push(chunk_from_scalar_rows(
            &types,
            &scalar_rows[off..off + take],
            &ctx.pool,
        )?);
        off += take;
    }
    Ok(Box::new(chunks.into_iter().map(Ok)))
}

/// Streaming morsel-driven columnar table scan with ZoneMap/Bloom pruning and optional residual filter.
///
/// Processes batches of `RowGroup` morsels in parallel (bounded by `threads`) so memory stays
/// strictly bounded even when scanning millions of rows under tight `BoundedMemoryPool` limits!
pub fn exec_scan(
    plan_id: usize,
    table_name: &str,
    projection: &[usize],
    pruning_predicate: Option<&PruningPredicate>,
    residual_filter: Option<&PhysicalExpr>,
    ctx: &ExecutionContext,
) -> Result<ChunkStream> {
    let table = ctx.catalog.get_table(table_name)?;
    let rgs = table.snapshot_row_groups(ctx.catalog.transaction_manager(), ctx.snapshot);
    if rgs.is_empty() {
        return Ok(Box::new(std::iter::empty()));
    }

    let proj = projection.to_vec();
    let pruning = pruning_predicate.cloned();
    let residual = residual_filter.cloned();

    // Bound parallel in-flight RowGroups based on available memory limit so tight-RAM spill tests
    // never blow past `BoundedMemoryPool` during a parallel scan!
    let pool_limit = ctx.pool.limit_bytes();
    let max_batch_rgs = if pool_limit <= 48 * 1024 * 1024 {
        1
    } else {
        ctx.config.threads.clamp(1, 8)
    };

    Ok(Box::new(MorselScanStream {
        plan_id,
        rgs: VecDeque::from(rgs),
        projection: proj,
        pruning,
        residual,
        ctx: ctx.clone(),
        max_batch_rgs,
        pending_chunks: VecDeque::new(),
        active_rg: None,
        next_sb_idx: 0,
        rg_row_base: 0,
    }))
}

struct MorselScanStream {
    plan_id: usize,
    rgs: VecDeque<Arc<RowGroup>>,
    projection: Vec<usize>,
    pruning: Option<PruningPredicate>,
    residual: Option<PhysicalExpr>,
    ctx: ExecutionContext,
    max_batch_rgs: usize,
    pending_chunks: VecDeque<DataChunk>,
    /// When `max_batch_rgs == 1`, stream sub-blocks lazily one 2,048-row chunk at a time
    /// so peak scan memory is only ~1 sub-block (< 256 KB) instead of a full 65,536-row RowGroup!
    active_rg: Option<Arc<RowGroup>>,
    next_sb_idx: usize,
    rg_row_base: u32,
}

impl MorselScanStream {
    fn next_low_mem_chunk(&mut self) -> Result<Option<DataChunk>> {
        let txns = self.ctx.catalog.transaction_manager();
        let snap = self.ctx.snapshot;
        loop {
            self.ctx.check_cancelled()?;
            if self.active_rg.is_none() {
                let Some(rg) = self.rgs.pop_front() else {
                    return Ok(None);
                };
                if let Some(pred) = &self.pruning {
                    if pred.evaluate(&rg.zone_maps, Some(&rg.blooms))
                        == vectra_storage::BlockPruningResult::AlwaysFalse
                    {
                        let sb_cnt = rg.sub_blocks.len() as u64;
                        self.ctx
                            .metrics
                            .record_zonemap(self.plan_id, sb_cnt, sb_cnt);
                        continue;
                    }
                }
                self.active_rg = Some(rg);
                self.next_sb_idx = 0;
                self.rg_row_base = 0;
            }

            let rg = self.active_rg.as_ref().unwrap();
            if self.next_sb_idx >= rg.sub_blocks.len() {
                self.active_rg = None;
                continue;
            }

            let sb = &rg.sub_blocks[self.next_sb_idx];
            self.next_sb_idx += 1;
            let sb_rows = sb.row_count;
            let base = self.rg_row_base;
            self.rg_row_base += sb_rows;

            if let Some(pred) = &self.pruning {
                if pred.evaluate(&sb.zone_maps, None)
                    == vectra_storage::BlockPruningResult::AlwaysFalse
                {
                    self.ctx.metrics.record_zonemap(self.plan_id, 1, 1);
                    continue;
                }
            }
            self.ctx.metrics.record_zonemap(self.plan_id, 0, 1);

            let visible_sel = {
                let del_guard = rg.deletions.read();
                if del_guard.is_empty() {
                    None
                } else {
                    let mut keep = Vec::with_capacity(sb_rows as usize);
                    let mut deleted_any = false;
                    for local in 0..sb_rows {
                        let is_del = del_guard
                            .get(&(base + local))
                            .is_some_and(|&dtxn| txns.is_visible(dtxn, snap));
                        if is_del {
                            deleted_any = true;
                        } else {
                            keep.push(local as u16);
                        }
                    }
                    if keep.is_empty() {
                        continue;
                    }
                    if deleted_any {
                        Some(keep)
                    } else {
                        None
                    }
                }
            };

            let mut cols = Vec::with_capacity(self.projection.len());
            for &col_idx in &self.projection {
                let flat =
                    vectra_storage::decode_page(&sb.pages[col_idx].raw_bytes, &self.ctx.pool)?;
                cols.push(Vector::Flat(flat));
            }
            let chunk = match visible_sel {
                None => DataChunk::new(cols, sb_rows as usize),
                Some(sel) => {
                    DataChunk::with_selection(cols, vectra_types::SelectionVector::from_vec(sel))
                }
            };
            self.ctx
                .metrics
                .record_input(self.plan_id, chunk.len() as u64);

            if let Some(res_pred) = &self.residual {
                let sel = eval_filter(res_pred, &chunk, &self.ctx.pool)?;
                if sel.is_empty() {
                    continue;
                }
                let filtered = chunk.filter(&sel);
                return Ok(Some(filtered.compact(&self.ctx.pool)?));
            }
            return Ok(Some(chunk));
        }
    }

    fn refill_parallel_batch(&mut self) -> Result<()> {
        let mut batch = Vec::with_capacity(self.max_batch_rgs);
        for _ in 0..self.max_batch_rgs {
            if let Some(rg) = self.rgs.pop_front() {
                batch.push(rg);
            } else {
                break;
            }
        }
        if batch.is_empty() {
            return Ok(());
        }

        let proj = &self.projection;
        let pruning = self.pruning.as_ref();
        let residual = self.residual.as_ref();
        let txns = self.ctx.catalog.transaction_manager();
        let snap = self.ctx.snapshot;
        let pool = &self.ctx.pool;
        let plan_id = self.plan_id;
        let metrics = &self.ctx.metrics;

        let batch_results = execute_morsels(
            batch,
            self.max_batch_rgs,
            &self.ctx.cancelled,
            |_idx, rg| {
                let (chunks, tel) = Table::scan_row_group(&rg, proj, pruning, txns, snap, pool)?;
                metrics.record_zonemap(
                    plan_id,
                    tel.sub_blocks_pruned as u64,
                    tel.sub_blocks_total as u64,
                );
                metrics.record_input(plan_id, tel.rows_scanned as u64);
                let mut out = Vec::with_capacity(chunks.len());
                for ch in chunks {
                    if let Some(res_pred) = residual {
                        let sel = eval_filter(res_pred, &ch, pool)?;
                        if !sel.is_empty() {
                            out.push(ch.filter(&sel).compact(pool)?);
                        }
                    } else if !ch.is_empty() {
                        out.push(ch);
                    }
                }
                Ok(out)
            },
        )?;

        for chunks in batch_results {
            for ch in chunks {
                self.pending_chunks.push_back(ch);
            }
        }
        Ok(())
    }
}

impl Iterator for MorselScanStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Err(e) = self.ctx.check_cancelled() {
            return Some(Err(e));
        }
        if self.max_batch_rgs <= 1 {
            return match self.next_low_mem_chunk() {
                Ok(Some(ch)) => Some(Ok(ch)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            };
        }

        while self.pending_chunks.is_empty() {
            if self.rgs.is_empty() {
                return None;
            }
            if let Err(e) = self.refill_parallel_batch() {
                return Some(Err(e));
            }
        }
        self.pending_chunks.pop_front().map(Ok)
    }
}

/// Executes `PhysicalOp::TableFunc` (`generate_series`, `read_parquet`, `read_csv`).
pub fn exec_table_func(
    func: &TableFunctionKind,
    schema: &Schema,
    ctx: &ExecutionContext,
) -> Result<ChunkStream> {
    match func {
        TableFunctionKind::GenerateSeries { start, stop, step } => {
            if *step == 0 {
                return Err(VectraError::ExecutionError(
                    "generate_series step cannot be zero".into(),
                ));
            }
            Ok(Box::new(GenerateSeriesStream {
                current: *start,
                stop: *stop,
                step: *step,
                done: (*step > 0 && *start > *stop) || (*step < 0 && *start < *stop),
                pool: Arc::clone(&ctx.pool),
                cancelled: Arc::clone(&ctx.cancelled),
            }))
        }
        TableFunctionKind::ReadParquet { path } => {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let bytes = std::fs::read(path)?;
                let (_pq_schema, chunks, _stats) =
                    read_parquet_bytes(&bytes, None, None, &ctx.pool)?;
                Ok(Box::new(chunks.into_iter().map(Ok)))
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = (path, schema);
                Err(VectraError::ExecutionError(
                    "read_parquet(path) is unavailable on wasm32; use import_parquet bytes".into(),
                ))
            }
        }
        TableFunctionKind::ReadCsv {
            path,
            delimiter,
            header,
        } => {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let text = std::fs::read_to_string(path)?;
                let opts = CsvOptions {
                    delimiter: *delimiter,
                    has_header: *header,
                    ..CsvOptions::default()
                };
                let (_s, chunks) = load_csv_chunks(&text, Some(schema), &opts, &ctx.pool)?;
                Ok(Box::new(chunks.into_iter().map(Ok)))
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = (path, delimiter, header, schema);
                Err(VectraError::ExecutionError(
                    "read_csv(path) is unavailable on wasm32; use import_csv bytes".into(),
                ))
            }
        }
    }
}

struct GenerateSeriesStream {
    current: i64,
    stop: i64,
    step: i64,
    done: bool,
    pool: Arc<dyn vectra_types::MemoryPool>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl Iterator for GenerateSeriesStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            self.done = true;
            return Some(Err(VectraError::Cancelled));
        }
        let mut vals = Vec::with_capacity(VECTOR_CAPACITY);
        while vals.len() < VECTOR_CAPACITY {
            vals.push(self.current);
            match self.current.checked_add(self.step) {
                Some(next_v)
                    if (self.step > 0 && next_v <= self.stop)
                        || (self.step < 0 && next_v >= self.stop) =>
                {
                    self.current = next_v;
                }
                _ => {
                    self.done = true;
                    break;
                }
            }
        }
        let len = vals.len();
        match FlatVector::from_i64_slice(&vals, &self.pool) {
            Ok(flat) => Some(Ok(DataChunk::new(vec![Vector::Flat(flat)], len))),
            Err(e) => Some(Err(e)),
        }
    }
}

/// Executes `PhysicalOp::CteScan`.
pub fn exec_cte_scan(cte_name: &str, ctx: &ExecutionContext) -> Result<ChunkStream> {
    let key = cte_name.to_ascii_lowercase();
    let chunks =
        ctx.cte_tables.get(&key).cloned().ok_or_else(|| {
            VectraError::ExecutionError(format!("unknown CTE table '{cte_name}'"))
        })?;
    Ok(Box::new(chunks.into_iter().map(Ok)))
}

/// Streaming vectorized `PhysicalOp::Filter`.
pub fn exec_filter(
    plan_id: usize,
    input: ChunkStream,
    predicate: PhysicalExpr,
    ctx: ExecutionContext,
) -> ChunkStream {
    Box::new(FilterStream {
        plan_id,
        input,
        predicate,
        ctx,
    })
}

struct FilterStream {
    plan_id: usize,
    input: ChunkStream,
    predicate: PhysicalExpr,
    ctx: ExecutionContext,
}

impl Iterator for FilterStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }
            let chunk = match self.input.next()? {
                Ok(c) => c,
                Err(e) => return Some(Err(e)),
            };
            if chunk.is_empty() {
                continue;
            }
            self.ctx
                .metrics
                .record_input(self.plan_id, chunk.len() as u64);
            match eval_filter(&self.predicate, &chunk, &self.ctx.pool) {
                Ok(sel) => {
                    if sel.is_empty() {
                        continue;
                    }
                    let filtered = chunk.filter(&sel);
                    // Compact when selectivity < 50% so filtered buffers release unreferenced data.
                    let out = if filtered.len() * 2 < chunk.len() {
                        match filtered.compact(&self.ctx.pool) {
                            Ok(c) => c,
                            Err(e) => return Some(Err(e)),
                        }
                    } else {
                        filtered
                    };
                    return Some(Ok(out));
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// Streaming vectorized `PhysicalOp::Project`.
pub fn exec_project(
    plan_id: usize,
    input: ChunkStream,
    exprs: Vec<PhysicalExpr>,
    ctx: ExecutionContext,
) -> ChunkStream {
    Box::new(ProjectStream {
        plan_id,
        input,
        exprs,
        ctx,
    })
}

struct ProjectStream {
    plan_id: usize,
    input: ChunkStream,
    exprs: Vec<PhysicalExpr>,
    ctx: ExecutionContext,
}

impl Iterator for ProjectStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }
            let chunk = match self.input.next()? {
                Ok(c) => c,
                Err(e) => return Some(Err(e)),
            };
            if chunk.is_empty() {
                continue;
            }
            self.ctx
                .metrics
                .record_input(self.plan_id, chunk.len() as u64);
            let n = chunk.len();
            let mut out_cols = Vec::with_capacity(self.exprs.len());
            for expr in &self.exprs {
                match eval_expr(expr, &chunk, &self.ctx.pool) {
                    Ok(v) => out_cols.push(v),
                    Err(e) => return Some(Err(e)),
                }
            }
            return Some(Ok(DataChunk::new(out_cols, n)));
        }
    }
}

/// Streaming `PhysicalOp::Limit` with offset skipping and early upstream termination!
pub fn exec_limit(
    plan_id: usize,
    input: ChunkStream,
    limit: Option<usize>,
    offset: usize,
    ctx: ExecutionContext,
) -> ChunkStream {
    Box::new(LimitStream {
        plan_id,
        input,
        remaining_offset: offset,
        remaining_limit: limit,
        ctx,
    })
}

struct LimitStream {
    plan_id: usize,
    input: ChunkStream,
    remaining_offset: usize,
    remaining_limit: Option<usize>,
    ctx: ExecutionContext,
}

impl Iterator for LimitStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining_limit == Some(0) {
            return None;
        }
        loop {
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }
            let chunk = match self.input.next()? {
                Ok(c) => c,
                Err(e) => return Some(Err(e)),
            };
            let n = chunk.len();
            if n == 0 {
                continue;
            }
            self.ctx.metrics.record_input(self.plan_id, n as u64);

            if self.remaining_offset >= n {
                self.remaining_offset -= n;
                continue;
            }
            let start = self.remaining_offset;
            self.remaining_offset = 0;
            let avail = n - start;
            let take = match self.remaining_limit {
                Some(rem) => avail.min(rem),
                None => avail,
            };
            if let Some(rem) = &mut self.remaining_limit {
                *rem -= take;
            }
            if take == 0 {
                return None;
            }
            if start == 0 && take == n {
                return Some(Ok(chunk));
            }
            let sliced = chunk.slice(start, take);
            return Some(sliced.compact(&self.ctx.pool));
        }
    }
}
