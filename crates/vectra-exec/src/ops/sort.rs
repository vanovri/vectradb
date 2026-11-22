use crate::context::ExecutionContext;
use crate::spill::{SpillFile, SpillReader};
use crate::stream::{chunk_from_scalar_rows, ChunkStream};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;
use vectra_expr::eval_expr;
use vectra_optimizer::PhysicalSortExpr;
use vectra_types::{
    encode_composite_sort_key, DataChunk, LogicalType, MemoryPoolExt, MemoryReservation, Result,
    ScalarValue, SortOrder, TupleBuffer, VectraError, VECTOR_CAPACITY,
};

const SPILL_BLOCK_ROWS: usize = 512;
const MAX_MERGE_FANIN: usize = 32;

/// Executes `PhysicalOp::ExternalSort`, spilling compressed sorted runs to disk whenever
/// memory approaches the `BoundedMemoryPool` ceiling and merging via a streaming k-way heap.
pub fn exec_external_sort(
    plan_id: usize,
    input: ChunkStream,
    order_by: Vec<PhysicalSortExpr>,
    output_types: Vec<LogicalType>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    if order_by.is_empty() {
        return Ok(input);
    }
    let sort_orders: Vec<SortOrder> = order_by.iter().map(|o| o.order).collect();
    let key_types: Vec<LogicalType> = order_by.iter().map(|o| o.expr.data_type()).collect();
    let sort_budget = ctx.operator_budget(36, 100);

    let mut run_buf = TupleBuffer::new(output_types.clone(), &ctx.pool)?;
    let mut key_arena: Vec<u8> = Vec::new();
    let mut entries: Vec<(u32, u32, u32)> = Vec::new();
    let mut meta_res: MemoryReservation = ctx.pool.reserve(0)?;
    let mut spilled_runs: Vec<SpillFile> = Vec::new();

    let mut row_key_scalars = Vec::with_capacity(order_by.len());
    let mut key_scratch = Vec::with_capacity(64);

    for chunk_res in input {
        ctx.check_cancelled()?;
        let chunk = chunk_res?;
        let n = chunk.len();
        if n == 0 {
            continue;
        }
        ctx.metrics.record_input(plan_id, n as u64);

        // Evaluate sort expressions vectorized once per chunk.
        let mut key_vecs = Vec::with_capacity(order_by.len());
        for o in &order_by {
            key_vecs.push(eval_expr(&o.expr, &chunk, &ctx.pool)?);
        }

        // Check if adding this chunk would overflow the sort budget; if so, flush current run first!
        let est_chunk_bytes = chunk.memory_bytes() + n * 48;
        if run_buf.row_count() > 0
            && (run_buf.memory_bytes() + meta_res.bytes() + est_chunk_bytes >= sort_budget
                || ctx.pool.current_bytes() + est_chunk_bytes
                    >= ctx.pool.limit_bytes().saturating_sub(2 * 1024 * 1024))
        {
            let run_file = spill_sorted_run(plan_id, &run_buf, &key_arena, &mut entries, &ctx)?;
            spilled_runs.push(run_file);
            run_buf = TupleBuffer::new(output_types.clone(), &ctx.pool)?;
            key_arena.clear();
            key_arena.shrink_to_fit();
            entries.clear();
            entries.shrink_to_fit();
            meta_res.shrink(meta_res.bytes());
        }

        let row_range = run_buf.append_chunk(&chunk)?;
        for (local_r, row_id) in row_range.enumerate() {
            row_key_scalars.clear();
            for kv in &key_vecs {
                row_key_scalars.push(kv.get_scalar(local_r));
            }
            key_scratch.clear();
            encode_composite_sort_key(&row_key_scalars, &key_types, &sort_orders, &mut key_scratch);
            let k_off = key_arena.len() as u32;
            let k_len = key_scratch.len() as u32;
            key_arena.extend_from_slice(&key_scratch);
            entries.push((k_off, k_len, row_id as u32));
        }

        let needed_meta =
            key_arena.capacity() + entries.capacity() * std::mem::size_of::<(u32, u32, u32)>();
        let cur_meta = meta_res.bytes();
        if needed_meta > cur_meta {
            let delta = needed_meta - cur_meta;
            if meta_res.try_grow(delta).is_err() {
                // Hard OOM guard: immediately spill this run!
                let run_file = spill_sorted_run(plan_id, &run_buf, &key_arena, &mut entries, &ctx)?;
                spilled_runs.push(run_file);
                run_buf = TupleBuffer::new(output_types.clone(), &ctx.pool)?;
                key_arena.clear();
                key_arena.shrink_to_fit();
                entries.clear();
                entries.shrink_to_fit();
                meta_res.shrink(meta_res.bytes());
            }
        }
        ctx.metrics
            .record_peak_memory(plan_id, ctx.pool.peak_bytes());

        if run_buf.row_count() > 0 && run_buf.memory_bytes() + meta_res.bytes() >= sort_budget {
            let run_file = spill_sorted_run(plan_id, &run_buf, &key_arena, &mut entries, &ctx)?;
            spilled_runs.push(run_file);
            run_buf = TupleBuffer::new(output_types.clone(), &ctx.pool)?;
            key_arena.clear();
            key_arena.shrink_to_fit();
            entries.clear();
            entries.shrink_to_fit();
            meta_res.shrink(meta_res.bytes());
        }
    }

    // Fast path: everything fit in memory with 0 spills!
    if spilled_runs.is_empty() {
        if entries.is_empty() {
            return Ok(Box::new(std::iter::empty()));
        }
        entries.sort_unstable_by(|a, b| {
            let ka = &key_arena[a.0 as usize..(a.0 + a.1) as usize];
            let kb = &key_arena[b.0 as usize..(b.0 + b.1) as usize];
            ka.cmp(kb).then_with(|| a.2.cmp(&b.2))
        });
        let sorted_ids: Vec<usize> = entries.iter().map(|e| e.2 as usize).collect();
        drop(key_arena);
        drop(entries);
        drop(meta_res);
        return Ok(Box::new(InMemorySortStream {
            buf: run_buf,
            sorted_ids,
            pos: 0,
            ctx,
        }));
    }

    // Flush any remaining in-memory rows into one final run, then free in-memory run buffers!
    if !entries.is_empty() {
        let run_file = spill_sorted_run(plan_id, &run_buf, &key_arena, &mut entries, &ctx)?;
        spilled_runs.push(run_file);
    }
    drop(run_buf);
    drop(key_arena);
    drop(entries);
    drop(meta_res);

    // Multi-pass fan-in consolidation if more than `MAX_MERGE_FANIN` runs were generated.
    while spilled_runs.len() > MAX_MERGE_FANIN {
        ctx.check_cancelled()?;
        let mut next_pass = Vec::with_capacity(spilled_runs.len().div_ceil(MAX_MERGE_FANIN));
        let mut iter = spilled_runs.into_iter();
        loop {
            let group: Vec<SpillFile> = iter.by_ref().take(MAX_MERGE_FANIN).collect();
            if group.is_empty() {
                break;
            }
            if group.len() == 1 {
                next_pass.push(group.into_iter().next().unwrap());
            } else {
                let merged = merge_runs_to_spill(plan_id, group, &output_types, &ctx)?;
                next_pass.push(merged);
            }
        }
        spilled_runs = next_pass;
    }

    let merge_stream = KWayMergeStream::new(spilled_runs, output_types, ctx)?;
    Ok(Box::new(merge_stream))
}

