use crate::context::ExecutionContext;
use crate::spill::{SpillFile, SpillReader, SpillWriter};
use crate::stream::{
    chunk_from_scalar_rows, eval_and_hash_keys, scalar_eq_not_distinct, CanonicalScalar,
    ChunkStream,
};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use vectra_expr::{eval_expr, eval_expr_scalar_reference, PhysicalExpr};
use vectra_optimizer::PhysicalJoinType;
use vectra_types::{
    DataChunk, LogicalType, MemoryPoolExt, MemoryReservation, Result, ScalarValue, TupleBuffer,
    VectraError, VECTOR_CAPACITY,
};

const NUM_PARTITIONS: usize = 64;
const MAX_RECURSIVE_DEPTH: usize = 3;

/// Open-addressed 16-bit salt-tagged bucket directory coalescing equal 64-bit hashes into
/// per-hash linked chains (`next_in_chain`), eliminating probe clustering under join skew!
struct SaltedHashDirectory {
    /// Each slot stores `(salt16, hash, head_plus_one)`. `head_plus_one == 0` means empty.
    slots: Vec<HashSlot>,
    mask: usize,
    /// `next_in_chain[row_id]` = 1-based next `row_id + 1`, or `0` for end of chain.
    next_in_chain: Vec<u32>,
    _reservation: MemoryReservation,
}

#[derive(Clone, Copy, Default)]
struct HashSlot {
    salt: u16,
    head_plus_one: u32,
    hash: u64,
}

impl SaltedHashDirectory {
    fn build(
        hashes: &[u64],
        eligible: &[bool],
        pool: &Arc<dyn vectra_types::MemoryPool>,
    ) -> Result<Self> {
        let count = hashes.len();
        let cap = (count.saturating_mul(2)).next_power_of_two().max(16);
        let mask = cap - 1;
        let bytes = cap * std::mem::size_of::<HashSlot>() + count * std::mem::size_of::<u32>();
        let reservation = pool.reserve(bytes)?;

        let mut slots = vec![HashSlot::default(); cap];
        let mut next_in_chain = vec![0u32; count];

        for (row_id, (&h, &ok)) in hashes.iter().zip(eligible.iter()).enumerate() {
            if !ok {
                continue;
            }
            let salt = (h >> 48) as u16;
            let mut idx = (h as usize) & mask;
            loop {
                let s = &mut slots[idx];
                if s.head_plus_one == 0 {
                    s.salt = salt;
                    s.hash = h;
                    s.head_plus_one = (row_id + 1) as u32;
                    break;
                }
                if s.salt == salt && s.hash == h {
                    next_in_chain[row_id] = s.head_plus_one;
                    s.head_plus_one = (row_id + 1) as u32;
                    break;
                }
                idx = (idx + 1) & mask;
            }
        }

        Ok(Self {
            slots,
            mask,
            next_in_chain,
            _reservation: reservation,
        })
    }

