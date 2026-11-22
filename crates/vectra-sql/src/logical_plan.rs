use crate::ast::{CopyFormat, ExplainFormat, SetOperator, WindowFrame};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use vectra_expr::{BinaryOperator, ScalarFunction, UnaryOperator};
use vectra_types::{Field, LogicalType, ScalarValue, Schema, SortOrder};

/// Globally unique bound column identifier across an entire query plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ColId(pub u32);

/// Monotonic generator for unique `ColId`s.
#[derive(Debug, Default, Clone)]
pub struct ColIdGen {
    next: u32,
}

impl ColIdGen {
    pub fn new() -> Self {
        Self { next: 1 }
    }

    pub fn next_id(&mut self) -> ColId {
        let id = ColId(self.next);
        self.next += 1;
        id
    }

    pub fn current_next(&self) -> u32 {
        self.next
    }

    pub fn set_min_next(&mut self, min_next: u32) {
        if self.next < min_next {
            self.next = min_next;
        }
    }
}

/// Named, typed output column with unique `ColId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BoundCol {
    pub id: ColId,
    pub qualifier: Option<String>,
    pub name: String,
    pub data_type: LogicalType,
    pub nullable: bool,
}

impl BoundCol {
    pub fn new(id: ColId, name: impl Into<String>, data_type: LogicalType) -> Self {
        Self {
            id,
            qualifier: None,
            name: name.into(),
            data_type,
            nullable: true,
        }
    }

    pub fn with_qualifier(mut self, q: impl Into<String>) -> Self {
        self.qualifier = Some(q.into());
        self
    }

    pub fn to_field(&self) -> Field {
        Field::new(self.name.clone(), self.data_type, self.nullable)
    }

    pub fn to_expr(&self) -> LogicalExpr {
        LogicalExpr::Col {
            id: self.id,
            name: self.name.clone(),
            data_type: self.data_type,
        }
    }
}

/// Bound, type-checked logical expression DAG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogicalExpr {
    Col {
        id: ColId,
        name: String,
        data_type: LogicalType,
    },
    /// Reference to an outer query scope column (eliminated by `M04` Neumann-Kemper unnesting!).
    CorrelatedCol {
        id: ColId,
        name: String,
        data_type: LogicalType,
        depth: usize,
    },
    Literal {
        value: ScalarValue,
        data_type: LogicalType,
    },
    BinaryOp {
        op: BinaryOperator,
        left: Box<LogicalExpr>,
        right: Box<LogicalExpr>,
        return_type: LogicalType,
    },
    UnaryOp {
        op: UnaryOperator,
        expr: Box<LogicalExpr>,
        return_type: LogicalType,
    },
    Cast {
        expr: Box<LogicalExpr>,
        target_type: LogicalType,
        try_cast: bool,
    },
    CaseWhen {
        branches: Vec<(LogicalExpr, LogicalExpr)>,
        else_expr: Option<Box<LogicalExpr>>,
        return_type: LogicalType,
    },
    Coalesce {
        args: Vec<LogicalExpr>,
        return_type: LogicalType,
    },
    InList {
        expr: Box<LogicalExpr>,
        list: Vec<LogicalExpr>,
        negated: bool,
    },
    Like {
        expr: Box<LogicalExpr>,
        pattern: Box<LogicalExpr>,
        escape: Option<char>,
        case_insensitive: bool,
        negated: bool,
    },
    Between {
        expr: Box<LogicalExpr>,
        low: Box<LogicalExpr>,
        high: Box<LogicalExpr>,
        negated: bool,
    },
    ScalarFunc {
        func: ScalarFunction,
        args: Vec<LogicalExpr>,
        return_type: LogicalType,
    },
}

impl LogicalExpr {
    pub fn data_type(&self) -> LogicalType {
        match self {
            Self::Col { data_type, .. }
            | Self::CorrelatedCol { data_type, .. }
            | Self::Literal { data_type, .. } => *data_type,
            Self::BinaryOp { return_type, .. }
            | Self::UnaryOp { return_type, .. }
            | Self::CaseWhen { return_type, .. }
            | Self::Coalesce { return_type, .. }
            | Self::ScalarFunc { return_type, .. } => *return_type,
            Self::Cast { target_type, .. } => *target_type,
            Self::InList { .. } | Self::Like { .. } | Self::Between { .. } => LogicalType::Boolean,
        }
    }

