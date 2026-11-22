use crate::stats::{ColumnStats, StatsCatalog};
use std::collections::HashMap;
use vectra_expr::{BinaryOperator, UnaryOperator};
use vectra_sql::{ColId, JoinType, LogicalExpr, LogicalPlan};
use vectra_types::ScalarValue;

/// Estimated cardinality and per-column NDV map propagated bottom-up through a plan.
#[derive(Debug, Clone)]
pub struct PlanStats {
    pub rows: f64,
    pub col_stats: HashMap<ColId, ColumnStats>,
}

impl PlanStats {
    pub fn ndv_of(&self, col_id: ColId) -> f64 {
        self.col_stats
            .get(&col_id)
            .map(|c| c.ndv.max(1.0))
            .unwrap_or_else(|| (self.rows * 0.5).max(1.0))
    }
}

/// Estimates row count and column NDVs for any `LogicalPlan` node.
pub fn estimate_plan_stats(plan: &LogicalPlan, stats_catalog: &StatsCatalog) -> PlanStats {
    match plan {
        LogicalPlan::EmptyRelation {
            produce_one_row, ..
        } => PlanStats {
            rows: if *produce_one_row { 1.0 } else { 0.0 },
            col_stats: HashMap::new(),
        },
        LogicalPlan::Values { rows, .. } => PlanStats {
            rows: rows.len() as f64,
            col_stats: HashMap::new(),
        },
        LogicalPlan::TableFunc { func, .. } => {
            let est = match func {
                vectra_sql::TableFunctionKind::GenerateSeries { start, stop, step } => {
                    if *step > 0 && stop >= start {
                        ((stop - start) / step + 1) as f64
                    } else if *step < 0 && start >= stop {
                        ((start - stop) / (-step) + 1) as f64
                    } else {
                        0.0
                    }
                }
                _ => 10_000.0,
            };
            PlanStats {
                rows: est,
                col_stats: HashMap::new(),
            }
        }
        LogicalPlan::CteScan { .. } => PlanStats {
            rows: 1_000.0,
            col_stats: HashMap::new(),
        },
        LogicalPlan::Scan {
            table_name,
            columns,
            table_col_indices,
            pushed_filters,
            ..
        } => {
            let mut rows = 1_000.0;
            let mut col_stats = HashMap::new();
            if let Some(tstats) = stats_catalog.get(table_name) {
                rows = (tstats.row_count as f64).max(1.0);
                for (bc, &tidx) in columns.iter().zip(table_col_indices.iter()) {
                    if let Some(cs) = tstats.columns.get(tidx) {
                        col_stats.insert(bc.id, cs.clone());
                    }
                }
            }
            let mut ps = PlanStats { rows, col_stats };
            if !pushed_filters.is_empty() {
                let sel = estimate_conjunction_selectivity(pushed_filters, &ps);
                ps.rows = (ps.rows * sel).max(1.0);
                scale_ndvs(&mut ps.col_stats, ps.rows);
            }
            ps
        }
        LogicalPlan::Filter { input, predicate } => {
            let mut child = estimate_plan_stats(input, stats_catalog);
            let mut conjuncts = Vec::new();
            split_conjuncts(predicate, &mut conjuncts);
            let sel = estimate_conjunction_selectivity(&conjuncts, &child);
            child.rows = (child.rows * sel).max(1.0);
            scale_ndvs(&mut child.col_stats, child.rows);
            child
        }
        LogicalPlan::Project { input, exprs } => {
            let child = estimate_plan_stats(input, stats_catalog);
            let mut col_stats = HashMap::new();
            for (e, bc) in exprs {
                if let LogicalExpr::Col { id, .. } = e {
                    if let Some(cs) = child.col_stats.get(id) {
                        col_stats.insert(bc.id, cs.clone());
                    }
                }
            }
            PlanStats {
                rows: child.rows,
                col_stats,
            }
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        }
        | LogicalPlan::DependentJoin {
            left,
            right,
            join_type,
            condition,
            ..
        } => {
            let ls = estimate_plan_stats(left, stats_catalog);
            let rs = estimate_plan_stats(right, stats_catalog);
            estimate_join_stats(&ls, &rs, join_type, condition.as_ref())
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            ..
        } => {
            let child = estimate_plan_stats(input, stats_catalog);
            let base_groups = if group_by.is_empty() {
                1.0
            } else {
                let mut prod = 1.0f64;
                for (e, _) in group_by {
                    let ndv = match e {
                        LogicalExpr::Col { id, .. } => child.ndv_of(*id),
                        _ => (child.rows * 0.2).max(1.0),
                    };
                    prod *= ndv;
                }
                // Cap at input row count with gentle correlation dampening
                prod.min(child.rows).max(1.0)
            };
            let mult = grouping_sets
                .as_ref()
                .map_or(1.0, |s| s.len().max(1) as f64);
            let out_rows = (base_groups * mult).max(1.0);
            let mut col_stats = HashMap::new();
            for (e, bc) in group_by {
                if let LogicalExpr::Col { id, .. } = e {
                    if let Some(mut cs) = child.col_stats.get(id).cloned() {
                        cs.ndv = cs.ndv.min(out_rows);
                        col_stats.insert(bc.id, cs);
                    }
                }
            }
            PlanStats {
                rows: out_rows,
                col_stats,
            }
        }
        LogicalPlan::Window { input, .. } | LogicalPlan::Sort { input, .. } => {
            estimate_plan_stats(input, stats_catalog)
        }
        LogicalPlan::Limit { input, limit, .. } => {
            let mut child = estimate_plan_stats(input, stats_catalog);
            if let Some(l) = limit {
                child.rows = child.rows.min(*l as f64);
            }
            child
        }
        LogicalPlan::TopN { input, limit, .. } => {
            let mut child = estimate_plan_stats(input, stats_catalog);
            child.rows = child.rows.min(*limit as f64);
            child
        }
        LogicalPlan::Distinct { input, on_cols } => {
            let child = estimate_plan_stats(input, stats_catalog);
            let mut prod = 1.0f64;
            for c in on_cols {
                prod *= child.ndv_of(c.id);
            }
            let rows = prod.min(child.rows).max(1.0);
            PlanStats {
                rows,
                col_stats: child.col_stats,
            }
        }
        LogicalPlan::SetOp {
            op, left, right, ..
        } => {
            let ls = estimate_plan_stats(left, stats_catalog);
            let rs = estimate_plan_stats(right, stats_catalog);
            let rows = match op {
                vectra_sql::SetOperator::Union => ls.rows + rs.rows,
                vectra_sql::SetOperator::Intersect => ls.rows.min(rs.rows) * 0.5,
                vectra_sql::SetOperator::Except => (ls.rows - rs.rows * 0.5).max(1.0),
            };
            PlanStats {
                rows: rows.max(1.0),
                col_stats: ls.col_stats,
            }
        }
        LogicalPlan::RecursiveCte { consumer, .. } => estimate_plan_stats(consumer, stats_catalog),
    }
}

