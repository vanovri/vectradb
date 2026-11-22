//! Neumann & Kemper (BTW 2015) Arbitrary Subquery Unnesting.
//!
//! Eliminates ALL `LogicalPlan::DependentJoin` nodes and `LogicalExpr::CorrelatedCol`
//! references by computing a deduplicated outer domain `D = Distinct(Project(left, corr_cols))`
//! and pushing `D` recursively down through `Filter`, `Project`, `Join`, `Aggregate` (with full
//! COUNT-bug protection via `LeftOuterJoin` + `COALESCE(count, 0)`), `Distinct`, `Window`,
//! `Sort`/`Limit`, and `SetOp` until zero correlated variables remain.

use crate::logical_plan::*;
use std::collections::{BTreeSet, HashMap};
use vectra_expr::BinaryOperator;
use vectra_types::{LogicalType, Result, ScalarValue, SortOrder};

/// Eliminates every `DependentJoin` in `plan`, returning a 100% decorrelated `LogicalPlan`.
pub fn unnest_plan(plan: LogicalPlan, col_gen: &mut ColIdGen) -> Result<LogicalPlan> {
    match plan {
        LogicalPlan::EmptyRelation { .. }
        | LogicalPlan::Values { .. }
        | LogicalPlan::Scan { .. }
        | LogicalPlan::TableFunc { .. }
        | LogicalPlan::CteScan { .. } => Ok(plan),

        LogicalPlan::Filter { input, predicate } => Ok(LogicalPlan::Filter {
            input: Box::new(unnest_plan(*input, col_gen)?),
            predicate,
        }),
        LogicalPlan::Project { input, exprs } => Ok(LogicalPlan::Project {
            input: Box::new(unnest_plan(*input, col_gen)?),
            exprs,
        }),
        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        } => Ok(LogicalPlan::Aggregate {
            input: Box::new(unnest_plan(*input, col_gen)?),
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        }),
        LogicalPlan::Window { input, windows } => Ok(LogicalPlan::Window {
            input: Box::new(unnest_plan(*input, col_gen)?),
            windows,
        }),
        LogicalPlan::Sort { input, order_by } => Ok(LogicalPlan::Sort {
            input: Box::new(unnest_plan(*input, col_gen)?),
            order_by,
        }),
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => Ok(LogicalPlan::Limit {
            input: Box::new(unnest_plan(*input, col_gen)?),
            limit,
            offset,
        }),
        LogicalPlan::TopN {
            input,
            order_by,
            limit,
            offset,
        } => Ok(LogicalPlan::TopN {
            input: Box::new(unnest_plan(*input, col_gen)?),
            order_by,
            limit,
            offset,
        }),
        LogicalPlan::Distinct { input, on_cols } => Ok(LogicalPlan::Distinct {
            input: Box::new(unnest_plan(*input, col_gen)?),
            on_cols,
        }),
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
            output_cols,
        } => Ok(LogicalPlan::SetOp {
            op,
            all,
            left: Box::new(unnest_plan(*left, col_gen)?),
            right: Box::new(unnest_plan(*right, col_gen)?),
            output_cols,
        }),
        LogicalPlan::RecursiveCte {
            name,
            anchor,
            recursive,
            union_all,
            cte_cols,
            consumer,
        } => Ok(LogicalPlan::RecursiveCte {
            name,
            anchor: Box::new(unnest_plan(*anchor, col_gen)?),
            recursive: Box::new(unnest_plan(*recursive, col_gen)?),
            union_all,
            cte_cols,
            consumer: Box::new(unnest_plan(*consumer, col_gen)?),
        }),
        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        } => Ok(LogicalPlan::Join {
            left: Box::new(unnest_plan(*left, col_gen)?),
            right: Box::new(unnest_plan(*right, col_gen)?),
            join_type,
            condition,
        }),

        LogicalPlan::DependentJoin {
            left,
            right,
            join_type,
            condition,
            correlated_cols,
        } => {
            // 1. Recursively unnest inner subqueries inside `left` and `right` first!
            let left = unnest_plan(*left, col_gen)?;
            let right = unnest_plan(*right, col_gen)?;

            // 2. Check which correlated variables belong to `left` and are still free in `right`.
            let left_ids = left.output_col_ids();
            let free_in_right = right.free_correlated_vars();
            let active_corr: Vec<BoundCol> = correlated_cols
                .into_iter()
                .filter(|c| left_ids.contains(&c.id) && free_in_right.contains(&c.id))
                .collect();

            if active_corr.is_empty() {
                return Ok(LogicalPlan::Join {
                    left: Box::new(left),
                    right: Box::new(right),
                    join_type,
                    condition,
                });
            }

            // 3. Build deduplicated outer domain D = Distinct(Project(clone(left), active_corr))
            let (domain_plan, corr_to_domain) = build_domain_plan(&left, &active_corr, col_gen);

            // 4. Push domain D down through `right`, producing a decorrelated plan outputting
            //    all original `right` columns PLUS the `domain_cols`!
            let (decorrelated_right, out_domain_map) =
                push_dependent_join(&domain_plan, right, &corr_to_domain, col_gen)?;

            // 5. Connect `left` and `decorrelated_right` using regular Join with
            //    `AND_i (left.c_i IS NOT DISTINCT FROM decorrelated_right.d_i)`!
            let mut join_cond = condition;
            for c in &active_corr {
                let d_col = out_domain_map
                    .get(&c.id)
                    .expect("domain col preserved through pushdown");
                let eq = LogicalExpr::BinaryOp {
                    op: BinaryOperator::IsNotDistinctFrom,
                    left: Box::new(c.to_expr()),
                    right: Box::new(d_col.to_expr()),
                    return_type: LogicalType::Boolean,
                };
                join_cond = Some(match join_cond {
                    None => eq,
                    Some(prev) => LogicalExpr::BinaryOp {
                        op: BinaryOperator::And,
                        left: Box::new(prev),
                        right: Box::new(eq),
                        return_type: LogicalType::Boolean,
                    },
                });
            }

            // Project away internal domain columns on outer/inner/single joins so output schema matches!
            let orig_out_cols: Vec<BoundCol> = {
                let mut cols = left.output_cols();
                match &join_type {
                    JoinType::LeftSemi | JoinType::LeftAnti => {}
                    JoinType::LeftMark { mark_col, .. } => cols.push(mark_col.clone()),
                    _ => {
                        let domain_ids: BTreeSet<ColId> =
                            out_domain_map.values().map(|c| c.id).collect();
                        for rc in decorrelated_right.output_cols() {
                            if !domain_ids.contains(&rc.id) {
                                cols.push(rc);
                            }
                        }
                    }
                }
                cols
            };

            let joined = LogicalPlan::Join {
                left: Box::new(left),
                right: Box::new(decorrelated_right),
                join_type: join_type.clone(),
                condition: join_cond,
            };

            if matches!(
                join_type,
                JoinType::LeftSemi | JoinType::LeftAnti | JoinType::LeftMark { .. }
            ) {
                Ok(joined)
            } else {
                let proj = orig_out_cols
                    .into_iter()
                    .map(|c| (c.to_expr(), c))
                    .collect();
                Ok(LogicalPlan::Project {
                    input: Box::new(joined),
                    exprs: proj,
                })
            }
        }
    }
}