    #[inline]
    fn probe_head(&self, hash: u64) -> Option<usize> {
        let salt = (hash >> 48) as u16;
        let mut idx = (hash as usize) & self.mask;
        loop {
            let s = self.slots[idx];
            if s.head_plus_one == 0 {
                return None;
            }
            if s.salt == salt && s.hash == hash {
                return Some((s.head_plus_one - 1) as usize);
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline]
    fn next_row(&self, row_id: usize) -> Option<usize> {
        let nxt = self.next_in_chain[row_id];
        if nxt == 0 {
            None
        } else {
            Some((nxt - 1) as usize)
        }
    }
}

/// Complete in-memory hash build state for either an unspilled join or a single Grace partition.
struct HashBuildState {
    build_rows: TupleBuffer,
    build_keys: TupleBuffer,
    build_probe_rhs: Option<TupleBuffer>,
    dir: SaltedHashDirectory,
    /// Tracks whether each build row was matched (used for RightOuter, FullOuter, RightSemi, RightAnti).
    matched: Vec<bool>,
    _matched_res: MemoryReservation,
}

impl HashBuildState {
    #[allow(clippy::too_many_arguments)]
    fn build_from_chunks(
        chunks: Vec<DataChunk>,
        right_types: &[LogicalType],
        right_keys: &[PhysicalExpr],
        null_safe_keys: &[bool],
        probe_rhs_expr: Option<&PhysicalExpr>,
        track_matched: bool,
        ctx: &ExecutionContext,
    ) -> Result<Self> {
        let key_types: Vec<LogicalType> = right_keys.iter().map(|e| e.data_type()).collect();
        let mut build_rows = TupleBuffer::new(right_types.to_vec(), &ctx.pool)?;
        let mut build_keys = TupleBuffer::new(key_types, &ctx.pool)?;
        let mut build_probe_rhs = match probe_rhs_expr {
            Some(e) => Some(TupleBuffer::new(vec![e.data_type()], &ctx.pool)?),
            None => None,
        };
        let mut all_hashes: Vec<u64> = Vec::new();
        let mut eligible: Vec<bool> = Vec::new();

        for chunk in chunks {
            ctx.check_cancelled()?;
            let n = chunk.len();
            if n == 0 {
                continue;
            }
            let (kvecs, hashes) = eval_and_hash_keys(&chunk, right_keys, &ctx.pool)?;
            let key_chunk = DataChunk::new(kvecs, n);

            for r in 0..n {
                let mut ok = true;
                for (k, &ns) in null_safe_keys.iter().enumerate() {
                    if !ns && key_chunk.columns[k].get_scalar(r).is_null() {
                        ok = false;
                        break;
                    }
                }
                eligible.push(ok);
            }
            all_hashes.extend_from_slice(&hashes);
            build_rows.append_chunk(&chunk)?;
            build_keys.append_chunk(&key_chunk)?;
            if let (Some(expr), Some(tb)) = (probe_rhs_expr, &mut build_probe_rhs) {
                let v = eval_expr(expr, &chunk, &ctx.pool)?;
                let c = DataChunk::new(vec![v], n);
                tb.append_chunk(&c)?;
            }
        }

        let dir = SaltedHashDirectory::build(&all_hashes, &eligible, &ctx.pool)?;
        let matched_len = if track_matched {
            build_rows.row_count()
        } else {
            0
        };
        let matched_res = ctx.pool.reserve(matched_len)?;
        let matched = vec![false; matched_len];

        Ok(Self {
            build_rows,
            build_keys,
            build_probe_rhs,
            dir,
            matched,
            _matched_res: matched_res,
        })
    }
}

/// Parameters shared across in-memory and Grace-partitioned hash join execution.
#[derive(Clone)]
struct JoinSpec {
    plan_id: usize,
    join_type: PhysicalJoinType,
    left_types: Vec<LogicalType>,
    right_types: Vec<LogicalType>,
    output_types: Vec<LogicalType>,
    left_keys: Vec<PhysicalExpr>,
    right_keys: Vec<PhysicalExpr>,
    null_safe_keys: Vec<bool>,
    residual_filter: Option<PhysicalExpr>,
    probe_pair: Option<(PhysicalExpr, PhysicalExpr)>,
}

impl JoinSpec {
    fn tracks_build_matches(&self) -> bool {
        matches!(
            self.join_type,
            PhysicalJoinType::RightOuter
                | PhysicalJoinType::FullOuter
                | PhysicalJoinType::RightSemi
                | PhysicalJoinType::RightAnti
        )
    }

    fn emits_unmatched_left(&self) -> bool {
        matches!(
            self.join_type,
            PhysicalJoinType::LeftOuter
                | PhysicalJoinType::FullOuter
                | PhysicalJoinType::LeftAnti
                | PhysicalJoinType::LeftMark
                | PhysicalJoinType::Single
        )
    }
}

/// Executes `PhysicalOp::HashJoin` with 16-bit salt-tagged open-addressed hashing and
/// automatic 64-partition Radix Grace Hash Spill whenever build size approaches the memory ceiling!
#[allow(clippy::too_many_arguments)]
pub fn exec_hash_join(
    plan_id: usize,
    left_stream: ChunkStream,
    right_stream: ChunkStream,
    join_type: PhysicalJoinType,
    left_types: Vec<LogicalType>,
    right_types: Vec<LogicalType>,
    output_types: Vec<LogicalType>,
    left_keys: Vec<PhysicalExpr>,
    right_keys: Vec<PhysicalExpr>,
    null_safe_keys: Vec<bool>,
    residual_filter: Option<PhysicalExpr>,
    probe_pair: Option<(PhysicalExpr, PhysicalExpr)>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    let spec = JoinSpec {
        plan_id,
        join_type,
        left_types,
        right_types,
        output_types,
        left_keys,
        right_keys,
        null_safe_keys,
        residual_filter,
        probe_pair,
    };

    let build_budget = ctx.operator_budget(32, 100);
    let mut buffered_build_chunks: Vec<DataChunk> = Vec::new();
    let mut buffered_bytes: usize = 0;
    let mut spilled_build_parts: Option<ChunkPartitionWriters> = None;
    let mut unmatched_null_build_writer: Option<ChunkSpillBuffer> = None;

    // Phase 1: Ingest right (build) stream. Stay 100% in-memory while below `build_budget`;
    // seamlessly transition to 64-partition Radix Grace Hash Spill on overflow!
    for chunk_res in right_stream {
        ctx.check_cancelled()?;
        let chunk = chunk_res?;
        if chunk.is_empty() {
            continue;
        }
        ctx.metrics.record_input(plan_id, chunk.len() as u64);

        if let Some(parts) = &mut spilled_build_parts {
            partition_chunk_to_writers(
                &chunk,
                &spec.right_keys,
                &spec.null_safe_keys,
                parts,
                unmatched_null_build_writer.as_mut(),
                0,
                &ctx,
            )?;
            continue;
        }

        let compact = chunk.compact(&ctx.pool)?;
        buffered_bytes += compact.memory_bytes() + compact.len() * 48;
        buffered_build_chunks.push(compact);
        ctx.metrics
            .record_peak_memory(plan_id, ctx.pool.peak_bytes());

        let near_limit = buffered_bytes >= build_budget
            || ctx.pool.current_bytes() >= ctx.pool.limit_bytes().saturating_sub(4 * 1024 * 1024);

        if near_limit {
            // Transition dynamically into 64-partition Radix Grace Hash Join!
            let mut parts = ChunkPartitionWriters::new(
                plan_id,
                NUM_PARTITIONS,
                spec.right_types.clone(),
                &ctx,
            )?;
            let mut null_writer = if spec.tracks_build_matches() {
                Some(ChunkSpillBuffer::new(
                    plan_id,
                    spec.right_types.clone(),
                    &ctx,
                )?)
            } else {
                None
            };
            for prev in buffered_build_chunks.drain(..) {
                partition_chunk_to_writers(
                    &prev,
                    &spec.right_keys,
                    &spec.null_safe_keys,
                    &mut parts,
                    null_writer.as_mut(),
                    0,
                    &ctx,
                )?;
            }
            buffered_build_chunks.shrink_to_fit();
            buffered_bytes = 0;
            spilled_build_parts = Some(parts);
            unmatched_null_build_writer = null_writer;
        }
    }

    // Fast Path: build side fit cleanly in RAM -> stream probe side directly!
    if spilled_build_parts.is_none() {
        let probe_rhs = spec.probe_pair.as_ref().map(|(_, r)| r);
        let build_state = HashBuildState::build_from_chunks(
            buffered_build_chunks,
            &spec.right_types,
            &spec.right_keys,
            &spec.null_safe_keys,
            probe_rhs,
            spec.tracks_build_matches(),
            &ctx,
        )?;
        ctx.metrics
            .record_peak_memory(plan_id, ctx.pool.peak_bytes());
        return Ok(Box::new(InMemoryProbeStream {
            spec,
            build: build_state,
            left_stream,
            pending_out: VecDeque::new(),
            build_tail_emitted: false,
            ctx,
        }));
    }

    // Out-of-Core Grace Hash Join Path:
    // 1. Finalize all 64 build partitions.
    let build_part_files = spilled_build_parts.unwrap().finish_all_with_ctx(&ctx)?;
    let unmatched_null_build_file = match unmatched_null_build_writer {
        Some(mut w) => {
            w.flush(&ctx)?;
            let f = w.finish()?;
            if f.is_empty() {
                None
            } else {
                Some(f)
            }
        }
        None => None,
    };

    // 2. Partition the entire left (probe) stream into the SAME 64 radix partitions!
    let mut probe_parts =
        ChunkPartitionWriters::new(plan_id, NUM_PARTITIONS, spec.left_types.clone(), &ctx)?;
    let mut unmatched_null_probe_writer = if spec.emits_unmatched_left() {
        Some(ChunkSpillBuffer::new(
            plan_id,
            spec.left_types.clone(),
            &ctx,
        )?)
    } else {
        None
    };

    for chunk_res in left_stream {
        ctx.check_cancelled()?;
        let chunk = chunk_res?;
        if chunk.is_empty() {
            continue;
        }
        ctx.metrics.record_input(plan_id, chunk.len() as u64);
        partition_chunk_to_writers(
            &chunk,
            &spec.left_keys,
            &spec.null_safe_keys,
            &mut probe_parts,
            unmatched_null_probe_writer.as_mut(),
            0,
            &ctx,
        )?;
    }

    let probe_part_files = probe_parts.finish_all_with_ctx(&ctx)?;
    let unmatched_null_probe_file = match unmatched_null_probe_writer {
        Some(mut w) => {
            w.flush(&ctx)?;
            let f = w.finish()?;
            if f.is_empty() {
                None
            } else {
                Some(f)
            }
        }
        None => None,
    };

    let mut work_queue: VecDeque<GracePartitionPair> = VecDeque::with_capacity(NUM_PARTITIONS);
    for (bf, pf) in build_part_files.into_iter().zip(probe_part_files) {
        if bf.is_empty() && pf.is_empty() {
            continue;
        }
        work_queue.push_back(GracePartitionPair {
            build: bf,
            probe: pf,
            depth: 0,
        });
    }

    Ok(Box::new(GraceHashJoinStream {
        spec,
        build_budget,
        work_queue,
        active_probe: None,
        unmatched_null_probe: unmatched_null_probe_file
            .map(SpillChunkReader::new)
            .transpose()?,
        unmatched_null_build: unmatched_null_build_file
            .map(SpillChunkReader::new)
            .transpose()?,
        pending_out: VecDeque::new(),
        ctx,
    }))
}

#[inline]
fn hash_partition_idx(h: u64, depth: usize, num_parts: usize) -> usize {
    let shift = 58usize.saturating_sub(depth * 6);
    let mixed = if depth == 0 {
        h
    } else {
        h.wrapping_mul(0x9e37_79b9_7f4a_7c15u64.wrapping_add(depth as u64 * 31))
    };
    ((mixed >> shift) as usize) % num_parts
}

struct ChunkSpillBuffer {
    writer: SpillWriter,
    types: Vec<LogicalType>,
    rows: Vec<Vec<ScalarValue>>,
    max_rows: usize,
}

impl ChunkSpillBuffer {
    fn new(plan_id: usize, types: Vec<LogicalType>, ctx: &ExecutionContext) -> Result<Self> {
        let max_rows = if ctx.pool.limit_bytes() <= 8 * 1024 * 1024 {
            64
        } else {
            256
        };
        Ok(Self {
            writer: ctx
                .spill
                .create_writer(plan_id, Some(Arc::clone(&ctx.metrics)))?,
            types,
            rows: Vec::new(),
            max_rows,
        })
    }

    fn push_row(&mut self, row: Vec<ScalarValue>, ctx: &ExecutionContext) -> Result<()> {
        self.rows.push(row);
        if self.rows.len() >= self.max_rows {
            self.flush(ctx)?;
        }
        Ok(())
    }

    fn flush(&mut self, ctx: &ExecutionContext) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let chunk = chunk_from_scalar_rows(&self.types, &self.rows, &ctx.pool)?;
        self.rows.clear();
        let mut tb = TupleBuffer::new(self.types.clone(), &ctx.pool)?;
        tb.append_chunk(&chunk)?;
        let mut bytes = Vec::with_capacity(tb.memory_bytes() + 32);
        tb.serialize_to_bytes(&mut bytes);
        self.writer.write_block(&bytes)?;
        Ok(())
    }

    fn finish(self) -> Result<SpillFile> {
        // Note: callers flush before finish if any rows remain; guard if empty.
        self.writer.finish()
    }
}

struct ChunkPartitionWriters {
    parts: Vec<ChunkSpillBuffer>,
}

impl ChunkPartitionWriters {
    fn new(
        plan_id: usize,
        num_parts: usize,
        types: Vec<LogicalType>,
        ctx: &ExecutionContext,
    ) -> Result<Self> {
        let mut parts = Vec::with_capacity(num_parts);
        for _ in 0..num_parts {
            parts.push(ChunkSpillBuffer::new(plan_id, types.clone(), ctx)?);
        }
        Ok(Self { parts })
    }

