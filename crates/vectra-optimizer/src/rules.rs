//! Multi-pass heuristic logical rewrite rules:
//! 1. Constant folding & algebraic simplification + disjunctive common-factor extraction (TPC-H Q19!)
//! 2. `LeftMark` -> `LeftSemi` / `LeftAnti` simplification (NULL-safe)
//! 3. Outer-to-Inner join simplification under null-rejecting predicates
//! 4. Predicate pushdown across Project, Aggregate, Join, and leaf Scan::pushed_filters
//! 5. Projection pruning (top-down required ColId bitset dropping unused scan columns!)
//! 6. Limit + Sort -> TopN fusion & Limit pushdown through UNION ALL

use std::collections::{BTreeSet, HashMap};
use vectra_expr::{eval_expr_scalar_reference, BinaryOperator, PhysicalExpr, UnaryOperator};
use vectra_sql::{BoundCol, ColId, JoinType, LogicalExpr, LogicalPlan, SetOperator};
use vectra_types::{LogicalType, ScalarValue};

/// Runs the complete fixed-point logical rewrite pipeline before cost-based join reordering.
pub fn apply_logical_rewrites(mut plan: LogicalPlan) -> LogicalPlan {
    plan = fold_constants_plan(plan);
    plan = simplify_mark_and_outer_joins(plan);
    plan = pushdown_predicates(plan, Vec::new());
    plan = fold_constants_plan(plan);
    plan = simplify_mark_and_outer_joins(plan);
    plan = pushdown_predicates(plan, Vec::new());
    plan = pushdown_limit_topn(plan);
    let required: BTreeSet<ColId> = plan.output_cols().iter().map(|c| c.id).collect();
    plan = prune_projections(plan, &required);
    plan
}

// ============================================================================
// Pass 1: Constant Folding, Algebraic Simplification & Disjunctive Factoring
// ============================================================================

pub fn fold_constants_plan(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::EmptyRelation { .. }
        | LogicalPlan::TableFunc { .. }
        | LogicalPlan::CteScan { .. } => plan,
        LogicalPlan::Values { rows, columns } => LogicalPlan::Values {
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(simplify_expr).collect())
                .collect(),
            columns,
        },
        LogicalPlan::Scan {
            table_name,
            alias,
            columns,
            table_col_indices,
            pushed_filters,
        } => {
            let mut new_filters = Vec::new();
            for f in pushed_filters {
                let sf = simplify_expr(f);
                if matches!(
                    sf,
                    LogicalExpr::Literal {
                        value: ScalarValue::Boolean(false) | ScalarValue::Null,
                        ..
                    }
                ) {
                    return LogicalPlan::EmptyRelation {
                        produce_one_row: false,
                        columns,
                    };
                }
                if !matches!(
                    sf,
                    LogicalExpr::Literal {
                        value: ScalarValue::Boolean(true),
                        ..
                    }
                ) {
                    split_and_owned(sf, &mut new_filters);
                }
            }
            LogicalPlan::Scan {
                table_name,
                alias,
                columns,
                table_col_indices,
                pushed_filters: new_filters,
            }
        }
        LogicalPlan::Filter { input, predicate } => {
            let inp = fold_constants_plan(*input);
            let pred = simplify_expr(predicate);
            match pred {
                LogicalExpr::Literal {
                    value: ScalarValue::Boolean(true),
                    ..
                } => inp,
                LogicalExpr::Literal {
                    value: ScalarValue::Boolean(false) | ScalarValue::Null,
                    ..
                } => LogicalPlan::EmptyRelation {
                    produce_one_row: false,
                    columns: inp.output_cols(),
                },
                other => LogicalPlan::Filter {
                    input: Box::new(inp),
                    predicate: other,
                },
            }
        }
        LogicalPlan::Project { input, exprs } => {
            let inp = fold_constants_plan(*input);
            let exprs: Vec<(LogicalExpr, BoundCol)> = exprs
                .into_iter()
                .map(|(e, c)| (simplify_expr(e), c))
                .collect();
            LogicalPlan::Project {
                input: Box::new(inp),
                exprs,
            }
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        } => {
            let l = fold_constants_plan(*left);
            let r = fold_constants_plan(*right);
            let cond = condition.map(simplify_expr).filter(|c| {
                !matches!(
                    c,
                    LogicalExpr::Literal {
                        value: ScalarValue::Boolean(true),
                        ..
                    }
                )
            });
            let jt = match join_type {
                JoinType::LeftMark {
                    mark_col,
                    probe_pair,
                } => JoinType::LeftMark {
                    mark_col,
                    probe_pair: probe_pair.map(|(a, b)| (simplify_expr(a), simplify_expr(b))),
                },
                other => other,
            };
            LogicalPlan::Join {
                left: Box::new(l),
                right: Box::new(r),
                join_type: jt,
                condition: cond,
            }
        }
        LogicalPlan::DependentJoin {
            left,
            right,
            join_type,
            condition,
            correlated_cols,
        } => LogicalPlan::DependentJoin {
            left: Box::new(fold_constants_plan(*left)),
            right: Box::new(fold_constants_plan(*right)),
            join_type,
            condition: condition.map(simplify_expr),
            correlated_cols,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        } => LogicalPlan::Aggregate {
            input: Box::new(fold_constants_plan(*input)),
            group_by: group_by
                .into_iter()
                .map(|(e, c)| (simplify_expr(e), c))
                .collect(),
            grouping_sets,
            grouping_id_col,
            aggregates: aggregates
                .into_iter()
                .map(|(mut a, c)| {
                    a.args = a.args.into_iter().map(simplify_expr).collect();
                    a.filter = a.filter.map(simplify_expr);
                    (a, c)
                })
                .collect(),
        },
        LogicalPlan::Window { input, windows } => LogicalPlan::Window {
            input: Box::new(fold_constants_plan(*input)),
            windows: windows
                .into_iter()
                .map(|(mut w, c)| {
                    w.args = w.args.into_iter().map(simplify_expr).collect();
                    w.partition_by = w.partition_by.into_iter().map(simplify_expr).collect();
                    for o in &mut w.order_by {
                        o.expr = simplify_expr(o.expr.clone());
                    }
                    (w, c)
                })
                .collect(),
        },
        LogicalPlan::Sort {
            input,
            mut order_by,
        } => {
            for o in &mut order_by {
                o.expr = simplify_expr(o.expr.clone());
            }
            LogicalPlan::Sort {
                input: Box::new(fold_constants_plan(*input)),
                order_by,
            }
        }
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(fold_constants_plan(*input)),
            limit,
            offset,
        },
        LogicalPlan::TopN {
            input,
            mut order_by,
            limit,
            offset,
        } => {
            for o in &mut order_by {
                o.expr = simplify_expr(o.expr.clone());
            }
            LogicalPlan::TopN {
                input: Box::new(fold_constants_plan(*input)),
                order_by,
                limit,
                offset,
            }
        }
        LogicalPlan::Distinct { input, on_cols } => LogicalPlan::Distinct {
            input: Box::new(fold_constants_plan(*input)),
            on_cols,
        },
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
            output_cols,
        } => LogicalPlan::SetOp {
            op,
            all,
            left: Box::new(fold_constants_plan(*left)),
            right: Box::new(fold_constants_plan(*right)),
            output_cols,
        },
        LogicalPlan::RecursiveCte {
            name,
            anchor,
            recursive,
            union_all,
            cte_cols,
            consumer,
        } => LogicalPlan::RecursiveCte {
            name,
            anchor: Box::new(fold_constants_plan(*anchor)),
            recursive: Box::new(fold_constants_plan(*recursive)),
            union_all,
            cte_cols,
            consumer: Box::new(fold_constants_plan(*consumer)),
        },
    }
}