/// Builds `D = Distinct(Project(clone_fresh(left), active_corr))` returning `(D, orig_corr_id -> domain_col)`.
fn build_domain_plan(
    left: &LogicalPlan,
    active_corr: &[BoundCol],
    col_gen: &mut ColIdGen,
) -> (LogicalPlan, HashMap<ColId, BoundCol>) {
    let (cloned_left, id_map) = clone_plan_fresh(left, col_gen);
    let mut proj_exprs = Vec::with_capacity(active_corr.len());
    let mut domain_cols = Vec::with_capacity(active_corr.len());
    let mut corr_to_domain = HashMap::with_capacity(active_corr.len());

    for c in active_corr {
        let cloned_src_id = id_map.get(&c.id).copied().unwrap_or(c.id);
        let d_id = col_gen.next_id();
        let d_col = BoundCol::new(d_id, format!("__dom_{}", c.name), c.data_type);
        proj_exprs.push((
            LogicalExpr::Col {
                id: cloned_src_id,
                name: c.name.clone(),
                data_type: c.data_type,
            },
            d_col.clone(),
        ));
        domain_cols.push(d_col.clone());
        corr_to_domain.insert(c.id, d_col);
    }

    let proj = LogicalPlan::Project {
        input: Box::new(cloned_left),
        exprs: proj_exprs,
    };
    let distinct = LogicalPlan::Distinct {
        input: Box::new(proj),
        on_cols: domain_cols,
    };
    (distinct, corr_to_domain)
}