    fn finish_all_with_ctx(self, ctx: &ExecutionContext) -> Result<Vec<SpillFile>> {
        let mut files = Vec::with_capacity(self.parts.len());
        for mut p in self.parts {
            p.flush(ctx)?;
            files.push(p.finish()?);
        }
        Ok(files)
    }
}

fn partition_chunk_to_writers(
    chunk: &DataChunk,
    key_exprs: &[PhysicalExpr],
    null_safe_keys: &[bool],
    parts: &mut ChunkPartitionWriters,
    mut null_unmatched: Option<&mut ChunkSpillBuffer>,
    depth: usize,
    ctx: &ExecutionContext,
) -> Result<()> {
    let n = chunk.len();
    if n == 0 {
        return Ok(());
    }
    let num_parts = parts.parts.len();
    let (kvecs, hashes) = eval_and_hash_keys(chunk, key_exprs, &ctx.pool)?;

    for (r, &h) in hashes.iter().enumerate().take(n) {
        let mut has_non_safe_null = false;
        for (k, &ns) in null_safe_keys.iter().enumerate() {
            if !ns && kvecs[k].get_scalar(r).is_null() {
                has_non_safe_null = true;
                break;
            }
        }
        if has_non_safe_null {
            if let Some(nw) = null_unmatched.as_deref_mut() {
                nw.push_row(chunk.get_row(r), ctx)?;
            }
            continue;
        }
        let p = hash_partition_idx(h, depth, num_parts);
        parts.parts[p].push_row(chunk.get_row(r), ctx)?;
    }
    // Keep flush invariant inside finish_all_with_ctx; flush any oversized part eagerly in push_row.
    Ok(())
}

struct SpillChunkReader {
    reader: SpillReader,
}

impl SpillChunkReader {
    fn new(file: SpillFile) -> Result<Self> {
        Ok(Self {
            reader: file.into_reader()?,
        })
    }