pub fn estimate_join_stats(
    ls: &PlanStats,
    rs: &PlanStats,
    join_type: &JoinType,
    condition: Option<&LogicalExpr>,
) -> PlanStats {
    let mut combined_cols = ls.col_stats.clone();
    combined_cols.extend(rs.col_stats.clone());

    let out_rows = match join_type {
        JoinType::Cross => (ls.rows * rs.rows).max(1.0),
        JoinType::LeftMark { .. } | JoinType::Single => ls.rows.max(1.0),
        JoinType::LeftSemi | JoinType::LeftAnti => {
            let base = estimate_inner_join_rows(ls, rs, condition);
            base.min(ls.rows).max(1.0)
        }
        JoinType::RightSemi | JoinType::RightAnti => {
            let base = estimate_inner_join_rows(ls, rs, condition);
            base.min(rs.rows).max(1.0)
        }
        JoinType::Inner => estimate_inner_join_rows(ls, rs, condition),
        JoinType::LeftOuter => estimate_inner_join_rows(ls, rs, condition).max(ls.rows),
        JoinType::RightOuter => estimate_inner_join_rows(ls, rs, condition).max(rs.rows),
        JoinType::FullOuter => estimate_inner_join_rows(ls, rs, condition).max(ls.rows + rs.rows),
    };
    scale_ndvs(&mut combined_cols, out_rows);
    PlanStats {
        rows: out_rows.max(1.0),
        col_stats: combined_cols,
    }
}

