use crate::cardinality::{
    estimate_conjunction_selectivity, estimate_join_stats, estimate_plan_stats, extract_base_col,
    extract_literal, split_conjuncts, PlanStats,
};
use crate::rules::{combine_and, split_and_owned};
use crate::stats::StatsCatalog;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use vectra_expr::{BinaryOperator, PhysicalExpr, UnaryOperator};
use vectra_sql::{
    AggregateFunc, ColId, JoinType, LogicalExpr, LogicalPlan, SetOperator, TableFunctionKind,
    WindowFrame, WindowFunc,
};
use vectra_storage::PruningPredicate;
use vectra_types::{Field, LogicalType, Result, ScalarValue, Schema, SortOrder, VectraError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhysicalSortExpr {
    pub expr: PhysicalExpr,
    pub order: SortOrder,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhysicalAggExpr {
    pub func: AggregateFunc,
    pub args: Vec<PhysicalExpr>,
    pub distinct: bool,
    pub filter: Option<PhysicalExpr>,
    pub return_type: LogicalType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhysicalWindowExpr {
    pub func: WindowFunc,
    pub args: Vec<PhysicalExpr>,
    pub partition_by: Vec<PhysicalExpr>,
    pub order_by: Vec<PhysicalSortExpr>,
    pub frame: Option<WindowFrame>,
    pub return_type: LogicalType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhysicalJoinType {
    Inner,
    LeftOuter,
    RightOuter,
    FullOuter,
    Cross,
    LeftSemi,
    LeftAnti,
    RightSemi,
    RightAnti,
    LeftMark,
    Single,
}

/// Complete physical execution plan tree annotated with cost & cardinality estimates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalPlan {
    pub plan_id: usize,
    pub estimated_rows: f64,
    pub estimated_cost: f64,
    pub schema: Schema,
    pub output_col_ids: Vec<ColId>,
    pub op: PhysicalOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PhysicalOp {
    Empty {
        produce_one_row: bool,
    },
    Values {
        rows: Vec<Vec<PhysicalExpr>>,
    },
    Scan {
        table_name: String,
        alias: String,
        /// 0-based indices into the storage `Table::schema` (projection pruned!).
        projection: Vec<usize>,
        /// Block-level ZoneMap + Bloom pruning predicate indexed by original table schema columns.
        pruning_predicate: Option<PruningPredicate>,
        /// Vectorized residual filter evaluated over the projected output columns (`0..projection.len()`).
        residual_filter: Option<PhysicalExpr>,
    },
    TableFunc {
        func: TableFunctionKind,
    },
    CteScan {
        cte_name: String,
    },
    Filter {
        input: Box<PhysicalPlan>,
        predicate: PhysicalExpr,
    },
    Project {
        input: Box<PhysicalPlan>,
        exprs: Vec<PhysicalExpr>,
    },
    HashJoin {
        /// Probe side (streaming input).
        left: Box<PhysicalPlan>,
        /// Build side (smaller estimated cardinality whenever symmetric!).
        right: Box<PhysicalPlan>,
        join_type: PhysicalJoinType,
        left_keys: Vec<PhysicalExpr>,
        right_keys: Vec<PhysicalExpr>,
        /// True for `IS NOT DISTINCT FROM` keys (NULL == NULL matches), false for SQL `=`.
        null_safe_keys: Vec<bool>,
        /// Residual predicate evaluated over `[left_cols..., right_cols...]`.
        residual_filter: Option<PhysicalExpr>,
        /// Optional `(lhs_on_left, rhs_on_right)` pair for 3-valued SQL `IN`/`NOT IN` mark join.
        probe_pair: Option<(PhysicalExpr, PhysicalExpr)>,
    },
    NestedLoopJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        join_type: PhysicalJoinType,
        /// Condition evaluated over `[left_cols..., right_cols...]`.
        condition: Option<PhysicalExpr>,
        probe_pair: Option<(PhysicalExpr, PhysicalExpr)>,
    },
    HashAggregate {
        input: Box<PhysicalPlan>,
        group_by: Vec<PhysicalExpr>,
        grouping_sets: Option<Vec<Vec<usize>>>,
        has_grouping_id: bool,
        aggregates: Vec<PhysicalAggExpr>,
        /// True when single narrow integer key allows direct-indexed perfect-hash fast path.
        perfect_hash_hint: bool,
    },
    Window {
        input: Box<PhysicalPlan>,
        windows: Vec<PhysicalWindowExpr>,
    },
    ExternalSort {
        input: Box<PhysicalPlan>,
        order_by: Vec<PhysicalSortExpr>,
    },
    TopN {
        input: Box<PhysicalPlan>,
        order_by: Vec<PhysicalSortExpr>,
        limit: usize,
        offset: usize,
    },
    Limit {
        input: Box<PhysicalPlan>,
        limit: Option<usize>,
        offset: usize,
    },
    Distinct {
        input: Box<PhysicalPlan>,
    },
    SetOp {
        op: SetOperator,
        all: bool,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
    },
    RecursiveCte {
        name: String,
        anchor: Box<PhysicalPlan>,
        recursive: Box<PhysicalPlan>,
        union_all: bool,
        consumer: Box<PhysicalPlan>,
    },
}

impl PhysicalPlan {
    pub fn op_name(&self) -> &'static str {
        match &self.op {
            PhysicalOp::Empty { .. } => "Empty",
            PhysicalOp::Values { .. } => "Values",
            PhysicalOp::Scan { .. } => "ColumnarScan",
            PhysicalOp::TableFunc { .. } => "TableFunc",
            PhysicalOp::CteScan { .. } => "CteScan",
            PhysicalOp::Filter { .. } => "Filter",
            PhysicalOp::Project { .. } => "Project",
            PhysicalOp::HashJoin { .. } => "HashJoin",
            PhysicalOp::NestedLoopJoin { .. } => "NestedLoopJoin",
            PhysicalOp::HashAggregate { .. } => "HashAggregate",
            PhysicalOp::Window { .. } => "Window",
            PhysicalOp::ExternalSort { .. } => "ExternalSort",
            PhysicalOp::TopN { .. } => "TopN",
            PhysicalOp::Limit { .. } => "Limit",
            PhysicalOp::Distinct { .. } => "Distinct",
            PhysicalOp::SetOp { .. } => "SetOp",
            PhysicalOp::RecursiveCte { .. } => "RecursiveCte",
        }
    }

    pub fn children(&self) -> Vec<&PhysicalPlan> {
        match &self.op {
            PhysicalOp::Empty { .. }
            | PhysicalOp::Values { .. }
            | PhysicalOp::Scan { .. }
            | PhysicalOp::TableFunc { .. }
            | PhysicalOp::CteScan { .. } => Vec::new(),
            PhysicalOp::Filter { input, .. }
            | PhysicalOp::Project { input, .. }
            | PhysicalOp::HashAggregate { input, .. }
            | PhysicalOp::Window { input, .. }
            | PhysicalOp::ExternalSort { input, .. }
            | PhysicalOp::TopN { input, .. }
            | PhysicalOp::Limit { input, .. }
            | PhysicalOp::Distinct { input } => vec![input],
            PhysicalOp::HashJoin { left, right, .. }
            | PhysicalOp::NestedLoopJoin { left, right, .. }
            | PhysicalOp::SetOp { left, right, .. } => vec![left, right],
            PhysicalOp::RecursiveCte {
                anchor,
                recursive,
                consumer,
                ..
            } => vec![anchor, recursive, consumer],
        }
    }

    /// Formats a clean box-drawing ASCII execution tree with estimated rows & costs.
    pub fn format_tree(&self) -> String {
        let mut out = String::new();
        self.fmt_node("", true, &mut out);
        out
    }

    fn fmt_node(&self, prefix: &str, is_last: bool, out: &mut String) {
        let connector = if prefix.is_empty() {
            ""
        } else if is_last {
            "└─ "
        } else {
            "├─ "
        };
        let detail = match &self.op {
            PhysicalOp::Scan {
                table_name,
                projection,
                pruning_predicate,
                residual_filter,
                ..
            } => format!(
                "table={table_name}, cols={}, zonemap_prune={}, filter={}",
                projection.len(),
                pruning_predicate.is_some(),
                residual_filter.is_some()
            ),
            PhysicalOp::HashJoin {
                join_type,
                left_keys,
                residual_filter,
                ..
            } => format!(
                "type={join_type:?}, keys={}, residual={}",
                left_keys.len(),
                residual_filter.is_some()
            ),
            PhysicalOp::NestedLoopJoin { join_type, .. } => format!("type={join_type:?}"),
            PhysicalOp::HashAggregate {
                group_by,
                aggregates,
                perfect_hash_hint,
                ..
            } => format!(
                "groups={}, aggs={}, perfect_hash={perfect_hash_hint}",
                group_by.len(),
                aggregates.len()
            ),
            PhysicalOp::TopN { limit, offset, .. } => format!("limit={limit}, offset={offset}"),
            PhysicalOp::Limit { limit, offset, .. } => format!("limit={limit:?}, offset={offset}"),
            PhysicalOp::SetOp { op, all, .. } => format!("{op:?}(all={all})"),
            PhysicalOp::RecursiveCte {
                name, union_all, ..
            } => {
                format!("name={name}, all={union_all}")
            }
            _ => String::new(),
        };
        if detail.is_empty() {
            out.push_str(&format!(
                "{prefix}{connector}{} [id={}, est_rows={:.0}, cost={:.1}]\n",
                self.op_name(),
                self.plan_id,
                self.estimated_rows,
                self.estimated_cost
            ));
        } else {
            out.push_str(&format!(
                "{prefix}{connector}{} ({detail}) [id={}, est_rows={:.0}, cost={:.1}]\n",
                self.op_name(),
                self.plan_id,
                self.estimated_rows,
                self.estimated_cost
            ));
        }

        let kids = self.children();
        let next_prefix = if prefix.is_empty() {
            String::new()
        } else if is_last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}│  ")
        };
        for (i, kid) in kids.iter().enumerate() {
            kid.fmt_node(&next_prefix, i + 1 == kids.len(), out);
        }
    }
}