    fn next_chunk(
        &mut self,
        types: &[LogicalType],
        ctx: &ExecutionContext,
    ) -> Result<Option<DataChunk>> {
        let Some(bytes) = self.reader.next_block()? else {
            return Ok(None);
        };
        let tb = TupleBuffer::deserialize_from_bytes(types.to_vec(), &bytes, &ctx.pool)?;
        let ids: Vec<usize> = (0..tb.row_count()).collect();
        Ok(Some(tb.materialize_rows(&ids, &ctx.pool)?))
    }
}

/// Core vectorized probe engine over one probe `DataChunk` against an active `HashBuildState`.
fn probe_chunk_against_build(
    spec: &JoinSpec,
    build: &mut HashBuildState,
    probe_chunk: &DataChunk,
    out_chunks: &mut VecDeque<DataChunk>,
    ctx: &ExecutionContext,
) -> Result<()> {
    let n = probe_chunk.len();
    if n == 0 {
        return Ok(());
    }
    let (probe_key_vecs, probe_hashes) =
        eval_and_hash_keys(probe_chunk, &spec.left_keys, &ctx.pool)?;
    let probe_lhs_vec = match &spec.probe_pair {
        Some((lhs_expr, _)) => Some(eval_expr(lhs_expr, probe_chunk, &ctx.pool)?),
        None => None,
    };

    let l_cols = spec.left_types.len();
    let r_cols = spec.right_types.len();
    let mut out_rows: Vec<Vec<ScalarValue>> = Vec::new();
    let mut combined_buf: Vec<ScalarValue> = Vec::with_capacity(l_cols + r_cols);
    let mut probe_key_vals: Vec<ScalarValue> = Vec::with_capacity(spec.left_keys.len());

    let flush_rows =
        |rows: &mut Vec<Vec<ScalarValue>>, out_q: &mut VecDeque<DataChunk>| -> Result<()> {
            if !rows.is_empty() {
                out_q.push_back(chunk_from_scalar_rows(&spec.output_types, rows, &ctx.pool)?);
                rows.clear();
            }
            Ok(())
        };

    for (pr, &probe_hash) in probe_hashes.iter().enumerate().take(n) {
        let mut key_has_non_safe_null = false;
        probe_key_vals.clear();
        for (k, &ns) in spec.null_safe_keys.iter().enumerate() {
            let v = probe_key_vecs[k].get_scalar(pr);
            if !ns && v.is_null() {
                key_has_non_safe_null = true;
            }
            probe_key_vals.push(v);
        }

        let probe_row = probe_chunk.get_row(pr);

        // Handle LeftMark separately for crystal-clear 3-valued NULL semantics!
        if spec.join_type == PhysicalJoinType::LeftMark {
            let mark_val = if key_has_non_safe_null {
                ScalarValue::Boolean(false)
            } else {
                eval_left_mark_for_row(
                    spec,
                    build,
                    probe_hash,
                    &probe_key_vals,
                    &probe_row,
                    probe_lhs_vec.as_ref().map(|v| v.get_scalar(pr)),
                    &mut combined_buf,
                )?
            };
            let mut row = probe_row;
            row.push(mark_val);
            out_rows.push(row);
            if out_rows.len() >= VECTOR_CAPACITY {
                flush_rows(&mut out_rows, out_chunks)?;
            }
            continue;
        }

        let mut matched_count = 0usize;
        if !key_has_non_safe_null {
            let mut cur_br = build.dir.probe_head(probe_hash);
            while let Some(br) = cur_br {
                let mut keys_eq = true;
                for (k, &ns) in spec.null_safe_keys.iter().enumerate() {
                    let bv = build.build_keys.get_scalar(br, k);
                    if ns {
                        if !scalar_eq_not_distinct(&probe_key_vals[k], &bv) {
                            keys_eq = false;
                            break;
                        }
                    } else if bv.is_null() || !scalar_eq_not_distinct(&probe_key_vals[k], &bv) {
                        keys_eq = false;
                        break;
                    }
                }

                if keys_eq {
                    let pass_residual = if let Some(pred) = &spec.residual_filter {
                        combined_buf.clear();
                        combined_buf.extend_from_slice(&probe_row);
                        for c in 0..r_cols {
                            combined_buf.push(build.build_rows.get_scalar(br, c));
                        }
                        matches!(
                            eval_expr_scalar_reference(pred, &combined_buf)?,
                            ScalarValue::Boolean(true)
                        )
                    } else {
                        true
                    };

                    if pass_residual {
                        matched_count += 1;
                        if !build.matched.is_empty() {
                            build.matched[br] = true;
                        }

                        match spec.join_type {
                            PhysicalJoinType::Inner
                            | PhysicalJoinType::LeftOuter
                            | PhysicalJoinType::RightOuter
                            | PhysicalJoinType::FullOuter
                            | PhysicalJoinType::Cross => {
                                let mut row = Vec::with_capacity(l_cols + r_cols);
                                row.extend_from_slice(&probe_row);
                                for c in 0..r_cols {
                                    row.push(build.build_rows.get_scalar(br, c));
                                }
                                out_rows.push(row);
                                if out_rows.len() >= VECTOR_CAPACITY {
                                    flush_rows(&mut out_rows, out_chunks)?;
                                }
                            }
                            PhysicalJoinType::Single => {
                                if matched_count > 1 {
                                    return Err(VectraError::ExecutionError(
                                        "scalar subquery returned more than one row".into(),
                                    ));
                                }
                                let mut row = Vec::with_capacity(l_cols + r_cols);
                                row.extend_from_slice(&probe_row);
                                for c in 0..r_cols {
                                    row.push(build.build_rows.get_scalar(br, c));
                                }
                                out_rows.push(row);
                                if out_rows.len() >= VECTOR_CAPACITY {
                                    flush_rows(&mut out_rows, out_chunks)?;
                                }
                            }
                            PhysicalJoinType::LeftSemi => {
                                out_rows.push(probe_row.clone());
                                if out_rows.len() >= VECTOR_CAPACITY {
                                    flush_rows(&mut out_rows, out_chunks)?;
                                }
                                break;
                            }
                            PhysicalJoinType::LeftAnti => {
                                break;
                            }
                            PhysicalJoinType::RightSemi | PhysicalJoinType::RightAnti => {
                                // Only need to mark `build.matched[br] = true` (done above).
                            }
                            PhysicalJoinType::LeftMark => unreachable!(),
                        }
                    }
                }
                cur_br = build.dir.next_row(br);
            }
        }

        if matched_count == 0 {
            match spec.join_type {
                PhysicalJoinType::LeftOuter
                | PhysicalJoinType::FullOuter
                | PhysicalJoinType::Single => {
                    let mut row = Vec::with_capacity(l_cols + r_cols);
                    row.extend(probe_row);
                    row.resize(l_cols + r_cols, ScalarValue::Null);
                    out_rows.push(row);
                }
                PhysicalJoinType::LeftAnti => {
                    out_rows.push(probe_row);
                }
                _ => {}
            }
            if out_rows.len() >= VECTOR_CAPACITY {
                flush_rows(&mut out_rows, out_chunks)?;
            }
        }
    }

    flush_rows(&mut out_rows, out_chunks)?;
    Ok(())
}

fn eval_left_mark_for_row(
    spec: &JoinSpec,
    build: &HashBuildState,
    probe_hash: u64,
    probe_key_vals: &[ScalarValue],
    probe_row: &[ScalarValue],
    probe_lhs_val: Option<ScalarValue>,
    combined_buf: &mut Vec<ScalarValue>,
) -> Result<ScalarValue> {
    let r_cols = spec.right_types.len();
    let mut cur_br = build.dir.probe_head(probe_hash);

    if let Some(lhs_val) = probe_lhs_val {
        // SQL `IN` / `NOT IN` 3-valued NULL semantics within correlation bucket:
        let mut any_right_candidate = false;
        let mut saw_rhs_null = false;
        while let Some(br) = cur_br {
            let mut keys_eq = true;
            for (k, &ns) in spec.null_safe_keys.iter().enumerate() {
                let bv = build.build_keys.get_scalar(br, k);
                if ns {
                    if !scalar_eq_not_distinct(&probe_key_vals[k], &bv) {
                        keys_eq = false;
                        break;
                    }
                } else if bv.is_null() || !scalar_eq_not_distinct(&probe_key_vals[k], &bv) {
                    keys_eq = false;
                    break;
                }
            }
            if keys_eq {
                let pass_res = if let Some(pred) = &spec.residual_filter {
                    combined_buf.clear();
                    combined_buf.extend_from_slice(probe_row);
                    for c in 0..r_cols {
                        combined_buf.push(build.build_rows.get_scalar(br, c));
                    }
                    matches!(
                        eval_expr_scalar_reference(pred, combined_buf)?,
                        ScalarValue::Boolean(true)
                    )
                } else {
                    true
                };
                if pass_res {
                    any_right_candidate = true;
                    if lhs_val.is_null() {
                        // Left is NULL and at least 1 right candidate exists -> NULL!
                        return Ok(ScalarValue::Null);
                    }
                    let rhs_val = build.build_probe_rhs.as_ref().unwrap().get_scalar(br, 0);
                    if rhs_val.is_null() {
                        saw_rhs_null = true;
                    } else if scalar_eq_not_distinct(&lhs_val, &rhs_val) {
                        return Ok(ScalarValue::Boolean(true));
                    }
                }
            }
            cur_br = build.dir.next_row(br);
        }
        if !any_right_candidate {
            Ok(ScalarValue::Boolean(false))
        } else if saw_rhs_null {
            Ok(ScalarValue::Null)
        } else {
            Ok(ScalarValue::Boolean(false))
        }
    } else {
        // `EXISTS` or quantified `ANY`/`ALL` (Kleene OR over matching bucket):
        let mut saw_null = false;
        while let Some(br) = cur_br {
            let mut keys_eq = true;
            for (k, &ns) in spec.null_safe_keys.iter().enumerate() {
                let bv = build.build_keys.get_scalar(br, k);
                if ns {
                    if !scalar_eq_not_distinct(&probe_key_vals[k], &bv) {
                        keys_eq = false;
                        break;
                    }
                } else if bv.is_null() || !scalar_eq_not_distinct(&probe_key_vals[k], &bv) {
                    keys_eq = false;
                    break;
                }
            }
            if keys_eq {
                if let Some(pred) = &spec.residual_filter {
                    combined_buf.clear();
                    combined_buf.extend_from_slice(probe_row);
                    for c in 0..r_cols {
                        combined_buf.push(build.build_rows.get_scalar(br, c));
                    }
                    match eval_expr_scalar_reference(pred, combined_buf)? {
                        ScalarValue::Boolean(true) => return Ok(ScalarValue::Boolean(true)),
                        ScalarValue::Null => saw_null = true,
                        _ => {}
                    }
                } else {
                    return Ok(ScalarValue::Boolean(true));
                }
            }
            cur_br = build.dir.next_row(br);
        }
        if saw_null {
            Ok(ScalarValue::Null)
        } else {
            Ok(ScalarValue::Boolean(false))
        }
    }
}

/// Emits unmatched (or matched for `RightSemi`) build rows after exhausting probe side.
fn emit_build_tail(
    spec: &JoinSpec,
    build: &HashBuildState,
    out_chunks: &mut VecDeque<DataChunk>,
    ctx: &ExecutionContext,
) -> Result<()> {
    if !spec.tracks_build_matches() {
        return Ok(());
    }
    let want_matched = spec.join_type == PhysicalJoinType::RightSemi;
    let l_cols = spec.left_types.len();
    let r_cols = spec.right_types.len();
    let include_null_left = matches!(
        spec.join_type,
        PhysicalJoinType::RightOuter | PhysicalJoinType::FullOuter
    );

    let mut rows: Vec<Vec<ScalarValue>> = Vec::with_capacity(VECTOR_CAPACITY);
    for (br, &m) in build.matched.iter().enumerate() {
        if m != want_matched {
            continue;
        }
        if include_null_left {
            let mut row = vec![ScalarValue::Null; l_cols + r_cols];
            for c in 0..r_cols {
                row[l_cols + c] = build.build_rows.get_scalar(br, c);
            }
            rows.push(row);
        } else {
            rows.push(build.build_rows.get_row(br));
        }
        if rows.len() >= VECTOR_CAPACITY {
            out_chunks.push_back(chunk_from_scalar_rows(
                &spec.output_types,
                &rows,
                &ctx.pool,
            )?);
            rows.clear();
        }
    }
    if !rows.is_empty() {
        out_chunks.push_back(chunk_from_scalar_rows(
            &spec.output_types,
            &rows,
            &ctx.pool,
        )?);
    }
    Ok(())
}

struct InMemoryProbeStream {
    spec: JoinSpec,
    build: HashBuildState,
    left_stream: ChunkStream,
    pending_out: VecDeque<DataChunk>,
    build_tail_emitted: bool,
    ctx: ExecutionContext,
}

impl Iterator for InMemoryProbeStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ch) = self.pending_out.pop_front() {
                return Some(Ok(ch));
            }
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }
            match self.left_stream.next() {
                Some(Ok(probe_chunk)) => {
                    if probe_chunk.is_empty() {
                        continue;
                    }
                    self.ctx
                        .metrics
                        .record_input(self.spec.plan_id, probe_chunk.len() as u64);
                    if let Err(e) = probe_chunk_against_build(
                        &self.spec,
                        &mut self.build,
                        &probe_chunk,
                        &mut self.pending_out,
                        &self.ctx,
                    ) {
                        return Some(Err(e));
                    }
                }
                Some(Err(e)) => return Some(Err(e)),
                None => {
                    if !self.build_tail_emitted {
                        self.build_tail_emitted = true;
                        if let Err(e) = emit_build_tail(
                            &self.spec,
                            &self.build,
                            &mut self.pending_out,
                            &self.ctx,
                        ) {
                            return Some(Err(e));
                        }
                        if !self.pending_out.is_empty() {
                            continue;
                        }
                    }
                    return None;
                }
            }
        }
    }
}

struct GracePartitionPair {
    build: SpillFile,
    probe: SpillFile,
    depth: usize,
}

struct ActiveGracePartition {
    build: HashBuildState,
    probe_reader: Option<SpillChunkReader>,
    tail_emitted: bool,
}

struct GraceHashJoinStream {
    spec: JoinSpec,
    build_budget: usize,
    work_queue: VecDeque<GracePartitionPair>,
    active_probe: Option<ActiveGracePartition>,
    unmatched_null_probe: Option<SpillChunkReader>,
    unmatched_null_build: Option<SpillChunkReader>,
    pending_out: VecDeque<DataChunk>,
    ctx: ExecutionContext,
}