pub fn simplify_expr(expr: LogicalExpr) -> LogicalExpr {
    let folded = match expr {
        LogicalExpr::Col { .. }
        | LogicalExpr::CorrelatedCol { .. }
        | LogicalExpr::Literal { .. } => expr,
        LogicalExpr::UnaryOp {
            op,
            expr: inner,
            return_type,
        } => {
            let inner = simplify_expr(*inner);
            // NOT (NOT x) -> x
            if op == UnaryOperator::Not {
                if let LogicalExpr::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: dbl,
                    ..
                } = inner
                {
                    return *dbl;
                }
            }
            LogicalExpr::UnaryOp {
                op,
                expr: Box::new(inner),
                return_type,
            }
        }
        LogicalExpr::Cast {
            expr: inner,
            target_type,
            try_cast,
        } => {
            let inner = simplify_expr(*inner);
            if inner.data_type() == target_type {
                return inner;
            }
            LogicalExpr::Cast {
                expr: Box::new(inner),
                target_type,
                try_cast,
            }
        }
        LogicalExpr::BinaryOp {
            op,
            left,
            right,
            return_type,
        } => {
            let l = simplify_expr(*left);
            let r = simplify_expr(*right);
            match op {
                BinaryOperator::And => {
                    if is_bool_lit(&l, true) {
                        return r;
                    }
                    if is_bool_lit(&r, true) {
                        return l;
                    }
                    if is_bool_lit(&l, false) || is_bool_lit(&r, false) {
                        return bool_lit(false);
                    }
                }
                BinaryOperator::Or => {
                    if is_bool_lit(&l, false) {
                        return r;
                    }
                    if is_bool_lit(&r, false) {
                        return l;
                    }
                    if is_bool_lit(&l, true) || is_bool_lit(&r, true) {
                        return bool_lit(true);
                    }
                    // Extract common conjunctive factors out of OR branches (e.g., TPC-H Q19!)
                    let combined = LogicalExpr::BinaryOp {
                        op: BinaryOperator::Or,
                        left: Box::new(l),
                        right: Box::new(r),
                        return_type,
                    };
                    return factor_common_disjunctive_conjuncts(combined);
                }
                BinaryOperator::Add => {
                    if is_zero_lit(&r) && l.data_type() == return_type {
                        return l;
                    }
                    if is_zero_lit(&l) && r.data_type() == return_type {
                        return r;
                    }
                }
                BinaryOperator::Sub => {
                    if is_zero_lit(&r) && l.data_type() == return_type {
                        return l;
                    }
                }
                BinaryOperator::Mul => {
                    if is_one_lit(&r) && l.data_type() == return_type {
                        return l;
                    }
                    if is_one_lit(&l) && r.data_type() == return_type {
                        return r;
                    }
                }
                _ => {}
            }
            LogicalExpr::BinaryOp {
                op,
                left: Box::new(l),
                right: Box::new(r),
                return_type,
            }
        }
        LogicalExpr::CaseWhen {
            branches,
            else_expr,
            return_type,
        } => LogicalExpr::CaseWhen {
            branches: branches
                .into_iter()
                .map(|(c, t)| (simplify_expr(c), simplify_expr(t)))
                .collect(),
            else_expr: else_expr.map(|e| Box::new(simplify_expr(*e))),
            return_type,
        },
        LogicalExpr::Coalesce { args, return_type } => LogicalExpr::Coalesce {
            args: args.into_iter().map(simplify_expr).collect(),
            return_type,
        },
        LogicalExpr::InList {
            expr: target,
            list,
            negated,
        } => LogicalExpr::InList {
            expr: Box::new(simplify_expr(*target)),
            list: list.into_iter().map(simplify_expr).collect(),
            negated,
        },
        LogicalExpr::Like {
            expr: target,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => LogicalExpr::Like {
            expr: Box::new(simplify_expr(*target)),
            pattern: Box::new(simplify_expr(*pattern)),
            escape,
            case_insensitive,
            negated,
        },
        LogicalExpr::Between {
            expr: target,
            low,
            high,
            negated,
        } => LogicalExpr::Between {
            expr: Box::new(simplify_expr(*target)),
            low: Box::new(simplify_expr(*low)),
            high: Box::new(simplify_expr(*high)),
            negated,
        },
        LogicalExpr::ScalarFunc {
            func,
            args,
            return_type,
        } => LogicalExpr::ScalarFunc {
            func,
            args: args.into_iter().map(simplify_expr).collect(),
            return_type,
        },
    };

    // Constant evaluation: if expression references zero columns, fold it via `vectra-expr`!
    if !matches!(folded, LogicalExpr::Literal { .. }) && is_constant_expr(&folded) {
        if let Some(phys) = try_const_to_physical(&folded) {
            if let Ok(val) = eval_expr_scalar_reference(&phys, &[]) {
                let ty = if val.is_null() {
                    folded.data_type()
                } else {
                    val.logical_type()
                };
                return LogicalExpr::Literal {
                    value: val,
                    data_type: ty,
                };
            }
        }
    }
    folded
}

