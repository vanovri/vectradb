use crate::join_order::reorder_joins;
use crate::physical_plan::{build_physical_plan, PhysicalPlan};
use crate::rules::apply_logical_rewrites;
use crate::stats::StatsCatalog;
use serde::{Deserialize, Serialize};
use vectra_sql::{unnest_plan, ColIdGen, LogicalPlan};
use vectra_types::Result;

/// Captures every compilation stage for `EXPLAIN` and the M10 WASM Visual Plan DAG Explorer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizedBundle {
    pub bound_logical: LogicalPlan,
    pub unnested_logical: LogicalPlan,
    pub optimized_logical: LogicalPlan,
    pub physical: PhysicalPlan,
}

/// Top-level VectraDB Cost-Based Optimizer entry point.
pub struct Optimizer<'a> {
    stats: &'a StatsCatalog,
}

impl<'a> Optimizer<'a> {
    pub fn new(stats: &'a StatsCatalog) -> Self {
        Self { stats }
    }

    /// Runs full optimization pipeline:
    /// `Bound Logical -> Neumann-Kemper Unnest -> Heuristic Rewrites -> DPccp/GOO Join Reorder -> Physical Selection`.
    pub fn optimize_full(
        &self,
        bound_logical: LogicalPlan,
        col_gen: &mut ColIdGen,
    ) -> Result<OptimizedBundle> {
        let unnested_logical = unnest_plan(bound_logical.clone(), col_gen)?;
        let rewritten = apply_logical_rewrites(unnested_logical.clone());
        let reordered = reorder_joins(rewritten, self.stats);
        // Second lightweight projection prune after join reordering ensures clean scan footprints
        let req = reordered.output_col_ids();
        let optimized_logical = crate::rules::prune_projections(reordered, &req);
        let physical = build_physical_plan(&optimized_logical, self.stats)?;

        Ok(OptimizedBundle {
            bound_logical,
            unnested_logical,
            optimized_logical,
            physical,
        })
    }

    /// Optimizes an already-unnested `LogicalPlan` directly into a `PhysicalPlan`.
    pub fn optimize_logical(&self, unnested: LogicalPlan) -> Result<PhysicalPlan> {
        let rewritten = apply_logical_rewrites(unnested);
        let reordered = reorder_joins(rewritten, self.stats);
        let req = reordered.output_col_ids();
        let final_logical = crate::rules::prune_projections(reordered, &req);
        build_physical_plan(&final_logical, self.stats)
    }
}