struct InMemorySortStream {
    buf: TupleBuffer,
    sorted_ids: Vec<usize>,
    pos: usize,
    ctx: ExecutionContext,
}

impl Iterator for InMemorySortStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.sorted_ids.len() {
            return None;
        }
        if let Err(e) = self.ctx.check_cancelled() {
            return Some(Err(e));
        }
        let end = (self.pos + VECTOR_CAPACITY).min(self.sorted_ids.len());
        let slice = &self.sorted_ids[self.pos..end];
        self.pos = end;
        Some(self.buf.materialize_rows(slice, &self.ctx.pool))
    }
}

/// Sorts `entries` in place and writes compact `SPILL_BLOCK_ROWS`-sized blocks to a `SpillFile`.
/// Block binary format:
/// `[row_count: u32][tb_byte_len: u32][tb_bytes...][for each row: (key_len: u32, key_bytes)]`
fn spill_sorted_run(
    plan_id: usize,
    buf: &TupleBuffer,
    key_arena: &[u8],
    entries: &mut [(u32, u32, u32)],
    ctx: &ExecutionContext,
) -> Result<SpillFile> {
    entries.sort_unstable_by(|a, b| {
        let ka = &key_arena[a.0 as usize..(a.0 + a.1) as usize];
        let kb = &key_arena[b.0 as usize..(b.0 + b.1) as usize];
        ka.cmp(kb).then_with(|| a.2.cmp(&b.2))
    });

    let mut writer = ctx
        .spill
        .create_writer(plan_id, Some(Arc::clone(&ctx.metrics)))?;
    let types = buf.layout().types.clone();
    let mut block_payload = Vec::with_capacity(64 * 1024);
    let mut row_ids = Vec::with_capacity(SPILL_BLOCK_ROWS);

    let mut off = 0usize;
    while off < entries.len() {
        ctx.check_cancelled()?;
        let take = (entries.len() - off).min(SPILL_BLOCK_ROWS);
        row_ids.clear();
        for e in &entries[off..off + take] {
            row_ids.push(e.2 as usize);
        }
        let chunk = buf.materialize_rows(&row_ids, &ctx.pool)?;
        let mut sub_tb = TupleBuffer::new(types.clone(), &ctx.pool)?;
        sub_tb.append_chunk(&chunk)?;

        block_payload.clear();
        block_payload.extend_from_slice(&(take as u32).to_le_bytes());
        let tb_len_pos = block_payload.len();
        block_payload.extend_from_slice(&0u32.to_le_bytes());
        let tb_start = block_payload.len();
        sub_tb.serialize_to_bytes(&mut block_payload);
        let tb_len = (block_payload.len() - tb_start) as u32;
        block_payload[tb_len_pos..tb_len_pos + 4].copy_from_slice(&tb_len.to_le_bytes());

        for e in &entries[off..off + take] {
            let k = &key_arena[e.0 as usize..(e.0 + e.1) as usize];
            block_payload.extend_from_slice(&(k.len() as u32).to_le_bytes());
            block_payload.extend_from_slice(k);
        }

        writer.write_block(&block_payload)?;
        off += take;
    }

    writer.finish()
}