/// Factors identical conjuncts shared across all branches of an `OR` tree:
/// `(A AND B) OR (A AND C)` -> `A AND (B OR C)`.
/// Crucial for turning TPC-H Q19's 3-way OR over `p_partkey = l_partkey` into an Equi Hash Join!
fn factor_common_disjunctive_conjuncts(or_expr: LogicalExpr) -> LogicalExpr {
    let mut disjuncts = Vec::new();
    split_or_owned(or_expr, &mut disjuncts);
    if disjuncts.len() < 2 {
        return disjuncts.pop().unwrap();
    }

    let branch_conjuncts: Vec<Vec<LogicalExpr>> = disjuncts
        .into_iter()
        .map(|d| {
            let mut cs = Vec::new();
            split_and_owned(d, &mut cs);
            cs
        })
        .collect();

    let mut common = Vec::new();
    for cand in &branch_conjuncts[0] {
        if !common.contains(cand)
            && branch_conjuncts[1..]
                .iter()
                .all(|other| other.contains(cand))
        {
            common.push(cand.clone());
        }
    }

    if common.is_empty() {
        return combine_or(branch_conjuncts.into_iter().map(combine_and).collect());
    }

    let mut remaining_branches = Vec::with_capacity(branch_conjuncts.len());
    for bc in branch_conjuncts {
        let rem: Vec<LogicalExpr> = bc.into_iter().filter(|e| !common.contains(e)).collect();
        if rem.is_empty() {
            // One branch reduced to TRUE -> whole OR reduces to TRUE -> just `common`!
            return combine_and(common);
        }
        remaining_branches.push(combine_and(rem));
    }

    let mut all = common;
    all.push(combine_or(remaining_branches));
    combine_and(all)
}

fn is_constant_expr(expr: &LogicalExpr) -> bool {
    let mut cols = BTreeSet::new();
    let mut corr = BTreeSet::new();
    expr.collect_cols(&mut cols);
    expr.collect_correlated_cols(&mut corr);
    cols.is_empty() && corr.is_empty()
}

fn try_const_to_physical(expr: &LogicalExpr) -> Option<PhysicalExpr> {
    Some(match expr {
        LogicalExpr::Literal { value, data_type } => PhysicalExpr::Literal {
            value: value.clone(),
            data_type: *data_type,
        },
        LogicalExpr::UnaryOp {
            op,
            expr,
            return_type,
        } => PhysicalExpr::UnaryOp {
            op: *op,
            expr: Box::new(try_const_to_physical(expr)?),
            return_type: *return_type,
        },
        LogicalExpr::BinaryOp {
            op,
            left,
            right,
            return_type,
        } => PhysicalExpr::BinaryOp {
            op: *op,
            left: Box::new(try_const_to_physical(left)?),
            right: Box::new(try_const_to_physical(right)?),
            return_type: *return_type,
        },
        LogicalExpr::Cast {
            expr,
            target_type,
            try_cast,
        } => PhysicalExpr::Cast {
            expr: Box::new(try_const_to_physical(expr)?),
            target_type: *target_type,
            try_cast: *try_cast,
        },
        LogicalExpr::ScalarFunc {
            func,
            args,
            return_type,
        } => {
            let mut pargs = Vec::with_capacity(args.len());
            for a in args {
                pargs.push(try_const_to_physical(a)?);
            }
            PhysicalExpr::ScalarFunc {
                func: *func,
                args: pargs,
                return_type: *return_type,
            }
        }
        _ => return None,
    })
}

fn is_bool_lit(e: &LogicalExpr, expected: bool) -> bool {
    matches!(
        e,
        LogicalExpr::Literal {
            value: ScalarValue::Boolean(b),
            ..
        } if *b == expected
    )
}

fn is_zero_lit(e: &LogicalExpr) -> bool {
    matches!(
        e,
        LogicalExpr::Literal {
            value: ScalarValue::Int64(0) | ScalarValue::Int32(0),
            ..
        }
    )
}

fn is_one_lit(e: &LogicalExpr) -> bool {
    matches!(
        e,
        LogicalExpr::Literal {
            value: ScalarValue::Int64(1) | ScalarValue::Int32(1),
            ..
        }
    )
}

fn bool_lit(v: bool) -> LogicalExpr {
    LogicalExpr::Literal {
        value: ScalarValue::Boolean(v),
        data_type: LogicalType::Boolean,
    }
}

// ============================================================================
// Pass 2 & 3: MarkJoin -> Semi/Anti & Outer-to-Inner Join Simplification
// ============================================================================