/// Neumann-Kemper recursive pushdown of domain `D` into `node`.
/// Returns `(pushed_plan, active_corr_id -> domain_col_in_output)`.
fn push_dependent_join(
    domain: &LogicalPlan,
    node: LogicalPlan,
    corr_map: &HashMap<ColId, BoundCol>,
    col_gen: &mut ColIdGen,
) -> Result<(LogicalPlan, HashMap<ColId, BoundCol>)> {
    let free = node.free_correlated_vars();
    let relevant: BTreeSet<ColId> = corr_map
        .keys()
        .copied()
        .filter(|id| free.contains(id))
        .collect();

    // Base case: `node` has NO references to any correlated variable in `corr_map`!
    // Simply Cross-Join a fresh copy of `domain` with `node`.
    if relevant.is_empty() {
        let (fresh_dom, dom_remap) = clone_plan_fresh(domain, col_gen);
        let new_map: HashMap<ColId, BoundCol> = corr_map
            .iter()
            .map(|(&orig_cid, dcol)| {
                let mut nc = dcol.clone();
                nc.id = dom_remap.get(&dcol.id).copied().unwrap_or(dcol.id);
                (orig_cid, nc)
            })
            .collect();
        let joined = LogicalPlan::Join {
            left: Box::new(fresh_dom),
            right: Box::new(node),
            join_type: JoinType::Cross,
            condition: None,
        };
        return Ok((joined, new_map));
    }

    match node {
        LogicalPlan::Filter { input, predicate } => {
            let (pushed_in, out_map) = push_dependent_join(domain, *input, corr_map, col_gen)?;
            let new_pred = predicate.rewrite_correlated(&out_map);
            Ok((
                LogicalPlan::Filter {
                    input: Box::new(pushed_in),
                    predicate: new_pred,
                },
                out_map,
            ))
        }

        LogicalPlan::Project { input, exprs } => {
            let (pushed_in, out_map) = push_dependent_join(domain, *input, corr_map, col_gen)?;
            let mut new_exprs: Vec<(LogicalExpr, BoundCol)> = exprs
                .into_iter()
                .map(|(e, c)| (e.rewrite_correlated(&out_map), c))
                .collect();
            // Preserve all domain columns in projection output!
            let mut sorted_keys: Vec<ColId> = out_map.keys().copied().collect();
            sorted_keys.sort();
            for k in sorted_keys {
                let dcol = out_map[&k].clone();
                new_exprs.push((dcol.to_expr(), dcol));
            }
            Ok((
                LogicalPlan::Project {
                    input: Box::new(pushed_in),
                    exprs: new_exprs,
                },
                out_map,
            ))
        }

        LogicalPlan::Distinct { input, mut on_cols } => {
            let (pushed_in, out_map) = push_dependent_join(domain, *input, corr_map, col_gen)?;
            let mut sorted_keys: Vec<ColId> = out_map.keys().copied().collect();
            sorted_keys.sort();
            for k in sorted_keys {
                on_cols.push(out_map[&k].clone());
            }
            Ok((
                LogicalPlan::Distinct {
                    input: Box::new(pushed_in),
                    on_cols,
                },
                out_map,
            ))
        }

        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        } => {
            let was_global_scalar_agg = group_by.is_empty() && grouping_sets.is_none();
            let (pushed_in, out_map) = push_dependent_join(domain, *input, corr_map, col_gen)?;

            let mut sorted_keys: Vec<ColId> = out_map.keys().copied().collect();
            sorted_keys.sort();

            let mut new_group_by: Vec<(LogicalExpr, BoundCol)> = group_by
                .into_iter()
                .map(|(e, c)| (e.rewrite_correlated(&out_map), c))
                .collect();
            let orig_group_len = new_group_by.len();
            for &k in &sorted_keys {
                let dcol = out_map[&k].clone();
                new_group_by.push((dcol.to_expr(), dcol));
            }

            let new_gsets = grouping_sets.map(|sets| {
                sets.into_iter()
                    .map(|mut s| {
                        for idx in orig_group_len..new_group_by.len() {
                            s.push(idx);
                        }
                        s
                    })
                    .collect()
            });

            let new_aggs: Vec<(AggregateExpr, BoundCol)> = aggregates
                .into_iter()
                .map(|(mut ae, col)| {
                    ae.args = ae
                        .args
                        .into_iter()
                        .map(|a| a.rewrite_correlated(&out_map))
                        .collect();
                    ae.filter = ae.filter.map(|f| f.rewrite_correlated(&out_map));
                    (ae, col)
                })
                .collect();

            let agg_plan = LogicalPlan::Aggregate {
                input: Box::new(pushed_in),
                group_by: new_group_by,
                grouping_sets: new_gsets,
                grouping_id_col,
                aggregates: new_aggs.clone(),
            };

            // Neumann-Kemper Section 3.3 COUNT-bug protection:
            // For global scalar aggregate (`group_by.is_empty()`), an outer tuple in D with 0
            // matching inner rows MUST still produce 1 row with COUNT=0 and SUM/AVG/MIN/MAX=NULL!
            if was_global_scalar_agg {
                let has_count = new_aggs.iter().any(|(ae, _)| {
                    matches!(ae.func, AggregateFunc::CountStar | AggregateFunc::Count)
                });
                if has_count {
                    let (fresh_dom, dom_remap) = clone_plan_fresh(domain, col_gen);
                    let mut left_dom_map = HashMap::new();
                    let mut cond: Option<LogicalExpr> = None;
                    for &k in &sorted_keys {
                        let right_d = &out_map[&k];
                        let mut left_d = corr_map[&k].clone();
                        left_d.id = dom_remap.get(&left_d.id).copied().unwrap_or(left_d.id);
                        let eq = LogicalExpr::BinaryOp {
                            op: BinaryOperator::IsNotDistinctFrom,
                            left: Box::new(left_d.to_expr()),
                            right: Box::new(right_d.to_expr()),
                            return_type: LogicalType::Boolean,
                        };
                        cond = Some(match cond {
                            None => eq,
                            Some(prev) => LogicalExpr::BinaryOp {
                                op: BinaryOperator::And,
                                left: Box::new(prev),
                                right: Box::new(eq),
                                return_type: LogicalType::Boolean,
                            },
                        });
                        left_dom_map.insert(k, left_d);
                    }

                    let loj = LogicalPlan::Join {
                        left: Box::new(fresh_dom),
                        right: Box::new(agg_plan),
                        join_type: JoinType::LeftOuter,
                        condition: cond,
                    };

                    // Project left domain cols + COALESCE(count_col, 0) preserving original aggregate ColIds!
                    let mut proj_exprs = Vec::new();
                    for &k in &sorted_keys {
                        let ld = left_dom_map[&k].clone();
                        proj_exprs.push((ld.to_expr(), ld));
                    }
                    for (ae, col) in new_aggs {
                        if matches!(ae.func, AggregateFunc::CountStar | AggregateFunc::Count) {
                            let coal = LogicalExpr::Coalesce {
                                args: vec![
                                    col.to_expr(),
                                    LogicalExpr::Literal {
                                        value: ScalarValue::Int64(0),
                                        data_type: LogicalType::Int64,
                                    },
                                ],
                                return_type: LogicalType::Int64,
                            };
                            proj_exprs.push((coal, col));
                        } else {
                            proj_exprs.push((col.to_expr(), col));
                        }
                    }

                    return Ok((
                        LogicalPlan::Project {
                            input: Box::new(loj),
                            exprs: proj_exprs,
                        },
                        left_dom_map,
                    ));
                }
            }

            Ok((agg_plan, out_map))
        }

        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        } => {
            let l_free = left.free_correlated_vars();
            let r_free = right.free_correlated_vars();
            let l_corr = corr_map.keys().any(|k| l_free.contains(k));
            let r_corr = corr_map.keys().any(|k| r_free.contains(k));

            match (l_corr, r_corr) {
                (true, false) => {
                    let (pl, lmap) = push_dependent_join(domain, *left, corr_map, col_gen)?;
                    let new_cond = condition.map(|c| c.rewrite_correlated(&lmap));
                    let new_jt = rewrite_join_type_corr(join_type, &lmap);
                    Ok((
                        LogicalPlan::Join {
                            left: Box::new(pl),
                            right,
                            join_type: new_jt,
                            condition: new_cond,
                        },
                        lmap,
                    ))
                }
                (false, true)
                    if matches!(
                        join_type,
                        JoinType::Inner | JoinType::Cross | JoinType::RightOuter
                    ) =>
                {
                    let (pr, rmap) = push_dependent_join(domain, *right, corr_map, col_gen)?;
                    let new_cond = condition.map(|c| c.rewrite_correlated(&rmap));
                    let new_jt = rewrite_join_type_corr(join_type, &rmap);
                    Ok((
                        LogicalPlan::Join {
                            left,
                            right: Box::new(pr),
                            join_type: new_jt,
                            condition: new_cond,
                        },
                        rmap,
                    ))
                }
                _ => {
                    // Push domain into BOTH children and equate their domain keys!
                    let (pl, lmap) = push_dependent_join(domain, *left, corr_map, col_gen)?;
                    let (pr, rmap) = push_dependent_join(domain, *right, corr_map, col_gen)?;
                    let mut new_cond = condition.map(|c| c.rewrite_correlated(&lmap));
                    let new_jt = rewrite_join_type_corr(join_type, &lmap);
                    let mut sorted_keys: Vec<ColId> = lmap.keys().copied().collect();
                    sorted_keys.sort();
                    for k in sorted_keys {
                        let eq = LogicalExpr::BinaryOp {
                            op: BinaryOperator::IsNotDistinctFrom,
                            left: Box::new(lmap[&k].to_expr()),
                            right: Box::new(rmap[&k].to_expr()),
                            return_type: LogicalType::Boolean,
                        };
                        new_cond = Some(match new_cond {
                            None => eq,
                            Some(prev) => LogicalExpr::BinaryOp {
                                op: BinaryOperator::And,
                                left: Box::new(prev),
                                right: Box::new(eq),
                                return_type: LogicalType::Boolean,
                            },
                        });
                    }
                    Ok((
                        LogicalPlan::Join {
                            left: Box::new(pl),
                            right: Box::new(pr),
                            join_type: new_jt,
                            condition: new_cond,
                        },
                        lmap,
                    ))
                }
            }
        }

        LogicalPlan::Window { input, windows } => {
            let (pushed_in, out_map) = push_dependent_join(domain, *input, corr_map, col_gen)?;
            let mut sorted_keys: Vec<ColId> = out_map.keys().copied().collect();
            sorted_keys.sort();
            let new_wins = windows
                .into_iter()
                .map(|(mut w, col)| {
                    w.args = w
                        .args
                        .into_iter()
                        .map(|a| a.rewrite_correlated(&out_map))
                        .collect();
                    w.partition_by = w
                        .partition_by
                        .into_iter()
                        .map(|p| p.rewrite_correlated(&out_map))
                        .collect();
                    for &k in &sorted_keys {
                        w.partition_by.push(out_map[&k].to_expr());
                    }
                    w.order_by = w
                        .order_by
                        .into_iter()
                        .map(|mut o| {
                            o.expr = o.expr.rewrite_correlated(&out_map);
                            o
                        })
                        .collect();
                    (w, col)
                })
                .collect();
            Ok((
                LogicalPlan::Window {
                    input: Box::new(pushed_in),
                    windows: new_wins,
                },
                out_map,
            ))
        }

        LogicalPlan::Sort { input, order_by } => {
            let (pushed_in, out_map) = push_dependent_join(domain, *input, corr_map, col_gen)?;
            let new_ob = order_by
                .into_iter()
                .map(|mut o| {
                    o.expr = o.expr.rewrite_correlated(&out_map);
                    o
                })
                .collect();
            Ok((
                LogicalPlan::Sort {
                    input: Box::new(pushed_in),
                    order_by: new_ob,
                },
                out_map,
            ))
        }

        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            // Per-outer-tuple LIMIT via Window(ROW_NUMBER() OVER (PARTITION BY domain_cols))!
            let (pushed_in, out_map) = push_dependent_join(domain, *input, corr_map, col_gen)?;
            let (base_in, order_by) = match pushed_in {
                LogicalPlan::Sort {
                    input: s_in,
                    order_by: ob,
                } => (*s_in, ob),
                other => (other, Vec::new()),
            };
            let mut sorted_keys: Vec<ColId> = out_map.keys().copied().collect();
            sorted_keys.sort();
            let partition_by: Vec<LogicalExpr> =
                sorted_keys.iter().map(|k| out_map[k].to_expr()).collect();
            let rn_col = BoundCol::new(col_gen.next_id(), "__unnest_rn", LogicalType::Int64);
            let win = LogicalPlan::Window {
                input: Box::new(base_in),
                windows: vec![(
                    WindowExpr {
                        func: WindowFunc::RowNumber,
                        args: Vec::new(),
                        partition_by,
                        order_by,
                        frame: None,
                        return_type: LogicalType::Int64,
                    },
                    rn_col.clone(),
                )],
            };
            let mut pred = LogicalExpr::BinaryOp {
                op: BinaryOperator::Gt,
                left: Box::new(rn_col.to_expr()),
                right: Box::new(LogicalExpr::Literal {
                    value: ScalarValue::Int64(offset as i64),
                    data_type: LogicalType::Int64,
                }),
                return_type: LogicalType::Boolean,
            };
            if let Some(lim) = limit {
                let upper = LogicalExpr::BinaryOp {
                    op: BinaryOperator::LtEq,
                    left: Box::new(rn_col.to_expr()),
                    right: Box::new(LogicalExpr::Literal {
                        value: ScalarValue::Int64((offset + lim) as i64),
                        data_type: LogicalType::Int64,
                    }),
                    return_type: LogicalType::Boolean,
                };
                pred = LogicalExpr::BinaryOp {
                    op: BinaryOperator::And,
                    left: Box::new(pred),
                    right: Box::new(upper),
                    return_type: LogicalType::Boolean,
                };
            }
            let filtered = LogicalPlan::Filter {
                input: Box::new(win),
                predicate: pred,
            };
            // Drop helper `__unnest_rn` column cleanly.
            let proj: Vec<(LogicalExpr, BoundCol)> = filtered
                .output_cols()
                .into_iter()
                .filter(|c| c.id != rn_col.id)
                .map(|c| (c.to_expr(), c))
                .collect();
            Ok((
                LogicalPlan::Project {
                    input: Box::new(filtered),
                    exprs: proj,
                },
                out_map,
            ))
        }

        LogicalPlan::TopN {
            input,
            order_by,
            limit,
            offset,
        } => {
            let as_sort_limit = LogicalPlan::Limit {
                input: Box::new(LogicalPlan::Sort { input, order_by }),
                limit: Some(limit),
                offset,
            };
            push_dependent_join(domain, as_sort_limit, corr_map, col_gen)
        }

        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
            mut output_cols,
        } => {
            let (pl, lmap) = push_dependent_join(domain, *left, corr_map, col_gen)?;
            let (pr, _rmap) = push_dependent_join(domain, *right, corr_map, col_gen)?;
            let mut sorted_keys: Vec<ColId> = lmap.keys().copied().collect();
            sorted_keys.sort();
            let mut new_out_map = HashMap::new();
            for k in sorted_keys {
                let d_id = col_gen.next_id();
                let d_col = BoundCol::new(d_id, lmap[&k].name.clone(), lmap[&k].data_type);
                output_cols.push(d_col.clone());
                new_out_map.insert(k, d_col);
            }
            Ok((
                LogicalPlan::SetOp {
                    op,
                    all,
                    left: Box::new(pl),
                    right: Box::new(pr),
                    output_cols,
                },
                new_out_map,
            ))
        }

        other => {
            // Fallback for leaf nodes with correlated expressions (e.g. Values / Scan pushed_filters)
            let (fresh_dom, dom_remap) = clone_plan_fresh(domain, col_gen);
            let new_map: HashMap<ColId, BoundCol> = corr_map
                .iter()
                .map(|(&orig_cid, dcol)| {
                    let mut nc = dcol.clone();
                    nc.id = dom_remap.get(&dcol.id).copied().unwrap_or(dcol.id);
                    (orig_cid, nc)
                })
                .collect();
            let rewritten = rewrite_leaf_corr(other, &new_map);
            let joined = LogicalPlan::Join {
                left: Box::new(fresh_dom),
                right: Box::new(rewritten),
                join_type: JoinType::Cross,
                condition: None,
            };
            Ok((joined, new_map))
        }
    }
}