struct RunCursor {
    reader: SpillReader,
    types: Vec<LogicalType>,
    current_tb: Option<TupleBuffer>,
    current_keys: Vec<Vec<u8>>,
    pos: usize,
}

impl RunCursor {
    fn new(file: SpillFile, types: Vec<LogicalType>, ctx: &ExecutionContext) -> Result<Self> {
        let mut c = Self {
            reader: file.into_reader()?,
            types,
            current_tb: None,
            current_keys: Vec::new(),
            pos: 0,
        };
        c.load_next_block(ctx)?;
        Ok(c)
    }

    fn load_next_block(&mut self, ctx: &ExecutionContext) -> Result<bool> {
        self.current_tb = None;
        self.current_keys.clear();
        self.pos = 0;

        let Some(bytes) = self.reader.next_block()? else {
            return Ok(false);
        };
        if bytes.len() < 8 {
            return Err(VectraError::CorruptFormat(
                "truncated sort run block".into(),
            ));
        }
        let row_count = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let tb_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let tb_end = 8 + tb_len;
        if tb_end > bytes.len() {
            return Err(VectraError::CorruptFormat(
                "corrupt TupleBuffer slice in sort run".into(),
            ));
        }
        let tb =
            TupleBuffer::deserialize_from_bytes(self.types.clone(), &bytes[8..tb_end], &ctx.pool)?;
        let mut pos = tb_end;
        let mut keys = Vec::with_capacity(row_count);
        for _ in 0..row_count {
            if pos + 4 > bytes.len() {
                return Err(VectraError::CorruptFormat("truncated sort key len".into()));
            }
            let klen = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + klen > bytes.len() {
                return Err(VectraError::CorruptFormat(
                    "truncated sort key bytes".into(),
                ));
            }
            keys.push(bytes[pos..pos + klen].to_vec());
            pos += klen;
        }
        self.current_tb = Some(tb);
        self.current_keys = keys;
        Ok(row_count > 0)
    }

    fn has_current(&self) -> bool {
        self.pos < self.current_keys.len()
    }

    fn current_key(&self) -> Vec<u8> {
        self.current_keys[self.pos].clone()
    }

    fn pop_current_row(&mut self, ctx: &ExecutionContext) -> Result<(Vec<u8>, Vec<ScalarValue>)> {
        let row = self.current_tb.as_ref().unwrap().get_row(self.pos);
        let key = std::mem::take(&mut self.current_keys[self.pos]);
        self.pos += 1;
        if self.pos >= self.current_keys.len() {
            self.load_next_block(ctx)?;
        }
        Ok((key, row))
    }
}