fn simplify_mark_and_outer_joins(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            let child = simplify_mark_and_outer_joins(*input);
            let mut conjuncts = Vec::new();
            split_and_owned(predicate, &mut conjuncts);

            match child {
                // Convert `Filter(mark_col, LeftMarkJoin)` -> `LeftSemiJoin` / `LeftAntiJoin`!
                LogicalPlan::Join {
                    left,
                    right,
                    join_type:
                        JoinType::LeftMark {
                            mark_col,
                            probe_pair,
                        },
                    condition,
                } => {
                    let mut pos_idx = None;
                    let mut neg_idx = None;
                    for (i, c) in conjuncts.iter().enumerate() {
                        if matches!(c, LogicalExpr::Col { id, .. } if *id == mark_col.id) {
                            pos_idx = Some(i);
                            break;
                        }
                        if matches!(
                            c,
                            LogicalExpr::UnaryOp {
                                op: UnaryOperator::Not,
                                expr,
                                ..
                            } if matches!(expr.as_ref(), LogicalExpr::Col { id, .. } if *id == mark_col.id)
                        ) {
                            neg_idx = Some(i);
                            break;
                        }
                    }

                    // Check mark_col is not referenced in remaining conjuncts
                    if let Some(pi) = pos_idx {
                        let mut rem = conjuncts.clone();
                        rem.remove(pi);
                        if !exprs_reference_col(&rem, mark_col.id) {
                            let mut new_cond = condition;
                            if let Some((pl, pr)) = probe_pair {
                                let eq = LogicalExpr::BinaryOp {
                                    op: BinaryOperator::Eq,
                                    left: Box::new(pl),
                                    right: Box::new(pr),
                                    return_type: LogicalType::Boolean,
                                };
                                new_cond = combine_opt_and(new_cond, eq);
                            }
                            let semi = LogicalPlan::Join {
                                left,
                                right,
                                join_type: JoinType::LeftSemi,
                                condition: new_cond,
                            };
                            return wrap_filter_if_nonempty(semi, rem);
                        }
                    } else if let Some(ni) = neg_idx {
                        // EXISTS-style `NOT EXISTS` (probe_pair == None) safely converts to LeftAnti!
                        // (For `NOT IN` with probe_pair, keep LeftMark to preserve 3-valued NULL semantics!)
                        if probe_pair.is_none() {
                            let mut rem = conjuncts.clone();
                            rem.remove(ni);
                            if !exprs_reference_col(&rem, mark_col.id) {
                                let anti = LogicalPlan::Join {
                                    left,
                                    right,
                                    join_type: JoinType::LeftAnti,
                                    condition,
                                };
                                return wrap_filter_if_nonempty(anti, rem);
                            }
                        }
                    }

                    wrap_filter_if_nonempty(
                        LogicalPlan::Join {
                            left,
                            right,
                            join_type: JoinType::LeftMark {
                                mark_col,
                                probe_pair,
                            },
                            condition,
                        },
                        conjuncts,
                    )
                }

                // Outer-to-Inner Join Simplification under null-rejecting WHERE predicates!
                LogicalPlan::Join {
                    left,
                    right,
                    join_type,
                    condition,
                } => {
                    let l_ids = left.output_col_ids();
                    let r_ids = right.output_col_ids();
                    let rejects_left = conjuncts.iter().any(|c| is_null_rejecting(c, &l_ids));
                    let rejects_right = conjuncts.iter().any(|c| is_null_rejecting(c, &r_ids));

                    let new_jt = match join_type {
                        JoinType::LeftOuter if rejects_right => JoinType::Inner,
                        JoinType::RightOuter if rejects_left => JoinType::Inner,
                        JoinType::FullOuter if rejects_left && rejects_right => JoinType::Inner,
                        JoinType::FullOuter if rejects_left => JoinType::LeftOuter,
                        JoinType::FullOuter if rejects_right => JoinType::RightOuter,
                        other => other,
                    };
                    wrap_filter_if_nonempty(
                        LogicalPlan::Join {
                            left,
                            right,
                            join_type: new_jt,
                            condition,
                        },
                        conjuncts,
                    )
                }

                other => wrap_filter_if_nonempty(other, conjuncts),
            }
        }
        other => map_plan_children(other, simplify_mark_and_outer_joins),
    }
}

/// Conservative null-rejection check: returns true if `expr` CANNOT evaluate to TRUE
/// whenever all columns in `nullable_side` are NULL.
fn is_null_rejecting(expr: &LogicalExpr, nullable_side: &BTreeSet<ColId>) -> bool {
    match expr {
        LogicalExpr::UnaryOp {
            op: UnaryOperator::IsNotNull,
            expr: inner,
            ..
        } => {
            let mut cols = BTreeSet::new();
            inner.collect_cols(&mut cols);
            !cols.is_empty() && cols.iter().all(|c| nullable_side.contains(c))
        }
        LogicalExpr::BinaryOp {
            op, left, right, ..
        } => match op {
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq => {
                is_strict_on(left, nullable_side) || is_strict_on(right, nullable_side)
            }
            BinaryOperator::And => {
                is_null_rejecting(left, nullable_side) || is_null_rejecting(right, nullable_side)
            }
            BinaryOperator::Or => {
                is_null_rejecting(left, nullable_side) && is_null_rejecting(right, nullable_side)
            }
            _ => false,
        },
        LogicalExpr::Like { expr: inner, .. }
        | LogicalExpr::Between { expr: inner, .. }
        | LogicalExpr::InList { expr: inner, .. } => is_strict_on(inner, nullable_side),
        _ => false,
    }
}