/// Compiles an optimized `LogicalPlan` into a cost-annotated `PhysicalPlan` in a single bottom-up O(N) pass.
pub fn build_physical_plan(
    logical: &LogicalPlan,
    stats_catalog: &StatsCatalog,
) -> Result<PhysicalPlan> {
    let mut next_id = 1usize;
    let (plan, _) = compile_node(logical, stats_catalog, &mut next_id)?;
    Ok(plan)
}

fn compile_node(
    plan: &LogicalPlan,
    stats_catalog: &StatsCatalog,
    next_id: &mut usize,
) -> Result<(PhysicalPlan, PlanStats)> {
    let plan_id = *next_id;
    *next_id += 1;

    match plan {
        LogicalPlan::EmptyRelation {
            produce_one_row,
            columns,
        } => {
            let pstats = estimate_plan_stats(plan, stats_catalog);
            let schema = Schema::new(columns.iter().map(|c| c.to_field()).collect());
            let output_col_ids: Vec<ColId> = columns.iter().map(|c| c.id).collect();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pstats.rows,
                    estimated_cost: 0.0,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::Empty {
                        produce_one_row: *produce_one_row,
                    },
                },
                pstats,
            ))
        }

        LogicalPlan::Values { rows, columns } => {
            let pstats = estimate_plan_stats(plan, stats_catalog);
            let schema = Schema::new(columns.iter().map(|c| c.to_field()).collect());
            let output_col_ids: Vec<ColId> = columns.iter().map(|c| c.id).collect();
            let empty_map = HashMap::new();
            let mut phys_rows = Vec::with_capacity(rows.len());
            for row in rows {
                let mut pr = Vec::with_capacity(row.len());
                for e in row {
                    pr.push(compile_expr(e, &empty_map)?);
                }
                phys_rows.push(pr);
            }
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pstats.rows,
                    estimated_cost: rows.len() as f64,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::Values { rows: phys_rows },
                },
                pstats,
            ))
        }

        LogicalPlan::Scan {
            table_name,
            alias,
            columns,
            table_col_indices,
            pushed_filters,
        } => {
            let pstats = estimate_plan_stats(plan, stats_catalog);
            let schema = Schema::new(columns.iter().map(|c| c.to_field()).collect());
            let output_col_ids: Vec<ColId> = columns.iter().map(|c| c.id).collect();
            let col_map = build_col_index_map(&output_col_ids);
            let orig_col_map: HashMap<ColId, usize> = columns
                .iter()
                .zip(table_col_indices.iter())
                .map(|(c, &orig_idx)| (c.id, orig_idx))
                .collect();

            let pruning_predicate = build_pruning_predicate(pushed_filters, &orig_col_map);
            let residual_filter = if pushed_filters.is_empty() {
                None
            } else {
                let combined = combine_and(pushed_filters.clone());
                Some(compile_expr(&combined, &col_map)?)
            };

            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pstats.rows,
                    estimated_cost: pstats.rows,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::Scan {
                        table_name: table_name.clone(),
                        alias: alias.clone(),
                        projection: table_col_indices.clone(),
                        pruning_predicate,
                        residual_filter,
                    },
                },
                pstats,
            ))
        }

        LogicalPlan::TableFunc { func, columns } => {
            let pstats = estimate_plan_stats(plan, stats_catalog);
            let schema = Schema::new(columns.iter().map(|c| c.to_field()).collect());
            let output_col_ids: Vec<ColId> = columns.iter().map(|c| c.id).collect();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pstats.rows,
                    estimated_cost: pstats.rows,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::TableFunc { func: func.clone() },
                },
                pstats,
            ))
        }

        LogicalPlan::CteScan { cte_name, columns } => {
            let pstats = estimate_plan_stats(plan, stats_catalog);
            let schema = Schema::new(columns.iter().map(|c| c.to_field()).collect());
            let output_col_ids: Vec<ColId> = columns.iter().map(|c| c.id).collect();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pstats.rows,
                    estimated_cost: pstats.rows,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::CteScan {
                        cte_name: cte_name.clone(),
                    },
                },
                pstats,
            ))
        }

        LogicalPlan::Filter { input, predicate } => {
            let (child, mut cstats) = compile_node(input, stats_catalog, next_id)?;
            let cmap = build_col_index_map(&child.output_col_ids);
            let pred = compile_expr(predicate, &cmap)?;
            let mut conj = Vec::new();
            split_conjuncts(predicate, &mut conj);
            let sel = estimate_conjunction_selectivity(&conj, &cstats);
            cstats.rows = (cstats.rows * sel).max(1.0);
            let cost = child.estimated_cost + child.estimated_rows * 0.25;
            let schema = child.schema.clone();
            let output_col_ids = child.output_col_ids.clone();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: cstats.rows,
                    estimated_cost: cost,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::Filter {
                        input: Box::new(child),
                        predicate: pred,
                    },
                },
                cstats,
            ))
        }

        LogicalPlan::Project { input, exprs } => {
            let (child, cstats) = compile_node(input, stats_catalog, next_id)?;
            let cmap = build_col_index_map(&child.output_col_ids);
            let mut phys_exprs = Vec::with_capacity(exprs.len());
            let mut fields = Vec::with_capacity(exprs.len());
            let mut output_col_ids = Vec::with_capacity(exprs.len());
            let mut col_stats = HashMap::new();
            for (e, bc) in exprs {
                phys_exprs.push(compile_expr(e, &cmap)?);
                fields.push(bc.to_field());
                output_col_ids.push(bc.id);
                if let LogicalExpr::Col { id, .. } = e {
                    if let Some(cs) = cstats.col_stats.get(id) {
                        col_stats.insert(bc.id, cs.clone());
                    }
                }
            }
            let pstats = PlanStats {
                rows: cstats.rows,
                col_stats,
            };
            let cost = child.estimated_cost + child.estimated_rows * 0.1;
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pstats.rows,
                    estimated_cost: cost,
                    schema: Schema::new(fields),
                    output_col_ids,
                    op: PhysicalOp::Project {
                        input: Box::new(child),
                        exprs: phys_exprs,
                    },
                },
                pstats,
            ))
        }

        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        } => {
            let (mut lp, mut ls) = compile_node(left, stats_catalog, next_id)?;
            let (mut rp, mut rs) = compile_node(right, stats_catalog, next_id)?;

            // Enforce build-side cardinality invariant: smaller estimated rows on Right (Build) side
            // whenever join orientation is symmetric (Inner, LeftOuter <-> RightOuter).
            let mut eff_jt = join_type.clone();
            if lp.estimated_rows < rp.estimated_rows {
                match eff_jt {
                    JoinType::Inner => {
                        std::mem::swap(&mut lp, &mut rp);
                        std::mem::swap(&mut ls, &mut rs);
                    }
                    JoinType::LeftOuter => {
                        std::mem::swap(&mut lp, &mut rp);
                        std::mem::swap(&mut ls, &mut rs);
                        eff_jt = JoinType::RightOuter;
                    }
                    JoinType::RightOuter => {
                        std::mem::swap(&mut lp, &mut rp);
                        std::mem::swap(&mut ls, &mut rs);
                        eff_jt = JoinType::LeftOuter;
                    }
                    _ => {}
                }
            }

            let pstats = estimate_join_stats(&ls, &rs, &eff_jt, condition.as_ref());

            let l_map = build_col_index_map(&lp.output_col_ids);
            let r_map = build_col_index_map(&rp.output_col_ids);
            let mut combined_ids = lp.output_col_ids.clone();
            combined_ids.extend_from_slice(&rp.output_col_ids);
            let combined_map = build_col_index_map(&combined_ids);

            let (final_schema, final_col_ids) = match &eff_jt {
                JoinType::LeftSemi | JoinType::LeftAnti => {
                    (lp.schema.clone(), lp.output_col_ids.clone())
                }
                JoinType::RightSemi | JoinType::RightAnti => {
                    (rp.schema.clone(), rp.output_col_ids.clone())
                }
                JoinType::LeftMark { mark_col, .. } => {
                    let mut fields = lp.schema.fields.clone();
                    fields.push(mark_col.to_field());
                    let mut ids = lp.output_col_ids.clone();
                    ids.push(mark_col.id);
                    (Schema::new(fields), ids)
                }
                _ => {
                    let mut fields = lp.schema.fields.clone();
                    fields.extend(rp.schema.fields.clone());
                    (Schema::new(fields), combined_ids)
                }
            };

            let phys_jt = match &eff_jt {
                JoinType::Inner => PhysicalJoinType::Inner,
                JoinType::LeftOuter => PhysicalJoinType::LeftOuter,
                JoinType::RightOuter => PhysicalJoinType::RightOuter,
                JoinType::FullOuter => PhysicalJoinType::FullOuter,
                JoinType::Cross => PhysicalJoinType::Cross,
                JoinType::LeftSemi => PhysicalJoinType::LeftSemi,
                JoinType::LeftAnti => PhysicalJoinType::LeftAnti,
                JoinType::RightSemi => PhysicalJoinType::RightSemi,
                JoinType::RightAnti => PhysicalJoinType::RightAnti,
                JoinType::LeftMark { .. } => PhysicalJoinType::LeftMark,
                JoinType::Single => PhysicalJoinType::Single,
            };

            let probe_pair_phys = match &eff_jt {
                JoinType::LeftMark {
                    probe_pair: Some((a, b)),
                    ..
                } => Some((compile_expr(a, &l_map)?, compile_expr(b, &r_map)?)),
                _ => None,
            };

            // Extract equi-join key pairs (`=` and `IS NOT DISTINCT FROM`) vs residual predicates!
            let mut left_keys = Vec::new();
            let mut right_keys = Vec::new();
            let mut null_safe_keys = Vec::new();
            let mut residuals = Vec::new();

            if let Some(cond) = condition {
                let mut parts = Vec::new();
                split_and_owned(cond.clone(), &mut parts);
                for p in parts {
                    if let Some((lk, rk, ns)) = extract_equi_pair(&p, &l_map, &r_map) {
                        left_keys.push(compile_expr(&lk, &l_map)?);
                        right_keys.push(compile_expr(&rk, &r_map)?);
                        null_safe_keys.push(ns);
                    } else {
                        residuals.push(p);
                    }
                }
            }

            if !left_keys.is_empty() {
                let residual_filter = if residuals.is_empty() {
                    None
                } else {
                    Some(compile_expr(&combine_and(residuals), &combined_map)?)
                };
                let cost = lp.estimated_cost
                    + rp.estimated_cost
                    + 2.0 * rp.estimated_rows
                    + lp.estimated_rows
                    + pstats.rows;
                Ok((
                    PhysicalPlan {
                        plan_id,
                        estimated_rows: pstats.rows,
                        estimated_cost: cost,
                        schema: final_schema,
                        output_col_ids: final_col_ids,
                        op: PhysicalOp::HashJoin {
                            left: Box::new(lp),
                            right: Box::new(rp),
                            join_type: phys_jt,
                            left_keys,
                            right_keys,
                            null_safe_keys,
                            residual_filter,
                            probe_pair: probe_pair_phys,
                        },
                    },
                    pstats,
                ))
            } else {
                let phys_cond = match condition {
                    Some(c) => Some(compile_expr(c, &combined_map)?),
                    None => None,
                };
                let cost =
                    lp.estimated_cost + rp.estimated_cost + lp.estimated_rows * rp.estimated_rows;
                Ok((
                    PhysicalPlan {
                        plan_id,
                        estimated_rows: pstats.rows,
                        estimated_cost: cost,
                        schema: final_schema,
                        output_col_ids: final_col_ids,
                        op: PhysicalOp::NestedLoopJoin {
                            left: Box::new(lp),
                            right: Box::new(rp),
                            join_type: phys_jt,
                            condition: phys_cond,
                            probe_pair: probe_pair_phys,
                        },
                    },
                    pstats,
                ))
            }
        }

        LogicalPlan::DependentJoin { .. } => Err(VectraError::PlanError(
            "DependentJoin reached physical planner; unnest_plan must run first".into(),
        )),

        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        } => {
            let (child, cstats) = compile_node(input, stats_catalog, next_id)?;
            let cmap = build_col_index_map(&child.output_col_ids);
            let mut phys_gb = Vec::with_capacity(group_by.len());
            let mut out_fields = Vec::new();
            let mut output_col_ids = Vec::new();
            let mut prod_ndv = 1.0f64;
            for (e, bc) in group_by {
                phys_gb.push(compile_expr(e, &cmap)?);
                out_fields.push(bc.to_field());
                output_col_ids.push(bc.id);
                let ndv = match e {
                    LogicalExpr::Col { id, .. } => cstats.ndv_of(*id),
                    _ => (cstats.rows * 0.2).max(1.0),
                };
                prod_ndv *= ndv;
            }
            if let Some(gid) = grouping_id_col {
                out_fields.push(gid.to_field());
                output_col_ids.push(gid.id);
            }
            let mut phys_aggs = Vec::with_capacity(aggregates.len());
            for (a, bc) in aggregates {
                out_fields.push(bc.to_field());
                output_col_ids.push(bc.id);
                let mut pargs = Vec::with_capacity(a.args.len());
                for arg in &a.args {
                    pargs.push(compile_expr(arg, &cmap)?);
                }
                let pfilt = match &a.filter {
                    Some(f) => Some(compile_expr(f, &cmap)?),
                    None => None,
                };
                phys_aggs.push(PhysicalAggExpr {
                    func: a.func,
                    args: pargs,
                    distinct: a.distinct,
                    filter: pfilt,
                    return_type: a.return_type,
                });
            }
            let base_groups = if group_by.is_empty() {
                1.0
            } else {
                prod_ndv.min(cstats.rows).max(1.0)
            };
            let mult = grouping_sets
                .as_ref()
                .map_or(1.0, |s| s.len().max(1) as f64);
            let out_rows = (base_groups * mult).max(1.0);
            let mut col_stats = HashMap::new();
            for (e, bc) in group_by {
                if let LogicalExpr::Col { id, .. } = e {
                    if let Some(mut cs) = cstats.col_stats.get(id).cloned() {
                        cs.ndv = cs.ndv.min(out_rows);
                        col_stats.insert(bc.id, cs);
                    }
                }
            }
            let pstats = PlanStats {
                rows: out_rows,
                col_stats,
            };
            let perfect_hash_hint = phys_gb.len() == 1
                && grouping_sets.is_none()
                && phys_gb[0].data_type().is_integer()
                && pstats.rows <= 4096.0;

            let cost = child.estimated_cost + child.estimated_rows * 1.5;
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pstats.rows,
                    estimated_cost: cost,
                    schema: Schema::new(out_fields),
                    output_col_ids,
                    op: PhysicalOp::HashAggregate {
                        input: Box::new(child),
                        group_by: phys_gb,
                        grouping_sets: grouping_sets.clone(),
                        has_grouping_id: grouping_id_col.is_some(),
                        aggregates: phys_aggs,
                        perfect_hash_hint,
                    },
                },
                pstats,
            ))
        }

        LogicalPlan::Window { input, windows } => {
            let (child, cstats) = compile_node(input, stats_catalog, next_id)?;
            let cmap = build_col_index_map(&child.output_col_ids);
            let mut out_fields = child.schema.fields.clone();
            let mut output_col_ids = child.output_col_ids.clone();
            let mut pwins = Vec::with_capacity(windows.len());
            for (w, bc) in windows {
                out_fields.push(bc.to_field());
                output_col_ids.push(bc.id);
                let mut pargs = Vec::with_capacity(w.args.len());
                for a in &w.args {
                    pargs.push(compile_expr(a, &cmap)?);
                }
                let mut ppart = Vec::with_capacity(w.partition_by.len());
                for p in &w.partition_by {
                    ppart.push(compile_expr(p, &cmap)?);
                }
                let mut pord = Vec::with_capacity(w.order_by.len());
                for o in &w.order_by {
                    pord.push(PhysicalSortExpr {
                        expr: compile_expr(&o.expr, &cmap)?,
                        order: o.order,
                    });
                }
                pwins.push(PhysicalWindowExpr {
                    func: w.func,
                    args: pargs,
                    partition_by: ppart,
                    order_by: pord,
                    frame: w.frame.clone(),
                    return_type: w.return_type,
                });
            }
            let nlogn = child.estimated_rows * (child.estimated_rows.max(2.0).log2());
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: cstats.rows,
                    estimated_cost: child.estimated_cost + nlogn,
                    schema: Schema::new(out_fields),
                    output_col_ids,
                    op: PhysicalOp::Window {
                        input: Box::new(child),
                        windows: pwins,
                    },
                },
                cstats,
            ))
        }

        LogicalPlan::Sort { input, order_by } => {
            let (child, cstats) = compile_node(input, stats_catalog, next_id)?;
            let cmap = build_col_index_map(&child.output_col_ids);
            let mut pord = Vec::with_capacity(order_by.len());
            for o in order_by {
                pord.push(PhysicalSortExpr {
                    expr: compile_expr(&o.expr, &cmap)?,
                    order: o.order,
                });
            }
            let cost =
                child.estimated_cost + child.estimated_rows * child.estimated_rows.max(2.0).log2();
            let schema = child.schema.clone();
            let output_col_ids = child.output_col_ids.clone();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: cstats.rows,
                    estimated_cost: cost,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::ExternalSort {
                        input: Box::new(child),
                        order_by: pord,
                    },
                },
                cstats,
            ))
        }

        LogicalPlan::TopN {
            input,
            order_by,
            limit,
            offset,
        } => {
            let (child, mut cstats) = compile_node(input, stats_catalog, next_id)?;
            let cmap = build_col_index_map(&child.output_col_ids);
            let mut pord = Vec::with_capacity(order_by.len());
            for o in order_by {
                pord.push(PhysicalSortExpr {
                    expr: compile_expr(&o.expr, &cmap)?,
                    order: o.order,
                });
            }
            cstats.rows = cstats.rows.min(*limit as f64);
            let heap_k = (limit + offset).max(2) as f64;
            let cost = child.estimated_cost + child.estimated_rows * heap_k.log2();
            let schema = child.schema.clone();
            let output_col_ids = child.output_col_ids.clone();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: cstats.rows,
                    estimated_cost: cost,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::TopN {
                        input: Box::new(child),
                        order_by: pord,
                        limit: *limit,
                        offset: *offset,
                    },
                },
                cstats,
            ))
        }

        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let (child, mut cstats) = compile_node(input, stats_catalog, next_id)?;
            if let Some(l) = limit {
                cstats.rows = cstats.rows.min(*l as f64);
            }
            let cost = child.estimated_cost + cstats.rows * 0.05;
            let schema = child.schema.clone();
            let output_col_ids = child.output_col_ids.clone();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: cstats.rows,
                    estimated_cost: cost,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::Limit {
                        input: Box::new(child),
                        limit: *limit,
                        offset: *offset,
                    },
                },
                cstats,
            ))
        }

        LogicalPlan::Distinct { input, on_cols } => {
            let (child, mut cstats) = compile_node(input, stats_catalog, next_id)?;
            let mut prod = 1.0f64;
            for c in on_cols {
                prod *= cstats.ndv_of(c.id);
            }
            cstats.rows = prod.min(cstats.rows).max(1.0);
            let cost = child.estimated_cost + child.estimated_rows * 1.5;
            let schema = Schema::new(on_cols.iter().map(|c| c.to_field()).collect());
            let output_col_ids: Vec<ColId> = on_cols.iter().map(|c| c.id).collect();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: cstats.rows,
                    estimated_cost: cost,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::Distinct {
                        input: Box::new(child),
                    },
                },
                cstats,
            ))
        }

        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
            output_cols,
        } => {
            let (lp, ls) = compile_node(left, stats_catalog, next_id)?;
            let (rp, rs) = compile_node(right, stats_catalog, next_id)?;
            let rows = match op {
                SetOperator::Union => ls.rows + rs.rows,
                SetOperator::Intersect => ls.rows.min(rs.rows) * 0.5,
                SetOperator::Except => (ls.rows - rs.rows * 0.5).max(1.0),
            }
            .max(1.0);
            let pstats = PlanStats {
                rows,
                col_stats: ls.col_stats,
            };
            let cost =
                lp.estimated_cost + rp.estimated_cost + lp.estimated_rows + rp.estimated_rows;
            let schema = Schema::new(output_cols.iter().map(|c| c.to_field()).collect());
            let output_col_ids: Vec<ColId> = output_cols.iter().map(|c| c.id).collect();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pstats.rows,
                    estimated_cost: cost,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::SetOp {
                        op: *op,
                        all: *all,
                        left: Box::new(lp),
                        right: Box::new(rp),
                    },
                },
                pstats,
            ))
        }

        LogicalPlan::RecursiveCte {
            name,
            anchor,
            recursive,
            union_all,
            consumer,
            ..
        } => {
            let (pa, _) = compile_node(anchor, stats_catalog, next_id)?;
            let (pr, _) = compile_node(recursive, stats_catalog, next_id)?;
            let (pc, cs) = compile_node(consumer, stats_catalog, next_id)?;
            let cost = pa.estimated_cost + pr.estimated_cost * 5.0 + pc.estimated_cost;
            let schema = pc.schema.clone();
            let output_col_ids = pc.output_col_ids.clone();
            Ok((
                PhysicalPlan {
                    plan_id,
                    estimated_rows: pc.estimated_rows,
                    estimated_cost: cost,
                    schema,
                    output_col_ids,
                    op: PhysicalOp::RecursiveCte {
                        name: name.clone(),
                        anchor: Box::new(pa),
                        recursive: Box::new(pr),
                        union_all: *union_all,
                        consumer: Box::new(pc),
                    },
                },
                cs,
            ))
        }
    }
}

