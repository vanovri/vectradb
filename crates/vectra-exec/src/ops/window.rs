use crate::context::ExecutionContext;
use crate::ops::hash_agg::AggState;
use crate::ops::sort::exec_external_sort;
use crate::stream::{chunk_from_scalar_rows, scalar_eq_not_distinct, ChunkStream};
use std::collections::VecDeque;
use vectra_expr::{eval_expr, PhysicalExpr};
use vectra_optimizer::{PhysicalSortExpr, PhysicalWindowExpr};
use vectra_sql::{AggregateFunc, WindowFrame, WindowFrameBound, WindowFrameUnits, WindowFunc};
use vectra_types::{
    DataChunk, LogicalType, Result, ScalarValue, SortOrder, VectraError, VECTOR_CAPACITY,
};

type WindowSpecGroup = (
    Vec<PhysicalExpr>,
    Vec<PhysicalSortExpr>,
    Vec<(usize, PhysicalWindowExpr)>,
);

/// Executes `PhysicalOp::Window` with out-of-core partition streaming (via `ExternalSort`)
/// and an $O(\log P)$ iterative Segment Tree for sliding `ROWS` and `RANGE` aggregate frames!
pub fn exec_window(
    plan_id: usize,
    input: ChunkStream,
    windows: Vec<PhysicalWindowExpr>,
    input_types: Vec<LogicalType>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    if windows.is_empty() {
        return Ok(input);
    }

    // Group windows that share identical `(partition_by, order_by)` specifications so they
    // execute in a single out-of-core sort + single streaming partition pass!
    let mut groups: Vec<WindowSpecGroup> = Vec::new();
    for (orig_idx, win) in windows.into_iter().enumerate() {
        if let Some(g) = groups
            .iter_mut()
            .find(|(p, o, _)| p == &win.partition_by && o == &win.order_by)
        {
            g.2.push((orig_idx, win));
        } else {
            groups.push((
                win.partition_by.clone(),
                win.order_by.clone(),
                vec![(orig_idx, win)],
            ));
        }
    }

    let orig_input_cols = input_types.len();
    let total_wins: usize = groups.iter().map(|g| g.2.len()).sum();

    // Fast single-group path (preserves exact window column order directly!).
    if groups.len() == 1 {
        let (part_by, order_by, indexed_wins) = groups.into_iter().next().unwrap();
        let group_wins: Vec<PhysicalWindowExpr> =
            indexed_wins.into_iter().map(|(_, w)| w).collect();
        return exec_window_group(
            plan_id,
            input,
            part_by,
            order_by,
            group_wins,
            input_types,
            ctx,
        );
    }

    // Multi-spec path: pipeline each group through out-of-core partition sort, recording where
    // each `orig_idx` landed, then project columns back into exact `[input..., win_0..win_{k-1}]` order.
    let mut cur_stream = input;
    let mut cur_types = input_types;
    let mut placed_order: Vec<usize> = Vec::with_capacity(total_wins);

    for (part_by, order_by, indexed_wins) in groups {
        let mut group_wins = Vec::with_capacity(indexed_wins.len());
        let mut next_types = cur_types.clone();
        for (orig_idx, w) in indexed_wins {
            placed_order.push(orig_idx);
            next_types.push(w.return_type);
            group_wins.push(w);
        }
        cur_stream = exec_window_group(
            plan_id,
            cur_stream,
            part_by,
            order_by,
            group_wins,
            cur_types,
            ctx.clone(),
        )?;
        cur_types = next_types;
    }

    // Check if column order already matches `0..total_wins`.
    let already_ordered = placed_order
        .iter()
        .enumerate()
        .all(|(pos, &orig)| pos == orig);
    if already_ordered {
        return Ok(cur_stream);
    }

    let mut perm = Vec::with_capacity(orig_input_cols + total_wins);
    for c in 0..orig_input_cols {
        perm.push(c);
    }
    for want in 0..total_wins {
        let pos = placed_order.iter().position(|&orig| orig == want).unwrap();
        perm.push(orig_input_cols + pos);
    }

    Ok(Box::new(cur_stream.map(move |res| {
        res.map(|chunk| {
            let cols = perm.iter().map(|&i| chunk.columns[i].clone()).collect();
            DataChunk {
                columns: cols,
                sel: chunk.sel,
                count: chunk.count,
            }
        })
    })))
}

