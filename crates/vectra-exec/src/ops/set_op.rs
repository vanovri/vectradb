use crate::context::ExecutionContext;
use crate::executor::execute_physical_plan;
use crate::ops::hash_agg::exec_hash_aggregate;
use crate::stream::{chunk_from_scalar_rows, CanonicalKey, ChunkStream};
use std::collections::{HashMap, HashSet, VecDeque};
use vectra_expr::PhysicalExpr;
use vectra_optimizer::PhysicalPlan;
use vectra_sql::SetOperator;
use vectra_types::{DataChunk, LogicalType, Result, ScalarValue, VectraError, VECTOR_CAPACITY};

/// Executes `PhysicalOp::Distinct` by delegating to our out-of-core spilling `HashAggregate`
/// with 0 aggregates!
pub fn exec_distinct(
    plan_id: usize,
    input: ChunkStream,
    output_types: Vec<LogicalType>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    let group_by: Vec<PhysicalExpr> = output_types
        .iter()
        .enumerate()
        .map(|(index, &data_type)| PhysicalExpr::ColumnRef {
            index,
            name: format!("c{index}"),
            data_type,
        })
        .collect();
    exec_hash_aggregate(
        plan_id,
        input,
        group_by,
        None,
        false,
        Vec::new(),
        output_types,
        ctx,
    )
}