impl GraceHashJoinStream {
    fn open_next_partition(&mut self) -> Result<bool> {
        while let Some(pair) = self.work_queue.pop_front() {
            self.ctx.check_cancelled()?;

            // Shortcut: if build partition is empty!
            if pair.build.is_empty() {
                if !pair.probe.is_empty() && self.spec.emits_unmatched_left() {
                    let mut pr = SpillChunkReader::new(pair.probe)?;
                    while let Some(ch) = pr.next_chunk(&self.spec.left_types, &self.ctx)? {
                        emit_unmatched_probe_chunk(
                            &self.spec,
                            &ch,
                            &mut self.pending_out,
                            &self.ctx,
                        )?;
                    }
                    if !self.pending_out.is_empty() {
                        return Ok(true);
                    }
                }
                continue;
            }

            // Shortcut: if probe partition is empty!
            if pair.probe.is_empty() {
                if self.spec.tracks_build_matches()
                    && self.spec.join_type != PhysicalJoinType::RightSemi
                {
                    let mut br = SpillChunkReader::new(pair.build)?;
                    while let Some(ch) = br.next_chunk(&self.spec.right_types, &self.ctx)? {
                        emit_unmatched_build_chunk(
                            &self.spec,
                            &ch,
                            &mut self.pending_out,
                            &self.ctx,
                        )?;
                    }
                    if !self.pending_out.is_empty() {
                        return Ok(true);
                    }
                }
                continue;
            }

            // Load build partition chunks, checking whether skewed partition needs recursive re-partitioning!
            let mut br = SpillChunkReader::new(pair.build)?;
            let mut chunks = Vec::new();
            let mut est_bytes = 0usize;
            let mut overflowed = false;
            while let Some(ch) = br.next_chunk(&self.spec.right_types, &self.ctx)? {
                est_bytes += ch.memory_bytes() + ch.len() * 48;
                chunks.push(ch);
                if est_bytes >= self.build_budget && pair.depth < MAX_RECURSIVE_DEPTH {
                    overflowed = true;
                    break;
                }
            }

            if overflowed {
                // Recursive skew re-partitioning into 16 sub-partitions!
                let mut sub_b = ChunkPartitionWriters::new(
                    self.spec.plan_id,
                    16,
                    self.spec.right_types.clone(),
                    &self.ctx,
                )?;
                for ch in chunks.drain(..) {
                    partition_chunk_to_writers(
                        &ch,
                        &self.spec.right_keys,
                        &self.spec.null_safe_keys,
                        &mut sub_b,
                        None,
                        pair.depth + 1,
                        &self.ctx,
                    )?;
                }
                while let Some(ch) = br.next_chunk(&self.spec.right_types, &self.ctx)? {
                    partition_chunk_to_writers(
                        &ch,
                        &self.spec.right_keys,
                        &self.spec.null_safe_keys,
                        &mut sub_b,
                        None,
                        pair.depth + 1,
                        &self.ctx,
                    )?;
                }
                let sub_b_files = sub_b.finish_all_with_ctx(&self.ctx)?;

                let mut sub_p = ChunkPartitionWriters::new(
                    self.spec.plan_id,
                    16,
                    self.spec.left_types.clone(),
                    &self.ctx,
                )?;
                let mut pr = SpillChunkReader::new(pair.probe)?;
                while let Some(ch) = pr.next_chunk(&self.spec.left_types, &self.ctx)? {
                    partition_chunk_to_writers(
                        &ch,
                        &self.spec.left_keys,
                        &self.spec.null_safe_keys,
                        &mut sub_p,
                        None,
                        pair.depth + 1,
                        &self.ctx,
                    )?;
                }
                let sub_p_files = sub_p.finish_all_with_ctx(&self.ctx)?;

                for (bf, pf) in sub_b_files.into_iter().zip(sub_p_files).rev() {
                    if !bf.is_empty() || !pf.is_empty() {
                        self.work_queue.push_front(GracePartitionPair {
                            build: bf,
                            probe: pf,
                            depth: pair.depth + 1,
                        });
                    }
                }
                continue;
            }

            let probe_rhs = self.spec.probe_pair.as_ref().map(|(_, r)| r);
            let build_state = HashBuildState::build_from_chunks(
                chunks,
                &self.spec.right_types,
                &self.spec.right_keys,
                &self.spec.null_safe_keys,
                probe_rhs,
                self.spec.tracks_build_matches(),
                &self.ctx,
            )?;
            self.ctx
                .metrics
                .record_peak_memory(self.spec.plan_id, self.ctx.pool.peak_bytes());

            self.active_probe = Some(ActiveGracePartition {
                build: build_state,
                probe_reader: Some(SpillChunkReader::new(pair.probe)?),
                tail_emitted: false,
            });
            return Ok(true);
        }
        Ok(false)
    }
}

impl Iterator for GraceHashJoinStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ch) = self.pending_out.pop_front() {
                return Some(Ok(ch));
            }
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }

            // Step 1: Drain active Grace partition probe stream.
            if let Some(active) = &mut self.active_probe {
                if let Some(pr) = &mut active.probe_reader {
                    match pr.next_chunk(&self.spec.left_types, &self.ctx) {
                        Ok(Some(ch)) => {
                            if let Err(e) = probe_chunk_against_build(
                                &self.spec,
                                &mut active.build,
                                &ch,
                                &mut self.pending_out,
                                &self.ctx,
                            ) {
                                return Some(Err(e));
                            }
                            continue;
                        }
                        Ok(None) => {
                            active.probe_reader = None;
                        }
                        Err(e) => return Some(Err(e)),
                    }
                }
                if !active.tail_emitted {
                    active.tail_emitted = true;
                    if let Err(e) =
                        emit_build_tail(&self.spec, &active.build, &mut self.pending_out, &self.ctx)
                    {
                        return Some(Err(e));
                    }
                    // Drop the active partition's HashBuildState immediately so RAM is freed!
                    self.active_probe = None;
                    if !self.pending_out.is_empty() {
                        continue;
                    }
                } else {
                    self.active_probe = None;
                }
            }

            // Step 2: Open next Grace partition.
            match self.open_next_partition() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => return Some(Err(e)),
            }

            // Step 3: Emit non-null-safe NULL probe rows (unmatched Left/Full/Anti/Mark).
            if let Some(ur) = &mut self.unmatched_null_probe {
                match ur.next_chunk(&self.spec.left_types, &self.ctx) {
                    Ok(Some(ch)) => {
                        if let Err(e) = emit_unmatched_probe_chunk(
                            &self.spec,
                            &ch,
                            &mut self.pending_out,
                            &self.ctx,
                        ) {
                            return Some(Err(e));
                        }
                        continue;
                    }
                    Ok(None) => {
                        self.unmatched_null_probe = None;
                    }
                    Err(e) => return Some(Err(e)),
                }
            }

            // Step 4: Emit non-null-safe NULL build rows (unmatched Right/Full/RightAnti).
            if let Some(ub) = &mut self.unmatched_null_build {
                match ub.next_chunk(&self.spec.right_types, &self.ctx) {
                    Ok(Some(ch)) => {
                        if self.spec.join_type != PhysicalJoinType::RightSemi {
                            if let Err(e) = emit_unmatched_build_chunk(
                                &self.spec,
                                &ch,
                                &mut self.pending_out,
                                &self.ctx,
                            ) {
                                return Some(Err(e));
                            }
                            continue;
                        }
                    }
                    Ok(None) => {
                        self.unmatched_null_build = None;
                    }
                    Err(e) => return Some(Err(e)),
                }
            }

            return None;
        }
    }
}

fn emit_unmatched_probe_chunk(
    spec: &JoinSpec,
    chunk: &DataChunk,
    out_q: &mut VecDeque<DataChunk>,
    ctx: &ExecutionContext,
) -> Result<()> {
    let n = chunk.len();
    if n == 0 {
        return Ok(());
    }
    let l_cols = spec.left_types.len();
    let r_cols = spec.right_types.len();
    let mut rows = Vec::with_capacity(n);
    for r in 0..n {
        let mut row = chunk.get_row(r);
        match spec.join_type {
            PhysicalJoinType::LeftOuter
            | PhysicalJoinType::FullOuter
            | PhysicalJoinType::Single => {
                row.resize(l_cols + r_cols, ScalarValue::Null);
            }
            PhysicalJoinType::LeftMark => {
                row.push(ScalarValue::Boolean(false));
            }
            PhysicalJoinType::LeftAnti => {}
            _ => continue,
        }
        rows.push(row);
    }
    if !rows.is_empty() {
        out_q.push_back(chunk_from_scalar_rows(
            &spec.output_types,
            &rows,
            &ctx.pool,
        )?);
    }
    Ok(())
}