fn build_col_index_map(ids: &[ColId]) -> HashMap<ColId, usize> {
    ids.iter().enumerate().map(|(i, &id)| (id, i)).collect()
}

fn extract_equi_pair(
    expr: &LogicalExpr,
    l_map: &HashMap<ColId, usize>,
    r_map: &HashMap<ColId, usize>,
) -> Option<(LogicalExpr, LogicalExpr, bool)> {
    let LogicalExpr::BinaryOp {
        op, left, right, ..
    } = expr
    else {
        return None;
    };
    let null_safe = match op {
        BinaryOperator::Eq => false,
        BinaryOperator::IsNotDistinctFrom => true,
        _ => return None,
    };
    let mut lc = BTreeSet::new();
    let mut rc = BTreeSet::new();
    left.collect_cols(&mut lc);
    right.collect_cols(&mut rc);

    if lc.is_empty() || rc.is_empty() {
        return None;
    }
    let l_in_left = lc.iter().all(|id| l_map.contains_key(id));
    let r_in_right = rc.iter().all(|id| r_map.contains_key(id));
    if l_in_left && r_in_right {
        return Some(((**left).clone(), (**right).clone(), null_safe));
    }
    let l_in_right = lc.iter().all(|id| r_map.contains_key(id));
    let r_in_left = rc.iter().all(|id| l_map.contains_key(id));
    if l_in_right && r_in_left {
        return Some(((**right).clone(), (**left).clone(), null_safe));
    }
    None
}