    /// Collects all local `ColId`s referenced by this expression.
    pub fn collect_cols(&self, out: &mut BTreeSet<ColId>) {
        match self {
            Self::Col { id, .. } => {
                out.insert(*id);
            }
            Self::CorrelatedCol { .. } | Self::Literal { .. } => {}
            Self::BinaryOp { left, right, .. } => {
                left.collect_cols(out);
                right.collect_cols(out);
            }
            Self::UnaryOp { expr, .. } | Self::Cast { expr, .. } => expr.collect_cols(out),
            Self::CaseWhen {
                branches,
                else_expr,
                ..
            } => {
                for (c, t) in branches {
                    c.collect_cols(out);
                    t.collect_cols(out);
                }
                if let Some(e) = else_expr {
                    e.collect_cols(out);
                }
            }
            Self::Coalesce { args, .. } | Self::ScalarFunc { args, .. } => {
                for a in args {
                    a.collect_cols(out);
                }
            }
            Self::InList { expr, list, .. } => {
                expr.collect_cols(out);
                for item in list {
                    item.collect_cols(out);
                }
            }
            Self::Like { expr, pattern, .. } => {
                expr.collect_cols(out);
                pattern.collect_cols(out);
            }
            Self::Between {
                expr, low, high, ..
            } => {
                expr.collect_cols(out);
                low.collect_cols(out);
                high.collect_cols(out);
            }
        }
    }

    /// Collects all `CorrelatedCol` IDs referenced by this expression.
    pub fn collect_correlated_cols(&self, out: &mut BTreeSet<ColId>) {
        match self {
            Self::CorrelatedCol { id, .. } => {
                out.insert(*id);
            }
            Self::Col { .. } | Self::Literal { .. } => {}
            Self::BinaryOp { left, right, .. } => {
                left.collect_correlated_cols(out);
                right.collect_correlated_cols(out);
            }
            Self::UnaryOp { expr, .. } | Self::Cast { expr, .. } => {
                expr.collect_correlated_cols(out)
            }
            Self::CaseWhen {
                branches,
                else_expr,
                ..
            } => {
                for (c, t) in branches {
                    c.collect_correlated_cols(out);
                    t.collect_correlated_cols(out);
                }
                if let Some(e) = else_expr {
                    e.collect_correlated_cols(out);
                }
            }
            Self::Coalesce { args, .. } | Self::ScalarFunc { args, .. } => {
                for a in args {
                    a.collect_correlated_cols(out);
                }
            }
            Self::InList { expr, list, .. } => {
                expr.collect_correlated_cols(out);
                for item in list {
                    item.collect_correlated_cols(out);
                }
            }
            Self::Like { expr, pattern, .. } => {
                expr.collect_correlated_cols(out);
                pattern.collect_correlated_cols(out);
            }
            Self::Between {
                expr, low, high, ..
            } => {
                expr.collect_correlated_cols(out);
                low.collect_correlated_cols(out);
                high.collect_correlated_cols(out);
            }
        }
    }