fn estimate_inner_join_rows(
    ls: &PlanStats,
    rs: &PlanStats,
    condition: Option<&LogicalExpr>,
) -> f64 {
    let Some(cond) = condition else {
        return (ls.rows * rs.rows).max(1.0);
    };
    let mut conjuncts = Vec::new();
    split_conjuncts(cond, &mut conjuncts);

    // Classic System-R / DuckDB containment join cardinality:
    // |R ⋈_{R.a = S.b} S| = (|R| * |S|) / max(NDV(R.a), NDV(S.b))
    let mut equi_denoms = Vec::new();
    let mut residual_sels = Vec::new();

    for c in &conjuncts {
        if let LogicalExpr::BinaryOp {
            op: BinaryOperator::Eq | BinaryOperator::IsNotDistinctFrom,
            left,
            right,
            ..
        } = c
        {
            if let (Some(lid), Some(rid)) = (extract_base_col(left), extract_base_col(right)) {
                let d1 = ls.col_stats.get(&lid).or_else(|| rs.col_stats.get(&lid));
                let d2 = ls.col_stats.get(&rid).or_else(|| rs.col_stats.get(&rid));
                let ndv1 = d1
                    .map(|s| s.ndv)
                    .unwrap_or_else(|| (ls.rows * 0.5).max(1.0));
                let ndv2 = d2
                    .map(|s| s.ndv)
                    .unwrap_or_else(|| (rs.rows * 0.5).max(1.0));
                equi_denoms.push(ndv1.max(ndv2).max(1.0));
                continue;
            }
        }
        residual_sels.push(estimate_predicate_selectivity(c, ls));
    }

    let mut card = ls.rows * rs.rows;
    if !equi_denoms.is_empty() {
        // Sort largest NDV first and apply exponential backoff for multi-column join keys
        equi_denoms.sort_by(|a, b| b.total_cmp(a));
        for (i, d) in equi_denoms.into_iter().enumerate() {
            let eff_d = if i == 0 {
                d
            } else {
                d.powf(0.5f64.powi(i as i32))
            };
            card /= eff_d.max(1.0);
        }
    }
    for s in residual_sels {
        card *= s;
    }
    card.max(1.0)
}

pub fn split_conjuncts<'a>(expr: &'a LogicalExpr, out: &mut Vec<&'a LogicalExpr>) {
    match expr {
        LogicalExpr::BinaryOp {
            op: BinaryOperator::And,
            left,
            right,
            ..
        } => {
            split_conjuncts(left, out);
            split_conjuncts(right, out);
        }
        other => out.push(other),
    }
}

/// Estimates conjunction selectivity with exponential backoff `sel_0 * sqrt(sel_1) * sel_2^0.25 ...`
/// preventing catastrophic underestimation on correlated predicates.
pub fn estimate_conjunction_selectivity<T: std::borrow::Borrow<LogicalExpr>>(
    conjuncts: &[T],
    stats: &PlanStats,
) -> f64 {
    if conjuncts.is_empty() {
        return 1.0;
    }
    let mut sels: Vec<f64> = conjuncts
        .iter()
        .map(|c| estimate_predicate_selectivity(c.borrow(), stats))
        .collect();
    sels.sort_by(|a, b| a.total_cmp(b));
    let mut acc = 1.0f64;
    for (i, s) in sels.into_iter().enumerate() {
        let clamped = s.clamp(1e-6, 1.0);
        let weight = if i == 0 {
            1.0
        } else {
            0.5f64.powi((i as i32).min(4))
        };
        acc *= clamped.powf(weight);
    }
    acc.clamp(1e-6, 1.0)
}