#[derive(Eq, PartialEq)]
struct HeapEntry {
    key: Vec<u8>,
    run_idx: usize,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.run_idx.cmp(&other.run_idx))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn merge_runs_to_spill(
    plan_id: usize,
    runs: Vec<SpillFile>,
    types: &[LogicalType],
    ctx: &ExecutionContext,
) -> Result<SpillFile> {
    let mut cursors = Vec::with_capacity(runs.len());
    let mut heap: BinaryHeap<Reverse<HeapEntry>> = BinaryHeap::with_capacity(runs.len());
    for (idx, file) in runs.into_iter().enumerate() {
        let cursor = RunCursor::new(file, types.to_vec(), ctx)?;
        if cursor.has_current() {
            heap.push(Reverse(HeapEntry {
                key: cursor.current_key(),
                run_idx: idx,
            }));
        }
        cursors.push(cursor);
    }

    let mut writer = ctx
        .spill
        .create_writer(plan_id, Some(Arc::clone(&ctx.metrics)))?;
    let mut batch_rows: Vec<Vec<ScalarValue>> = Vec::with_capacity(SPILL_BLOCK_ROWS);
    let mut batch_keys: Vec<Vec<u8>> = Vec::with_capacity(SPILL_BLOCK_ROWS);
    let mut block_payload = Vec::with_capacity(64 * 1024);

    let flush_block = |rows: &mut Vec<Vec<ScalarValue>>,
                       keys: &mut Vec<Vec<u8>>,
                       payload: &mut Vec<u8>,
                       writer: &mut crate::spill::SpillWriter|
     -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let chunk = chunk_from_scalar_rows(types, rows, &ctx.pool)?;
        let mut tb = TupleBuffer::new(types.to_vec(), &ctx.pool)?;
        tb.append_chunk(&chunk)?;

        payload.clear();
        payload.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        let tb_len_pos = payload.len();
        payload.extend_from_slice(&0u32.to_le_bytes());
        let tb_start = payload.len();
        tb.serialize_to_bytes(payload);
        let tb_len = (payload.len() - tb_start) as u32;
        payload[tb_len_pos..tb_len_pos + 4].copy_from_slice(&tb_len.to_le_bytes());

        for k in keys.iter() {
            payload.extend_from_slice(&(k.len() as u32).to_le_bytes());
            payload.extend_from_slice(k);
        }
        writer.write_block(payload)?;
        rows.clear();
        keys.clear();
        Ok(())
    };

    while let Some(Reverse(entry)) = heap.pop() {
        ctx.check_cancelled()?;
        let ridx = entry.run_idx;
        let (key, row) = cursors[ridx].pop_current_row(ctx)?;
        batch_keys.push(key);
        batch_rows.push(row);
        if cursors[ridx].has_current() {
            heap.push(Reverse(HeapEntry {
                key: cursors[ridx].current_key(),
                run_idx: ridx,
            }));
        }
        if batch_rows.len() >= SPILL_BLOCK_ROWS {
            flush_block(
                &mut batch_rows,
                &mut batch_keys,
                &mut block_payload,
                &mut writer,
            )?;
        }
    }
    flush_block(
        &mut batch_rows,
        &mut batch_keys,
        &mut block_payload,
        &mut writer,
    )?;
    writer.finish()
}

struct KWayMergeStream {
    cursors: Vec<RunCursor>,
    heap: BinaryHeap<Reverse<HeapEntry>>,
    types: Vec<LogicalType>,
    ctx: ExecutionContext,
    done: bool,
}

impl KWayMergeStream {
    fn new(runs: Vec<SpillFile>, types: Vec<LogicalType>, ctx: ExecutionContext) -> Result<Self> {
        let mut cursors = Vec::with_capacity(runs.len());
        let mut heap = BinaryHeap::with_capacity(runs.len());
        for (idx, file) in runs.into_iter().enumerate() {
            let cursor = RunCursor::new(file, types.clone(), &ctx)?;
            if cursor.has_current() {
                heap.push(Reverse(HeapEntry {
                    key: cursor.current_key(),
                    run_idx: idx,
                }));
            }
            cursors.push(cursor);
        }
        Ok(Self {
            cursors,
            heap,
            types,
            ctx,
            done: false,
        })
    }
}