fn is_strict_on(expr: &LogicalExpr, nullable_side: &BTreeSet<ColId>) -> bool {
    match expr {
        LogicalExpr::Col { id, .. } => nullable_side.contains(id),
        LogicalExpr::Cast { expr: inner, .. } => is_strict_on(inner, nullable_side),
        LogicalExpr::BinaryOp {
            op, left, right, ..
        } if op.is_arithmetic() => {
            is_strict_on(left, nullable_side) || is_strict_on(right, nullable_side)
        }
        _ => false,
    }
}

// ============================================================================
// Pass 4: Predicate Pushdown (through Project, Aggregate, Join, into Scan)
// ============================================================================

fn pushdown_predicates(plan: LogicalPlan, mut pending: Vec<LogicalExpr>) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            split_and_owned(predicate, &mut pending);
            pushdown_predicates(*input, pending)
        }

        LogicalPlan::Scan {
            table_name,
            alias,
            columns,
            table_col_indices,
            mut pushed_filters,
        } => {
            let scan_ids: BTreeSet<ColId> = columns.iter().map(|c| c.id).collect();
            let mut remaining = Vec::new();
            for p in pending {
                let mut refs = BTreeSet::new();
                p.collect_cols(&mut refs);
                if refs.is_subset(&scan_ids) {
                    pushed_filters.push(p);
                } else {
                    remaining.push(p);
                }
            }
            let scan = LogicalPlan::Scan {
                table_name,
                alias,
                columns,
                table_col_indices,
                pushed_filters,
            };
            wrap_filter_if_nonempty(scan, remaining)
        }

        LogicalPlan::Project { input, exprs } => {
            // Substitute projected expressions into pending predicates so filters push BELOW Project!
            let subst: HashMap<ColId, LogicalExpr> =
                exprs.iter().map(|(e, c)| (c.id, e.clone())).collect();
            let mut push_down = Vec::new();
            let mut stay_above = Vec::new();
            for p in pending {
                if let Some(rewritten) = substitute_cols(&p, &subst) {
                    push_down.push(rewritten);
                } else {
                    stay_above.push(p);
                }
            }
            let child = pushdown_predicates(*input, push_down);
            let proj = LogicalPlan::Project {
                input: Box::new(child),
                exprs,
            };
            wrap_filter_if_nonempty(proj, stay_above)
        }

        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        } => {
            // Push predicates referencing ONLY GROUP BY columns below Aggregate (when no grouping_sets)!
            let mut push_down = Vec::new();
            let mut stay_above = Vec::new();
            if grouping_sets.is_none() {
                let gmap: HashMap<ColId, LogicalExpr> =
                    group_by.iter().map(|(e, c)| (c.id, e.clone())).collect();
                for p in pending {
                    let mut refs = BTreeSet::new();
                    p.collect_cols(&mut refs);
                    if !refs.is_empty() && refs.iter().all(|id| gmap.contains_key(id)) {
                        if let Some(rewritten) = substitute_cols(&p, &gmap) {
                            push_down.push(rewritten);
                            continue;
                        }
                    }
                    stay_above.push(p);
                }
            } else {
                stay_above = pending;
            }
            let child = pushdown_predicates(*input, push_down);
            let agg = LogicalPlan::Aggregate {
                input: Box::new(child),
                group_by,
                grouping_sets,
                grouping_id_col,
                aggregates,
            };
            wrap_filter_if_nonempty(agg, stay_above)
        }

        LogicalPlan::Join {
            left,
            right,
            mut join_type,
            condition,
        } => {
            let l_ids = left.output_col_ids();
            let r_ids = right.output_col_ids();

            let mut push_left = Vec::new();
            let mut push_right = Vec::new();
            let mut join_conds = Vec::new();
            let mut stay_above = Vec::new();

            // First classify predicates coming from ABOVE the join (WHERE clause)
            for p in pending {
                let mut refs = BTreeSet::new();
                p.collect_cols(&mut refs);
                let uses_l = refs.iter().any(|id| l_ids.contains(id));
                let uses_r = refs.iter().any(|id| r_ids.contains(id));

                match join_type {
                    JoinType::Inner | JoinType::Cross => {
                        if uses_l && !uses_r {
                            push_left.push(p);
                        } else if uses_r && !uses_l {
                            push_right.push(p);
                        } else if uses_l && uses_r {
                            // Upgrades CrossJoin -> InnerJoin!
                            join_type = JoinType::Inner;
                            join_conds.push(p);
                        } else {
                            stay_above.push(p);
                        }
                    }
                    JoinType::LeftOuter
                    | JoinType::LeftSemi
                    | JoinType::LeftAnti
                    | JoinType::LeftMark { .. }
                    | JoinType::Single => {
                        if uses_l && !uses_r && refs.iter().all(|id| l_ids.contains(id)) {
                            push_left.push(p);
                        } else {
                            stay_above.push(p);
                        }
                    }
                    JoinType::RightOuter | JoinType::RightSemi | JoinType::RightAnti => {
                        if uses_r && !uses_l && refs.iter().all(|id| r_ids.contains(id)) {
                            push_right.push(p);
                        } else {
                            stay_above.push(p);
                        }
                    }
                    JoinType::FullOuter => stay_above.push(p),
                }
            }

            // Now classify existing ON condition conjuncts
            if let Some(cond) = condition {
                let mut on_parts = Vec::new();
                split_and_owned(cond, &mut on_parts);
                for p in on_parts {
                    let mut refs = BTreeSet::new();
                    p.collect_cols(&mut refs);
                    let uses_l = refs.iter().any(|id| l_ids.contains(id));
                    let uses_r = refs.iter().any(|id| r_ids.contains(id));

                    match join_type {
                        JoinType::Inner | JoinType::Cross => {
                            if uses_l && !uses_r {
                                push_left.push(p);
                            } else if uses_r && !uses_l {
                                push_right.push(p);
                            } else {
                                join_conds.push(p);
                            }
                        }
                        JoinType::LeftOuter | JoinType::LeftSemi | JoinType::LeftAnti => {
                            // ON filter referencing ONLY right side can safely push into right!
                            if uses_r && !uses_l {
                                push_right.push(p);
                            } else {
                                join_conds.push(p);
                            }
                        }
                        JoinType::RightOuter | JoinType::RightSemi | JoinType::RightAnti => {
                            if uses_l && !uses_r {
                                push_left.push(p);
                            } else {
                                join_conds.push(p);
                            }
                        }
                        _ => join_conds.push(p),
                    }
                }
            }

            let new_l = pushdown_predicates(*left, push_left);
            let new_r = pushdown_predicates(*right, push_right);
            let new_cond = if join_conds.is_empty() {
                None
            } else {
                Some(combine_and(join_conds))
            };
            let joined = LogicalPlan::Join {
                left: Box::new(new_l),
                right: Box::new(new_r),
                join_type,
                condition: new_cond,
            };
            wrap_filter_if_nonempty(joined, stay_above)
        }

        LogicalPlan::Sort { input, order_by } => {
            let child = pushdown_predicates(*input, pending);
            LogicalPlan::Sort {
                input: Box::new(child),
                order_by,
            }
        }

        LogicalPlan::Distinct { input, on_cols } => {
            let child = pushdown_predicates(*input, pending);
            LogicalPlan::Distinct {
                input: Box::new(child),
                on_cols,
            }
        }

        other => {
            let rewritten = map_plan_children(other, |c| pushdown_predicates(c, Vec::new()));
            wrap_filter_if_nonempty(rewritten, pending)
        }
    }
}

