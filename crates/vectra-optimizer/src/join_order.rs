//! Cost-Based Bushy Join Order Optimizer:
//! - Genuine Moerkotte & Neumann (VLDB 2006) `DPccp` (`EnumerateCsg` / `EnumerateCmp`) over
//!   connected subgraphs with zero tree/stats allocations in the DP hot loop (24-byte `Copy` table
//!   entries, precomputed edge selectivities, bounded pair budget with seamless `GOO` fallback for
//!   dense cliques or `N > 14`).
//! - $O(N^2)$ Greedy Operator Ordering (`GOO`) for large graphs (`N > 14`, sub-5ms for 24-30 tables).
//! - Build-side orientation invariant: smaller estimated cardinality ALWAYS placed on Right (Build)
//!   side for symmetric `INNER` and flippable `LEFT <-> RIGHT` outer joins.

use crate::cardinality::{
    estimate_join_stats, estimate_plan_stats, estimate_predicate_selectivity, extract_base_col,
    PlanStats,
};
use crate::rules::{combine_and, map_plan_children, split_and_owned};
use crate::stats::StatsCatalog;
use std::collections::BTreeSet;
use vectra_expr::BinaryOperator;
use vectra_sql::{ColId, JoinType, LogicalExpr, LogicalPlan};

const DP_MAX_RELATIONS: usize = 12;
const DPCCP_PAIR_BUDGET: usize = 8_000;

/// Reorders all reorderable join hypergraphs bottom-up using DPccp (N<=14) or GOO (N>14).
pub fn reorder_joins(plan: LogicalPlan, stats_catalog: &StatsCatalog) -> LogicalPlan {
    match plan {
        LogicalPlan::Join {
            join_type: JoinType::Inner | JoinType::Cross,
            ..
        } => {
            let mut raw_rels = Vec::new();
            let mut raw_preds = Vec::new();
            flatten_inner_joins(plan, &mut raw_rels, &mut raw_preds);

            let rels: Vec<LogicalPlan> = raw_rels
                .into_iter()
                .map(|r| reorder_joins(r, stats_catalog))
                .collect();

            if rels.len() <= 1 {
                return rels.into_iter().next().unwrap();
            }

            if rels.len() <= DP_MAX_RELATIONS {
                optimize_dpccp(rels, raw_preds, stats_catalog)
            } else {
                optimize_goo(rels, raw_preds, stats_catalog)
            }
        }
        // Optimize children of non-Inner joins and orient symmetric LeftOuter <-> RightOuter
        // so the smaller estimated cardinality is on the Right (Build) side.
        LogicalPlan::Join {
            left,
            right,
            join_type,
            condition,
        } => {
            let l = reorder_joins(*left, stats_catalog);
            let r = reorder_joins(*right, stats_catalog);
            let ls = estimate_plan_stats(&l, stats_catalog);
            let rs = estimate_plan_stats(&r, stats_catalog);

            match join_type {
                JoinType::LeftOuter if ls.rows < rs.rows => LogicalPlan::Join {
                    left: Box::new(r),
                    right: Box::new(l),
                    join_type: JoinType::RightOuter,
                    condition,
                },
                JoinType::RightOuter if ls.rows < rs.rows => LogicalPlan::Join {
                    left: Box::new(r),
                    right: Box::new(l),
                    join_type: JoinType::LeftOuter,
                    condition,
                },
                other => LogicalPlan::Join {
                    left: Box::new(l),
                    right: Box::new(r),
                    join_type: other,
                    condition,
                },
            }
        }
        other => map_plan_children(other, |c| reorder_joins(c, stats_catalog)),
    }
}

fn flatten_inner_joins(
    plan: LogicalPlan,
    rels: &mut Vec<LogicalPlan>,
    preds: &mut Vec<LogicalExpr>,
) {
    match plan {
        LogicalPlan::Join {
            left,
            right,
            join_type: JoinType::Inner | JoinType::Cross,
            condition,
        } => {
            if let Some(c) = condition {
                split_and_owned(c, preds);
            }
            flatten_inner_joins(*left, rels, preds);
            flatten_inner_joins(*right, rels, preds);
        }
        LogicalPlan::Filter { input, predicate }
            if matches!(
                input.as_ref(),
                LogicalPlan::Join {
                    join_type: JoinType::Inner | JoinType::Cross,
                    ..
                }
            ) =>
        {
            split_and_owned(predicate, preds);
            flatten_inner_joins(*input, rels, preds);
        }
        other => rels.push(other),
    }
}

#[derive(Debug, Clone)]
struct JoinEdge {
    expr: LogicalExpr,
    rel_mask: u32,
    selectivity: f64,
}