pub fn estimate_predicate_selectivity(expr: &LogicalExpr, stats: &PlanStats) -> f64 {
    match expr {
        LogicalExpr::Literal {
            value: ScalarValue::Boolean(true),
            ..
        } => 1.0,
        LogicalExpr::Literal {
            value: ScalarValue::Boolean(false) | ScalarValue::Null,
            ..
        } => 0.0,
        LogicalExpr::UnaryOp {
            op: UnaryOperator::Not,
            expr: inner,
            ..
        } => (1.0 - estimate_predicate_selectivity(inner, stats)).clamp(0.01, 0.99),
        LogicalExpr::UnaryOp {
            op: UnaryOperator::IsNull,
            expr: inner,
            ..
        } => extract_base_col(inner)
            .and_then(|id| stats.col_stats.get(&id))
            .map(|cs| cs.null_fraction.clamp(0.001, 1.0))
            .unwrap_or(0.05),
        LogicalExpr::UnaryOp {
            op: UnaryOperator::IsNotNull,
            expr: inner,
            ..
        } => extract_base_col(inner)
            .and_then(|id| stats.col_stats.get(&id))
            .map(|cs| (1.0 - cs.null_fraction).clamp(0.001, 1.0))
            .unwrap_or(0.95),
        LogicalExpr::BinaryOp {
            op: BinaryOperator::And,
            ..
        } => {
            let mut cs = Vec::new();
            split_conjuncts(expr, &mut cs);
            estimate_conjunction_selectivity(&cs, stats)
        }
        LogicalExpr::BinaryOp {
            op: BinaryOperator::Or,
            left,
            right,
            ..
        } => {
            let a = estimate_predicate_selectivity(left, stats);
            let b = estimate_predicate_selectivity(right, stats);
            (a + b - a * b).clamp(0.0, 1.0)
        }
        LogicalExpr::BinaryOp {
            op, left, right, ..
        } => {
            // Normalize `lit op col` -> `col flipped_op lit`
            let (col_id, lit, eff_op) = match (extract_base_col(left), extract_literal(right)) {
                (Some(c), Some(l)) => (c, l, *op),
                _ => match (extract_literal(left), extract_base_col(right)) {
                    (Some(l), Some(c)) => match op.flip() {
                        Some(fop) => (c, l, fop),
                        None => return 0.25,
                    },
                    _ => return 0.25,
                },
            };
            if lit.is_null() {
                return 0.0;
            }
            let Some(cs) = stats.col_stats.get(&col_id) else {
                return match eff_op {
                    BinaryOperator::Eq => 0.1,
                    BinaryOperator::NotEq => 0.9,
                    _ => 0.33,
                };
            };
            let non_null = (1.0 - cs.null_fraction).clamp(0.0, 1.0);
            match eff_op {
                BinaryOperator::Eq => {
                    if let (Some(min), Some(max)) = (&cs.min_value, &cs.max_value) {
                        if lit.total_cmp(min).is_lt() || lit.total_cmp(max).is_gt() {
                            return 0.0001;
                        }
                    }
                    (non_null / cs.ndv.max(1.0)).clamp(0.0001, 1.0)
                }
                BinaryOperator::NotEq => {
                    (non_null * (1.0 - 1.0 / cs.ndv.max(1.0))).clamp(0.01, 1.0)
                }
                BinaryOperator::Lt => {
                    cs.histogram.selectivity_lt(lit, false).unwrap_or(0.33) * non_null
                }
                BinaryOperator::LtEq => {
                    cs.histogram.selectivity_lt(lit, true).unwrap_or(0.33) * non_null
                }
                BinaryOperator::Gt => {
                    (1.0 - cs.histogram.selectivity_lt(lit, true).unwrap_or(0.67)) * non_null
                }
                BinaryOperator::GtEq => {
                    (1.0 - cs.histogram.selectivity_lt(lit, false).unwrap_or(0.67)) * non_null
                }
                _ => 0.25,
            }
        }
        LogicalExpr::Between {
            expr: target,
            low,
            high,
            negated,
        } => {
            let base = match (
                extract_base_col(target),
                extract_literal(low),
                extract_literal(high),
            ) {
                (Some(cid), Some(lo), Some(hi)) => stats
                    .col_stats
                    .get(&cid)
                    .and_then(|cs| {
                        cs.histogram
                            .selectivity_between(lo, hi)
                            .map(|f| f * (1.0 - cs.null_fraction))
                    })
                    .unwrap_or(0.25),
                _ => 0.25,
            };
            if *negated {
                (1.0 - base).clamp(0.01, 1.0)
            } else {
                base.clamp(0.0001, 1.0)
            }
        }
        LogicalExpr::InList {
            expr: target,
            list,
            negated,
        } => {
            let k = list.len() as f64;
            let base = extract_base_col(target)
                .and_then(|cid| stats.col_stats.get(&cid))
                .map(|cs| (k / cs.ndv.max(1.0)).min(1.0) * (1.0 - cs.null_fraction))
                .unwrap_or((k * 0.05).min(0.5));
            if *negated {
                (1.0 - base).clamp(0.01, 1.0)
            } else {
                base.clamp(0.0001, 1.0)
            }
        }
        LogicalExpr::Like { negated, .. } => {
            if *negated {
                0.85
            } else {
                0.15
            }
        }
        _ => 0.25,
    }
}

pub fn extract_base_col(expr: &LogicalExpr) -> Option<ColId> {
    match expr {
        LogicalExpr::Col { id, .. } => Some(*id),
        LogicalExpr::Cast { expr: inner, .. } => extract_base_col(inner),
        _ => None,
    }
}

pub fn extract_literal(expr: &LogicalExpr) -> Option<&ScalarValue> {
    match expr {
        LogicalExpr::Literal { value, .. } => Some(value),
        LogicalExpr::Cast { expr: inner, .. } => extract_literal(inner),
        _ => None,
    }
}

fn scale_ndvs(col_stats: &mut HashMap<ColId, ColumnStats>, new_rows: f64) {
    for cs in col_stats.values_mut() {
        cs.ndv = cs.ndv.min(new_rows.max(1.0));
    }
}