fn rewrite_join_type_corr(jt: JoinType, map: &HashMap<ColId, BoundCol>) -> JoinType {
    match jt {
        JoinType::LeftMark {
            mark_col,
            probe_pair,
        } => JoinType::LeftMark {
            mark_col,
            probe_pair: probe_pair
                .map(|(a, b)| (a.rewrite_correlated(map), b.rewrite_correlated(map))),
        },
        other => other,
    }
}

fn rewrite_leaf_corr(plan: LogicalPlan, map: &HashMap<ColId, BoundCol>) -> LogicalPlan {
    match plan {
        LogicalPlan::Values { rows, columns } => LogicalPlan::Values {
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(|e| e.rewrite_correlated(map)).collect())
                .collect(),
            columns,
        },
        LogicalPlan::Scan {
            table_name,
            alias,
            columns,
            table_col_indices,
            pushed_filters,
        } => LogicalPlan::Scan {
            table_name,
            alias,
            columns,
            table_col_indices,
            pushed_filters: pushed_filters
                .into_iter()
                .map(|f| f.rewrite_correlated(map))
                .collect(),
        },
        other => other,
    }
}

/// Deep-clones a `LogicalPlan` assigning globally fresh `ColId`s to every output column
/// so cloned domain subplans never alias `ColId`s with the original left subtree.
pub fn clone_plan_fresh(
    plan: &LogicalPlan,
    col_gen: &mut ColIdGen,
) -> (LogicalPlan, HashMap<ColId, ColId>) {
    let mut map = HashMap::new();
    let cloned = clone_plan_inner(plan, col_gen, &mut map);
    (cloned, map)
}