fn exec_window_group(
    plan_id: usize,
    input: ChunkStream,
    partition_by: Vec<PhysicalExpr>,
    order_by: Vec<PhysicalSortExpr>,
    windows: Vec<PhysicalWindowExpr>,
    input_types: Vec<LogicalType>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    // Build combined sort key: `[partition_by ASC NULLS FIRST..., order_by...]`.
    let mut sort_exprs = Vec::with_capacity(partition_by.len() + order_by.len());
    for p in &partition_by {
        sort_exprs.push(PhysicalSortExpr {
            expr: p.clone(),
            order: SortOrder::ASC_NULLS_FIRST,
        });
    }
    sort_exprs.extend(order_by.iter().cloned());

    let sorted_stream = if sort_exprs.is_empty() {
        input
    } else {
        exec_external_sort(plan_id, input, sort_exprs, input_types.clone(), ctx.clone())?
    };

    let mut output_types = input_types.clone();
    for w in &windows {
        output_types.push(w.return_type);
    }

    Ok(Box::new(PartitionWindowStream {
        plan_id,
        sorted_stream,
        partition_by,
        order_by,
        windows,
        input_types,
        output_types,
        cur_part_key: None,
        cur_part_rows: Vec::new(),
        pending_out: VecDeque::new(),
        upstream_done: false,
        ctx,
    }))
}

struct PartitionWindowStream {
    plan_id: usize,
    sorted_stream: ChunkStream,
    partition_by: Vec<PhysicalExpr>,
    order_by: Vec<PhysicalSortExpr>,
    windows: Vec<PhysicalWindowExpr>,
    input_types: Vec<LogicalType>,
    output_types: Vec<LogicalType>,
    cur_part_key: Option<Vec<ScalarValue>>,
    cur_part_rows: Vec<Vec<ScalarValue>>,
    pending_out: VecDeque<DataChunk>,
    upstream_done: bool,
    ctx: ExecutionContext,
}

impl PartitionWindowStream {
    fn flush_current_partition(&mut self) -> Result<()> {
        if self.cur_part_rows.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.cur_part_rows);
        let p_len = rows.len();

        // Build temporary partition chunks to evaluate window argument & order_by expressions cleanly.
        let mut ord_vals: Vec<Vec<ScalarValue>> =
            vec![Vec::with_capacity(p_len); self.order_by.len()];
        let mut arg_vals: Vec<Vec<Vec<ScalarValue>>> = self
            .windows
            .iter()
            .map(|w| vec![Vec::with_capacity(p_len); w.args.len()])
            .collect();

        let mut off = 0;
        while off < p_len {
            let take = (p_len - off).min(VECTOR_CAPACITY);
            let sub_chunk =
                chunk_from_scalar_rows(&self.input_types, &rows[off..off + take], &self.ctx.pool)?;
            for (oi, oexpr) in self.order_by.iter().enumerate() {
                let v = eval_expr(&oexpr.expr, &sub_chunk, &self.ctx.pool)?;
                for r in 0..take {
                    ord_vals[oi].push(v.get_scalar(r));
                }
            }
            for (wi, w) in self.windows.iter().enumerate() {
                for (ai, aexpr) in w.args.iter().enumerate() {
                    let v = eval_expr(aexpr, &sub_chunk, &self.ctx.pool)?;
                    for r in 0..take {
                        arg_vals[wi][ai].push(v.get_scalar(r));
                    }
                }
            }
            off += take;
        }