fn substitute_cols(expr: &LogicalExpr, subst: &HashMap<ColId, LogicalExpr>) -> Option<LogicalExpr> {
    Some(match expr {
        LogicalExpr::Col { id, .. } => subst.get(id)?.clone(),
        LogicalExpr::CorrelatedCol { .. } | LogicalExpr::Literal { .. } => expr.clone(),
        LogicalExpr::UnaryOp {
            op,
            expr,
            return_type,
        } => LogicalExpr::UnaryOp {
            op: *op,
            expr: Box::new(substitute_cols(expr, subst)?),
            return_type: *return_type,
        },
        LogicalExpr::BinaryOp {
            op,
            left,
            right,
            return_type,
        } => LogicalExpr::BinaryOp {
            op: *op,
            left: Box::new(substitute_cols(left, subst)?),
            right: Box::new(substitute_cols(right, subst)?),
            return_type: *return_type,
        },
        LogicalExpr::Cast {
            expr,
            target_type,
            try_cast,
        } => LogicalExpr::Cast {
            expr: Box::new(substitute_cols(expr, subst)?),
            target_type: *target_type,
            try_cast: *try_cast,
        },
        LogicalExpr::CaseWhen {
            branches,
            else_expr,
            return_type,
        } => {
            let mut nb = Vec::with_capacity(branches.len());
            for (c, t) in branches {
                nb.push((substitute_cols(c, subst)?, substitute_cols(t, subst)?));
            }
            let ne = match else_expr {
                Some(e) => Some(Box::new(substitute_cols(e, subst)?)),
                None => None,
            };
            LogicalExpr::CaseWhen {
                branches: nb,
                else_expr: ne,
                return_type: *return_type,
            }
        }
        LogicalExpr::Coalesce { args, return_type } => {
            let mut na = Vec::with_capacity(args.len());
            for a in args {
                na.push(substitute_cols(a, subst)?);
            }
            LogicalExpr::Coalesce {
                args: na,
                return_type: *return_type,
            }
        }
        LogicalExpr::InList {
            expr,
            list,
            negated,
        } => {
            let mut nl = Vec::with_capacity(list.len());
            for item in list {
                nl.push(substitute_cols(item, subst)?);
            }
            LogicalExpr::InList {
                expr: Box::new(substitute_cols(expr, subst)?),
                list: nl,
                negated: *negated,
            }
        }
        LogicalExpr::Like {
            expr,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => LogicalExpr::Like {
            expr: Box::new(substitute_cols(expr, subst)?),
            pattern: Box::new(substitute_cols(pattern, subst)?),
            escape: *escape,
            case_insensitive: *case_insensitive,
            negated: *negated,
        },
        LogicalExpr::Between {
            expr,
            low,
            high,
            negated,
        } => LogicalExpr::Between {
            expr: Box::new(substitute_cols(expr, subst)?),
            low: Box::new(substitute_cols(low, subst)?),
            high: Box::new(substitute_cols(high, subst)?),
            negated: *negated,
        },
        LogicalExpr::ScalarFunc {
            func,
            args,
            return_type,
        } => {
            let mut na = Vec::with_capacity(args.len());
            for a in args {
                na.push(substitute_cols(a, subst)?);
            }
            LogicalExpr::ScalarFunc {
                func: *func,
                args: na,
                return_type: *return_type,
            }
        }
    })
}

// ============================================================================
// Pass 5: Projection Pruning (Top-Down Required ColId Propagation)
// ============================================================================

pub fn prune_projections(plan: LogicalPlan, required: &BTreeSet<ColId>) -> LogicalPlan {
    match plan {
        LogicalPlan::Scan {
            table_name,
            alias,
            columns,
            table_col_indices,
            pushed_filters,
        } => {
            let mut needed = required.clone();
            for f in &pushed_filters {
                f.collect_cols(&mut needed);
            }
            let mut new_cols = Vec::new();
            let mut new_indices = Vec::new();
            for (c, idx) in columns.into_iter().zip(table_col_indices) {
                if needed.contains(&c.id) {
                    new_cols.push(c);
                    new_indices.push(idx);
                }
            }
            // Keep at least 1 column for COUNT(*) scans if every column was unreferenced.
            LogicalPlan::Scan {
                table_name,
                alias,
                columns: new_cols,
                table_col_indices: new_indices,
                pushed_filters,
            }
        }

        LogicalPlan::Filter { input, predicate } => {
            let mut child_req = required.clone();
            predicate.collect_cols(&mut child_req);
            LogicalPlan::Filter {
                input: Box::new(prune_projections(*input, &child_req)),
                predicate,
            }
        }

        LogicalPlan::Project { input, exprs } => {
            let kept: Vec<(LogicalExpr, BoundCol)> = exprs
                .into_iter()
                .filter(|(_, c)| required.contains(&c.id))
                .collect();
            let mut child_req = BTreeSet::new();
            for (e, _) in &kept {
                e.collect_cols(&mut child_req);
            }
            let child = prune_projections(*input, &child_req);
            // Eliminate pure identity Project when output ColIds and order already match!
            let child_cols = child.output_cols();
            let is_identity = kept.len() == child_cols.len()
                && kept
                    .iter()
                    .zip(child_cols.iter())
                    .all(|((e, bc), cc)| matches!(e, LogicalExpr::Col { id, .. } if *id == cc.id && bc.id == cc.id));
            if is_identity {
                child
            } else {
                LogicalPlan::Project {
                    input: Box::new(child),
                    exprs: kept,
                }
            }
        }

        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        } => {
            let mut child_req = required.clone();
            if let Some(c) = &condition {
                c.collect_cols(&mut child_req);
            }
            if let JoinType::LeftMark {
                probe_pair: Some((a, b)),
                ..
            } = &join_type
            {
                a.collect_cols(&mut child_req);
                b.collect_cols(&mut child_req);
            }
            LogicalPlan::Join {
                left: Box::new(prune_projections(*left, &child_req)),
                right: Box::new(prune_projections(*right, &child_req)),
                join_type,
                condition,
            }
        }

        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        } => {
            let mut child_req = BTreeSet::new();
            for (g, _) in &group_by {
                g.collect_cols(&mut child_req);
            }
            let kept_aggs: Vec<(vectra_sql::AggregateExpr, BoundCol)> = aggregates
                .into_iter()
                .filter(|(_, c)| required.contains(&c.id))
                .collect();
            for (a, _) in &kept_aggs {
                for arg in &a.args {
                    arg.collect_cols(&mut child_req);
                }
                if let Some(f) = &a.filter {
                    f.collect_cols(&mut child_req);
                }
            }
            LogicalPlan::Aggregate {
                input: Box::new(prune_projections(*input, &child_req)),
                group_by,
                grouping_sets,
                grouping_id_col,
                aggregates: kept_aggs,
            }
        }

        LogicalPlan::Window { input, windows } => {
            let mut child_req = required.clone();
            for (w, _) in &windows {
                for a in &w.args {
                    a.collect_cols(&mut child_req);
                }
                for p in &w.partition_by {
                    p.collect_cols(&mut child_req);
                }
                for o in &w.order_by {
                    o.expr.collect_cols(&mut child_req);
                }
            }
            LogicalPlan::Window {
                input: Box::new(prune_projections(*input, &child_req)),
                windows,
            }
        }

        LogicalPlan::Sort { input, order_by } => {
            let mut child_req = required.clone();
            for o in &order_by {
                o.expr.collect_cols(&mut child_req);
            }
            LogicalPlan::Sort {
                input: Box::new(prune_projections(*input, &child_req)),
                order_by,
            }
        }

        LogicalPlan::TopN {
            input,
            order_by,
            limit,
            offset,
        } => {
            let mut child_req = required.clone();
            for o in &order_by {
                o.expr.collect_cols(&mut child_req);
            }
            LogicalPlan::TopN {
                input: Box::new(prune_projections(*input, &child_req)),
                order_by,
                limit,
                offset,
            }
        }

        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(prune_projections(*input, required)),
            limit,
            offset,
        },

        LogicalPlan::Distinct { input, on_cols } => {
            let mut child_req = required.clone();
            for c in &on_cols {
                child_req.insert(c.id);
            }
            LogicalPlan::Distinct {
                input: Box::new(prune_projections(*input, &child_req)),
                on_cols,
            }
        }

        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
            output_cols,
        } => {
            // SetOps require full positional alignment on both sides.
            let lreq = left.output_col_ids();
            let rreq = right.output_col_ids();
            LogicalPlan::SetOp {
                op,
                all,
                left: Box::new(prune_projections(*left, &lreq)),
                right: Box::new(prune_projections(*right, &rreq)),
                output_cols,
            }
        }

        LogicalPlan::RecursiveCte {
            name,
            anchor,
            recursive,
            union_all,
            cte_cols,
            consumer,
        } => {
            let areq = anchor.output_col_ids();
            let rreq = recursive.output_col_ids();
            LogicalPlan::RecursiveCte {
                name,
                anchor: Box::new(prune_projections(*anchor, &areq)),
                recursive: Box::new(prune_projections(*recursive, &rreq)),
                union_all,
                cte_cols,
                consumer: Box::new(prune_projections(*consumer, required)),
            }
        }

        other => other,
    }
}