/// Executes `PhysicalOp::SetOp` (`UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`) with exact
/// SQL `IS NOT DISTINCT FROM` NULL equality and multiset multiplicity semantics!
pub fn exec_set_op(
    plan_id: usize,
    op: SetOperator,
    all: bool,
    left: ChunkStream,
    right: ChunkStream,
    output_types: Vec<LogicalType>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    match op {
        SetOperator::Union => {
            let chained: ChunkStream = Box::new(left.chain(right));
            if all {
                Ok(chained)
            } else {
                exec_distinct(plan_id, chained, output_types, ctx)
            }
        }
        SetOperator::Intersect | SetOperator::Except => {
            // Count right-side row multiplicities first.
            let mut right_counts: HashMap<CanonicalKey, usize> = HashMap::new();
            for chunk_res in right {
                ctx.check_cancelled()?;
                let chunk = chunk_res?;
                let n = chunk.len();
                if n == 0 {
                    continue;
                }
                ctx.metrics.record_input(plan_id, n as u64);
                for r in 0..n {
                    let key = CanonicalKey(chunk.get_row(r));
                    *right_counts.entry(key).or_insert(0) += 1;
                }
            }

            let mut out_rows: Vec<Vec<ScalarValue>> = Vec::new();
            let mut emitted_distinct: HashSet<CanonicalKey> = HashSet::new();
            let mut chunks: VecDeque<DataChunk> = VecDeque::new();

            for chunk_res in left {
                ctx.check_cancelled()?;
                let chunk = chunk_res?;
                let n = chunk.len();
                if n == 0 {
                    continue;
                }
                ctx.metrics.record_input(plan_id, n as u64);
                for r in 0..n {
                    let row = chunk.get_row(r);
                    let key = CanonicalKey(row.clone());
                    let emit = match (op, all) {
                        (SetOperator::Intersect, false) => {
                            right_counts.contains_key(&key) && emitted_distinct.insert(key)
                        }
                        (SetOperator::Intersect, true) => {
                            if let Some(cnt) = right_counts.get_mut(&key) {
                                if *cnt > 0 {
                                    *cnt -= 1;
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        }
                        (SetOperator::Except, false) => {
                            !right_counts.contains_key(&key) && emitted_distinct.insert(key)
                        }
                        (SetOperator::Except, true) => {
                            if let Some(cnt) = right_counts.get_mut(&key) {
                                if *cnt > 0 {
                                    *cnt -= 1;
                                    false
                                } else {
                                    true
                                }
                            } else {
                                true
                            }
                        }
                        (SetOperator::Union, _) => unreachable!(),
                    };
                    if emit {
                        out_rows.push(row);
                        if out_rows.len() >= VECTOR_CAPACITY {
                            chunks.push_back(chunk_from_scalar_rows(
                                &output_types,
                                &out_rows,
                                &ctx.pool,
                            )?);
                            out_rows.clear();
                        }
                    }
                }
            }
            if !out_rows.is_empty() {
                chunks.push_back(chunk_from_scalar_rows(&output_types, &out_rows, &ctx.pool)?);
            }
            Ok(Box::new(chunks.into_iter().map(Ok)))
        }
    }
}

/// Executes `PhysicalOp::RecursiveCte` via **Semi-Naive Fixpoint Evaluation** with
/// cycle/max-iteration protection!
pub fn exec_recursive_cte(
    plan_id: usize,
    name: &str,
    anchor: &PhysicalPlan,
    recursive: &PhysicalPlan,
    union_all: bool,
    consumer: &PhysicalPlan,
    ctx: &ExecutionContext,
) -> Result<ChunkStream> {
    let cte_types = anchor.schema.logical_types();
    let max_iters = ctx.config.max_recursive_iterations;

    let mut seen: HashSet<CanonicalKey> = HashSet::new();
    let mut accumulated: Vec<DataChunk> = Vec::new();
    let mut delta: Vec<DataChunk> = Vec::new();

    // 1. Evaluate anchor term.
    let anchor_stream = execute_physical_plan(anchor, ctx)?;
    for chunk_res in anchor_stream {
        ctx.check_cancelled()?;
        let chunk = chunk_res?.compact(&ctx.pool)?;
        if chunk.is_empty() {
            continue;
        }
        ctx.metrics.record_input(plan_id, chunk.len() as u64);
        if union_all {
            accumulated.push(chunk.clone());
            delta.push(chunk);
        } else {
            let mut kept = Vec::new();
            for r in 0..chunk.len() {
                let row = chunk.get_row(r);
                if seen.insert(CanonicalKey(row.clone())) {
                    kept.push(row);
                }
            }
            if !kept.is_empty() {
                let c = chunk_from_scalar_rows(&cte_types, &kept, &ctx.pool)?;
                accumulated.push(c.clone());
                delta.push(c);
            }
        }
    }

    // 2. Semi-naive fixpoint iteration over `delta`.
    let mut iter = 0usize;
    while !delta.is_empty() {
        ctx.check_cancelled()?;
        iter += 1;
        if iter > max_iters {
            return Err(VectraError::ExecutionError(format!(
                "recursive CTE '{name}' exceeded max_recursive_iterations ({max_iters})"
            )));
        }
        let step_ctx = ctx.with_cte_binding(name.to_string(), delta);
        let rec_stream = execute_physical_plan(recursive, &step_ctx)?;
        let mut next_delta: Vec<DataChunk> = Vec::new();

        for chunk_res in rec_stream {
            ctx.check_cancelled()?;
            let chunk = chunk_res?.compact(&ctx.pool)?;
            if chunk.is_empty() {
                continue;
            }
            ctx.metrics.record_input(plan_id, chunk.len() as u64);
            if union_all {
                accumulated.push(chunk.clone());
                next_delta.push(chunk);
            } else {
                let mut kept = Vec::new();
                for r in 0..chunk.len() {
                    let row = chunk.get_row(r);
                    if seen.insert(CanonicalKey(row.clone())) {
                        kept.push(row);
                    }
                }
                if !kept.is_empty() {
                    let c = chunk_from_scalar_rows(&cte_types, &kept, &ctx.pool)?;
                    accumulated.push(c.clone());
                    next_delta.push(c);
                }
            }
        }
        delta = next_delta;
    }

    // 3. Execute consumer with full accumulated CTE table bound.
    let consumer_ctx = ctx.with_cte_binding(name.to_string(), accumulated);
    execute_physical_plan(consumer, &consumer_ctx)
}