        // Precompute peer group boundaries for `RANK`, `DENSE_RANK`, and `RANGE` frames.
        let (peer_start, peer_end) = compute_peer_bounds(p_len, &ord_vals);

        // Compute every window function column over this partition!
        let mut win_results: Vec<Vec<ScalarValue>> = Vec::with_capacity(self.windows.len());
        for (wi, w) in self.windows.iter().enumerate() {
            self.ctx.check_cancelled()?;
            let col = eval_window_on_partition(
                w,
                p_len,
                &arg_vals[wi],
                &ord_vals,
                &self.order_by,
                &peer_start,
                &peer_end,
            )?;
            win_results.push(col);
        }

        // Assemble output rows in 2,048-row chunks and queue for streaming emission.
        let mut batch: Vec<Vec<ScalarValue>> = Vec::with_capacity(VECTOR_CAPACITY);
        for (i, mut row) in rows.into_iter().enumerate() {
            row.reserve(win_results.len());
            for wcol in &win_results {
                row.push(wcol[i].clone());
            }
            batch.push(row);
            if batch.len() >= VECTOR_CAPACITY {
                self.pending_out.push_back(chunk_from_scalar_rows(
                    &self.output_types,
                    &batch,
                    &self.ctx.pool,
                )?);
                batch.clear();
            }
        }
        if !batch.is_empty() {
            self.pending_out.push_back(chunk_from_scalar_rows(
                &self.output_types,
                &batch,
                &self.ctx.pool,
            )?);
        }
        Ok(())
    }
}

impl Iterator for PartitionWindowStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ch) = self.pending_out.pop_front() {
                return Some(Ok(ch));
            }
            if self.upstream_done {
                return None;
            }
            if let Err(e) = self.ctx.check_cancelled() {
                return Some(Err(e));
            }

            match self.sorted_stream.next() {
                Some(Ok(chunk)) => {
                    let n = chunk.len();
                    if n == 0 {
                        continue;
                    }
                    self.ctx.metrics.record_input(self.plan_id, n as u64);

                    if self.partition_by.is_empty() {
                        for r in 0..n {
                            self.cur_part_rows.push(chunk.get_row(r));
                        }
                        continue;
                    }

                    let mut pvecs = Vec::with_capacity(self.partition_by.len());
                    for pe in &self.partition_by {
                        match eval_expr(pe, &chunk, &self.ctx.pool) {
                            Ok(v) => pvecs.push(v),
                            Err(e) => return Some(Err(e)),
                        }
                    }

                    let mut row_pkey = Vec::with_capacity(pvecs.len());
                    for r in 0..n {
                        row_pkey.clear();
                        for pv in &pvecs {
                            row_pkey.push(pv.get_scalar(r));
                        }
                        let same_part = match &self.cur_part_key {
                            None => true,
                            Some(cur) => cur
                                .iter()
                                .zip(row_pkey.iter())
                                .all(|(a, b)| scalar_eq_not_distinct(a, b)),
                        };
                        if !same_part {
                            if let Err(e) = self.flush_current_partition() {
                                return Some(Err(e));
                            }
                            self.cur_part_key = Some(row_pkey.clone());
                        } else if self.cur_part_key.is_none() {
                            self.cur_part_key = Some(row_pkey.clone());
                        }
                        self.cur_part_rows.push(chunk.get_row(r));
                    }
                }
                Some(Err(e)) => return Some(Err(e)),
                None => {
                    self.upstream_done = true;
                    if let Err(e) = self.flush_current_partition() {
                        return Some(Err(e));
                    }
                }
            }
        }
    }
}