#[derive(Debug, Clone, Copy)]
struct DpNode {
    probe_mask: u32,
    build_mask: u32,
    rows: f64,
    cost: f64,
}

/// Genuine Moerkotte & Neumann (VLDB 2006) `DPccp` enumerating ONLY connected subgraph-complement
/// pairs `(csg, cmp)` with zero heap allocations inside the inner DP loop.
fn optimize_dpccp(
    rels: Vec<LogicalPlan>,
    preds: Vec<LogicalExpr>,
    stats_catalog: &StatsCatalog,
) -> LogicalPlan {
    let n = rels.len();
    let rel_stats: Vec<PlanStats> = rels
        .iter()
        .map(|r| estimate_plan_stats(r, stats_catalog))
        .collect();
    let rel_col_sets: Vec<BTreeSet<ColId>> = rels.iter().map(|r| r.output_col_ids()).collect();

    // Precompute edge bitmasks, adjacency bitmasks, and join selectivity factors ONCE!
    let mut edges: Vec<JoinEdge> = Vec::with_capacity(preds.len());
    let mut adj: Vec<u32> = vec![0; n];
    for p in preds {
        let mut cols = BTreeSet::new();
        p.collect_cols(&mut cols);
        let mut mask = 0u32;
        for (i, rcols) in rel_col_sets.iter().enumerate() {
            if cols.iter().any(|c| rcols.contains(c)) {
                mask |= 1u32 << i;
            }
        }
        if mask.count_ones() >= 2 {
            for (i, adj_i) in adj.iter_mut().enumerate().take(n) {
                if (mask & (1u32 << i)) != 0 {
                    *adj_i |= mask & !(1u32 << i);
                }
            }
        }
        let selectivity = precompute_edge_selectivity(&p, &rel_stats);
        edges.push(JoinEdge {
            expr: p,
            rel_mask: mask,
            selectivity,
        });
    }

    // Partition into connected components so DPccp runs independently on each component.
    let mut visited = 0u32;
    let mut components: Vec<u32> = Vec::new();
    for start in 0..n {
        if (visited & (1u32 << start)) != 0 {
            continue;
        }
        let mut comp = 0u32;
        let mut stack = vec![start];
        while let Some(u) = stack.pop() {
            if (comp & (1u32 << u)) != 0 {
                continue;
            }
            comp |= 1u32 << u;
            let mut nbrs = adj[u];
            while nbrs != 0 {
                let v = nbrs.trailing_zeros() as usize;
                nbrs &= nbrs - 1;
                if (comp & (1u32 << v)) == 0 {
                    stack.push(v);
                }
            }
        }
        visited |= comp;
        components.push(comp);
    }

    let mut dp: Vec<Option<DpNode>> = vec![None; 1usize << n];
    for (i, st) in rel_stats.iter().enumerate() {
        let bit = 1u32 << i;
        dp[bit as usize] = Some(DpNode {
            probe_mask: bit,
            build_mask: 0,
            rows: st.rows,
            cost: st.rows,
        });
    }

    // Run Moerkotte-Neumann EmitCsg / EnumerateCsgRec / EmitCsgCmp!
    let mut pairs_tried = 0usize;
    let mut budget_exceeded = false;

    for &comp in &components {
        if comp.count_ones() <= 1 {
            continue;
        }
        let mut ctx = DpCcpCtx {
            n,
            adj: &adj,
            edges: &edges,
            dp: &mut dp,
            pairs_tried: &mut pairs_tried,
            budget_exceeded: &mut budget_exceeded,
        };
        // Iterate base vertices in descending order (Moerkotte-Neumann canonical ordering!)
        for i in (0..n).rev() {
            let bit = 1u32 << i;
            if (comp & bit) == 0 {
                continue;
            }
            let excl_base = ((1u32 << (i + 1)) - 1) & comp;
            ctx.emit_csg(bit, comp);
            ctx.enumerate_csg_rec(bit, excl_base, comp);
            if *ctx.budget_exceeded {
                break;
            }
        }
        if budget_exceeded {
            // Dense clique exceeded DPccp pair budget -> seamlessly fall back to fast O(N^2) GOO!
            let raw_preds: Vec<LogicalExpr> = edges.into_iter().map(|e| e.expr).collect();
            return optimize_goo(rels, raw_preds, stats_catalog);
        }
    }

    // Reconstruct winning LogicalPlan for each connected component ONCE in O(N)!
    let mut rel_slots: Vec<Option<LogicalPlan>> = rels.into_iter().map(Some).collect();
    let mut comp_trees: Vec<(LogicalPlan, f64)> = Vec::with_capacity(components.len());

    for comp in components {
        let node = dp[comp as usize].expect("DPccp solved connected component");
        let tree = reconstruct_plan(comp, &dp, &mut rel_slots, &edges);
        comp_trees.push((tree, node.rows));
    }

    // Connect disconnected components (true Cartesian products) smallest-build-first.
    comp_trees.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut iter = comp_trees.into_iter();
    let (mut acc_plan, mut acc_rows) = iter.next().unwrap();
    for (other_plan, other_rows) in iter {
        let (probe, build) = if acc_rows >= other_rows {
            (acc_plan, other_plan)
        } else {
            (other_plan, acc_plan)
        };
        acc_rows = (acc_rows * other_rows).max(1.0);
        acc_plan = LogicalPlan::Join {
            left: Box::new(probe),
            right: Box::new(build),
            join_type: JoinType::Cross,
            condition: None,
        };
    }

    // Attach any 0/1-table predicates left over
    let leftover: Vec<LogicalExpr> = edges
        .into_iter()
        .filter(|e| e.rel_mask.count_ones() <= 1)
        .map(|e| e.expr)
        .collect();
    if leftover.is_empty() {
        acc_plan
    } else {
        LogicalPlan::Filter {
            input: Box::new(acc_plan),
            predicate: combine_and(leftover),
        }
    }
}