    /// Rewrites `CorrelatedCol`s matching `map` into regular `Col` references.
    pub fn rewrite_correlated(&self, map: &std::collections::HashMap<ColId, BoundCol>) -> Self {
        match self {
            Self::CorrelatedCol { id, .. } => {
                if let Some(bc) = map.get(id) {
                    bc.to_expr()
                } else {
                    self.clone()
                }
            }
            Self::Col { .. } | Self::Literal { .. } => self.clone(),
            Self::BinaryOp {
                op,
                left,
                right,
                return_type,
            } => Self::BinaryOp {
                op: *op,
                left: Box::new(left.rewrite_correlated(map)),
                right: Box::new(right.rewrite_correlated(map)),
                return_type: *return_type,
            },
            Self::UnaryOp {
                op,
                expr,
                return_type,
            } => Self::UnaryOp {
                op: *op,
                expr: Box::new(expr.rewrite_correlated(map)),
                return_type: *return_type,
            },
            Self::Cast {
                expr,
                target_type,
                try_cast,
            } => Self::Cast {
                expr: Box::new(expr.rewrite_correlated(map)),
                target_type: *target_type,
                try_cast: *try_cast,
            },
            Self::CaseWhen {
                branches,
                else_expr,
                return_type,
            } => Self::CaseWhen {
                branches: branches
                    .iter()
                    .map(|(c, t)| (c.rewrite_correlated(map), t.rewrite_correlated(map)))
                    .collect(),
                else_expr: else_expr
                    .as_ref()
                    .map(|e| Box::new(e.rewrite_correlated(map))),
                return_type: *return_type,
            },
            Self::Coalesce { args, return_type } => Self::Coalesce {
                args: args.iter().map(|a| a.rewrite_correlated(map)).collect(),
                return_type: *return_type,
            },
            Self::InList {
                expr,
                list,
                negated,
            } => Self::InList {
                expr: Box::new(expr.rewrite_correlated(map)),
                list: list.iter().map(|e| e.rewrite_correlated(map)).collect(),
                negated: *negated,
            },
            Self::Like {
                expr,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => Self::Like {
                expr: Box::new(expr.rewrite_correlated(map)),
                pattern: Box::new(pattern.rewrite_correlated(map)),
                escape: *escape,
                case_insensitive: *case_insensitive,
                negated: *negated,
            },
            Self::Between {
                expr,
                low,
                high,
                negated,
            } => Self::Between {
                expr: Box::new(expr.rewrite_correlated(map)),
                low: Box::new(low.rewrite_correlated(map)),
                high: Box::new(high.rewrite_correlated(map)),
                negated: *negated,
            },
            Self::ScalarFunc {
                func,
                args,
                return_type,
            } => Self::ScalarFunc {
                func: *func,
                args: args.iter().map(|a| a.rewrite_correlated(map)).collect(),
                return_type: *return_type,
            },
        }
    }

    /// Rewrites regular `Col` IDs using `map` (used when cloning subplans with fresh `ColId`s).
    pub fn remap_cols(&self, map: &std::collections::HashMap<ColId, ColId>) -> Self {
        match self {
            Self::Col {
                id,
                name,
                data_type,
            } => Self::Col {
                id: map.get(id).copied().unwrap_or(*id),
                name: name.clone(),
                data_type: *data_type,
            },
            Self::CorrelatedCol { .. } | Self::Literal { .. } => self.clone(),
            Self::BinaryOp {
                op,
                left,
                right,
                return_type,
            } => Self::BinaryOp {
                op: *op,
                left: Box::new(left.remap_cols(map)),
                right: Box::new(right.remap_cols(map)),
                return_type: *return_type,
            },
            Self::UnaryOp {
                op,
                expr,
                return_type,
            } => Self::UnaryOp {
                op: *op,
                expr: Box::new(expr.remap_cols(map)),
                return_type: *return_type,
            },
            Self::Cast {
                expr,
                target_type,
                try_cast,
            } => Self::Cast {
                expr: Box::new(expr.remap_cols(map)),
                target_type: *target_type,
                try_cast: *try_cast,
            },
            Self::CaseWhen {
                branches,
                else_expr,
                return_type,
            } => Self::CaseWhen {
                branches: branches
                    .iter()
                    .map(|(c, t)| (c.remap_cols(map), t.remap_cols(map)))
                    .collect(),
                else_expr: else_expr.as_ref().map(|e| Box::new(e.remap_cols(map))),
                return_type: *return_type,
            },
            Self::Coalesce { args, return_type } => Self::Coalesce {
                args: args.iter().map(|a| a.remap_cols(map)).collect(),
                return_type: *return_type,
            },
            Self::InList {
                expr,
                list,
                negated,
            } => Self::InList {
                expr: Box::new(expr.remap_cols(map)),
                list: list.iter().map(|e| e.remap_cols(map)).collect(),
                negated: *negated,
            },
            Self::Like {
                expr,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => Self::Like {
                expr: Box::new(expr.remap_cols(map)),
                pattern: Box::new(pattern.remap_cols(map)),
                escape: *escape,
                case_insensitive: *case_insensitive,
                negated: *negated,
            },
            Self::Between {
                expr,
                low,
                high,
                negated,
            } => Self::Between {
                expr: Box::new(expr.remap_cols(map)),
                low: Box::new(low.remap_cols(map)),
                high: Box::new(high.remap_cols(map)),
                negated: *negated,
            },
            Self::ScalarFunc {
                func,
                args,
                return_type,
            } => Self::ScalarFunc {
                func: *func,
                args: args.iter().map(|a| a.remap_cols(map)).collect(),
                return_type: *return_type,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AggregateFunc {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    StddevSamp,
    StddevPop,
    BoolAnd,
    BoolOr,
    StringAgg,
    ApproxCountDistinct,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregateExpr {
    pub func: AggregateFunc,
    pub args: Vec<LogicalExpr>,
    pub distinct: bool,
    pub filter: Option<LogicalExpr>,
    pub return_type: LogicalType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    Ntile,
    Lead,
    Lag,
    FirstValue,
    LastValue,
    Aggregate(AggregateFunc),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogicalSortExpr {
    pub expr: LogicalExpr,
    pub order: SortOrder,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowExpr {
    pub func: WindowFunc,
    pub args: Vec<LogicalExpr>,
    pub partition_by: Vec<LogicalExpr>,
    pub order_by: Vec<LogicalSortExpr>,
    pub frame: Option<WindowFrame>,
    pub return_type: LogicalType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JoinType {
    Inner,
    LeftOuter,
    RightOuter,
    FullOuter,
    Cross,
    LeftSemi,
    LeftAnti,
    RightSemi,
    RightAnti,
    /// Preserves 3-valued NULL semantics for `OR EXISTS`, `IN`, and `NOT IN` subqueries!
    LeftMark {
        mark_col: BoundCol,
        /// Optional `(lhs_probe_expr, rhs_build_expr)` pair for SQL `IN`/`NOT IN` NULL propagation.
        probe_pair: Option<(LogicalExpr, LogicalExpr)>,
    },
    /// Scalar subquery join: left outer join asserting at most 1 matching right row per left row.
    Single,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TableFunctionKind {
    GenerateSeries {
        start: i64,
        stop: i64,
        step: i64,
    },
    ReadParquet {
        path: String,
    },
    ReadCsv {
        path: String,
        delimiter: u8,
        header: bool,
    },
}

/// Core Relational Algebra Logical Plan IR (M04 output -> M05 optimizer input).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogicalPlan {
    EmptyRelation {
        produce_one_row: bool,
        columns: Vec<BoundCol>,
    },
    Values {
        rows: Vec<Vec<LogicalExpr>>,
        columns: Vec<BoundCol>,
    },
    Scan {
        table_name: String,
        alias: String,
        columns: Vec<BoundCol>,
        /// Original 0-based column indices inside the storage `Table::schema`.
        table_col_indices: Vec<usize>,
        pushed_filters: Vec<LogicalExpr>,
    },
    TableFunc {
        func: TableFunctionKind,
        columns: Vec<BoundCol>,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: LogicalExpr,
    },
    Project {
        input: Box<LogicalPlan>,
        exprs: Vec<(LogicalExpr, BoundCol)>,
    },
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        join_type: JoinType,
        condition: Option<LogicalExpr>,
    },
    /// Correlated subquery representation before Neumann-Kemper unnesting (`M04`).
    DependentJoin {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        join_type: JoinType,
        condition: Option<LogicalExpr>,
        correlated_cols: Vec<BoundCol>,
    },
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<(LogicalExpr, BoundCol)>,
        /// Optional grouping sets (each set lists indices into `group_by`).
        grouping_sets: Option<Vec<Vec<usize>>>,
        grouping_id_col: Option<BoundCol>,
        aggregates: Vec<(AggregateExpr, BoundCol)>,
    },
    Window {
        input: Box<LogicalPlan>,
        windows: Vec<(WindowExpr, BoundCol)>,
    },
    Sort {
        input: Box<LogicalPlan>,
        order_by: Vec<LogicalSortExpr>,
    },
    Limit {
        input: Box<LogicalPlan>,
        limit: Option<usize>,
        offset: usize,
    },
    TopN {
        input: Box<LogicalPlan>,
        order_by: Vec<LogicalSortExpr>,
        limit: usize,
        offset: usize,
    },
    Distinct {
        input: Box<LogicalPlan>,
        on_cols: Vec<BoundCol>,
    },
    SetOp {
        op: SetOperator,
        all: bool,
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        output_cols: Vec<BoundCol>,
    },
    RecursiveCte {
        name: String,
        anchor: Box<LogicalPlan>,
        recursive: Box<LogicalPlan>,
        union_all: bool,
        cte_cols: Vec<BoundCol>,
        consumer: Box<LogicalPlan>,
    },
    CteScan {
        cte_name: String,
        columns: Vec<BoundCol>,
    },
}

impl LogicalPlan {
    /// Returns the ordered output columns of this logical plan node.
    pub fn output_cols(&self) -> Vec<BoundCol> {
        match self {
            Self::EmptyRelation { columns, .. }
            | Self::Values { columns, .. }
            | Self::Scan { columns, .. }
            | Self::TableFunc { columns, .. }
            | Self::CteScan { columns, .. } => columns.clone(),
            Self::Filter { input, .. }
            | Self::Sort { input, .. }
            | Self::Limit { input, .. }
            | Self::TopN { input, .. } => input.output_cols(),
            Self::Distinct { on_cols, .. } => on_cols.clone(),
            Self::Project { exprs, .. } => exprs.iter().map(|(_, c)| c.clone()).collect(),
            Self::Join {
                left,
                right,
                join_type,
                ..
            }
            | Self::DependentJoin {
                left,
                right,
                join_type,
                ..
            } => match join_type {
                JoinType::LeftSemi | JoinType::LeftAnti => left.output_cols(),
                JoinType::RightSemi | JoinType::RightAnti => right.output_cols(),
                JoinType::LeftMark { mark_col, .. } => {
                    let mut out = left.output_cols();
                    out.push(mark_col.clone());
                    out
                }
                JoinType::Inner
                | JoinType::LeftOuter
                | JoinType::RightOuter
                | JoinType::FullOuter
                | JoinType::Cross
                | JoinType::Single => {
                    let mut out = left.output_cols();
                    out.extend(right.output_cols());
                    out
                }
            },
            Self::Aggregate {
                group_by,
                grouping_id_col,
                aggregates,
                ..
            } => {
                let mut out: Vec<BoundCol> = group_by.iter().map(|(_, c)| c.clone()).collect();
                if let Some(gid) = grouping_id_col {
                    out.push(gid.clone());
                }
                out.extend(aggregates.iter().map(|(_, c)| c.clone()));
                out
            }
            Self::Window { input, windows } => {
                let mut out = input.output_cols();
                out.extend(windows.iter().map(|(_, c)| c.clone()));
                out
            }
            Self::SetOp { output_cols, .. } => output_cols.clone(),
            Self::RecursiveCte { consumer, .. } => consumer.output_cols(),
        }
    }

    pub fn output_schema(&self) -> Schema {
        Schema::new(self.output_cols().iter().map(BoundCol::to_field).collect())
    }

    pub fn output_col_ids(&self) -> BTreeSet<ColId> {
        self.output_cols().into_iter().map(|c| c.id).collect()
    }

    /// Returns true if any `DependentJoin` node remains anywhere in the plan tree.
    pub fn contains_dependent_join(&self) -> bool {
        match self {
            Self::DependentJoin { .. } => true,
            Self::EmptyRelation { .. }
            | Self::Values { .. }
            | Self::Scan { .. }
            | Self::TableFunc { .. }
            | Self::CteScan { .. } => false,
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Aggregate { input, .. }
            | Self::Window { input, .. }
            | Self::Sort { input, .. }
            | Self::Limit { input, .. }
            | Self::TopN { input, .. }
            | Self::Distinct { input, .. } => input.contains_dependent_join(),
            Self::Join { left, right, .. } | Self::SetOp { left, right, .. } => {
                left.contains_dependent_join() || right.contains_dependent_join()
            }
            Self::RecursiveCte {
                anchor,
                recursive,
                consumer,
                ..
            } => {
                anchor.contains_dependent_join()
                    || recursive.contains_dependent_join()
                    || consumer.contains_dependent_join()
            }
        }
    }

    /// Collects all `CorrelatedCol` IDs referenced anywhere in this plan subtree.
    pub fn free_correlated_vars(&self) -> BTreeSet<ColId> {
        let mut out = BTreeSet::new();
        self.collect_correlated_vars_inner(&mut out);
        out
    }

    fn collect_correlated_vars_inner(&self, out: &mut BTreeSet<ColId>) {
        match self {
            Self::EmptyRelation { .. } | Self::TableFunc { .. } | Self::CteScan { .. } => {}
            Self::Values { rows, .. } => {
                for row in rows {
                    for e in row {
                        e.collect_correlated_cols(out);
                    }
                }
            }
            Self::Scan { pushed_filters, .. } => {
                for f in pushed_filters {
                    f.collect_correlated_cols(out);
                }
            }
            Self::Filter { input, predicate } => {
                input.collect_correlated_vars_inner(out);
                predicate.collect_correlated_cols(out);
            }
            Self::Project { input, exprs } => {
                input.collect_correlated_vars_inner(out);
                for (e, _) in exprs {
                    e.collect_correlated_cols(out);
                }
            }
            Self::Join {
                left,
                right,
                join_type,
                condition,
            } => {
                left.collect_correlated_vars_inner(out);
                right.collect_correlated_vars_inner(out);
                if let Some(c) = condition {
                    c.collect_correlated_cols(out);
                }
                if let JoinType::LeftMark {
                    probe_pair: Some((a, b)),
                    ..
                } = join_type
                {
                    a.collect_correlated_cols(out);
                    b.collect_correlated_cols(out);
                }
            }
            Self::DependentJoin {
                left,
                right,
                condition,
                join_type,
                ..
            } => {
                left.collect_correlated_vars_inner(out);
                right.collect_correlated_vars_inner(out);
                if let Some(c) = condition {
                    c.collect_correlated_cols(out);
                }
                if let JoinType::LeftMark {
                    probe_pair: Some((a, b)),
                    ..
                } = join_type
                {
                    a.collect_correlated_cols(out);
                    b.collect_correlated_cols(out);
                }
            }
            Self::Aggregate {
                input,
                group_by,
                aggregates,
                ..
            } => {
                input.collect_correlated_vars_inner(out);
                for (g, _) in group_by {
                    g.collect_correlated_cols(out);
                }
                for (a, _) in aggregates {
                    for arg in &a.args {
                        arg.collect_correlated_cols(out);
                    }
                    if let Some(f) = &a.filter {
                        f.collect_correlated_cols(out);
                    }
                }
            }
            Self::Window { input, windows } => {
                input.collect_correlated_vars_inner(out);
                for (w, _) in windows {
                    for a in &w.args {
                        a.collect_correlated_cols(out);
                    }
                    for p in &w.partition_by {
                        p.collect_correlated_cols(out);
                    }
                    for o in &w.order_by {
                        o.expr.collect_correlated_cols(out);
                    }
                }
            }
            Self::Sort { input, order_by }
            | Self::TopN {
                input, order_by, ..
            } => {
                input.collect_correlated_vars_inner(out);
                for o in order_by {
                    o.expr.collect_correlated_cols(out);
                }
            }
            Self::Limit { input, .. } | Self::Distinct { input, .. } => {
                input.collect_correlated_vars_inner(out);
            }
            Self::SetOp { left, right, .. } => {
                left.collect_correlated_vars_inner(out);
                right.collect_correlated_vars_inner(out);
            }
            Self::RecursiveCte {
                anchor,
                recursive,
                consumer,
                ..
            } => {
                anchor.collect_correlated_vars_inner(out);
                recursive.collect_correlated_vars_inner(out);
                consumer.collect_correlated_vars_inner(out);
            }
        }
    }

    /// Formats a clean human-readable ASCII tree of the `LogicalPlan` (for `EXPLAIN LOGICAL / UNNESTED`).
    pub fn format_tree(&self) -> String {
        let mut out = String::new();
        self.fmt_node(0, &mut out);
        out
    }

    fn fmt_node(&self, indent: usize, out: &mut String) {
        let pad = "  ".repeat(indent);
        match self {
            Self::EmptyRelation {
                produce_one_row, ..
            } => {
                out.push_str(&format!("{pad}EmptyRelation(one_row={produce_one_row})\n"));
            }
            Self::Values { rows, .. } => {
                out.push_str(&format!("{pad}Values(rows={})\n", rows.len()));
            }
            Self::Scan {
                table_name,
                alias,
                columns,
                pushed_filters,
                ..
            } => {
                let col_names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
                out.push_str(&format!(
                    "{pad}Scan({table_name} AS {alias}, cols=[{}], filters={})\n",
                    col_names.join(", "),
                    pushed_filters.len()
                ));
            }
            Self::TableFunc { func, .. } => {
                out.push_str(&format!("{pad}TableFunc({func:?})\n"));
            }
            Self::Filter { input, predicate } => {
                out.push_str(&format!("{pad}Filter({predicate:?})\n"));
                input.fmt_node(indent + 1, out);
            }
            Self::Project { input, exprs } => {
                let names: Vec<String> = exprs
                    .iter()
                    .map(|(_, c)| format!("{}#{}", c.name, c.id.0))
                    .collect();
                out.push_str(&format!("{pad}Project([{}])\n", names.join(", ")));
                input.fmt_node(indent + 1, out);
            }
            Self::Join {
                left,
                right,
                join_type,
                condition,
            } => {
                out.push_str(&format!(
                    "{pad}Join(type={join_type:?}, on={})\n",
                    condition.is_some()
                ));
                left.fmt_node(indent + 1, out);
                right.fmt_node(indent + 1, out);
            }
            Self::DependentJoin {
                left,
                right,
                join_type,
                correlated_cols,
                ..
            } => {
                let cids: Vec<String> = correlated_cols
                    .iter()
                    .map(|c| format!("{}#{}", c.name, c.id.0))
                    .collect();
                out.push_str(&format!(
                    "{pad}DependentJoin(type={join_type:?}, corr=[{}])\n",
                    cids.join(", ")
                ));
                left.fmt_node(indent + 1, out);
                right.fmt_node(indent + 1, out);
            }
            Self::Aggregate {
                input,
                group_by,
                aggregates,
                ..
            } => {
                out.push_str(&format!(
                    "{pad}Aggregate(groups={}, aggs={})\n",
                    group_by.len(),
                    aggregates.len()
                ));
                input.fmt_node(indent + 1, out);
            }
            Self::Window { input, windows } => {
                out.push_str(&format!("{pad}Window(funcs={})\n", windows.len()));
                input.fmt_node(indent + 1, out);
            }
            Self::Sort { input, order_by } => {
                out.push_str(&format!("{pad}Sort(keys={})\n", order_by.len()));
                input.fmt_node(indent + 1, out);
            }
            Self::Limit {
                input,
                limit,
                offset,
            } => {
                out.push_str(&format!("{pad}Limit(limit={limit:?}, offset={offset})\n"));
                input.fmt_node(indent + 1, out);
            }
            Self::TopN {
                input,
                limit,
                offset,
                ..
            } => {
                out.push_str(&format!("{pad}TopN(limit={limit}, offset={offset})\n"));
                input.fmt_node(indent + 1, out);
            }
            Self::Distinct { input, .. } => {
                out.push_str(&format!("{pad}Distinct\n"));
                input.fmt_node(indent + 1, out);
            }
            Self::SetOp {
                op,
                all,
                left,
                right,
                ..
            } => {
                out.push_str(&format!("{pad}SetOp({op:?}, all={all})\n"));
                left.fmt_node(indent + 1, out);
                right.fmt_node(indent + 1, out);
            }
            Self::RecursiveCte {
                name,
                anchor,
                recursive,
                union_all,
                consumer,
                ..
            } => {
                out.push_str(&format!("{pad}RecursiveCte({name}, all={union_all})\n"));
                anchor.fmt_node(indent + 1, out);
                recursive.fmt_node(indent + 1, out);
                consumer.fmt_node(indent + 1, out);
            }
            Self::CteScan { cte_name, .. } => {
                out.push_str(&format!("{pad}CteScan({cte_name})\n"));
            }
        }
    }
}

/// Top-level bound statement ready for optimization and execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogicalStatement {
    Query(Box<LogicalPlan>),
    CreateTable {
        name: String,
        if_not_exists: bool,
        or_replace: bool,
        schema: Schema,
        as_plan: Option<Box<LogicalPlan>>,
    },
    CreateView {
        name: String,
        or_replace: bool,
        sql_query: String,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    DropView {
        name: String,
        if_exists: bool,
    },
    Insert {
        table_name: String,
        target_col_indices: Vec<usize>,
        source: Box<LogicalPlan>,
    },
    Delete {
        table_name: String,
        filter: Option<LogicalExpr>,
        table_cols: Vec<BoundCol>,
    },
    Update {
        table_name: String,
        assignments: Vec<(usize, LogicalExpr)>,
        filter: Option<LogicalExpr>,
        table_cols: Vec<BoundCol>,
    },
    Copy {
        table_name: Option<String>,
        query: Option<Box<LogicalPlan>>,
        path: String,
        to_file: bool,
        format: CopyFormat,
        delimiter: u8,
        header: bool,
    },
    Analyze {
        table_name: Option<String>,
    },
    Begin,
    Commit,
    Rollback,
    Explain {
        format: ExplainFormat,
        bound_plan: Box<LogicalPlan>,
        unnested_plan: Box<LogicalPlan>,
    },
    Set {
        variable: String,
        value: String,
    },
    Show {
        variable: String,
    },
}