fn compute_peer_bounds(p_len: usize, ord_vals: &[Vec<ScalarValue>]) -> (Vec<usize>, Vec<usize>) {
    let mut peer_start = vec![0usize; p_len];
    let mut peer_end = vec![0usize; p_len];
    if p_len == 0 {
        return (peer_start, peer_end);
    }
    if ord_vals.is_empty() {
        peer_start.fill(0);
        peer_end.fill(p_len - 1);
        return (peer_start, peer_end);
    }

    let mut i = 0;
    while i < p_len {
        let mut j = i + 1;
        while j < p_len {
            let same = ord_vals
                .iter()
                .all(|col| scalar_eq_not_distinct(&col[i], &col[j]));
            if !same {
                break;
            }
            j += 1;
        }
        for k in i..j {
            peer_start[k] = i;
            peer_end[k] = j - 1;
        }
        i = j;
    }
    (peer_start, peer_end)
}

fn eval_window_on_partition(
    win: &PhysicalWindowExpr,
    p_len: usize,
    args: &[Vec<ScalarValue>],
    ord_vals: &[Vec<ScalarValue>],
    order_by: &[PhysicalSortExpr],
    peer_start: &[usize],
    peer_end: &[usize],
) -> Result<Vec<ScalarValue>> {
    match win.func {
        WindowFunc::RowNumber => Ok((1..=p_len as i64).map(ScalarValue::Int64).collect()),
        WindowFunc::Rank => Ok((0..p_len)
            .map(|i| ScalarValue::Int64((peer_start[i] + 1) as i64))
            .collect()),
        WindowFunc::DenseRank => {
            let mut out = Vec::with_capacity(p_len);
            let mut rank = 0i64;
            for i in 0..p_len {
                if i == 0 || peer_start[i] != peer_start[i - 1] {
                    rank += 1;
                }
                out.push(ScalarValue::Int64(rank));
            }
            Ok(out)
        }
        WindowFunc::Ntile => {
            let buckets = match args.first().and_then(|c| c.first()) {
                Some(ScalarValue::Int64(n)) => *n,
                Some(ScalarValue::Int32(n)) => i64::from(*n),
                Some(ScalarValue::Int16(n)) => i64::from(*n),
                Some(ScalarValue::Int8(n)) => i64::from(*n),
                _ => {
                    return Err(VectraError::ExecutionError(
                        "NTILE requires a positive integer argument".into(),
                    ));
                }
            };
            if buckets <= 0 {
                return Err(VectraError::ExecutionError(
                    "NTILE argument must be positive".into(),
                ));
            }
            let n = buckets as usize;
            let base = p_len / n;
            let rem = p_len % n;
            let mut out = Vec::with_capacity(p_len);
            for i in 0..p_len {
                let b = if i < rem * (base + 1) {
                    i / (base + 1) + 1
                } else {
                    match (i - rem * (base + 1)).checked_div(base) {
                        Some(q) => rem + q + 1,
                        None => i + 1,
                    }
                };
                out.push(ScalarValue::Int64(b as i64));
            }
            Ok(out)
        }
        WindowFunc::Lead | WindowFunc::Lag => {
            let is_lead = win.func == WindowFunc::Lead;
            let val_col = args.first().ok_or_else(|| {
                VectraError::ExecutionError("LEAD/LAG requires value argument".into())
            })?;
            let mut out = Vec::with_capacity(p_len);
            for i in 0..p_len {
                let offset = match args.get(1).map(|c| &c[i]) {
                    None => 1i64,
                    Some(ScalarValue::Int64(k)) => *k,
                    Some(ScalarValue::Int32(k)) => i64::from(*k),
                    Some(ScalarValue::Int16(k)) => i64::from(*k),
                    Some(ScalarValue::Int8(k)) => i64::from(*k),
                    Some(ScalarValue::Null) => {
                        out.push(ScalarValue::Null);
                        continue;
                    }
                    _ => 1i64,
                };
                let target = if is_lead {
                    (i as i64).checked_add(offset)
                } else {
                    (i as i64).checked_sub(offset)
                };
                if let Some(t) = target {
                    if t >= 0 && (t as usize) < p_len {
                        out.push(val_col[t as usize].clone());
                        continue;
                    }
                }
                let default_v = args
                    .get(2)
                    .map(|c| c[i].clone())
                    .unwrap_or(ScalarValue::Null);
                out.push(default_v);
            }
            Ok(out)
        }
        WindowFunc::FirstValue | WindowFunc::LastValue => {
            let is_first = win.func == WindowFunc::FirstValue;
            let val_col = args.first().ok_or_else(|| {
                VectraError::ExecutionError("FIRST_VALUE/LAST_VALUE requires argument".into())
            })?;
            let frame = effective_frame(&win.frame, !order_by.is_empty());
            let mut out = Vec::with_capacity(p_len);
            for i in 0..p_len {
                match resolve_frame_bounds(
                    i, p_len, &frame, ord_vals, order_by, peer_start, peer_end,
                ) {
                    Some((lo, hi)) => {
                        let idx = if is_first { lo } else { hi };
                        out.push(val_col[idx].clone());
                    }
                    None => out.push(ScalarValue::Null),
                }
            }
            Ok(out)
        }
        WindowFunc::Aggregate(agg_func) => {
            let frame = effective_frame(&win.frame, !order_by.is_empty());
            eval_window_agg_segment_tree(
                agg_func,
                win.return_type,
                p_len,
                args,
                &frame,
                ord_vals,
                order_by,
                peer_start,
                peer_end,
            )
        }
    }
}

