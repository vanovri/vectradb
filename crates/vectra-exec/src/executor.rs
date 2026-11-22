use crate::context::ExecutionContext;
use crate::metrics::WallTimer;
use crate::ops::{hash_agg, hash_join, scan, set_op, sort, window};
use crate::stream::{instrumented_stream, ChunkStream};
use vectra_optimizer::{PhysicalOp, PhysicalPlan};
use vectra_types::{DataChunk, Result};

/// Compiles and opens a pull-based streaming execution pipeline for `plan`.
pub fn execute_physical_plan(plan: &PhysicalPlan, ctx: &ExecutionContext) -> Result<ChunkStream> {
    ctx.check_cancelled()?;
    let init_timer = WallTimer::start();
    let plan_id = plan.plan_id;
    let output_types = plan.schema.logical_types();

    let raw_stream: ChunkStream = match &plan.op {
        PhysicalOp::Empty { produce_one_row } => {
            scan::exec_empty(*produce_one_row, &plan.schema, ctx)?
        }
        PhysicalOp::Values { rows } => scan::exec_values(rows, &plan.schema, ctx)?,
        PhysicalOp::Scan {
            table_name,
            projection,
            pruning_predicate,
            residual_filter,
            ..
        } => scan::exec_scan(
            plan_id,
            table_name,
            projection,
            pruning_predicate.as_ref(),
            residual_filter.as_ref(),
            ctx,
        )?,
        PhysicalOp::TableFunc { func } => scan::exec_table_func(func, &plan.schema, ctx)?,
        PhysicalOp::CteScan { cte_name } => scan::exec_cte_scan(cte_name, ctx)?,
        PhysicalOp::Filter { input, predicate } => {
            let child = execute_physical_plan(input, ctx)?;
            scan::exec_filter(plan_id, child, predicate.clone(), ctx.clone())
        }
        PhysicalOp::Project { input, exprs } => {
            let child = execute_physical_plan(input, ctx)?;
            scan::exec_project(plan_id, child, exprs.clone(), ctx.clone())
        }
        PhysicalOp::Limit {
            input,
            limit,
            offset,
        } => {
            let child = execute_physical_plan(input, ctx)?;
            scan::exec_limit(plan_id, child, *limit, *offset, ctx.clone())
        }
        PhysicalOp::HashJoin {
            left,
            right,
            join_type,
            left_keys,
            right_keys,
            null_safe_keys,
            residual_filter,
            probe_pair,
        } => {
            let l_stream = execute_physical_plan(left, ctx)?;
            let r_stream = execute_physical_plan(right, ctx)?;
            hash_join::exec_hash_join(
                plan_id,
                l_stream,
                r_stream,
                join_type.clone(),
                left.schema.logical_types(),
                right.schema.logical_types(),
                output_types,
                left_keys.clone(),
                right_keys.clone(),
                null_safe_keys.clone(),
                residual_filter.clone(),
                probe_pair.clone(),
                ctx.clone(),
            )?
        }
        PhysicalOp::NestedLoopJoin {
            left,
            right,
            join_type,
            condition,
            probe_pair,
        } => {
            let l_stream = execute_physical_plan(left, ctx)?;
            let r_stream = execute_physical_plan(right, ctx)?;
            hash_join::exec_nested_loop_join(
                plan_id,
                l_stream,
                r_stream,
                join_type.clone(),
                left.schema.logical_types(),
                right.schema.logical_types(),
                output_types,
                condition.clone(),
                probe_pair.clone(),
                ctx.clone(),
            )?
        }
        PhysicalOp::HashAggregate {
            input,
            group_by,
            grouping_sets,
            has_grouping_id,
            aggregates,
            ..
        } => {
            let child = execute_physical_plan(input, ctx)?;
            hash_agg::exec_hash_aggregate(
                plan_id,
                child,
                group_by.clone(),
                grouping_sets.clone(),
                *has_grouping_id,
                aggregates.clone(),
                output_types,
                ctx.clone(),
            )?
        }
        PhysicalOp::Window { input, windows } => {
            let child = execute_physical_plan(input, ctx)?;
            window::exec_window(
                plan_id,
                child,
                windows.clone(),
                input.schema.logical_types(),
                ctx.clone(),
            )?
        }
        PhysicalOp::ExternalSort { input, order_by } => {
            let child = execute_physical_plan(input, ctx)?;
            sort::exec_external_sort(plan_id, child, order_by.clone(), output_types, ctx.clone())?
        }
        PhysicalOp::TopN {
            input,
            order_by,
            limit,
            offset,
        } => {
            let child = execute_physical_plan(input, ctx)?;
            sort::exec_top_n(
                plan_id,
                child,
                order_by.clone(),
                *limit,
                *offset,
                output_types,
                ctx.clone(),
            )?
        }
        PhysicalOp::Distinct { input } => {
            let child = execute_physical_plan(input, ctx)?;
            set_op::exec_distinct(plan_id, child, output_types, ctx.clone())?
        }
        PhysicalOp::SetOp {
            op,
            all,
            left,
            right,
        } => {
            let l_stream = execute_physical_plan(left, ctx)?;
            let r_stream = execute_physical_plan(right, ctx)?;
            set_op::exec_set_op(
                plan_id,
                *op,
                *all,
                l_stream,
                r_stream,
                output_types,
                ctx.clone(),
            )?
        }
        PhysicalOp::RecursiveCte {
            name,
            anchor,
            recursive,
            union_all,
            consumer,
        } => {
            set_op::exec_recursive_cte(plan_id, name, anchor, recursive, *union_all, consumer, ctx)?
        }
    };

    ctx.metrics.add_elapsed(plan_id, init_timer.elapsed_nanos());
    Ok(instrumented_stream(plan_id, raw_stream, ctx.clone()))
}

/// Drains a `ChunkStream` into a `Vec<DataChunk>`, checking cancellation between batches.
pub fn collect_chunks(stream: ChunkStream, ctx: &ExecutionContext) -> Result<Vec<DataChunk>> {
    let mut out = Vec::new();
    for item in stream {
        ctx.check_cancelled()?;
        let ch = item?;
        if !ch.is_empty() {
            out.push(ch);
        }
    }
    Ok(out)
}