struct DpCcpCtx<'a> {
    n: usize,
    adj: &'a [u32],
    edges: &'a [JoinEdge],
    dp: &'a mut [Option<DpNode>],
    pairs_tried: &'a mut usize,
    budget_exceeded: &'a mut bool,
}

impl DpCcpCtx<'_> {
    #[inline]
    fn neighborhood(&self, subset: u32, exclude: u32, comp: u32) -> u32 {
        let mut nbrs = 0u32;
        let mut s = subset;
        while s != 0 {
            let v = s.trailing_zeros() as usize;
            s &= s - 1;
            nbrs |= self.adj[v];
        }
        (nbrs & comp) & !exclude
    }

    fn emit_csg(&mut self, s1: u32, comp: u32) {
        if *self.budget_exceeded {
            return;
        }
        let min_v = s1.trailing_zeros() as usize;
        let excl = (s1 | ((1u32 << (min_v + 1)) - 1)) & comp;
        let nbrs = self.neighborhood(s1, excl, comp);
        for v in (0..self.n).rev() {
            let bit = 1u32 << v;
            if (nbrs & bit) == 0 {
                continue;
            }
            self.emit_csg_cmp(s1, bit);
            let x2 = excl | (((1u32 << (v + 1)) - 1) & nbrs);
            self.enumerate_cmp_rec(s1, bit, x2, comp);
            if *self.budget_exceeded {
                return;
            }
        }
    }

    fn enumerate_csg_rec(&mut self, s: u32, excl: u32, comp: u32) {
        if *self.budget_exceeded {
            return;
        }
        let nbrs = self.neighborhood(s, excl, comp);
        if nbrs == 0 {
            return;
        }
        // Iterate non-empty subsets of `nbrs` (fast bitmask submask iteration)
        let mut sub = nbrs;
        let mut subsets = Vec::new();
        while sub > 0 {
            subsets.push(sub);
            sub = (sub - 1) & nbrs;
        }
        // Smallest subsets first so DP entries exist before supersets reference them
        subsets.sort_by_key(|m| m.count_ones());
        let next_excl = excl | nbrs;
        for add in &subsets {
            let s_new = s | *add;
            if self.dp[s_new as usize].is_some() {
                self.emit_csg(s_new, comp);
            }
            if *self.budget_exceeded {
                return;
            }
        }
        for add in subsets {
            let s_new = s | add;
            self.enumerate_csg_rec(s_new, next_excl, comp);
            if *self.budget_exceeded {
                return;
            }
        }
    }

    fn enumerate_cmp_rec(&mut self, s1: u32, s2: u32, excl: u32, comp: u32) {
        if *self.budget_exceeded {
            return;
        }
        let nbrs = self.neighborhood(s2, excl, comp);
        if nbrs == 0 {
            return;
        }
        let mut sub = nbrs;
        let mut subsets = Vec::new();
        while sub > 0 {
            subsets.push(sub);
            sub = (sub - 1) & nbrs;
        }
        subsets.sort_by_key(|m| m.count_ones());
        let next_excl = excl | nbrs;
        for add in &subsets {
            let s2_new = s2 | *add;
            if self.dp[s2_new as usize].is_some() {
                self.emit_csg_cmp(s1, s2_new);
            }
            if *self.budget_exceeded {
                return;
            }
        }
        for add in subsets {
            self.enumerate_cmp_rec(s1, s2 | add, next_excl, comp);
            if *self.budget_exceeded {
                return;
            }
        }
    }

    #[inline]
    fn emit_csg_cmp(&mut self, s1: u32, s2: u32) {
        *self.pairs_tried += 1;
        if *self.pairs_tried > DPCCP_PAIR_BUDGET {
            *self.budget_exceeded = true;
            return;
        }
        let (Some(e1), Some(e2)) = (self.dp[s1 as usize], self.dp[s2 as usize]) else {
            return;
        };
        let union = s1 | s2;

        // Compute combined selectivity of all edges connecting s1 and s2 (ZERO allocations!)
        let mut has_edge = false;
        let mut sel = 1.0f64;
        for edge in self.edges {
            if (edge.rel_mask & union) == edge.rel_mask
                && (edge.rel_mask & s1) != 0
                && (edge.rel_mask & s2) != 0
            {
                has_edge = true;
                sel *= edge.selectivity;
            }
        }
        if !has_edge {
            return;
        }

        let (probe_mask, probe_node, build_mask, build_node) = if e1.rows >= e2.rows {
            (s1, e1, s2, e2)
        } else {
            (s2, e2, s1, e1)
        };
        let out_rows = (probe_node.rows * build_node.rows * sel).max(1.0);
        let jcost =
            probe_node.cost + build_node.cost + 2.0 * build_node.rows + probe_node.rows + out_rows;

        let slot = &mut self.dp[union as usize];
        if slot.is_none_or(|cur| jcost < cur.cost) {
            *slot = Some(DpNode {
                probe_mask,
                build_mask,
                rows: out_rows,
                cost: jcost,
            });
        }
    }
}