// ============================================================================
// Pass 6: Limit + Sort -> TopN Fusion & Limit Pushdown through UNION ALL
// ============================================================================

fn pushdown_limit_topn(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let child = pushdown_limit_topn(*input);
            if let Some(lim) = limit {
                match child {
                    LogicalPlan::Sort {
                        input: sort_in,
                        order_by,
                    } => {
                        return LogicalPlan::TopN {
                            input: sort_in,
                            order_by,
                            limit: lim,
                            offset,
                        };
                    }
                    LogicalPlan::SetOp {
                        op: SetOperator::Union,
                        all: true,
                        left,
                        right,
                        output_cols,
                    } => {
                        let child_lim = lim.saturating_add(offset);
                        return LogicalPlan::Limit {
                            input: Box::new(LogicalPlan::SetOp {
                                op: SetOperator::Union,
                                all: true,
                                left: Box::new(LogicalPlan::Limit {
                                    input: left,
                                    limit: Some(child_lim),
                                    offset: 0,
                                }),
                                right: Box::new(LogicalPlan::Limit {
                                    input: right,
                                    limit: Some(child_lim),
                                    offset: 0,
                                }),
                                output_cols,
                            }),
                            limit: Some(lim),
                            offset,
                        };
                    }
                    other => {
                        return LogicalPlan::Limit {
                            input: Box::new(other),
                            limit: Some(lim),
                            offset,
                        };
                    }
                }
            }
            LogicalPlan::Limit {
                input: Box::new(child),
                limit,
                offset,
            }
        }
        other => map_plan_children(other, pushdown_limit_topn),
    }
}