fn emit_unmatched_build_chunk(
    spec: &JoinSpec,
    chunk: &DataChunk,
    out_q: &mut VecDeque<DataChunk>,
    ctx: &ExecutionContext,
) -> Result<()> {
    let n = chunk.len();
    if n == 0 {
        return Ok(());
    }
    let l_cols = spec.left_types.len();
    let r_cols = spec.right_types.len();
    let include_null_left = matches!(
        spec.join_type,
        PhysicalJoinType::RightOuter | PhysicalJoinType::FullOuter
    );
    let mut rows = Vec::with_capacity(n);
    for r in 0..n {
        if include_null_left {
            let mut row = vec![ScalarValue::Null; l_cols + r_cols];
            let br = chunk.get_row(r);
            row[l_cols..l_cols + r_cols].clone_from_slice(&br);
            rows.push(row);
        } else {
            rows.push(chunk.get_row(r));
        }
    }
    if !rows.is_empty() {
        out_q.push_back(chunk_from_scalar_rows(
            &spec.output_types,
            &rows,
            &ctx.pool,
        )?);
    }
    Ok(())
}

// ============================================================================
// NestedLoopJoin + Out-of-Core Hash Fast Path for Uncorrelated `IN`/`NOT IN` LeftMark!
// ============================================================================

/// Executes `PhysicalOp::NestedLoopJoin`.
///
/// Automatically upgrades uncorrelated `LeftMark` with `probe_pair = Some((lhs, rhs))` and
/// `condition = None` (SQL `x IN (SELECT y ...)` / `x NOT IN (SELECT y ...)`) to an $O(N + M)$
/// **Spill-Capable Radix Hash Mark Join** with exact 3-valued NULL semantics!
#[allow(clippy::too_many_arguments)]
pub fn exec_nested_loop_join(
    plan_id: usize,
    left_stream: ChunkStream,
    right_stream: ChunkStream,
    join_type: PhysicalJoinType,
    left_types: Vec<LogicalType>,
    right_types: Vec<LogicalType>,
    output_types: Vec<LogicalType>,
    condition: Option<PhysicalExpr>,
    probe_pair: Option<(PhysicalExpr, PhysicalExpr)>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    // Fast O(N + M) spill-capable Hash Mark Join for uncorrelated `IN`/`NOT IN`!
    if join_type == PhysicalJoinType::LeftMark && condition.is_none() {
        if let Some((lhs_expr, rhs_expr)) = probe_pair {
            return exec_uncorrelated_hash_mark_join(
                plan_id,
                left_stream,
                right_stream,
                left_types,
                output_types,
                lhs_expr,
                rhs_expr,
                ctx,
            );
        }
    }

    // General NestedLoopJoin: materialize right side into `TupleBuffer`.
    let mut right_tb = TupleBuffer::new(right_types.clone(), &ctx.pool)?;
    let mut right_probe_rhs = match &probe_pair {
        Some((_, r)) => Some(TupleBuffer::new(vec![r.data_type()], &ctx.pool)?),
        None => None,
    };
    for chunk_res in right_stream {
        ctx.check_cancelled()?;
        let chunk = chunk_res?;
        if chunk.is_empty() {
            continue;
        }
        ctx.metrics.record_input(plan_id, chunk.len() as u64);
        right_tb.append_chunk(&chunk)?;
        if let (Some((_, r_expr)), Some(tb)) = (&probe_pair, &mut right_probe_rhs) {
            let v = eval_expr(r_expr, &chunk, &ctx.pool)?;
            let c = DataChunk::new(vec![v], chunk.len());
            tb.append_chunk(&c)?;
        }
    }

    let track_matched = matches!(
        join_type,
        PhysicalJoinType::RightOuter
            | PhysicalJoinType::FullOuter
            | PhysicalJoinType::RightSemi
            | PhysicalJoinType::RightAnti
    );
    let matched = if track_matched {
        vec![false; right_tb.row_count()]
    } else {
        Vec::new()
    };

    Ok(Box::new(NestedLoopStream {
        plan_id,
        join_type,
        left_types,
        right_types,
        output_types,
        condition,
        probe_pair,
        left_stream,
        right_tb,
        right_probe_rhs,
        matched,
        tail_emitted: false,
        pending_out: VecDeque::new(),
        ctx,
    }))
}

struct NestedLoopStream {
    plan_id: usize,
    join_type: PhysicalJoinType,
    left_types: Vec<LogicalType>,
    right_types: Vec<LogicalType>,
    output_types: Vec<LogicalType>,
    condition: Option<PhysicalExpr>,
    probe_pair: Option<(PhysicalExpr, PhysicalExpr)>,
    left_stream: ChunkStream,
    right_tb: TupleBuffer,
    right_probe_rhs: Option<TupleBuffer>,
    matched: Vec<bool>,
    tail_emitted: bool,
    pending_out: VecDeque<DataChunk>,
    ctx: ExecutionContext,
}