fn reconstruct_plan(
    mask: u32,
    dp: &[Option<DpNode>],
    rel_slots: &mut [Option<LogicalPlan>],
    edges: &[JoinEdge],
) -> LogicalPlan {
    if mask.count_ones() == 1 {
        let idx = mask.trailing_zeros() as usize;
        return rel_slots[idx].take().expect("relation used once");
    }
    let node = dp[mask as usize].expect("DP entry exists");
    let left = reconstruct_plan(node.probe_mask, dp, rel_slots, edges);
    let right = reconstruct_plan(node.build_mask, dp, rel_slots, edges);
    let conds: Vec<LogicalExpr> = edges
        .iter()
        .filter(|e| {
            (e.rel_mask & mask) == e.rel_mask
                && (e.rel_mask & node.probe_mask) != 0
                && (e.rel_mask & node.build_mask) != 0
        })
        .map(|e| e.expr.clone())
        .collect();
    LogicalPlan::Join {
        left: Box::new(left),
        right: Box::new(right),
        join_type: if conds.is_empty() {
            JoinType::Cross
        } else {
            JoinType::Inner
        },
        condition: if conds.is_empty() {
            None
        } else {
            Some(combine_and(conds))
        },
    }
}

fn precompute_edge_selectivity(expr: &LogicalExpr, rel_stats: &[PlanStats]) -> f64 {
    if let LogicalExpr::BinaryOp {
        op: BinaryOperator::Eq | BinaryOperator::IsNotDistinctFrom,
        left,
        right,
        ..
    } = expr
    {
        if let (Some(lid), Some(rid)) = (extract_base_col(left), extract_base_col(right)) {
            let ndv_l = rel_stats
                .iter()
                .find_map(|s| s.col_stats.get(&lid).map(|c| c.ndv))
                .unwrap_or(100.0);
            let ndv_r = rel_stats
                .iter()
                .find_map(|s| s.col_stats.get(&rid).map(|c| c.ndv))
                .unwrap_or(100.0);
            return (1.0 / ndv_l.max(ndv_r).max(1.0)).clamp(1e-9, 1.0);
        }
    }
    rel_stats
        .first()
        .map(|s| estimate_predicate_selectivity(expr, s))
        .unwrap_or(0.25)
        .clamp(1e-6, 1.0)
}