fn effective_frame(frame: &Option<WindowFrame>, has_order_by: bool) -> WindowFrame {
    match frame {
        Some(f) => f.clone(),
        None => {
            if has_order_by {
                WindowFrame {
                    units: WindowFrameUnits::Range,
                    start: WindowFrameBound::UnboundedPreceding,
                    end: WindowFrameBound::CurrentRow,
                }
            } else {
                WindowFrame {
                    units: WindowFrameUnits::Rows,
                    start: WindowFrameBound::UnboundedPreceding,
                    end: WindowFrameBound::UnboundedFollowing,
                }
            }
        }
    }
}

/// Resolves exact inclusive partition index range `[lo, hi]` for row `i`, or `None` if empty frame.
fn resolve_frame_bounds(
    i: usize,
    p_len: usize,
    frame: &WindowFrame,
    ord_vals: &[Vec<ScalarValue>],
    order_by: &[PhysicalSortExpr],
    peer_start: &[usize],
    peer_end: &[usize],
) -> Option<(usize, usize)> {
    if p_len == 0 {
        return None;
    }
    match frame.units {
        WindowFrameUnits::Rows => {
            let lo = match frame.start {
                WindowFrameBound::UnboundedPreceding => 0isize,
                WindowFrameBound::Preceding(k) => (i as isize).saturating_sub(k as isize),
                WindowFrameBound::CurrentRow => i as isize,
                WindowFrameBound::Following(k) => (i as isize).saturating_add(k as isize),
                WindowFrameBound::UnboundedFollowing => (p_len - 1) as isize,
            };
            let hi = match frame.end {
                WindowFrameBound::UnboundedPreceding => 0isize,
                WindowFrameBound::Preceding(k) => (i as isize).saturating_sub(k as isize),
                WindowFrameBound::CurrentRow => i as isize,
                WindowFrameBound::Following(k) => (i as isize).saturating_add(k as isize),
                WindowFrameBound::UnboundedFollowing => (p_len - 1) as isize,
            };
            if hi < 0 || lo >= p_len as isize || lo > hi {
                return None;
            }
            let cl_lo = lo.max(0) as usize;
            let cl_hi = hi.min((p_len - 1) as isize) as usize;
            if cl_lo <= cl_hi {
                Some((cl_lo, cl_hi))
            } else {
                None
            }
        }
        WindowFrameUnits::Range => {
            let lo = resolve_range_bound(
                i,
                p_len,
                &frame.start,
                true,
                ord_vals,
                order_by,
                peer_start,
                peer_end,
            );
            let hi = resolve_range_bound(
                i, p_len, &frame.end, false, ord_vals, order_by, peer_start, peer_end,
            );
            if hi < 0 || lo >= p_len as isize || lo > hi {
                return None;
            }
            Some((lo.max(0) as usize, hi.min((p_len - 1) as isize) as usize))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_range_bound(
    i: usize,
    p_len: usize,
    bound: &WindowFrameBound,
    is_start: bool,
    ord_vals: &[Vec<ScalarValue>],
    order_by: &[PhysicalSortExpr],
    peer_start: &[usize],
    peer_end: &[usize],
) -> isize {
    match bound {
        WindowFrameBound::UnboundedPreceding => 0,
        WindowFrameBound::UnboundedFollowing => (p_len - 1) as isize,
        WindowFrameBound::CurrentRow => {
            if is_start {
                peer_start[i] as isize
            } else {
                peer_end[i] as isize
            }
        }
        WindowFrameBound::Preceding(k) | WindowFrameBound::Following(k) => {
            let is_prec = matches!(bound, WindowFrameBound::Preceding(_));
            let Some(col) = ord_vals.first() else {
                return if is_start { 0 } else { (p_len - 1) as isize };
            };
            let asc = order_by.first().is_none_or(|o| o.order.ascending);
            let Some(cur) = scalar_as_f64(&col[i]) else {
                return if is_start {
                    peer_start[i] as isize
                } else {
                    peer_end[i] as isize
                };
            };
            let offset = *k as f64;
            let target = match (asc, is_prec) {
                (true, true) => cur - offset,
                (true, false) => cur + offset,
                (false, true) => cur + offset,
                (false, false) => cur - offset,
            };

            if is_start {
                // First index `idx` whose order value is >= target (ASC) or <= target (DESC).
                let mut pos = 0usize;
                while pos < p_len {
                    if let Some(v) = scalar_as_f64(&col[pos]) {
                        let ok = if asc { v >= target } else { v <= target };
                        if ok {
                            break;
                        }
                    }
                    pos += 1;
                }
                pos as isize
            } else {
                // Last index `idx` whose order value is <= target (ASC) or >= target (DESC).
                let mut pos = p_len as isize - 1;
                while pos >= 0 {
                    if let Some(v) = scalar_as_f64(&col[pos as usize]) {
                        let ok = if asc { v <= target } else { v >= target };
                        if ok {
                            break;
                        }
                    }
                    pos -= 1;
                }
                pos
            }
        }
    }
}

#[inline]
fn scalar_as_f64(v: &ScalarValue) -> Option<f64> {
    match v {
        ScalarValue::Int8(x) => Some(f64::from(*x)),
        ScalarValue::Int16(x) => Some(f64::from(*x)),
        ScalarValue::Int32(x) | ScalarValue::Date32(x) => Some(f64::from(*x)),
        ScalarValue::Int64(x) | ScalarValue::TimestampMicros(x) => Some(*x as f64),
        ScalarValue::Float64(x) => Some(*x),
        ScalarValue::Decimal128 { value, scale, .. } => {
            Some((*value as f64) / 10f64.powi(i32::from(*scale)))
        }
        _ => None,
    }
}

/// Iterative bottom-up $O(\log P)$ Segment Tree over monoid `AggState`s!
/// Guarantees $O(P \log P)$ total evaluation time for arbitrary sliding `ROWS` and `RANGE` frames
/// (never quadratic $O(P^2)$!).
#[allow(clippy::too_many_arguments)]
fn eval_window_agg_segment_tree(
    agg_func: AggregateFunc,
    return_type: LogicalType,
    p_len: usize,
    args: &[Vec<ScalarValue>],
    frame: &WindowFrame,
    ord_vals: &[Vec<ScalarValue>],
    order_by: &[PhysicalSortExpr],
    peer_start: &[usize],
    peer_end: &[usize],
) -> Result<Vec<ScalarValue>> {
    if p_len == 0 {
        return Ok(Vec::new());
    }

    // Fast O(P) prefix path when frame is `UNBOUNDED PRECEDING AND CURRENT ROW`!
    if frame.start == WindowFrameBound::UnboundedPreceding
        && frame.end == WindowFrameBound::CurrentRow
    {
        let mut acc = AggState::new_non_distinct(agg_func, return_type);
        let mut out = vec![ScalarValue::Null; p_len];
        let mut row_args = Vec::with_capacity(args.len());

        if frame.units == WindowFrameUnits::Rows {
            for (i, slot) in out.iter_mut().enumerate() {
                row_args.clear();
                for col in args {
                    row_args.push(col[i].clone());
                }
                acc.update(&row_args)?;
                *slot = acc.finalize(return_type)?;
            }
        } else {
            let mut i = 0;
            while i < p_len {
                let end = peer_end[i];
                for r in i..=end {
                    row_args.clear();
                    for col in args {
                        row_args.push(col[r].clone());
                    }
                    acc.update(&row_args)?;
                }
                let val = acc.finalize(return_type)?;
                for slot in &mut out[i..=end] {
                    *slot = val.clone();
                }
                i = end + 1;
            }
        }
        return Ok(out);
    }

    // Fast O(P) whole-partition path when frame is `UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING`!
    if frame.start == WindowFrameBound::UnboundedPreceding
        && frame.end == WindowFrameBound::UnboundedFollowing
    {
        let mut acc = AggState::new_non_distinct(agg_func, return_type);
        let mut row_args = Vec::with_capacity(args.len());
        for i in 0..p_len {
            row_args.clear();
            for col in args {
                row_args.push(col[i].clone());
            }
            acc.update(&row_args)?;
        }
        let val = acc.finalize(return_type)?;
        return Ok(vec![val; p_len]);
    }

    // General Sliding Frame: Build iterative bottom-up Segment Tree in O(P)!
    let cap = p_len.next_power_of_two();
    let empty_state = || AggState::new_non_distinct(agg_func, return_type);
    let mut tree: Vec<AggState> = (0..(2 * cap)).map(|_| empty_state()).collect();

    let mut row_args = Vec::with_capacity(args.len());
    for i in 0..p_len {
        row_args.clear();
        for col in args {
            row_args.push(col[i].clone());
        }
        tree[cap + i].update(&row_args)?;
    }

    for node in (1..cap).rev() {
        let mut combined = tree[2 * node].clone();
        combined.merge_from(tree[2 * node + 1].clone())?;
        tree[node] = combined;
    }

    // Answer every row's `[lo, hi]` frame query in O(log P) preserving left-to-right monoid order!
    let mut out = Vec::with_capacity(p_len);
    let mut right_nodes = Vec::with_capacity(32);

    for i in 0..p_len {
        let Some((lo, hi)) =
            resolve_frame_bounds(i, p_len, frame, ord_vals, order_by, peer_start, peer_end)
        else {
            out.push(empty_state().finalize(return_type)?);
            continue;
        };

        let mut left_acc = empty_state();
        right_nodes.clear();

        let mut l = lo + cap;
        let mut r = hi + cap;
        while l <= r {
            if (l & 1) == 1 {
                left_acc.merge_from(tree[l].clone())?;
                l += 1;
            }
            if (r & 1) == 0 {
                right_nodes.push(r);
                if r == 0 {
                    break;
                }
                r -= 1;
            }
            l >>= 1;
            r >>= 1;
        }
        for &rn in right_nodes.iter().rev() {
            left_acc.merge_from(tree[rn].clone())?;
        }
        out.push(left_acc.finalize(return_type)?);
    }

    Ok(out)
}