impl Iterator for NestedLoopStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        let l_cols = self.left_types.len();
        let r_cols = self.right_types.len();
        let r_count = self.right_tb.row_count();

        loop {
            if let Some(ch) = self.pending_out.pop_front() {
                return Some(Ok(ch));
            }
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }

            match self.left_stream.next() {
                Some(Ok(chunk)) => {
                    let n = chunk.len();
                    if n == 0 {
                        continue;
                    }
                    self.ctx.metrics.record_input(self.plan_id, n as u64);

                    let lhs_vec = match &self.probe_pair {
                        Some((lhs_e, _)) => match eval_expr(lhs_e, &chunk, &self.ctx.pool) {
                            Ok(v) => Some(v),
                            Err(e) => return Some(Err(e)),
                        },
                        None => None,
                    };

                    let mut out_rows: Vec<Vec<ScalarValue>> = Vec::new();
                    let mut combined = Vec::with_capacity(l_cols + r_cols);

                    for pr in 0..n {
                        let probe_row = chunk.get_row(pr);

                        if self.join_type == PhysicalJoinType::LeftMark {
                            let mark = if let Some(lv) = &lhs_vec {
                                let lhs_val = lv.get_scalar(pr);
                                let mut any_cand = false;
                                let mut found_true = false;
                                let mut saw_null = false;
                                for br in 0..r_count {
                                    let pass = if let Some(cond) = &self.condition {
                                        combined.clear();
                                        combined.extend_from_slice(&probe_row);
                                        for c in 0..r_cols {
                                            combined.push(self.right_tb.get_scalar(br, c));
                                        }
                                        match eval_expr_scalar_reference(cond, &combined) {
                                            Ok(ScalarValue::Boolean(true)) => true,
                                            Ok(_) => false,
                                            Err(e) => return Some(Err(e)),
                                        }
                                    } else {
                                        true
                                    };
                                    if pass {
                                        any_cand = true;
                                        if lhs_val.is_null() {
                                            saw_null = true;
                                            break;
                                        }
                                        let rv = self
                                            .right_probe_rhs
                                            .as_ref()
                                            .unwrap()
                                            .get_scalar(br, 0);
                                        if rv.is_null() {
                                            saw_null = true;
                                        } else if scalar_eq_not_distinct(&lhs_val, &rv) {
                                            found_true = true;
                                            break;
                                        }
                                    }
                                }
                                if found_true {
                                    ScalarValue::Boolean(true)
                                } else if !any_cand {
                                    ScalarValue::Boolean(false)
                                } else if saw_null {
                                    ScalarValue::Null
                                } else {
                                    ScalarValue::Boolean(false)
                                }
                            } else {
                                // EXISTS / ANY / ALL
                                let mut found_true = false;
                                let mut saw_null = false;
                                for br in 0..r_count {
                                    if let Some(cond) = &self.condition {
                                        combined.clear();
                                        combined.extend_from_slice(&probe_row);
                                        for c in 0..r_cols {
                                            combined.push(self.right_tb.get_scalar(br, c));
                                        }
                                        match eval_expr_scalar_reference(cond, &combined) {
                                            Ok(ScalarValue::Boolean(true)) => {
                                                found_true = true;
                                                break;
                                            }
                                            Ok(ScalarValue::Null) => saw_null = true,
                                            Ok(_) => {}
                                            Err(e) => return Some(Err(e)),
                                        }
                                    } else {
                                        found_true = true;
                                        break;
                                    }
                                }
                                if found_true {
                                    ScalarValue::Boolean(true)
                                } else if saw_null {
                                    ScalarValue::Null
                                } else {
                                    ScalarValue::Boolean(false)
                                }
                            };
                            let mut row = probe_row;
                            row.push(mark);
                            out_rows.push(row);
                            if out_rows.len() >= VECTOR_CAPACITY {
                                match chunk_from_scalar_rows(
                                    &self.output_types,
                                    &out_rows,
                                    &self.ctx.pool,
                                ) {
                                    Ok(c) => {
                                        self.pending_out.push_back(c);
                                        out_rows.clear();
                                    }
                                    Err(e) => return Some(Err(e)),
                                }
                            }
                            continue;
                        }

                        let mut matched_cnt = 0usize;
                        for br in 0..r_count {
                            let pass = if let Some(cond) = &self.condition {
                                combined.clear();
                                combined.extend_from_slice(&probe_row);
                                for c in 0..r_cols {
                                    combined.push(self.right_tb.get_scalar(br, c));
                                }
                                match eval_expr_scalar_reference(cond, &combined) {
                                    Ok(ScalarValue::Boolean(true)) => true,
                                    Ok(_) => false,
                                    Err(e) => return Some(Err(e)),
                                }
                            } else {
                                true
                            };
                            if pass {
                                matched_cnt += 1;
                                if !self.matched.is_empty() {
                                    self.matched[br] = true;
                                }
                                match self.join_type {
                                    PhysicalJoinType::Inner
                                    | PhysicalJoinType::LeftOuter
                                    | PhysicalJoinType::RightOuter
                                    | PhysicalJoinType::FullOuter
                                    | PhysicalJoinType::Cross => {
                                        let mut row = Vec::with_capacity(l_cols + r_cols);
                                        row.extend_from_slice(&probe_row);
                                        for c in 0..r_cols {
                                            row.push(self.right_tb.get_scalar(br, c));
                                        }
                                        out_rows.push(row);
                                    }
                                    PhysicalJoinType::Single => {
                                        if matched_cnt > 1 {
                                            return Some(Err(VectraError::ExecutionError(
                                                "scalar subquery returned more than one row".into(),
                                            )));
                                        }
                                        let mut row = Vec::with_capacity(l_cols + r_cols);
                                        row.extend_from_slice(&probe_row);
                                        for c in 0..r_cols {
                                            row.push(self.right_tb.get_scalar(br, c));
                                        }
                                        out_rows.push(row);
                                    }
                                    PhysicalJoinType::LeftSemi => {
                                        out_rows.push(probe_row.clone());
                                        break;
                                    }
                                    PhysicalJoinType::LeftAnti => break,
                                    PhysicalJoinType::RightSemi | PhysicalJoinType::RightAnti => {}
                                    PhysicalJoinType::LeftMark => unreachable!(),
                                }
                                if out_rows.len() >= VECTOR_CAPACITY {
                                    match chunk_from_scalar_rows(
                                        &self.output_types,
                                        &out_rows,
                                        &self.ctx.pool,
                                    ) {
                                        Ok(c) => {
                                            self.pending_out.push_back(c);
                                            out_rows.clear();
                                        }
                                        Err(e) => return Some(Err(e)),
                                    }
                                }
                            }
                        }

                        if matched_cnt == 0 {
                            match self.join_type {
                                PhysicalJoinType::LeftOuter
                                | PhysicalJoinType::FullOuter
                                | PhysicalJoinType::Single => {
                                    let mut row = Vec::with_capacity(l_cols + r_cols);
                                    row.extend(probe_row);
                                    row.resize(l_cols + r_cols, ScalarValue::Null);
                                    out_rows.push(row);
                                }
                                PhysicalJoinType::LeftAnti => out_rows.push(probe_row),
                                _ => {}
                            }
                            if out_rows.len() >= VECTOR_CAPACITY {
                                match chunk_from_scalar_rows(
                                    &self.output_types,
                                    &out_rows,
                                    &self.ctx.pool,
                                ) {
                                    Ok(c) => {
                                        self.pending_out.push_back(c);
                                        out_rows.clear();
                                    }
                                    Err(e) => return Some(Err(e)),
                                }
                            }
                        }
                    }

                    if !out_rows.is_empty() {
                        match chunk_from_scalar_rows(&self.output_types, &out_rows, &self.ctx.pool)
                        {
                            Ok(c) => self.pending_out.push_back(c),
                            Err(e) => return Some(Err(e)),
                        }
                    }
                }
                Some(Err(e)) => return Some(Err(e)),
                None => {
                    if !self.tail_emitted && !self.matched.is_empty() {
                        self.tail_emitted = true;
                        let want_matched = self.join_type == PhysicalJoinType::RightSemi;
                        let include_null_left = matches!(
                            self.join_type,
                            PhysicalJoinType::RightOuter | PhysicalJoinType::FullOuter
                        );
                        let mut rows = Vec::new();
                        for (br, &m) in self.matched.iter().enumerate() {
                            if m != want_matched {
                                continue;
                            }
                            if include_null_left {
                                let mut row = vec![ScalarValue::Null; l_cols + r_cols];
                                for c in 0..r_cols {
                                    row[l_cols + c] = self.right_tb.get_scalar(br, c);
                                }
                                rows.push(row);
                            } else {
                                rows.push(self.right_tb.get_row(br));
                            }
                            if rows.len() >= VECTOR_CAPACITY {
                                match chunk_from_scalar_rows(
                                    &self.output_types,
                                    &rows,
                                    &self.ctx.pool,
                                ) {
                                    Ok(c) => {
                                        self.pending_out.push_back(c);
                                        rows.clear();
                                    }
                                    Err(e) => return Some(Err(e)),
                                }
                            }
                        }
                        if !rows.is_empty() {
                            match chunk_from_scalar_rows(&self.output_types, &rows, &self.ctx.pool)
                            {
                                Ok(c) => self.pending_out.push_back(c),
                                Err(e) => return Some(Err(e)),
                            }
                        }
                        if !self.pending_out.is_empty() {
                            continue;
                        }
                    }
                    return None;
                }
            }
        }
    }
}