impl Iterator for KWayMergeStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.heap.is_empty() {
            self.done = true;
            return None;
        }
        if let Err(e) = self.ctx.check_cancelled() {
            self.done = true;
            return Some(Err(e));
        }

        let mut rows = Vec::with_capacity(VECTOR_CAPACITY);
        while rows.len() < VECTOR_CAPACITY {
            let Some(Reverse(entry)) = self.heap.pop() else {
                break;
            };
            let ridx = entry.run_idx;
            match self.cursors[ridx].pop_current_row(&self.ctx) {
                Ok((_key, row)) => {
                    rows.push(row);
                    if self.cursors[ridx].has_current() {
                        self.heap.push(Reverse(HeapEntry {
                            key: self.cursors[ridx].current_key(),
                            run_idx: ridx,
                        }));
                    }
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        if rows.is_empty() {
            self.done = true;
            return None;
        }
        Some(chunk_from_scalar_rows(&self.types, &rows, &self.ctx.pool))
    }
}

/// Bounded max-heap entry for `PhysicalTopN` (evicts largest key whenever `heap.len() > k`).
struct TopNEntry {
    key: Vec<u8>,
    seq: u64,
    row: Vec<ScalarValue>,
}

impl PartialEq for TopNEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.seq == other.seq
    }
}
impl Eq for TopNEntry {}

impl Ord for TopNEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for TopNEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Executes `PhysicalOp::TopN` using a bounded `O(N log K)` binary max-heap of capacity `limit + offset`.
pub fn exec_top_n(
    plan_id: usize,
    input: ChunkStream,
    order_by: Vec<PhysicalSortExpr>,
    limit: usize,
    offset: usize,
    output_types: Vec<LogicalType>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    let keep = limit.saturating_add(offset);
    if limit == 0 {
        return Ok(Box::new(std::iter::empty()));
    }
    // If keep is very large relative to memory, delegate to ExternalSort + Limit!
    if keep > 65_536 {
        let sorted = exec_external_sort(plan_id, input, order_by, output_types, ctx.clone())?;
        return Ok(crate::ops::scan::exec_limit(
            plan_id,
            sorted,
            Some(limit),
            offset,
            ctx,
        ));
    }

    let sort_orders: Vec<SortOrder> = order_by.iter().map(|o| o.order).collect();
    let key_types: Vec<LogicalType> = order_by.iter().map(|o| o.expr.data_type()).collect();
    let mut heap: BinaryHeap<TopNEntry> = BinaryHeap::with_capacity(keep + 1);
    let mut seq = 0u64;
    let mut key_scalars = Vec::with_capacity(order_by.len());
    let mut key_buf = Vec::with_capacity(64);

    for chunk_res in input {
        ctx.check_cancelled()?;
        let chunk = chunk_res?;
        let n = chunk.len();
        if n == 0 {
            continue;
        }
        ctx.metrics.record_input(plan_id, n as u64);

        let mut key_vecs = Vec::with_capacity(order_by.len());
        for o in &order_by {
            key_vecs.push(eval_expr(&o.expr, &chunk, &ctx.pool)?);
        }

        for r in 0..n {
            key_scalars.clear();
            for kv in &key_vecs {
                key_scalars.push(kv.get_scalar(r));
            }
            key_buf.clear();
            encode_composite_sort_key(&key_scalars, &key_types, &sort_orders, &mut key_buf);
            let cur_seq = seq;
            seq += 1;

            if heap.len() < keep {
                heap.push(TopNEntry {
                    key: key_buf.clone(),
                    seq: cur_seq,
                    row: chunk.get_row(r),
                });
            } else if let Some(top) = heap.peek() {
                if key_buf < top.key {
                    heap.pop();
                    heap.push(TopNEntry {
                        key: key_buf.clone(),
                        seq: cur_seq,
                        row: chunk.get_row(r),
                    });
                }
            }
        }
    }

    let mut sorted = heap.into_sorted_vec();
    if offset >= sorted.len() {
        return Ok(Box::new(std::iter::empty()));
    }
    let slice = sorted.drain(offset..);
    let mut out_rows: Vec<Vec<ScalarValue>> = slice.take(limit).map(|e| e.row).collect();

    let mut chunks = VecDeque::new();
    let mut off = 0;
    while off < out_rows.len() {
        let take = (out_rows.len() - off).min(VECTOR_CAPACITY);
        chunks.push_back(chunk_from_scalar_rows(
            &output_types,
            &out_rows[off..off + take],
            &ctx.pool,
        )?);
        off += take;
    }
    out_rows.clear();
    Ok(Box::new(chunks.into_iter().map(Ok)))
}