// ============================================================================
// Helpers
// ============================================================================

pub fn split_and_owned(expr: LogicalExpr, out: &mut Vec<LogicalExpr>) {
    match expr {
        LogicalExpr::BinaryOp {
            op: BinaryOperator::And,
            left,
            right,
            ..
        } => {
            split_and_owned(*left, out);
            split_and_owned(*right, out);
        }
        other => out.push(other),
    }
}

fn split_or_owned(expr: LogicalExpr, out: &mut Vec<LogicalExpr>) {
    match expr {
        LogicalExpr::BinaryOp {
            op: BinaryOperator::Or,
            left,
            right,
            ..
        } => {
            split_or_owned(*left, out);
            split_or_owned(*right, out);
        }
        other => out.push(other),
    }
}

pub fn combine_and(mut exprs: Vec<LogicalExpr>) -> LogicalExpr {
    let mut iter = exprs.drain(..);
    let mut acc = iter.next().expect("non-empty conjuncts");
    for e in iter {
        acc = LogicalExpr::BinaryOp {
            op: BinaryOperator::And,
            left: Box::new(acc),
            right: Box::new(e),
            return_type: LogicalType::Boolean,
        };
    }
    acc
}

fn combine_or(mut exprs: Vec<LogicalExpr>) -> LogicalExpr {
    let mut iter = exprs.drain(..);
    let mut acc = iter.next().expect("non-empty disjuncts");
    for e in iter {
        acc = LogicalExpr::BinaryOp {
            op: BinaryOperator::Or,
            left: Box::new(acc),
            right: Box::new(e),
            return_type: LogicalType::Boolean,
        };
    }
    acc
}

fn combine_opt_and(existing: Option<LogicalExpr>, extra: LogicalExpr) -> Option<LogicalExpr> {
    Some(match existing {
        None => extra,
        Some(prev) => LogicalExpr::BinaryOp {
            op: BinaryOperator::And,
            left: Box::new(prev),
            right: Box::new(extra),
            return_type: LogicalType::Boolean,
        },
    })
}

fn wrap_filter_if_nonempty(plan: LogicalPlan, preds: Vec<LogicalExpr>) -> LogicalPlan {
    if preds.is_empty() {
        plan
    } else {
        LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: combine_and(preds),
        }
    }
}

fn exprs_reference_col(exprs: &[LogicalExpr], col_id: ColId) -> bool {
    let mut cols = BTreeSet::new();
    for e in exprs {
        e.collect_cols(&mut cols);
    }
    cols.contains(&col_id)
}

pub fn map_plan_children<F>(plan: LogicalPlan, mut f: F) -> LogicalPlan
where
    F: FnMut(LogicalPlan) -> LogicalPlan,
{
    match plan {
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(f(*input)),
            predicate,
        },
        LogicalPlan::Project { input, exprs } => LogicalPlan::Project {
            input: Box::new(f(*input)),
            exprs,
        },
        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        } => LogicalPlan::Join {
            left: Box::new(f(*left)),
            right: Box::new(f(*right)),
            join_type,
            condition,
        },
        LogicalPlan::DependentJoin {
            left,
            right,
            join_type,
            condition,
            correlated_cols,
        } => LogicalPlan::DependentJoin {
            left: Box::new(f(*left)),
            right: Box::new(f(*right)),
            join_type,
            condition,
            correlated_cols,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        } => LogicalPlan::Aggregate {
            input: Box::new(f(*input)),
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        },
        LogicalPlan::Window { input, windows } => LogicalPlan::Window {
            input: Box::new(f(*input)),
            windows,
        },
        LogicalPlan::Sort { input, order_by } => LogicalPlan::Sort {
            input: Box::new(f(*input)),
            order_by,
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(f(*input)),
            limit,
            offset,
        },
        LogicalPlan::TopN {
            input,
            order_by,
            limit,
            offset,
        } => LogicalPlan::TopN {
            input: Box::new(f(*input)),
            order_by,
            limit,
            offset,
        },
        LogicalPlan::Distinct { input, on_cols } => LogicalPlan::Distinct {
            input: Box::new(f(*input)),
            on_cols,
        },
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
            output_cols,
        } => LogicalPlan::SetOp {
            op,
            all,
            left: Box::new(f(*left)),
            right: Box::new(f(*right)),
            output_cols,
        },
        LogicalPlan::RecursiveCte {
            name,
            anchor,
            recursive,
            union_all,
            cte_cols,
            consumer,
        } => LogicalPlan::RecursiveCte {
            name,
            anchor: Box::new(f(*anchor)),
            recursive: Box::new(f(*recursive)),
            union_all,
            cte_cols,
            consumer: Box::new(f(*consumer)),
        },
        leaf => leaf,
    }
}