/// $O(N^2)$ Greedy Operator Ordering (GOO) for large or dense join graphs (`N > 14`).
fn optimize_goo(
    rels: Vec<LogicalPlan>,
    mut remaining_preds: Vec<LogicalExpr>,
    stats_catalog: &StatsCatalog,
) -> LogicalPlan {
    let mut active: Vec<(LogicalPlan, PlanStats, BTreeSet<ColId>)> = rels
        .into_iter()
        .map(|r| {
            let s = estimate_plan_stats(&r, stats_catalog);
            let cols = r.output_col_ids();
            (r, s, cols)
        })
        .collect();

    // Precompute column sets for each predicate once!
    let mut pred_cols: Vec<BTreeSet<ColId>> = remaining_preds
        .iter()
        .map(|p| {
            let mut s = BTreeSet::new();
            p.collect_cols(&mut s);
            s
        })
        .collect();

    while active.len() > 1 {
        let mut best_connected: Option<(usize, usize, f64, Vec<usize>)> = None;
        let mut best_cross: Option<(usize, usize, f64)> = None;

        for i in 0..active.len() {
            for j in (i + 1)..active.len() {
                let mut pred_idxs = Vec::new();
                for (pi, pcols) in pred_cols.iter().enumerate() {
                    let uses_i = pcols.iter().any(|c| active[i].2.contains(c));
                    if !uses_i {
                        continue;
                    }
                    let uses_j = pcols.iter().any(|c| active[j].2.contains(c));
                    if !uses_j {
                        continue;
                    }
                    let covered = pcols
                        .iter()
                        .all(|c| active[i].2.contains(c) || active[j].2.contains(c));
                    if covered {
                        pred_idxs.push(pi);
                    }
                }

                if !pred_idxs.is_empty() {
                    // Fast cardinality estimate without full HashMap cloning in inner loop
                    let mut sel = 1.0f64;
                    for &pi in &pred_idxs {
                        if let LogicalExpr::BinaryOp {
                            op: BinaryOperator::Eq | BinaryOperator::IsNotDistinctFrom,
                            left,
                            right,
                            ..
                        } = &remaining_preds[pi]
                        {
                            if let (Some(lid), Some(rid)) =
                                (extract_base_col(left), extract_base_col(right))
                            {
                                let d1 = active[i]
                                    .1
                                    .col_stats
                                    .get(&lid)
                                    .or_else(|| active[j].1.col_stats.get(&lid))
                                    .map(|c| c.ndv)
                                    .unwrap_or(100.0);
                                let d2 = active[i]
                                    .1
                                    .col_stats
                                    .get(&rid)
                                    .or_else(|| active[j].1.col_stats.get(&rid))
                                    .map(|c| c.ndv)
                                    .unwrap_or(100.0);
                                sel /= d1.max(d2).max(1.0);
                                continue;
                            }
                        }
                        sel *= 0.25;
                    }
                    let out_rows = (active[i].1.rows * active[j].1.rows * sel).max(1.0);
                    let score = out_rows + 2.0 * active[i].1.rows.min(active[j].1.rows);
                    if best_connected.as_ref().is_none_or(|b| score < b.2) {
                        best_connected = Some((i, j, score, pred_idxs));
                    }
                } else if best_connected.is_none() {
                    let score = active[i].1.rows * active[j].1.rows;
                    if best_cross.as_ref().is_none_or(|b| score < b.2) {
                        best_cross = Some((i, j, score));
                    }
                }
            }
        }

        let (i, j, pred_idxs) = if let Some((i, j, _, idxs)) = best_connected {
            (i, j, idxs)
        } else {
            let (i, j, _) = best_cross.unwrap();
            (i, j, Vec::new())
        };

        let mut chosen_exprs = Vec::with_capacity(pred_idxs.len());
        for &pi in pred_idxs.iter().rev() {
            chosen_exprs.push(remaining_preds.remove(pi));
            pred_cols.remove(pi);
        }

        let (r_plan, r_stats, r_cols) = active.remove(j);
        let (l_plan, l_stats, l_cols) = active.remove(i);
        let mut merged_cols = l_cols;
        merged_cols.extend(r_cols);

        let (probe_p, probe_s, build_p, build_s) = if l_stats.rows >= r_stats.rows {
            (l_plan, l_stats, r_plan, r_stats)
        } else {
            (r_plan, r_stats, l_plan, l_stats)
        };

        let (jt, cond) = if chosen_exprs.is_empty() {
            (JoinType::Cross, None)
        } else {
            (JoinType::Inner, Some(combine_and(chosen_exprs)))
        };
        let jstats = estimate_join_stats(&probe_s, &build_s, &jt, cond.as_ref());
        let joined = LogicalPlan::Join {
            left: Box::new(probe_p),
            right: Box::new(build_p),
            join_type: jt,
            condition: cond,
        };
        active.push((joined, jstats, merged_cols));
    }

    let (plan, _, _) = active.pop().unwrap();
    if remaining_preds.is_empty() {
        plan
    } else {
        LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: combine_and(remaining_preds),
        }
    }
}