/// Compiles a `LogicalExpr` referencing `ColId`s into a zero-overhead positional `PhysicalExpr`.
pub fn compile_expr(expr: &LogicalExpr, col_map: &HashMap<ColId, usize>) -> Result<PhysicalExpr> {
    Ok(match expr {
        LogicalExpr::Col {
            id,
            name,
            data_type,
        } => {
            let &index = col_map.get(id).ok_or_else(|| {
                VectraError::PlanError(format!(
                    "column `{name}` (#{}) not found in input schema",
                    id.0
                ))
            })?;
            PhysicalExpr::ColumnRef {
                index,
                name: name.clone(),
                data_type: *data_type,
            }
        }
        LogicalExpr::CorrelatedCol { id, name, .. } => {
            return Err(VectraError::PlanError(format!(
                "unresolved correlated column `{name}` (#{}) reached physical compilation",
                id.0
            )));
        }
        LogicalExpr::Literal { value, data_type } => PhysicalExpr::Literal {
            value: value.clone(),
            data_type: *data_type,
        },
        LogicalExpr::BinaryOp {
            op,
            left,
            right,
            return_type,
        } => PhysicalExpr::BinaryOp {
            op: *op,
            left: Box::new(compile_expr(left, col_map)?),
            right: Box::new(compile_expr(right, col_map)?),
            return_type: *return_type,
        },
        LogicalExpr::UnaryOp {
            op,
            expr: inner,
            return_type,
        } => PhysicalExpr::UnaryOp {
            op: *op,
            expr: Box::new(compile_expr(inner, col_map)?),
            return_type: *return_type,
        },
        LogicalExpr::Cast {
            expr: inner,
            target_type,
            try_cast,
        } => PhysicalExpr::Cast {
            expr: Box::new(compile_expr(inner, col_map)?),
            target_type: *target_type,
            try_cast: *try_cast,
        },
        LogicalExpr::CaseWhen {
            branches,
            else_expr,
            return_type,
        } => {
            let mut pb = Vec::with_capacity(branches.len());
            for (c, t) in branches {
                pb.push((compile_expr(c, col_map)?, compile_expr(t, col_map)?));
            }
            let pe = match else_expr {
                Some(e) => Some(Box::new(compile_expr(e, col_map)?)),
                None => None,
            };
            PhysicalExpr::CaseWhen {
                branches: pb,
                else_expr: pe,
                return_type: *return_type,
            }
        }
        LogicalExpr::Coalesce { args, return_type } => {
            let mut pa = Vec::with_capacity(args.len());
            for a in args {
                pa.push(compile_expr(a, col_map)?);
            }
            PhysicalExpr::Coalesce {
                args: pa,
                return_type: *return_type,
            }
        }
        LogicalExpr::InList {
            expr: target,
            list,
            negated,
        } => {
            let mut pl = Vec::with_capacity(list.len());
            for item in list {
                pl.push(compile_expr(item, col_map)?);
            }
            PhysicalExpr::InList {
                expr: Box::new(compile_expr(target, col_map)?),
                list: pl,
                negated: *negated,
            }
        }
        LogicalExpr::Like {
            expr: target,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => PhysicalExpr::Like {
            expr: Box::new(compile_expr(target, col_map)?),
            pattern: Box::new(compile_expr(pattern, col_map)?),
            escape: *escape,
            case_insensitive: *case_insensitive,
            negated: *negated,
        },
        LogicalExpr::Between {
            expr: target,
            low,
            high,
            negated,
        } => PhysicalExpr::Between {
            expr: Box::new(compile_expr(target, col_map)?),
            low: Box::new(compile_expr(low, col_map)?),
            high: Box::new(compile_expr(high, col_map)?),
            negated: *negated,
        },
        LogicalExpr::ScalarFunc {
            func,
            args,
            return_type,
        } => {
            let mut pa = Vec::with_capacity(args.len());
            for a in args {
                pa.push(compile_expr(a, col_map)?);
            }
            PhysicalExpr::ScalarFunc {
                func: *func,
                args: pa,
                return_type: *return_type,
            }
        }
    })
}

/// Translates pushed-down scan filters into M03 `PruningPredicate` for ZoneMap + Bloom skipping.
fn build_pruning_predicate(
    filters: &[LogicalExpr],
    orig_col_map: &HashMap<ColId, usize>,
) -> Option<PruningPredicate> {
    let mut acc: Option<PruningPredicate> = None;
    for f in filters {
        if let Some(pp) = expr_to_pruning(f, orig_col_map) {
            acc = Some(match acc {
                None => pp,
                Some(prev) => PruningPredicate::And(Box::new(prev), Box::new(pp)),
            });
        }
    }
    acc
}

fn expr_to_pruning(
    expr: &LogicalExpr,
    orig_col_map: &HashMap<ColId, usize>,
) -> Option<PruningPredicate> {
    match expr {
        LogicalExpr::BinaryOp {
            op: BinaryOperator::And,
            left,
            right,
            ..
        } => match (
            expr_to_pruning(left, orig_col_map),
            expr_to_pruning(right, orig_col_map),
        ) {
            (Some(a), Some(b)) => Some(PruningPredicate::And(Box::new(a), Box::new(b))),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        },
        LogicalExpr::BinaryOp {
            op: BinaryOperator::Or,
            left,
            right,
            ..
        } => Some(PruningPredicate::Or(
            Box::new(expr_to_pruning(left, orig_col_map)?),
            Box::new(expr_to_pruning(right, orig_col_map)?),
        )),
        LogicalExpr::UnaryOp {
            op: UnaryOperator::IsNull,
            expr: inner,
            ..
        } => {
            let cid = extract_base_col(inner)?;
            Some(PruningPredicate::IsNull(*orig_col_map.get(&cid)?))
        }
        LogicalExpr::UnaryOp {
            op: UnaryOperator::IsNotNull,
            expr: inner,
            ..
        } => {
            let cid = extract_base_col(inner)?;
            Some(PruningPredicate::IsNotNull(*orig_col_map.get(&cid)?))
        }
        LogicalExpr::Between {
            expr: target,
            low,
            high,
            negated: false,
        } => {
            let cid = extract_base_col(target)?;
            let idx = *orig_col_map.get(&cid)?;
            let lo = extract_literal(low)?.clone();
            let hi = extract_literal(high)?.clone();
            if lo.logical_type() == target.data_type() && hi.logical_type() == target.data_type() {
                Some(PruningPredicate::Between(idx, lo, hi))
            } else {
                None
            }
        }
        LogicalExpr::InList {
            expr: target,
            list,
            negated: false,
        } => {
            let cid = extract_base_col(target)?;
            let idx = *orig_col_map.get(&cid)?;
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                let lit = extract_literal(item)?;
                if lit.logical_type() != target.data_type() {
                    return None;
                }
                vals.push(lit.clone());
            }
            Some(PruningPredicate::In(idx, vals))
        }
        LogicalExpr::BinaryOp {
            op, left, right, ..
        } => {
            let (cid, col_ty, lit, eff_op) = match (extract_base_col(left), extract_literal(right))
            {
                (Some(c), Some(l)) => (c, left.data_type(), l, *op),
                _ => match (extract_literal(left), extract_base_col(right)) {
                    (Some(l), Some(c)) => (c, right.data_type(), l, op.flip()?),
                    _ => return None,
                },
            };
            // Only emit pruning predicate if types match cleanly (avoids mismatched Decimal/Int compares)
            if lit.is_null() || lit.logical_type() != col_ty {
                return None;
            }
            let idx = *orig_col_map.get(&cid)?;
            match eff_op {
                BinaryOperator::Eq => Some(PruningPredicate::Eq(idx, lit.clone())),
                BinaryOperator::NotEq => Some(PruningPredicate::Ne(idx, lit.clone())),
                BinaryOperator::Lt => Some(PruningPredicate::Lt(idx, lit.clone())),
                BinaryOperator::LtEq => Some(PruningPredicate::Le(idx, lit.clone())),
                BinaryOperator::Gt => Some(PruningPredicate::Gt(idx, lit.clone())),
                BinaryOperator::GtEq => Some(PruningPredicate::Ge(idx, lit.clone())),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Ensures `Field`, `LogicalType`, `ScalarValue` imports stay clean.
pub fn dummy_schema_field() -> Field {
    Field::new("_", LogicalType::Null, true)
}
pub fn dummy_null_scalar() -> ScalarValue {
    ScalarValue::Null
}