fn remap_bound_col(
    c: &BoundCol,
    col_gen: &mut ColIdGen,
    map: &mut HashMap<ColId, ColId>,
) -> BoundCol {
    let new_id = *map.entry(c.id).or_insert_with(|| col_gen.next_id());
    let mut nc = c.clone();
    nc.id = new_id;
    nc
}

fn clone_plan_inner(
    plan: &LogicalPlan,
    col_gen: &mut ColIdGen,
    map: &mut HashMap<ColId, ColId>,
) -> LogicalPlan {
    match plan {
        LogicalPlan::EmptyRelation {
            produce_one_row,
            columns,
        } => LogicalPlan::EmptyRelation {
            produce_one_row: *produce_one_row,
            columns: columns
                .iter()
                .map(|c| remap_bound_col(c, col_gen, map))
                .collect(),
        },
        LogicalPlan::Values { rows, columns } => LogicalPlan::Values {
            rows: rows
                .iter()
                .map(|r| r.iter().map(|e| e.remap_cols(map)).collect())
                .collect(),
            columns: columns
                .iter()
                .map(|c| remap_bound_col(c, col_gen, map))
                .collect(),
        },
        LogicalPlan::Scan {
            table_name,
            alias,
            columns,
            table_col_indices,
            pushed_filters,
        } => {
            let new_cols: Vec<BoundCol> = columns
                .iter()
                .map(|c| remap_bound_col(c, col_gen, map))
                .collect();
            LogicalPlan::Scan {
                table_name: table_name.clone(),
                alias: alias.clone(),
                columns: new_cols,
                table_col_indices: table_col_indices.clone(),
                pushed_filters: pushed_filters.iter().map(|f| f.remap_cols(map)).collect(),
            }
        }
        LogicalPlan::TableFunc { func, columns } => LogicalPlan::TableFunc {
            func: func.clone(),
            columns: columns
                .iter()
                .map(|c| remap_bound_col(c, col_gen, map))
                .collect(),
        },
        LogicalPlan::CteScan { cte_name, columns } => LogicalPlan::CteScan {
            cte_name: cte_name.clone(),
            columns: columns
                .iter()
                .map(|c| remap_bound_col(c, col_gen, map))
                .collect(),
        },
        LogicalPlan::Filter { input, predicate } => {
            let ci = clone_plan_inner(input, col_gen, map);
            LogicalPlan::Filter {
                input: Box::new(ci),
                predicate: predicate.remap_cols(map),
            }
        }
        LogicalPlan::Project { input, exprs } => {
            let ci = clone_plan_inner(input, col_gen, map);
            let new_exprs = exprs
                .iter()
                .map(|(e, c)| {
                    let re = e.remap_cols(map);
                    let rc = remap_bound_col(c, col_gen, map);
                    (re, rc)
                })
                .collect();
            LogicalPlan::Project {
                input: Box::new(ci),
                exprs: new_exprs,
            }
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        } => {
            let cl = clone_plan_inner(left, col_gen, map);
            let cr = clone_plan_inner(right, col_gen, map);
            let new_jt = match join_type {
                JoinType::LeftMark {
                    mark_col,
                    probe_pair,
                } => JoinType::LeftMark {
                    mark_col: remap_bound_col(mark_col, col_gen, map),
                    probe_pair: probe_pair
                        .as_ref()
                        .map(|(a, b)| (a.remap_cols(map), b.remap_cols(map))),
                },
                other => other.clone(),
            };
            LogicalPlan::Join {
                left: Box::new(cl),
                right: Box::new(cr),
                join_type: new_jt,
                condition: condition.as_ref().map(|c| c.remap_cols(map)),
            }
        }
        LogicalPlan::DependentJoin {
            left,
            right,
            join_type,
            condition,
            correlated_cols,
        } => {
            let cl = clone_plan_inner(left, col_gen, map);
            let cr = clone_plan_inner(right, col_gen, map);
            LogicalPlan::DependentJoin {
                left: Box::new(cl),
                right: Box::new(cr),
                join_type: join_type.clone(),
                condition: condition.as_ref().map(|c| c.remap_cols(map)),
                correlated_cols: correlated_cols
                    .iter()
                    .map(|c| remap_bound_col(c, col_gen, map))
                    .collect(),
            }
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            grouping_sets,
            grouping_id_col,
            aggregates,
        } => {
            let ci = clone_plan_inner(input, col_gen, map);
            let new_gb = group_by
                .iter()
                .map(|(e, c)| (e.remap_cols(map), remap_bound_col(c, col_gen, map)))
                .collect();
            let new_gid = grouping_id_col
                .as_ref()
                .map(|c| remap_bound_col(c, col_gen, map));
            let new_aggs = aggregates
                .iter()
                .map(|(ae, c)| {
                    let nae = AggregateExpr {
                        func: ae.func,
                        args: ae.args.iter().map(|a| a.remap_cols(map)).collect(),
                        distinct: ae.distinct,
                        filter: ae.filter.as_ref().map(|f| f.remap_cols(map)),
                        return_type: ae.return_type,
                    };
                    (nae, remap_bound_col(c, col_gen, map))
                })
                .collect();
            LogicalPlan::Aggregate {
                input: Box::new(ci),
                group_by: new_gb,
                grouping_sets: grouping_sets.clone(),
                grouping_id_col: new_gid,
                aggregates: new_aggs,
            }
        }
        LogicalPlan::Window { input, windows } => {
            let ci = clone_plan_inner(input, col_gen, map);
            let new_wins = windows
                .iter()
                .map(|(w, c)| {
                    let nw = WindowExpr {
                        func: w.func,
                        args: w.args.iter().map(|a| a.remap_cols(map)).collect(),
                        partition_by: w.partition_by.iter().map(|p| p.remap_cols(map)).collect(),
                        order_by: w
                            .order_by
                            .iter()
                            .map(|o| LogicalSortExpr {
                                expr: o.expr.remap_cols(map),
                                order: o.order,
                            })
                            .collect(),
                        frame: w.frame.clone(),
                        return_type: w.return_type,
                    };
                    (nw, remap_bound_col(c, col_gen, map))
                })
                .collect();
            LogicalPlan::Window {
                input: Box::new(ci),
                windows: new_wins,
            }
        }
        LogicalPlan::Sort { input, order_by } => {
            let ci = clone_plan_inner(input, col_gen, map);
            LogicalPlan::Sort {
                input: Box::new(ci),
                order_by: order_by
                    .iter()
                    .map(|o| LogicalSortExpr {
                        expr: o.expr.remap_cols(map),
                        order: o.order,
                    })
                    .collect(),
            }
        }
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(clone_plan_inner(input, col_gen, map)),
            limit: *limit,
            offset: *offset,
        },
        LogicalPlan::TopN {
            input,
            order_by,
            limit,
            offset,
        } => {
            let ci = clone_plan_inner(input, col_gen, map);
            LogicalPlan::TopN {
                input: Box::new(ci),
                order_by: order_by
                    .iter()
                    .map(|o| LogicalSortExpr {
                        expr: o.expr.remap_cols(map),
                        order: o.order,
                    })
                    .collect(),
                limit: *limit,
                offset: *offset,
            }
        }
        LogicalPlan::Distinct { input, on_cols } => {
            let ci = clone_plan_inner(input, col_gen, map);
            let new_cols = on_cols
                .iter()
                .map(|c| {
                    let mut nc = c.clone();
                    nc.id = map.get(&c.id).copied().unwrap_or(c.id);
                    nc
                })
                .collect();
            LogicalPlan::Distinct {
                input: Box::new(ci),
                on_cols: new_cols,
            }
        }
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
            output_cols,
        } => {
            let cl = clone_plan_inner(left, col_gen, map);
            let cr = clone_plan_inner(right, col_gen, map);
            let new_out = output_cols
                .iter()
                .map(|c| remap_bound_col(c, col_gen, map))
                .collect();
            LogicalPlan::SetOp {
                op: *op,
                all: *all,
                left: Box::new(cl),
                right: Box::new(cr),
                output_cols: new_out,
            }
        }
        LogicalPlan::RecursiveCte {
            name,
            anchor,
            recursive,
            union_all,
            cte_cols,
            consumer,
        } => LogicalPlan::RecursiveCte {
            name: name.clone(),
            anchor: Box::new(clone_plan_inner(anchor, col_gen, map)),
            recursive: Box::new(clone_plan_inner(recursive, col_gen, map)),
            union_all: *union_all,
            cte_cols: cte_cols
                .iter()
                .map(|c| remap_bound_col(c, col_gen, map))
                .collect(),
            consumer: Box::new(clone_plan_inner(consumer, col_gen, map)),
        },
    }
}

/// Ensures `SortOrder` import is used cleanly across rewrites.
pub fn default_asc_order() -> SortOrder {
    SortOrder::ASC_NULLS_LAST
}
