#![deny(unsafe_code)]

pub mod cardinality;
pub mod join_order;
pub mod optimizer;
pub mod physical_plan;
pub mod rules;
pub mod stats;

pub use cardinality::{estimate_join_stats, estimate_plan_stats, PlanStats};
pub use join_order::reorder_joins;
pub use optimizer::{OptimizedBundle, Optimizer};
pub use physical_plan::{
    build_physical_plan, compile_expr, PhysicalAggExpr, PhysicalJoinType, PhysicalOp, PhysicalPlan,
    PhysicalSortExpr, PhysicalWindowExpr,
};
pub use rules::{apply_logical_rewrites, fold_constants_plan, prune_projections, simplify_expr};
pub use stats::{
    ColumnStats, EquiHeightHistogram, HistogramBucket, HyperLogLog, StatsCatalog, TableStats,
};