/// Out-of-Core Spill-Capable $O(N + M)$ Radix Hash Mark Join for uncorrelated `IN`/`NOT IN`
/// preserving exact 3-valued SQL NULL semantics under tight `BoundedMemoryPool` limits!
#[allow(clippy::too_many_arguments)]
fn exec_uncorrelated_hash_mark_join(
    plan_id: usize,
    left_stream: ChunkStream,
    right_stream: ChunkStream,
    left_types: Vec<LogicalType>,
    output_types: Vec<LogicalType>,
    lhs_expr: PhysicalExpr,
    rhs_expr: PhysicalExpr,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    let rhs_type = rhs_expr.data_type();
    let budget = ctx.operator_budget(30, 100);

    let mut right_is_empty = true;
    let mut right_has_null = false;
    let mut set: HashSet<CanonicalScalar> = HashSet::new();
    let mut mem_res = ctx.pool.reserve(0)?;
    let mut spill_rhs_parts: Option<ChunkPartitionWriters> = None;

    for chunk_res in right_stream {
        ctx.check_cancelled()?;
        let chunk = chunk_res?;
        let n = chunk.len();
        if n == 0 {
            continue;
        }
        ctx.metrics.record_input(plan_id, n as u64);
        right_is_empty = false;

        let rv = eval_expr(&rhs_expr, &chunk, &ctx.pool)?;

        if let Some(parts) = &mut spill_rhs_parts {
            for r in 0..n {
                let val = rv.get_scalar(r);
                if val.is_null() {
                    right_has_null = true;
                } else {
                    let p = hash_partition_idx(val.canonical_hash(), 0, NUM_PARTITIONS);
                    parts.parts[p].push_row(vec![val], &ctx)?;
                }
            }
            continue;
        }

        for r in 0..n {
            let val = rv.get_scalar(r);
            if val.is_null() {
                right_has_null = true;
                continue;
            }
            let item_bytes = crate::stream::scalar_heap_bytes(&val) + 24;
            if set.insert(CanonicalScalar(val))
                && (mem_res.bytes() + item_bytes >= budget || mem_res.try_grow(item_bytes).is_err())
            {
                let mut parts =
                    ChunkPartitionWriters::new(plan_id, NUM_PARTITIONS, vec![rhs_type], &ctx)?;
                for existing in set.drain() {
                    let p = hash_partition_idx(existing.0.canonical_hash(), 0, NUM_PARTITIONS);
                    parts.parts[p].push_row(vec![existing.0], &ctx)?;
                }
                set.shrink_to_fit();
                mem_res.shrink(mem_res.bytes());
                // Push remaining rows from this chunk into spill parts too!
                for rem in (r + 1)..n {
                    let v2 = rv.get_scalar(rem);
                    if v2.is_null() {
                        right_has_null = true;
                    } else {
                        let p = hash_partition_idx(v2.canonical_hash(), 0, NUM_PARTITIONS);
                        parts.parts[p].push_row(vec![v2], &ctx)?;
                    }
                }
                spill_rhs_parts = Some(parts);
                break;
            }
        }
        ctx.metrics
            .record_peak_memory(plan_id, ctx.pool.peak_bytes());
    }

    // Fast Path: right distinct set fit completely in memory!
    if spill_rhs_parts.is_none() {
        return Ok(Box::new(InMemoryMarkProbeStream {
            plan_id,
            left_stream,
            output_types,
            lhs_expr,
            rhs_set: set,
            _mem_res: mem_res,
            right_is_empty,
            right_has_null,
            ctx,
        }));
    }

    // Spilled Path: partition left stream by `hash(lhs_expr)` into the SAME 64 partitions!
    let rhs_files = spill_rhs_parts.unwrap().finish_all_with_ctx(&ctx)?;
    drop(set);
    drop(mem_res);

    let mut lhs_parts =
        ChunkPartitionWriters::new(plan_id, NUM_PARTITIONS, left_types.clone(), &ctx)?;
    let mut null_lhs_writer = ChunkSpillBuffer::new(plan_id, left_types.clone(), &ctx)?;

    for chunk_res in left_stream {
        ctx.check_cancelled()?;
        let chunk = chunk_res?;
        let n = chunk.len();
        if n == 0 {
            continue;
        }
        ctx.metrics.record_input(plan_id, n as u64);
        let lv = eval_expr(&lhs_expr, &chunk, &ctx.pool)?;
        for r in 0..n {
            let val = lv.get_scalar(r);
            if val.is_null() {
                null_lhs_writer.push_row(chunk.get_row(r), &ctx)?;
            } else {
                let p = hash_partition_idx(val.canonical_hash(), 0, NUM_PARTITIONS);
                lhs_parts.parts[p].push_row(chunk.get_row(r), &ctx)?;
            }
        }
    }
    null_lhs_writer.flush(&ctx)?;
    let null_lhs_file = null_lhs_writer.finish()?;
    let lhs_files = lhs_parts.finish_all_with_ctx(&ctx)?;

    let queue: VecDeque<(SpillFile, SpillFile)> = rhs_files
        .into_iter()
        .zip(lhs_files)
        .filter(|(_, lf)| !lf.is_empty())
        .collect();

    Ok(Box::new(SpilledMarkJoinStream {
        plan_id,
        left_types,
        rhs_type,
        output_types,
        lhs_expr,
        right_is_empty,
        right_has_null,
        queue,
        null_lhs_reader: if null_lhs_file.is_empty() {
            None
        } else {
            Some(SpillChunkReader::new(null_lhs_file)?)
        },
        active_set: HashSet::new(),
        active_lhs_reader: None,
        ctx,
    }))
}

struct InMemoryMarkProbeStream {
    plan_id: usize,
    left_stream: ChunkStream,
    output_types: Vec<LogicalType>,
    lhs_expr: PhysicalExpr,
    rhs_set: HashSet<CanonicalScalar>,
    _mem_res: MemoryReservation,
    right_is_empty: bool,
    right_has_null: bool,
    ctx: ExecutionContext,
}

impl Iterator for InMemoryMarkProbeStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }
            let chunk = match self.left_stream.next()? {
                Ok(c) => c,
                Err(e) => return Some(Err(e)),
            };
            let n = chunk.len();
            if n == 0 {
                continue;
            }
            self.ctx.metrics.record_input(self.plan_id, n as u64);
            let lv = match eval_expr(&self.lhs_expr, &chunk, &self.ctx.pool) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let mut rows = Vec::with_capacity(n);
            for r in 0..n {
                let mut row = chunk.get_row(r);
                let mark = if self.right_is_empty {
                    ScalarValue::Boolean(false)
                } else {
                    let val = lv.get_scalar(r);
                    if val.is_null() {
                        ScalarValue::Null
                    } else if self.rhs_set.contains(&CanonicalScalar(val)) {
                        ScalarValue::Boolean(true)
                    } else if self.right_has_null {
                        ScalarValue::Null
                    } else {
                        ScalarValue::Boolean(false)
                    }
                };
                row.push(mark);
                rows.push(row);
            }
            return Some(chunk_from_scalar_rows(
                &self.output_types,
                &rows,
                &self.ctx.pool,
            ));
        }
    }
}

struct SpilledMarkJoinStream {
    plan_id: usize,
    left_types: Vec<LogicalType>,
    rhs_type: LogicalType,
    output_types: Vec<LogicalType>,
    lhs_expr: PhysicalExpr,
    right_is_empty: bool,
    right_has_null: bool,
    queue: VecDeque<(SpillFile, SpillFile)>,
    null_lhs_reader: Option<SpillChunkReader>,
    active_set: HashSet<CanonicalScalar>,
    active_lhs_reader: Option<SpillChunkReader>,
    ctx: ExecutionContext,
}

impl Iterator for SpilledMarkJoinStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }
            if let Some(lr) = &mut self.active_lhs_reader {
                match lr.next_chunk(&self.left_types, &self.ctx) {
                    Ok(Some(chunk)) => {
                        let n = chunk.len();
                        let lv = match eval_expr(&self.lhs_expr, &chunk, &self.ctx.pool) {
                            Ok(v) => v,
                            Err(e) => return Some(Err(e)),
                        };
                        let mut rows = Vec::with_capacity(n);
                        for r in 0..n {
                            let mut row = chunk.get_row(r);
                            let val = lv.get_scalar(r);
                            let mark = if self.right_is_empty {
                                ScalarValue::Boolean(false)
                            } else if self.active_set.contains(&CanonicalScalar(val)) {
                                ScalarValue::Boolean(true)
                            } else if self.right_has_null {
                                ScalarValue::Null
                            } else {
                                ScalarValue::Boolean(false)
                            };
                            row.push(mark);
                            rows.push(row);
                        }
                        return Some(chunk_from_scalar_rows(
                            &self.output_types,
                            &rows,
                            &self.ctx.pool,
                        ));
                    }
                    Ok(None) => {
                        self.active_lhs_reader = None;
                        self.active_set.clear();
                        self.active_set.shrink_to_fit();
                    }
                    Err(e) => return Some(Err(e)),
                }
            }

            if let Some((rf, lf)) = self.queue.pop_front() {
                self.active_set.clear();
                if !rf.is_empty() {
                    let mut rr = match SpillChunkReader::new(rf) {
                        Ok(r) => r,
                        Err(e) => return Some(Err(e)),
                    };
                    loop {
                        match rr.next_chunk(&[self.rhs_type], &self.ctx) {
                            Ok(Some(ch)) => {
                                for r in 0..ch.len() {
                                    self.active_set
                                        .insert(CanonicalScalar(ch.columns[0].get_scalar(r)));
                                }
                            }
                            Ok(None) => break,
                            Err(e) => return Some(Err(e)),
                        }
                    }
                }
                self.ctx
                    .metrics
                    .record_peak_memory(self.plan_id, self.ctx.pool.peak_bytes());
                self.active_lhs_reader = match SpillChunkReader::new(lf) {
                    Ok(r) => Some(r),
                    Err(e) => return Some(Err(e)),
                };
                continue;
            }

            if let Some(nr) = &mut self.null_lhs_reader {
                match nr.next_chunk(&self.left_types, &self.ctx) {
                    Ok(Some(chunk)) => {
                        let mark = if self.right_is_empty {
                            ScalarValue::Boolean(false)
                        } else {
                            ScalarValue::Null
                        };
                        let mut rows = Vec::with_capacity(chunk.len());
                        for r in 0..chunk.len() {
                            let mut row = chunk.get_row(r);
                            row.push(mark.clone());
                            rows.push(row);
                        }
                        return Some(chunk_from_scalar_rows(
                            &self.output_types,
                            &rows,
                            &self.ctx.pool,
                        ));
                    }
                    Ok(None) => {
                        self.null_lhs_reader = None;
                    }
                    Err(e) => return Some(Err(e)),
                }
            }

            return None;
        }
    }
}
