use crate::ast::*;
use crate::coercion::{arithmetic_result_type, coerce_expr, common_supertype};
use crate::logical_plan::*;
use crate::parser::parse_single_statement;
use crate::pretty::pretty_print_query;
use std::collections::{BTreeSet, HashMap};
use vectra_expr::{BinaryOperator, DatePart, ScalarFunction, UnaryOperator};
use vectra_storage::Catalog;
use vectra_types::{
    parse_date32, parse_decimal, parse_timestamp_micros, Field, LogicalType, Result, ScalarValue,
    Schema, SortOrder, VectraError, MAX_DECIMAL_PRECISION, MICROS_PER_HOUR, MICROS_PER_MINUTE,
    MICROS_PER_SECOND,
};

/// Scope mapping visible columns in a `SELECT` / subquery block.
#[derive(Debug, Clone, Default)]
struct Scope {
    cols: Vec<BoundCol>,
}

impl Scope {
    fn from_cols(cols: Vec<BoundCol>) -> Self {
        Self { cols }
    }

    fn extend(&mut self, more: Vec<BoundCol>) {
        self.cols.extend(more);
    }

    /// Resolves `(qualifier, name)` inside this scope, returning `Err` on ambiguity.
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<Option<BoundCol>> {
        let mut matches = Vec::new();
        for c in &self.cols {
            let name_ok = c.name.eq_ignore_ascii_case(name);
            let qual_ok = match qualifier {
                None => true,
                Some(q) => c
                    .qualifier
                    .as_deref()
                    .is_some_and(|cq| cq.eq_ignore_ascii_case(q)),
            };
            if name_ok && qual_ok {
                matches.push(c.clone());
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(matches.pop().unwrap())),
            _ => {
                // Deduplicate identical ColIds (e.g., from USING join).
                let first_id = matches[0].id;
                if matches.iter().all(|m| m.id == first_id) {
                    Ok(Some(matches.pop().unwrap()))
                } else {
                    let display = match qualifier {
                        Some(q) => format!("{q}.{name}"),
                        None => name.to_string(),
                    };
                    Err(VectraError::BindError(format!(
                        "ambiguous column reference `{display}`"
                    )))
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum CteEntry {
    NonRecursive { ast: Cte },
    RecursivePlaceholder { name: String, cols: Vec<BoundCol> },
    RecursiveBound { name: String, cols: Vec<BoundCol> },
}

/// Semantic SQL Binder transforming AST statements into type-checked `LogicalStatement`s.
pub struct Binder<'a> {
    catalog: &'a Catalog,
    views: &'a HashMap<String, String>,
    params: &'a [ScalarValue],
    col_gen: ColIdGen,
    outer_scopes: Vec<Scope>,
    ctes: HashMap<String, CteEntry>,
    expanding_ctes: Vec<String>,
    named_windows: HashMap<String, WindowSpec>,
}

impl<'a> Binder<'a> {
    pub fn new(catalog: &'a Catalog, views: &'a HashMap<String, String>) -> Self {
        Self::with_params(catalog, views, &[])
    }

    pub fn with_params(
        catalog: &'a Catalog,
        views: &'a HashMap<String, String>,
        params: &'a [ScalarValue],
    ) -> Self {
        Self {
            catalog,
            views,
            params,
            col_gen: ColIdGen::new(),
            outer_scopes: Vec::new(),
            ctes: HashMap::new(),
            expanding_ctes: Vec::new(),
            named_windows: HashMap::new(),
        }
    }

    pub fn next_col_id(&self) -> u32 {
        self.col_gen.current_next()
    }

    pub fn bind_statement(&mut self, stmt: &Statement) -> Result<LogicalStatement> {
        match stmt {
            Statement::Query(q) => Ok(LogicalStatement::Query(Box::new(self.bind_query(q)?))),
            Statement::CreateTable {
                name,
                if_not_exists,
                or_replace,
                columns,
                as_query,
            } => {
                if let Some(q) = as_query {
                    let plan = self.bind_query(q)?;
                    let schema = plan.output_schema();
                    Ok(LogicalStatement::CreateTable {
                        name: name.clone(),
                        if_not_exists: *if_not_exists,
                        or_replace: *or_replace,
                        schema,
                        as_plan: Some(Box::new(plan)),
                    })
                } else {
                    let fields = columns
                        .iter()
                        .map(|c| Field::new(c.name.clone(), c.data_type, c.nullable))
                        .collect();
                    Ok(LogicalStatement::CreateTable {
                        name: name.clone(),
                        if_not_exists: *if_not_exists,
                        or_replace: *or_replace,
                        schema: Schema::new(fields),
                        as_plan: None,
                    })
                }
            }
            Statement::CreateView {
                name,
                or_replace,
                column_aliases,
                query,
            } => {
                // Validate view query binds cleanly right away!
                let plan = self.bind_query(query)?;
                if !column_aliases.is_empty() && column_aliases.len() != plan.output_cols().len() {
                    return Err(VectraError::BindError(format!(
                        "VIEW `{name}` column alias count {} != query columns {}",
                        column_aliases.len(),
                        plan.output_cols().len()
                    )));
                }
                let mut sql_q = pretty_print_query(query);
                if !column_aliases.is_empty() {
                    let cols = column_aliases.join(", ");
                    sql_q = format!("SELECT * FROM ({sql_q}) AS _v ({cols})");
                }
                Ok(LogicalStatement::CreateView {
                    name: name.clone(),
                    or_replace: *or_replace,
                    sql_query: sql_q,
                })
            }
            Statement::DropTable { name, if_exists } => Ok(LogicalStatement::DropTable {
                name: name.clone(),
                if_exists: *if_exists,
            }),
            Statement::DropView { name, if_exists } => Ok(LogicalStatement::DropView {
                name: name.clone(),
                if_exists: *if_exists,
            }),
            Statement::Insert {
                table_name,
                columns,
                source,
            } => {
                let tbl = self.catalog.get_table(table_name)?;
                let target_col_indices: Vec<usize> = if columns.is_empty() {
                    (0..tbl.schema.len()).collect()
                } else {
                    let mut idxs = Vec::with_capacity(columns.len());
                    for cname in columns {
                        let idx = tbl.schema.index_of(cname).ok_or_else(|| {
                            VectraError::BindError(format!(
                                "column `{cname}` does not exist in table `{table_name}`"
                            ))
                        })?;
                        idxs.push(idx);
                    }
                    idxs
                };
                let src_plan = match source {
                    InsertSource::Values(rows) => {
                        let target_types: Vec<LogicalType> = target_col_indices
                            .iter()
                            .map(|&i| tbl.schema.field(i).logical_type)
                            .collect();
                        self.bind_values(rows, Some(&target_types))?
                    }
                    InsertSource::Query(q) => {
                        let p = self.bind_query(q)?;
                        let pcols = p.output_cols();
                        if pcols.len() != target_col_indices.len() {
                            return Err(VectraError::BindError(format!(
                                "INSERT column count mismatch: expected {}, got {}",
                                target_col_indices.len(),
                                pcols.len()
                            )));
                        }
                        let exprs: Vec<(LogicalExpr, BoundCol)> = pcols
                            .into_iter()
                            .zip(target_col_indices.iter())
                            .map(|(bc, &tidx)| {
                                let target_ty = tbl.schema.field(tidx).logical_type;
                                let coerced = coerce_expr(bc.to_expr(), target_ty);
                                let new_col =
                                    BoundCol::new(self.col_gen.next_id(), bc.name, target_ty);
                                (coerced, new_col)
                            })
                            .collect();
                        LogicalPlan::Project {
                            input: Box::new(p),
                            exprs,
                        }
                    }
                };
                Ok(LogicalStatement::Insert {
                    table_name: table_name.clone(),
                    target_col_indices,
                    source: Box::new(src_plan),
                })
            }
            Statement::Delete {
                table_name,
                selection,
            } => {
                let tbl = self.catalog.get_table(table_name)?;
                let mut cols = Vec::with_capacity(tbl.schema.len());
                for f in &tbl.schema.fields {
                    cols.push(
                        BoundCol::new(self.col_gen.next_id(), f.name.clone(), f.logical_type)
                            .with_qualifier(table_name.clone()),
                    );
                }
                let scope = Scope::from_cols(cols.clone());
                let filter = match selection {
                    Some(e) => {
                        let mut dummy = LogicalPlan::EmptyRelation {
                            produce_one_row: true,
                            columns: cols.clone(),
                        };
                        let bound = self.bind_expr(e, &scope, &mut dummy, false)?;
                        Some(coerce_expr(bound, LogicalType::Boolean))
                    }
                    None => None,
                };
                Ok(LogicalStatement::Delete {
                    table_name: table_name.clone(),
                    filter,
                    table_cols: cols,
                })
            }
            Statement::Update {
                table_name,
                assignments,
                selection,
            } => {
                let tbl = self.catalog.get_table(table_name)?;
                let mut cols = Vec::with_capacity(tbl.schema.len());
                for f in &tbl.schema.fields {
                    cols.push(
                        BoundCol::new(self.col_gen.next_id(), f.name.clone(), f.logical_type)
                            .with_qualifier(table_name.clone()),
                    );
                }
                let scope = Scope::from_cols(cols.clone());
                let mut dummy = LogicalPlan::EmptyRelation {
                    produce_one_row: true,
                    columns: cols.clone(),
                };
                let mut bound_assigns = Vec::with_capacity(assignments.len());
                for (cname, expr) in assignments {
                    let idx = tbl.schema.index_of(cname).ok_or_else(|| {
                        VectraError::BindError(format!(
                            "column `{cname}` does not exist in table `{table_name}`"
                        ))
                    })?;
                    let target_ty = tbl.schema.field(idx).logical_type;
                    let be = self.bind_expr(expr, &scope, &mut dummy, false)?;
                    bound_assigns.push((idx, coerce_expr(be, target_ty)));
                }
                let filter = match selection {
                    Some(e) => {
                        let be = self.bind_expr(e, &scope, &mut dummy, false)?;
                        Some(coerce_expr(be, LogicalType::Boolean))
                    }
                    None => None,
                };
                Ok(LogicalStatement::Update {
                    table_name: table_name.clone(),
                    assignments: bound_assigns,
                    filter,
                    table_cols: cols,
                })
            }
            Statement::Copy {
                target,
                path,
                to_file,
                format,
                delimiter,
                header,
            } => {
                let (table_name, query) = match target {
                    CopyTarget::Table(t) => (Some(t.clone()), None),
                    CopyTarget::Query(q) => (None, Some(Box::new(self.bind_query(q)?))),
                };
                Ok(LogicalStatement::Copy {
                    table_name,
                    query,
                    path: path.clone(),
                    to_file: *to_file,
                    format: *format,
                    delimiter: delimiter.map(|c| c as u8).unwrap_or(b','),
                    header: header.unwrap_or(true),
                })
            }
            Statement::Analyze { table_name } => Ok(LogicalStatement::Analyze {
                table_name: table_name.clone(),
            }),
            Statement::Begin => Ok(LogicalStatement::Begin),
            Statement::Commit => Ok(LogicalStatement::Commit),
            Statement::Rollback => Ok(LogicalStatement::Rollback),
            Statement::Explain { format, stmt } => {
                let bound = self.bind_statement(stmt)?;
                let LogicalStatement::Query(plan) = bound else {
                    return Err(VectraError::BindError(
                        "EXPLAIN requires a SELECT / query statement".into(),
                    ));
                };
                let unnested = crate::unnest::unnest_plan(*plan.clone(), &mut self.col_gen)?;
                Ok(LogicalStatement::Explain {
                    format: *format,
                    bound_plan: plan,
                    unnested_plan: Box::new(unnested),
                })
            }
            Statement::Set { variable, value } => Ok(LogicalStatement::Set {
                variable: variable.clone(),
                value: value.clone(),
            }),
            Statement::Show { variable } => Ok(LogicalStatement::Show {
                variable: variable.clone(),
            }),
        }
    }

    pub fn bind_query(&mut self, query: &Query) -> Result<LogicalPlan> {
        let saved_ctes = self.ctes.clone();
        let mut recursive_wrappers: Vec<(String, LogicalPlan, LogicalPlan, bool, Vec<BoundCol>)> =
            Vec::new();

        if let Some(with) = &query.with {
            for cte in &with.ctes {
                let key = cte.name.to_ascii_lowercase();
                if with.recursive {
                    if let SetExpr::SetOperation {
                        op: SetOperator::Union,
                        all,
                        left,
                        right,
                    } = cte.query.body.as_ref()
                    {
                        let anchor = self.bind_set_expr(left)?;
                        let mut cte_cols = anchor.output_cols();
                        if !cte.column_aliases.is_empty() {
                            if cte.column_aliases.len() != cte_cols.len() {
                                return Err(VectraError::BindError(format!(
                                    "CTE `{}` alias count mismatch",
                                    cte.name
                                )));
                            }
                            for (c, alias) in cte_cols.iter_mut().zip(&cte.column_aliases) {
                                c.name = alias.clone();
                            }
                        }
                        self.ctes.insert(
                            key.clone(),
                            CteEntry::RecursivePlaceholder {
                                name: key.clone(),
                                cols: cte_cols.clone(),
                            },
                        );
                        let recursive = self.bind_set_expr(right)?;
                        if recursive.output_cols().len() != cte_cols.len() {
                            return Err(VectraError::BindError(format!(
                                "recursive CTE `{}` anchor/recursive column count mismatch",
                                cte.name
                            )));
                        }
                        self.ctes.insert(
                            key.clone(),
                            CteEntry::RecursiveBound {
                                name: key.clone(),
                                cols: cte_cols.clone(),
                            },
                        );
                        recursive_wrappers.push((key, anchor, recursive, *all, cte_cols));
                        continue;
                    }
                }
                self.ctes
                    .insert(key, CteEntry::NonRecursive { ast: cte.clone() });
            }
        }

        let res = (|| -> Result<LogicalPlan> {
            let limit_val = match &query.limit {
                Some(e) => Some(self.eval_const_usize(e)?),
                None => None,
            };
            let offset_val = match &query.offset {
                Some(e) => self.eval_const_usize(e)?,
                None => 0,
            };

            let mut plan = match query.body.as_ref() {
                SetExpr::Select(sel) if !query.order_by.is_empty() && !sel.distinct => {
                    self.bind_select_with_order(sel, &query.order_by, limit_val, offset_val)?
                }
                _ => {
                    let mut p = self.bind_set_expr(&query.body)?;
                    if !query.order_by.is_empty() {
                        let out_cols = p.output_cols();
                        let scope = Scope::from_cols(out_cols.clone());
                        let mut order_exprs = Vec::with_capacity(query.order_by.len());
                        for ob in &query.order_by {
                            let expr = if let Expr::Literal(LiteralValue::Integer(ord)) = &ob.expr {
                                let idx = (*ord as usize).checked_sub(1).ok_or_else(|| {
                                    VectraError::BindError(format!(
                                        "invalid ORDER BY ordinal {ord}"
                                    ))
                                })?;
                                out_cols
                                    .get(idx)
                                    .ok_or_else(|| {
                                        VectraError::BindError(format!(
                                            "ORDER BY ordinal {ord} out of range 1..={}",
                                            out_cols.len()
                                        ))
                                    })?
                                    .to_expr()
                            } else {
                                self.bind_expr(&ob.expr, &scope, &mut p, false)?
                            };
                            let asc = ob.asc.unwrap_or(true);
                            let nulls_first = ob.nulls_first.unwrap_or(!asc);
                            order_exprs.push(LogicalSortExpr {
                                expr,
                                order: SortOrder {
                                    ascending: asc,
                                    nulls_first,
                                },
                            });
                        }
                        p = LogicalPlan::Sort {
                            input: Box::new(p),
                            order_by: order_exprs,
                        };
                    }
                    if limit_val.is_some() || offset_val > 0 {
                        p = LogicalPlan::Limit {
                            input: Box::new(p),
                            limit: limit_val,
                            offset: offset_val,
                        };
                    }
                    p
                }
            };

            // Wrap in any recursive CTE definitions (innermost first).
            for (name, anchor, recursive, union_all, cte_cols) in
                recursive_wrappers.into_iter().rev()
            {
                plan = LogicalPlan::RecursiveCte {
                    name,
                    anchor: Box::new(anchor),
                    recursive: Box::new(recursive),
                    union_all,
                    cte_cols,
                    consumer: Box::new(plan),
                };
            }
            Ok(plan)
        })();

        self.ctes = saved_ctes;
        res
    }

    fn eval_const_usize(&self, e: &Expr) -> Result<usize> {
        match e {
            Expr::Literal(LiteralValue::Integer(v)) if *v >= 0 => Ok(*v as usize),
            Expr::Param(idx) => {
                let p = self
                    .params
                    .get((*idx as usize).saturating_sub(1))
                    .ok_or_else(|| VectraError::BindError(format!("missing parameter ${idx}")))?;
                match p {
                    ScalarValue::Int64(v) if *v >= 0 => Ok(*v as usize),
                    ScalarValue::Int32(v) if *v >= 0 => Ok(*v as usize),
                    _ => Err(VectraError::BindError(
                        "LIMIT/OFFSET must be non-negative integer".into(),
                    )),
                }
            }
            Expr::Nested(inner) => self.eval_const_usize(inner),
            _ => Err(VectraError::BindError(
                "LIMIT/OFFSET must be a non-negative integer literal".into(),
            )),
        }
    }

    fn bind_set_expr(&mut self, set_expr: &SetExpr) -> Result<LogicalPlan> {
        match set_expr {
            SetExpr::Select(s) => self.bind_select(s),
            SetExpr::Query(q) => self.bind_query(q),
            SetExpr::Values(rows) => self.bind_values(rows, None),
            SetExpr::SetOperation {
                op,
                all,
                left,
                right,
            } => {
                let mut lp = self.bind_set_expr(left)?;
                let mut rp = self.bind_set_expr(right)?;
                let lcols = lp.output_cols();
                let rcols = rp.output_cols();
                if lcols.len() != rcols.len() {
                    return Err(VectraError::BindError(format!(
                        "{op:?} arity mismatch: left has {} columns, right has {}",
                        lcols.len(),
                        rcols.len()
                    )));
                }
                let mut out_cols = Vec::with_capacity(lcols.len());
                let mut l_exprs = Vec::with_capacity(lcols.len());
                let mut r_exprs = Vec::with_capacity(rcols.len());
                let mut need_l_cast = false;
                let mut need_r_cast = false;

                for (lc, rc) in lcols.iter().zip(rcols.iter()) {
                    let common = common_supertype(lc.data_type, rc.data_type)?;
                    let final_ty = if common == LogicalType::Null {
                        LogicalType::Int64
                    } else {
                        common
                    };
                    if lc.data_type != final_ty {
                        need_l_cast = true;
                    }
                    if rc.data_type != final_ty {
                        need_r_cast = true;
                    }
                    l_exprs.push((
                        coerce_expr(lc.to_expr(), final_ty),
                        BoundCol::new(self.col_gen.next_id(), lc.name.clone(), final_ty),
                    ));
                    r_exprs.push((
                        coerce_expr(rc.to_expr(), final_ty),
                        BoundCol::new(self.col_gen.next_id(), rc.name.clone(), final_ty),
                    ));
                    out_cols.push(BoundCol::new(
                        self.col_gen.next_id(),
                        lc.name.clone(),
                        final_ty,
                    ));
                }
                if need_l_cast {
                    lp = LogicalPlan::Project {
                        input: Box::new(lp),
                        exprs: l_exprs,
                    };
                }
                if need_r_cast {
                    rp = LogicalPlan::Project {
                        input: Box::new(rp),
                        exprs: r_exprs,
                    };
                }
                Ok(LogicalPlan::SetOp {
                    op: *op,
                    all: *all,
                    left: Box::new(lp),
                    right: Box::new(rp),
                    output_cols: out_cols,
                })
            }
        }
    }

    fn bind_values(
        &mut self,
        rows: &[Vec<Expr>],
        target_types: Option<&[LogicalType]>,
    ) -> Result<LogicalPlan> {
        if rows.is_empty() {
            return Err(VectraError::BindError(
                "VALUES requires at least 1 row".into(),
            ));
        }
        let width = rows[0].len();
        let empty_scope = Scope::default();
        let mut dummy = LogicalPlan::EmptyRelation {
            produce_one_row: true,
            columns: Vec::new(),
        };
        let mut raw_rows = Vec::with_capacity(rows.len());
        let mut col_types: Vec<LogicalType> = match target_types {
            Some(tt) => {
                if tt.len() != width {
                    return Err(VectraError::BindError(format!(
                        "VALUES row has {width} expressions, expected {}",
                        tt.len()
                    )));
                }
                tt.to_vec()
            }
            None => vec![LogicalType::Null; width],
        };

        for row in rows {
            if row.len() != width {
                return Err(VectraError::BindError(
                    "VALUES rows must all have identical column count".into(),
                ));
            }
            let mut bound_row = Vec::with_capacity(width);
            for (c, expr) in row.iter().enumerate() {
                let be = self.bind_expr(expr, &empty_scope, &mut dummy, false)?;
                if target_types.is_none() {
                    col_types[c] = common_supertype(col_types[c], be.data_type())?;
                }
                bound_row.push(be);
            }
            raw_rows.push(bound_row);
        }

        for ty in &mut col_types {
            if *ty == LogicalType::Null {
                *ty = LogicalType::Int64;
            }
        }

        let coerced_rows: Vec<Vec<LogicalExpr>> = raw_rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .enumerate()
                    .map(|(c, e)| coerce_expr(e, col_types[c]))
                    .collect()
            })
            .collect();

        let columns: Vec<BoundCol> = col_types
            .iter()
            .enumerate()
            .map(|(c, &ty)| BoundCol::new(self.col_gen.next_id(), format!("col{}", c + 1), ty))
            .collect();

        Ok(LogicalPlan::Values {
            rows: coerced_rows,
            columns,
        })
    }

    // ========================================================================
    // SELECT Binding (FROM -> WHERE -> GROUP BY/AGG -> HAVING -> WINDOW -> QUALIFY -> PROJECT -> DISTINCT)
    // ========================================================================

    fn bind_select(&mut self, select: &Select) -> Result<LogicalPlan> {
        self.bind_select_with_order(select, &[], None, 0)
    }

    fn bind_select_with_order(
        &mut self,
        select: &Select,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
    ) -> Result<LogicalPlan> {
        let saved_windows = self.named_windows.clone();
        self.named_windows.clear();
        for (wname, spec) in &select.named_windows {
            self.named_windows
                .insert(wname.to_ascii_lowercase(), spec.clone());
        }

        let res = self.bind_select_inner(select, order_by, limit, offset);
        self.named_windows = saved_windows;
        res
    }

    fn bind_select_inner(
        &mut self,
        select: &Select,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
    ) -> Result<LogicalPlan> {
        // 1. FROM clause
        let (mut plan, from_scope) = if select.from.is_empty() {
            (
                LogicalPlan::EmptyRelation {
                    produce_one_row: true,
                    columns: Vec::new(),
                },
                Scope::default(),
            )
        } else {
            let mut iter = select.from.iter();
            let first = iter.next().unwrap();
            let (mut acc_plan, mut acc_scope) = self.bind_table_with_joins(first)?;
            for twj in iter {
                let (rp, rscope) = self.bind_table_with_joins(twj)?;
                acc_plan = LogicalPlan::Join {
                    left: Box::new(acc_plan),
                    right: Box::new(rp),
                    join_type: JoinType::Cross,
                    condition: None,
                };
                acc_scope.extend(rscope.cols);
            }
            (acc_plan, acc_scope)
        };

        // 2. WHERE clause (aggregates strictly forbidden!)
        // Bind non-subquery conjuncts BEFORE subquery conjuncts so DependentJoin domains
        // inherit all join predicates and table filters (preventing unjoined CrossJoins in D!).
        if let Some(where_expr) = &select.selection {
            if expr_contains_aggregate(where_expr) {
                return Err(VectraError::BindError(
                    "aggregate functions are not allowed in WHERE clause".into(),
                ));
            }
            if expr_contains_window(where_expr) {
                return Err(VectraError::BindError(
                    "window functions are not allowed in WHERE clause".into(),
                ));
            }
            let mut ast_conjuncts = Vec::new();
            split_ast_and(where_expr, &mut ast_conjuncts);
            let mut plain_preds = Vec::new();
            let mut subq_preds = Vec::new();
            for c in ast_conjuncts {
                if expr_contains_subquery(c) {
                    subq_preds.push(c);
                } else {
                    plain_preds.push(c);
                }
            }
            if !plain_preds.is_empty() {
                let mut acc: Option<LogicalExpr> = None;
                for p in plain_preds {
                    let b = coerce_expr(
                        self.bind_expr(p, &from_scope, &mut plan, false)?,
                        LogicalType::Boolean,
                    );
                    acc = Some(match acc {
                        None => b,
                        Some(prev) => LogicalExpr::BinaryOp {
                            op: BinaryOperator::And,
                            left: Box::new(prev),
                            right: Box::new(b),
                            return_type: LogicalType::Boolean,
                        },
                    });
                }
                if let Some(pred) = acc {
                    plan = LogicalPlan::Filter {
                        input: Box::new(plan),
                        predicate: pred,
                    };
                }
            }
            for sp in subq_preds {
                let pred = coerce_expr(
                    self.bind_expr(sp, &from_scope, &mut plan, false)?,
                    LogicalType::Boolean,
                );
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: pred,
                };
            }
        }

        // Expand wildcards in projection early to analyze aggregates & windows.
        let mut expanded_items: Vec<(Expr, Option<String>)> = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::Wildcard => {
                    if from_scope.cols.is_empty() {
                        return Err(VectraError::BindError(
                            "SELECT * with no tables specified".into(),
                        ));
                    }
                    for c in &from_scope.cols {
                        let e = match &c.qualifier {
                            Some(q) => Expr::CompoundIdentifier(vec![q.clone(), c.name.clone()]),
                            None => Expr::Identifier(c.name.clone()),
                        };
                        expanded_items.push((e, Some(c.name.clone())));
                    }
                }
                SelectItem::QualifiedWildcard(qual) => {
                    let matching: Vec<&BoundCol> = from_scope
                        .cols
                        .iter()
                        .filter(|c| {
                            c.qualifier
                                .as_deref()
                                .is_some_and(|q| q.eq_ignore_ascii_case(qual))
                        })
                        .collect();
                    if matching.is_empty() {
                        return Err(VectraError::BindError(format!(
                            "unknown table qualifier `{qual}` in `{qual}.*`"
                        )));
                    }
                    for c in matching {
                        expanded_items.push((
                            Expr::CompoundIdentifier(vec![qual.clone(), c.name.clone()]),
                            Some(c.name.clone()),
                        ));
                    }
                }
                SelectItem::UnnamedExpr(e) => expanded_items.push((e.clone(), None)),
                SelectItem::ExprWithAlias { expr, alias } => {
                    expanded_items.push((expr.clone(), Some(alias.clone())))
                }
            }
        }

        // Map of SELECT aliases for GROUP BY alias resolution (PostgreSQL/DuckDB convenience)
        let mut select_alias_map: HashMap<String, Expr> = HashMap::new();
        for (e, alias) in &expanded_items {
            if let Some(a) = alias {
                select_alias_map.insert(a.to_ascii_lowercase(), e.clone());
            }
        }

        // 3. Determine whether this SELECT requires Aggregation
        let has_agg = !select.group_by.is_empty()
            || select.having.is_some()
            || expanded_items
                .iter()
                .any(|(e, _)| expr_contains_aggregate(e));

        let mut active_scope = from_scope.clone();
        let mut agg_collector: Vec<(AggregateExpr, BoundCol)> = Vec::new();
        let mut group_by_bound: Vec<(LogicalExpr, BoundCol)> = Vec::new();
        let mut grouping_id_col: Option<BoundCol> = None;

        if has_agg {
            // Resolve GROUP BY expressions (including 1-based ordinals and ROLLUP/CUBE/GROUPING SETS)
            let (raw_group_exprs, grouping_sets) = match &select.group_by {
                GroupByClause::Expressions(v) => (v.clone(), None),
                GroupByClause::Rollup(v) => {
                    let mut sets = Vec::with_capacity(v.len() + 1);
                    for k in (0..=v.len()).rev() {
                        sets.push((0..k).collect());
                    }
                    (v.clone(), Some(sets))
                }
                GroupByClause::Cube(v) => {
                    let n = v.len();
                    let mut sets = Vec::with_capacity(1 << n);
                    for mask in (0..(1usize << n)).rev() {
                        let mut s = Vec::new();
                        for bit in 0..n {
                            if (mask & (1 << bit)) != 0 {
                                s.push(bit);
                            }
                        }
                        sets.push(s);
                    }
                    (v.clone(), Some(sets))
                }
                GroupByClause::GroupingSets(sets) => {
                    let mut uniq: Vec<Expr> = Vec::new();
                    let mut mapped_sets = Vec::with_capacity(sets.len());
                    for set in sets {
                        let mut idxs = Vec::with_capacity(set.len());
                        for e in set {
                            if let Some(pos) = uniq.iter().position(|u| u == e) {
                                idxs.push(pos);
                            } else {
                                idxs.push(uniq.len());
                                uniq.push(e.clone());
                            }
                        }
                        mapped_sets.push(idxs);
                    }
                    (uniq, Some(mapped_sets))
                }
            };

            let mut group_ast_exprs = Vec::with_capacity(raw_group_exprs.len());
            for g in &raw_group_exprs {
                let resolved_g = if let Expr::Literal(LiteralValue::Integer(ord)) = g {
                    let idx = (*ord as usize).checked_sub(1).ok_or_else(|| {
                        VectraError::BindError(format!("invalid GROUP BY ordinal {ord}"))
                    })?;
                    expanded_items
                        .get(idx)
                        .ok_or_else(|| {
                            VectraError::BindError(format!("GROUP BY ordinal {ord} out of range"))
                        })?
                        .0
                        .clone()
                } else if let Expr::Identifier(name) = g {
                    if from_scope.resolve(None, name)?.is_none() {
                        if let Some(aliased) = select_alias_map.get(&name.to_ascii_lowercase()) {
                            aliased.clone()
                        } else {
                            g.clone()
                        }
                    } else {
                        g.clone()
                    }
                } else {
                    g.clone()
                };
                if expr_contains_aggregate(&resolved_g) {
                    return Err(VectraError::BindError(
                        "aggregate functions are not allowed in GROUP BY".into(),
                    ));
                }
                let bound_g = self.bind_expr(&resolved_g, &from_scope, &mut plan, false)?;
                let g_name = derive_expr_name(&resolved_g);
                let g_col = BoundCol::new(self.col_gen.next_id(), g_name, bound_g.data_type());
                group_ast_exprs.push(resolved_g);
                group_by_bound.push((bound_g, g_col));
            }

            if grouping_sets.is_some() {
                grouping_id_col = Some(BoundCol::new(
                    self.col_gen.next_id(),
                    "__grouping_id",
                    LogicalType::Int64,
                ));
            }

            // Collect aggregates from SELECT projection, HAVING, and QUALIFY by rewriting
            // aggregate calls and group expressions into references to Aggregate output ColIds!
            let mut post_agg_proj: Vec<(LogicalExpr, BoundCol)> =
                Vec::with_capacity(expanded_items.len());
            let mut window_collector: Vec<(WindowExpr, BoundCol)> = Vec::new();

            // Rewrite HAVING first (so any subqueries in HAVING attach cleanly after Aggregate!)
            let mut bound_having_expr = None;
            let mut agg_ctx = AggRewriteCtx {
                from_scope: &from_scope,
                group_by_bound: &group_by_bound,
                group_ast_exprs: &group_ast_exprs,
                grouping_id_col: grouping_id_col.as_ref(),
                aggregates: &mut agg_collector,
                windows: &mut window_collector,
                allow_windows: false,
            };
            if let Some(h) = &select.having {
                // Note: scalar subqueries in HAVING (like TPC-H Q11!) are handled on post-agg plan below.
                if !expr_contains_subquery(h) {
                    bound_having_expr =
                        Some(self.bind_post_agg_expr(h, &mut agg_ctx, &mut plan)?);
                } else {
                    // Pre-extract any aggregates inside HAVING before binding subqueries on top of Aggregate!
                    collect_aggs_from_ast(self, h, &mut agg_ctx, &mut plan)?;
                }
            }

            agg_ctx.allow_windows = true;
            for (expr, alias) in &expanded_items {
                let bound_e = self.bind_post_agg_expr(expr, &mut agg_ctx, &mut plan)?;
                let out_name = alias.clone().unwrap_or_else(|| derive_expr_name(expr));
                let out_col = BoundCol::new(self.col_gen.next_id(), out_name, bound_e.data_type());
                post_agg_proj.push((bound_e, out_col));
            }

            // Emit Aggregate plan node!
            plan = LogicalPlan::Aggregate {
                input: Box::new(plan),
                group_by: group_by_bound.clone(),
                grouping_sets,
                grouping_id_col: grouping_id_col.clone(),
                aggregates: agg_collector.clone(),
            };

            // Build post-aggregate scope (for HAVING with subqueries like TPC-H Q11!)
            let mut post_agg_cols: Vec<BoundCol> =
                group_by_bound.iter().map(|(_, c)| c.clone()).collect();
            if let Some(gid) = &grouping_id_col {
                post_agg_cols.push(gid.clone());
            }
            post_agg_cols.extend(agg_collector.iter().map(|(_, c)| c.clone()));
            let post_agg_scope = Scope::from_cols(post_agg_cols);

            if let Some(h) = &select.having {
                let h_expr = if let Some(already) = bound_having_expr {
                    already
                } else {
                    // Bind HAVING containing subqueries (e.g., TPC-H Q11 `HAVING SUM(...) > (SELECT ...)`!)
                    self.bind_having_with_subquery(
                        h,
                        &from_scope,
                        &post_agg_scope,
                        &group_by_bound,
                        &group_ast_exprs,
                        &agg_collector,
                        &mut plan,
                    )?
                };
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: coerce_expr(h_expr, LogicalType::Boolean),
                };
            }

            if !window_collector.is_empty() {
                plan = LogicalPlan::Window {
                    input: Box::new(plan),
                    windows: window_collector,
                };
            }

            if let Some(qual_expr) = &select.qualify {
                let qscope = Scope::from_cols(plan.output_cols());
                let qpred = self.bind_expr(qual_expr, &qscope, &mut plan, false)?;
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: coerce_expr(qpred, LogicalType::Boolean),
                };
            }

            plan = self.finish_projection_and_order(
                plan,
                post_agg_proj,
                &post_agg_scope,
                order_by,
                limit,
                offset,
            )?;
        } else {
            // Non-aggregate SELECT: handle Windows, QUALIFY, Projection
            let mut window_collector: Vec<(WindowExpr, BoundCol)> = Vec::new();
            let mut proj_exprs: Vec<(LogicalExpr, BoundCol)> =
                Vec::with_capacity(expanded_items.len());

            for (expr, alias) in &expanded_items {
                let bound_e = self.bind_expr_with_windows(
                    expr,
                    &active_scope,
                    &mut plan,
                    &mut window_collector,
                )?;
                let out_name = alias.clone().unwrap_or_else(|| derive_expr_name(expr));
                let out_col = BoundCol::new(self.col_gen.next_id(), out_name, bound_e.data_type());
                proj_exprs.push((bound_e, out_col));
            }

            if !window_collector.is_empty() {
                plan = LogicalPlan::Window {
                    input: Box::new(plan),
                    windows: window_collector.clone(),
                };
                active_scope.extend(window_collector.iter().map(|(_, c)| c.clone()).collect());
            }

            if let Some(qual_expr) = &select.qualify {
                let mut extra_wins = Vec::new();
                let mut qual_scope = active_scope.clone();
                // Allow QUALIFY to reference projection aliases as well
                for (le, bc) in &proj_exprs {
                    if let LogicalExpr::Col { id, .. } = le {
                        qual_scope
                            .cols
                            .push(BoundCol::new(*id, bc.name.clone(), bc.data_type));
                    }
                }
                let qpred = self.bind_expr_with_windows(
                    qual_expr,
                    &qual_scope,
                    &mut plan,
                    &mut extra_wins,
                )?;
                if !extra_wins.is_empty() {
                    plan = LogicalPlan::Window {
                        input: Box::new(plan),
                        windows: extra_wins,
                    };
                }
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: coerce_expr(qpred, LogicalType::Boolean),
                };
            }

            plan = self.finish_projection_and_order(
                plan,
                proj_exprs,
                &active_scope,
                order_by,
                limit,
                offset,
            )?;
        }

        // 4. DISTINCT
        if select.distinct {
            let on_cols = plan.output_cols();
            plan = LogicalPlan::Distinct {
                input: Box::new(plan),
                on_cols,
            };
        }

        Ok(plan)
    }

    /// Projects `proj_exprs` and resolves `ORDER BY` against BOTH projection aliases/ordinals
    /// AND the underlying `pre_proj_scope` (supporting SQL `SELECT b FROM t ORDER BY a LIMIT 1`).
    fn finish_projection_and_order(
        &mut self,
        mut plan: LogicalPlan,
        mut proj_exprs: Vec<(LogicalExpr, BoundCol)>,
        pre_proj_scope: &Scope,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
    ) -> Result<LogicalPlan> {
        if order_by.is_empty() {
            let mut out = LogicalPlan::Project {
                input: Box::new(plan),
                exprs: proj_exprs,
            };
            if limit.is_some() || offset > 0 {
                out = LogicalPlan::Limit {
                    input: Box::new(out),
                    limit,
                    offset,
                };
            }
            return Ok(out);
        }

        let visible_cols: Vec<BoundCol> = proj_exprs.iter().map(|(_, c)| c.clone()).collect();
        let alias_scope = Scope::from_cols(visible_cols.clone());
        let mut extra_sort_cols = false;
        let mut sort_exprs = Vec::with_capacity(order_by.len());

        for (idx, ob) in order_by.iter().enumerate() {
            let expr = if let Expr::Literal(LiteralValue::Integer(ord)) = &ob.expr {
                let col_idx = (*ord as usize).checked_sub(1).ok_or_else(|| {
                    VectraError::BindError(format!("invalid ORDER BY ordinal {ord}"))
                })?;
                visible_cols
                    .get(col_idx)
                    .ok_or_else(|| {
                        VectraError::BindError(format!(
                            "ORDER BY ordinal {ord} out of range 1..={}",
                            visible_cols.len()
                        ))
                    })?
                    .to_expr()
            } else if let Ok(bound_alias) = self.bind_expr(&ob.expr, &alias_scope, &mut plan, false)
            {
                bound_alias
            } else {
                let bound_pre = self.bind_expr(&ob.expr, pre_proj_scope, &mut plan, false)?;
                let sort_col = BoundCol::new(
                    self.col_gen.next_id(),
                    format!("__sort_key_{idx}"),
                    bound_pre.data_type(),
                );
                proj_exprs.push((bound_pre, sort_col.clone()));
                extra_sort_cols = true;
                sort_col.to_expr()
            };
            let asc = ob.asc.unwrap_or(true);
            let nulls_first = ob.nulls_first.unwrap_or(!asc);
            sort_exprs.push(LogicalSortExpr {
                expr,
                order: SortOrder {
                    ascending: asc,
                    nulls_first,
                },
            });
        }

        let mut out = LogicalPlan::Project {
            input: Box::new(plan),
            exprs: proj_exprs,
        };
        out = LogicalPlan::Sort {
            input: Box::new(out),
            order_by: sort_exprs,
        };
        if limit.is_some() || offset > 0 {
            out = LogicalPlan::Limit {
                input: Box::new(out),
                limit,
                offset,
            };
        }
        if extra_sort_cols {
            let strip = visible_cols.into_iter().map(|c| (c.to_expr(), c)).collect();
            out = LogicalPlan::Project {
                input: Box::new(out),
                exprs: strip,
            };
        }
        Ok(out)
    }

    fn bind_table_with_joins(&mut self, twj: &TableWithJoins) -> Result<(LogicalPlan, Scope)> {
        let (mut left_plan, mut left_scope) = self.bind_table_factor(&twj.relation)?;

        for join in &twj.joins {
            let (right_plan, right_scope) = self.bind_table_factor(&join.relation)?;
            let join_type = match join.kind {
                AstJoinKind::Inner => JoinType::Inner,
                AstJoinKind::LeftOuter => JoinType::LeftOuter,
                AstJoinKind::RightOuter => JoinType::RightOuter,
                AstJoinKind::FullOuter => JoinType::FullOuter,
                AstJoinKind::Cross => JoinType::Cross,
                AstJoinKind::LeftSemi => JoinType::LeftSemi,
                AstJoinKind::LeftAnti => JoinType::LeftAnti,
            };

            let mut combined_scope = left_scope.clone();
            combined_scope.extend(right_scope.cols.clone());

            let condition = match &join.constraint {
                JoinConstraint::None => None,
                JoinConstraint::On(expr) => {
                    let mut dummy = LogicalPlan::Join {
                        left: Box::new(left_plan.clone()),
                        right: Box::new(right_plan.clone()),
                        join_type: join_type.clone(),
                        condition: None,
                    };
                    let pred = self.bind_expr(expr, &combined_scope, &mut dummy, false)?;
                    Some(coerce_expr(pred, LogicalType::Boolean))
                }
                JoinConstraint::Using(col_names) => {
                    let mut cond: Option<LogicalExpr> = None;
                    for cname in col_names {
                        let lc = left_scope.resolve(None, cname)?.ok_or_else(|| {
                            VectraError::BindError(format!(
                                "USING column `{cname}` not found on left side of join"
                            ))
                        })?;
                        let rc = right_scope.resolve(None, cname)?.ok_or_else(|| {
                            VectraError::BindError(format!(
                                "USING column `{cname}` not found on right side of join"
                            ))
                        })?;
                        let common = common_supertype(lc.data_type, rc.data_type)?;
                        let eq = LogicalExpr::BinaryOp {
                            op: BinaryOperator::Eq,
                            left: Box::new(coerce_expr(lc.to_expr(), common)),
                            right: Box::new(coerce_expr(rc.to_expr(), common)),
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
                    }
                    cond
                }
                JoinConstraint::Natural => {
                    let mut cond: Option<LogicalExpr> = None;
                    for lc in &left_scope.cols {
                        if let Some(rc) = right_scope.resolve(None, &lc.name)? {
                            let common = common_supertype(lc.data_type, rc.data_type)?;
                            let eq = LogicalExpr::BinaryOp {
                                op: BinaryOperator::Eq,
                                left: Box::new(coerce_expr(lc.to_expr(), common)),
                                right: Box::new(coerce_expr(rc.to_expr(), common)),
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
                        }
                    }
                    cond
                }
            };

            let out_scope = match join_type {
                JoinType::LeftSemi | JoinType::LeftAnti => left_scope,
                _ => {
                    let mut s = left_scope;
                    // For NATURAL / USING, keep qualified right columns but avoid unqualified ambiguity
                    if matches!(
                        join.constraint,
                        JoinConstraint::Using(_) | JoinConstraint::Natural
                    ) {
                        let using_set: BTreeSet<String> = match &join.constraint {
                            JoinConstraint::Using(cs) => {
                                cs.iter().map(|x| x.to_ascii_lowercase()).collect()
                            }
                            JoinConstraint::Natural => {
                                s.cols.iter().map(|c| c.name.to_ascii_lowercase()).collect()
                            }
                            _ => BTreeSet::new(),
                        };
                        for rc in right_scope.cols {
                            if !using_set.contains(&rc.name.to_ascii_lowercase()) {
                                s.cols.push(rc);
                            }
                        }
                    } else {
                        s.extend(right_scope.cols);
                    }
                    s
                }
            };

            left_plan = LogicalPlan::Join {
                left: Box::new(left_plan),
                right: Box::new(right_plan),
                join_type,
                condition,
            };
            left_scope = out_scope;
        }

        Ok((left_plan, left_scope))
    }

    fn bind_table_factor(&mut self, tf: &TableFactor) -> Result<(LogicalPlan, Scope)> {
        match tf {
            TableFactor::Table { name, alias } => {
                let key = name.to_ascii_lowercase();

                // 1. Check CTEs first
                if let Some(cte_entry) = self.ctes.get(&key).cloned() {
                    return match cte_entry {
                        CteEntry::NonRecursive { ast } => {
                            if self.expanding_ctes.contains(&key) {
                                return Err(VectraError::BindError(format!(
                                    "cyclic CTE reference `{key}` without WITH RECURSIVE"
                                )));
                            }
                            self.expanding_ctes.push(key.clone());
                            let sub_plan = self.bind_query(&ast.query);
                            self.expanding_ctes.pop();
                            let plan = sub_plan?;
                            let eff_alias = alias
                                .as_ref()
                                .map(|a| a.name.clone())
                                .unwrap_or_else(|| ast.name.clone());
                            let col_aliases = if let Some(a) = alias {
                                if !a.columns.is_empty() {
                                    &a.columns
                                } else {
                                    &ast.column_aliases
                                }
                            } else {
                                &ast.column_aliases
                            };
                            self.apply_relation_alias(plan, &eff_alias, col_aliases)
                        }
                        CteEntry::RecursivePlaceholder { name: cname, cols }
                        | CteEntry::RecursiveBound { name: cname, cols } => {
                            let eff_alias = alias
                                .as_ref()
                                .map(|a| a.name.clone())
                                .unwrap_or_else(|| cname.clone());
                            let fresh_cols: Vec<BoundCol> = cols
                                .iter()
                                .enumerate()
                                .map(|(i, c)| {
                                    let col_name = alias
                                        .as_ref()
                                        .and_then(|a| a.columns.get(i))
                                        .cloned()
                                        .unwrap_or_else(|| c.name.clone());
                                    BoundCol::new(self.col_gen.next_id(), col_name, c.data_type)
                                        .with_qualifier(eff_alias.clone())
                                })
                                .collect();
                            let plan = LogicalPlan::CteScan {
                                cte_name: cname,
                                columns: fresh_cols.clone(),
                            };
                            Ok((plan, Scope::from_cols(fresh_cols)))
                        }
                    };
                }

                // 2. Check registered SQL Views (e.g., TPC-H Q15 `revenue0`)
                if let Some(view_sql) = self.views.get(&key) {
                    let stmt = parse_single_statement(view_sql)?;
                    let Statement::Query(vq) = stmt else {
                        return Err(VectraError::BindError(format!(
                            "view `{name}` does not contain a query"
                        )));
                    };
                    let plan = self.bind_query(&vq)?;
                    let eff_alias = alias
                        .as_ref()
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| key.clone());
                    let col_aliases = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                    return self.apply_relation_alias(plan, &eff_alias, col_aliases);
                }

                // 3. Base storage table in Catalog
                let tbl = self.catalog.get_table(&key)?;
                let eff_alias = alias
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| tbl.name.clone());
                let mut cols = Vec::with_capacity(tbl.schema.len());
                let mut indices = Vec::with_capacity(tbl.schema.len());
                for (i, field) in tbl.schema.fields.iter().enumerate() {
                    let col_name = alias
                        .as_ref()
                        .and_then(|a| a.columns.get(i))
                        .cloned()
                        .unwrap_or_else(|| field.name.clone());
                    let mut bc =
                        BoundCol::new(self.col_gen.next_id(), col_name, field.logical_type)
                            .with_qualifier(eff_alias.clone());
                    bc.nullable = field.nullable;
                    cols.push(bc);
                    indices.push(i);
                }
                let plan = LogicalPlan::Scan {
                    table_name: tbl.name.clone(),
                    alias: eff_alias,
                    columns: cols.clone(),
                    table_col_indices: indices,
                    pushed_filters: Vec::new(),
                };
                Ok((plan, Scope::from_cols(cols)))
            }
            TableFactor::Derived { subquery, alias } => {
                let plan = self.bind_query(subquery)?;
                let eff_alias = alias
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| "_subquery".to_string());
                let col_aliases = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                self.apply_relation_alias(plan, &eff_alias, col_aliases)
            }
            TableFactor::NestedJoin(twj) => self.bind_table_with_joins(twj),
            TableFactor::TableFunction { name, args, alias } => {
                let fname = name.to_ascii_lowercase();
                let eff_alias = alias
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| fname.clone());
                match fname.as_str() {
                    "generate_series" => {
                        if !(2..=3).contains(&args.len()) {
                            return Err(VectraError::BindError(
                                "generate_series expects 2 or 3 integer arguments".into(),
                            ));
                        }
                        let start = self.eval_const_i64(&args[0])?;
                        let stop = self.eval_const_i64(&args[1])?;
                        let step = if args.len() == 3 {
                            self.eval_const_i64(&args[2])?
                        } else if start <= stop {
                            1
                        } else {
                            -1
                        };
                        if step == 0 {
                            return Err(VectraError::BindError(
                                "generate_series step cannot be 0".into(),
                            ));
                        }
                        let col_name = alias
                            .as_ref()
                            .and_then(|a| a.columns.first())
                            .cloned()
                            .unwrap_or_else(|| "generate_series".to_string());
                        let col =
                            BoundCol::new(self.col_gen.next_id(), col_name, LogicalType::Int64)
                                .with_qualifier(eff_alias);
                        let plan = LogicalPlan::TableFunc {
                            func: TableFunctionKind::GenerateSeries { start, stop, step },
                            columns: vec![col.clone()],
                        };
                        Ok((plan, Scope::from_cols(vec![col])))
                    }
                    "read_parquet" => {
                        if args.len() != 1 {
                            return Err(VectraError::BindError(
                                "read_parquet expects 1 string path argument".into(),
                            ));
                        }
                        let path = self.eval_const_str(&args[0])?;
                        let schema = infer_parquet_schema_from_path(&path)?;
                        let cols: Vec<BoundCol> = schema
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(i, f)| {
                                let cname = alias
                                    .as_ref()
                                    .and_then(|a| a.columns.get(i))
                                    .cloned()
                                    .unwrap_or_else(|| f.name.clone());
                                BoundCol::new(self.col_gen.next_id(), cname, f.logical_type)
                                    .with_qualifier(eff_alias.clone())
                            })
                            .collect();
                        let plan = LogicalPlan::TableFunc {
                            func: TableFunctionKind::ReadParquet { path },
                            columns: cols.clone(),
                        };
                        Ok((plan, Scope::from_cols(cols)))
                    }
                    "read_csv" => {
                        if args.is_empty() {
                            return Err(VectraError::BindError(
                                "read_csv expects at least 1 string path argument".into(),
                            ));
                        }
                        let path = self.eval_const_str(&args[0])?;
                        let delimiter = if args.len() >= 2 {
                            self.eval_const_str(&args[1])?
                                .as_bytes()
                                .first()
                                .copied()
                                .unwrap_or(b',')
                        } else {
                            b','
                        };
                        let schema = infer_csv_schema_from_path(&path, delimiter)?;
                        let cols: Vec<BoundCol> = schema
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(i, f)| {
                                let cname = alias
                                    .as_ref()
                                    .and_then(|a| a.columns.get(i))
                                    .cloned()
                                    .unwrap_or_else(|| f.name.clone());
                                BoundCol::new(self.col_gen.next_id(), cname, f.logical_type)
                                    .with_qualifier(eff_alias.clone())
                            })
                            .collect();
                        let plan = LogicalPlan::TableFunc {
                            func: TableFunctionKind::ReadCsv {
                                path,
                                delimiter,
                                header: true,
                            },
                            columns: cols.clone(),
                        };
                        Ok((plan, Scope::from_cols(cols)))
                    }
                    _ => Err(VectraError::BindError(format!(
                        "unknown table function `{name}`"
                    ))),
                }
            }
        }
    }

    fn apply_relation_alias(
        &mut self,
        plan: LogicalPlan,
        alias_name: &str,
        col_aliases: &[String],
    ) -> Result<(LogicalPlan, Scope)> {
        let orig_cols = plan.output_cols();
        if !col_aliases.is_empty() && col_aliases.len() != orig_cols.len() {
            return Err(VectraError::BindError(format!(
                "alias `{alias_name}` specifies {} columns, but relation has {}",
                col_aliases.len(),
                orig_cols.len()
            )));
        }
        let scope_cols: Vec<BoundCol> = orig_cols
            .into_iter()
            .enumerate()
            .map(|(i, mut c)| {
                if let Some(ca) = col_aliases.get(i) {
                    c.name = ca.clone();
                }
                c.qualifier = Some(alias_name.to_string());
                c
            })
            .collect();
        Ok((plan, Scope::from_cols(scope_cols)))
    }

    fn eval_const_i64(&self, e: &Expr) -> Result<i64> {
        match e {
            Expr::Literal(LiteralValue::Integer(v)) => Ok(*v),
            Expr::UnaryOp {
                op: AstUnaryOp::Neg,
                expr,
            } => Ok(-self.eval_const_i64(expr)?),
            Expr::Nested(inner) => self.eval_const_i64(inner),
            _ => Err(VectraError::BindError(
                "expected constant integer argument".into(),
            )),
        }
    }

    fn eval_const_str(&self, e: &Expr) -> Result<String> {
        match e {
            Expr::Literal(LiteralValue::String(s)) => Ok(s.clone()),
            Expr::Nested(inner) => self.eval_const_str(inner),
            _ => Err(VectraError::BindError(
                "expected constant string argument".into(),
            )),
        }
    }

    // ========================================================================
    // Expression Binding (with Correlated Subqueries & Window Functions)
    // ========================================================================

    fn bind_expr(
        &mut self,
        expr: &Expr,
        scope: &Scope,
        plan: &mut LogicalPlan,
        allow_outer_only: bool,
    ) -> Result<LogicalExpr> {
        let _ = allow_outer_only;
        let mut dummy_wins = Vec::new();
        self.bind_expr_internal(expr, scope, plan, &mut dummy_wins, false)
    }

    fn bind_expr_with_windows(
        &mut self,
        expr: &Expr,
        scope: &Scope,
        plan: &mut LogicalPlan,
        windows: &mut Vec<(WindowExpr, BoundCol)>,
    ) -> Result<LogicalExpr> {
        self.bind_expr_internal(expr, scope, plan, windows, true)
    }

    fn bind_expr_internal(
        &mut self,
        expr: &Expr,
        scope: &Scope,
        plan: &mut LogicalPlan,
        windows: &mut Vec<(WindowExpr, BoundCol)>,
        allow_windows: bool,
    ) -> Result<LogicalExpr> {
        match expr {
            Expr::Nested(inner) => {
                self.bind_expr_internal(inner, scope, plan, windows, allow_windows)
            }
            Expr::Identifier(name) => self.resolve_column_ref(None, name, scope),
            Expr::CompoundIdentifier(parts) => {
                if parts.len() == 2 {
                    self.resolve_column_ref(Some(&parts[0]), &parts[1], scope)
                } else {
                    let col = parts.last().unwrap();
                    let qual = parts.get(parts.len() - 2).map(String::as_str);
                    self.resolve_column_ref(qual, col, scope)
                }
            }
            Expr::Param(idx) => {
                let p = self
                    .params
                    .get((*idx as usize).saturating_sub(1))
                    .cloned()
                    .unwrap_or(ScalarValue::Null);
                let ty = if p.is_null() {
                    LogicalType::Null
                } else {
                    p.logical_type()
                };
                Ok(LogicalExpr::Literal {
                    value: p,
                    data_type: ty,
                })
            }
            Expr::Literal(lit) => self.bind_literal(lit),
            Expr::UnaryOp { op, expr: inner } => {
                // Fold negative numeric literal directly (`-42`, `-1.5`)
                if *op == AstUnaryOp::Neg {
                    if let Expr::Literal(LiteralValue::Integer(v)) = inner.as_ref() {
                        if let Some(nv) = v.checked_neg() {
                            return Ok(LogicalExpr::Literal {
                                value: ScalarValue::Int64(nv),
                                data_type: LogicalType::Int64,
                            });
                        }
                    }
                    if let Expr::Literal(LiteralValue::Decimal(s)) = inner.as_ref() {
                        return self.bind_literal(&LiteralValue::Decimal(format!("-{s}")));
                    }
                }
                let b = self.bind_expr_internal(inner, scope, plan, windows, allow_windows)?;
                match op {
                    AstUnaryOp::Pos => Ok(b),
                    AstUnaryOp::Neg => {
                        let ty = b.data_type();
                        Ok(LogicalExpr::UnaryOp {
                            op: UnaryOperator::Neg,
                            expr: Box::new(b),
                            return_type: ty,
                        })
                    }
                    AstUnaryOp::Not => Ok(LogicalExpr::UnaryOp {
                        op: UnaryOperator::Not,
                        expr: Box::new(coerce_expr(b, LogicalType::Boolean)),
                        return_type: LogicalType::Boolean,
                    }),
                }
            }
            Expr::IsNull {
                expr: inner,
                negated,
            } => {
                let b = self.bind_expr_internal(inner, scope, plan, windows, allow_windows)?;
                Ok(LogicalExpr::UnaryOp {
                    op: if *negated {
                        UnaryOperator::IsNotNull
                    } else {
                        UnaryOperator::IsNull
                    },
                    expr: Box::new(b),
                    return_type: LogicalType::Boolean,
                })
            }
            Expr::BinaryOp { left, op, right } => {
                // Special handling for `DATE/TIMESTAMP +/- INTERVAL`!
                if matches!(op, AstBinaryOp::Add | AstBinaryOp::Sub) {
                    if let Expr::Literal(LiteralValue::Interval { value, unit }) = right.as_ref() {
                        let lv =
                            self.bind_expr_internal(left, scope, plan, windows, allow_windows)?;
                        let signed_val = if *op == AstBinaryOp::Sub {
                            -*value
                        } else {
                            *value
                        };
                        return bind_date_interval_op(lv, signed_val, *unit);
                    }
                }

                let lb = self.bind_expr_internal(left, scope, plan, windows, allow_windows)?;
                let rb = self.bind_expr_internal(right, scope, plan, windows, allow_windows)?;
                self.build_binary_expr(*op, lb, rb)
            }
            Expr::Between {
                expr: target,
                low,
                high,
                negated,
            } => {
                let tb = self.bind_expr_internal(target, scope, plan, windows, allow_windows)?;
                let lb = self.bind_expr_internal(low, scope, plan, windows, allow_windows)?;
                let hb = self.bind_expr_internal(high, scope, plan, windows, allow_windows)?;
                let c1 = common_supertype(tb.data_type(), lb.data_type())?;
                let common = common_supertype(c1, hb.data_type())?;
                Ok(LogicalExpr::Between {
                    expr: Box::new(coerce_expr(tb, common)),
                    low: Box::new(coerce_expr(lb, common)),
                    high: Box::new(coerce_expr(hb, common)),
                    negated: *negated,
                })
            }
            Expr::InList {
                expr: target,
                list,
                negated,
            } => {
                let tb = self.bind_expr_internal(target, scope, plan, windows, allow_windows)?;
                let mut bound_items = Vec::with_capacity(list.len());
                let mut common = tb.data_type();
                for item in list {
                    let ib = self.bind_expr_internal(item, scope, plan, windows, allow_windows)?;
                    common = common_supertype(common, ib.data_type())?;
                    bound_items.push(ib);
                }
                Ok(LogicalExpr::InList {
                    expr: Box::new(coerce_expr(tb, common)),
                    list: bound_items
                        .into_iter()
                        .map(|e| coerce_expr(e, common))
                        .collect(),
                    negated: *negated,
                })
            }
            Expr::Like {
                expr: target,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => {
                let tb = self.bind_expr_internal(target, scope, plan, windows, allow_windows)?;
                let pb = self.bind_expr_internal(pattern, scope, plan, windows, allow_windows)?;
                Ok(LogicalExpr::Like {
                    expr: Box::new(coerce_expr(tb, LogicalType::Utf8)),
                    pattern: Box::new(coerce_expr(pb, LogicalType::Utf8)),
                    escape: *escape,
                    case_insensitive: *case_insensitive,
                    negated: *negated,
                })
            }
            Expr::Case {
                operand,
                branches,
                else_result,
            } => {
                let op_bound = match operand {
                    Some(o) => {
                        Some(self.bind_expr_internal(o, scope, plan, windows, allow_windows)?)
                    }
                    None => None,
                };
                let mut bound_branches = Vec::with_capacity(branches.len());
                let mut ret_ty = LogicalType::Null;
                for (w, t) in branches {
                    let wb = self.bind_expr_internal(w, scope, plan, windows, allow_windows)?;
                    let cond = match &op_bound {
                        Some(ob) => {
                            let common = common_supertype(ob.data_type(), wb.data_type())?;
                            LogicalExpr::BinaryOp {
                                op: BinaryOperator::Eq,
                                left: Box::new(coerce_expr(ob.clone(), common)),
                                right: Box::new(coerce_expr(wb, common)),
                                return_type: LogicalType::Boolean,
                            }
                        }
                        None => coerce_expr(wb, LogicalType::Boolean),
                    };
                    let tb = self.bind_expr_internal(t, scope, plan, windows, allow_windows)?;
                    ret_ty = common_supertype(ret_ty, tb.data_type())?;
                    bound_branches.push((cond, tb));
                }
                let else_bound = match else_result {
                    Some(e) => {
                        let eb = self.bind_expr_internal(e, scope, plan, windows, allow_windows)?;
                        ret_ty = common_supertype(ret_ty, eb.data_type())?;
                        Some(eb)
                    }
                    None => None,
                };
                if ret_ty == LogicalType::Null {
                    ret_ty = LogicalType::Int64;
                }
                let coerced_branches = bound_branches
                    .into_iter()
                    .map(|(c, t)| (c, coerce_expr(t, ret_ty)))
                    .collect();
                Ok(LogicalExpr::CaseWhen {
                    branches: coerced_branches,
                    else_expr: else_bound.map(|e| Box::new(coerce_expr(e, ret_ty))),
                    return_type: ret_ty,
                })
            }
            Expr::Cast {
                expr: inner,
                data_type,
                try_cast,
            } => {
                let b = self.bind_expr_internal(inner, scope, plan, windows, allow_windows)?;
                Ok(LogicalExpr::Cast {
                    expr: Box::new(b),
                    target_type: *data_type,
                    try_cast: *try_cast,
                })
            }
            Expr::Extract { part, expr: inner } => {
                let b = self.bind_expr_internal(inner, scope, plan, windows, allow_windows)?;
                Ok(LogicalExpr::ScalarFunc {
                    func: ScalarFunction::Extract(*part),
                    args: vec![b],
                    return_type: LogicalType::Int64,
                })
            }
            Expr::Function {
                name,
                args,
                distinct,
                wildcard_arg,
                filter,
                over,
            } => {
                if let Some(wspec) = over {
                    if !allow_windows {
                        return Err(VectraError::BindError(
                            "window functions not allowed in this clause".into(),
                        ));
                    }
                    let wexpr =
                        self.bind_window_function(name, args, *wildcard_arg, wspec, scope, plan)?;
                    let wcol = BoundCol::new(
                        self.col_gen.next_id(),
                        format!("__win_{}_{}", name, windows.len()),
                        wexpr.return_type,
                    );
                    windows.push((wexpr, wcol.clone()));
                    return Ok(wcol.to_expr());
                }
                if is_aggregate_name(name) {
                    return Err(VectraError::BindError(format!(
                        "aggregate function `{name}` not allowed here"
                    )));
                }
                let _ = (distinct, filter, wildcard_arg);
                self.bind_scalar_function(name, args, scope, plan, windows, allow_windows)
            }
            // Subqueries!
            Expr::Exists { subquery, negated } => {
                self.outer_scopes.push(scope.clone());
                let sub_res = self.bind_query(subquery);
                self.outer_scopes.pop();
                let sub_plan = sub_res?;

                let mark_col = BoundCol::new(
                    self.col_gen.next_id(),
                    format!("__mark_exists_{}", self.col_gen.current_next()),
                    LogicalType::Boolean,
                );
                let corr_cols = extract_local_correlated_cols(&sub_plan, scope);
                let jt = JoinType::LeftMark {
                    mark_col: mark_col.clone(),
                    probe_pair: None,
                };
                attach_subquery_join(plan, sub_plan, jt, None, corr_cols);
                let m = mark_col.to_expr();
                if *negated {
                    Ok(LogicalExpr::UnaryOp {
                        op: UnaryOperator::Not,
                        expr: Box::new(m),
                        return_type: LogicalType::Boolean,
                    })
                } else {
                    Ok(m)
                }
            }
            Expr::InSubquery {
                expr: lhs_ast,
                subquery,
                negated,
            } => {
                let lhs = self.bind_expr_internal(lhs_ast, scope, plan, windows, allow_windows)?;
                self.outer_scopes.push(scope.clone());
                let sub_res = self.bind_query(subquery);
                self.outer_scopes.pop();
                let sub_plan = sub_res?;

                let sub_cols = sub_plan.output_cols();
                if sub_cols.len() != 1 {
                    return Err(VectraError::BindError(format!(
                        "IN subquery must return exactly 1 column, got {}",
                        sub_cols.len()
                    )));
                }
                let rhs_col = &sub_cols[0];
                let common = common_supertype(lhs.data_type(), rhs_col.data_type)?;
                let probe_lhs = coerce_expr(lhs, common);
                let probe_rhs = coerce_expr(rhs_col.to_expr(), common);

                let mark_col = BoundCol::new(
                    self.col_gen.next_id(),
                    format!("__mark_in_{}", self.col_gen.current_next()),
                    LogicalType::Boolean,
                );
                let corr_cols = extract_local_correlated_cols(&sub_plan, scope);
                let jt = JoinType::LeftMark {
                    mark_col: mark_col.clone(),
                    probe_pair: Some((probe_lhs, probe_rhs)),
                };
                attach_subquery_join(plan, sub_plan, jt, None, corr_cols);
                let m = mark_col.to_expr();
                if *negated {
                    Ok(LogicalExpr::UnaryOp {
                        op: UnaryOperator::Not,
                        expr: Box::new(m),
                        return_type: LogicalType::Boolean,
                    })
                } else {
                    Ok(m)
                }
            }
            Expr::QuantifiedSubquery {
                expr: lhs_ast,
                op,
                quantifier,
                subquery,
            } => {
                if *op == AstBinaryOp::Eq && *quantifier == Quantifier::Any {
                    return self.bind_expr_internal(
                        &Expr::InSubquery {
                            expr: lhs_ast.clone(),
                            subquery: subquery.clone(),
                            negated: false,
                        },
                        scope,
                        plan,
                        windows,
                        allow_windows,
                    );
                }
                if *op == AstBinaryOp::NotEq && *quantifier == Quantifier::All {
                    return self.bind_expr_internal(
                        &Expr::InSubquery {
                            expr: lhs_ast.clone(),
                            subquery: subquery.clone(),
                            negated: true,
                        },
                        scope,
                        plan,
                        windows,
                        allow_windows,
                    );
                }
                let lhs = self.bind_expr_internal(lhs_ast, scope, plan, windows, allow_windows)?;
                self.outer_scopes.push(scope.clone());
                let sub_res = self.bind_query(subquery);
                self.outer_scopes.pop();
                let sub_plan = sub_res?;
                let sub_cols = sub_plan.output_cols();
                if sub_cols.len() != 1 {
                    return Err(VectraError::BindError(
                        "ANY/ALL subquery must return exactly 1 column".into(),
                    ));
                }
                let rhs = sub_cols[0].to_expr();
                let common = common_supertype(lhs.data_type(), rhs.data_type())?;
                let eff_op = match quantifier {
                    Quantifier::Any => ast_binop_to_phys(*op)?,
                    Quantifier::All => negate_cmp_op(ast_binop_to_phys(*op)?),
                };
                let cond = LogicalExpr::BinaryOp {
                    op: eff_op,
                    left: Box::new(coerce_expr(lhs, common)),
                    right: Box::new(coerce_expr(rhs, common)),
                    return_type: LogicalType::Boolean,
                };
                let mark_col = BoundCol::new(
                    self.col_gen.next_id(),
                    format!("__mark_quant_{}", self.col_gen.current_next()),
                    LogicalType::Boolean,
                );
                let corr_cols = extract_local_correlated_cols(&sub_plan, scope);
                let jt = JoinType::LeftMark {
                    mark_col: mark_col.clone(),
                    probe_pair: None,
                };
                attach_subquery_join(plan, sub_plan, jt, Some(cond), corr_cols);
                let m = mark_col.to_expr();
                if *quantifier == Quantifier::All {
                    Ok(LogicalExpr::UnaryOp {
                        op: UnaryOperator::Not,
                        expr: Box::new(m),
                        return_type: LogicalType::Boolean,
                    })
                } else {
                    Ok(m)
                }
            }
            Expr::Subquery(subquery) => {
                self.outer_scopes.push(scope.clone());
                let sub_res = self.bind_query(subquery);
                self.outer_scopes.pop();
                let sub_plan = sub_res?;
                let sub_cols = sub_plan.output_cols();
                if sub_cols.len() != 1 {
                    return Err(VectraError::BindError(format!(
                        "scalar subquery must return exactly 1 column, got {}",
                        sub_cols.len()
                    )));
                }
                let out_col = sub_cols[0].clone();
                let corr_cols = extract_local_correlated_cols(&sub_plan, scope);
                attach_subquery_join(plan, sub_plan, JoinType::Single, None, corr_cols);
                Ok(out_col.to_expr())
            }
        }
    }

    fn resolve_column_ref(
        &self,
        qualifier: Option<&str>,
        name: &str,
        current_scope: &Scope,
    ) -> Result<LogicalExpr> {
        if let Some(bc) = current_scope.resolve(qualifier, name)? {
            return Ok(bc.to_expr());
        }
        // Search enclosing scopes from innermost to outermost for correlated column bindings!
        for (depth_idx, outer) in self.outer_scopes.iter().rev().enumerate() {
            if let Some(bc) = outer.resolve(qualifier, name)? {
                return Ok(LogicalExpr::CorrelatedCol {
                    id: bc.id,
                    name: bc.name,
                    data_type: bc.data_type,
                    depth: depth_idx + 1,
                });
            }
        }
        let disp = match qualifier {
            Some(q) => format!("{q}.{name}"),
            None => name.to_string(),
        };
        Err(VectraError::BindError(format!(
            "column `{disp}` does not exist"
        )))
    }

    fn bind_literal(&self, lit: &LiteralValue) -> Result<LogicalExpr> {
        match lit {
            LiteralValue::Null => Ok(LogicalExpr::Literal {
                value: ScalarValue::Null,
                data_type: LogicalType::Null,
            }),
            LiteralValue::Boolean(b) => Ok(LogicalExpr::Literal {
                value: ScalarValue::Boolean(*b),
                data_type: LogicalType::Boolean,
            }),
            LiteralValue::Integer(i) => Ok(LogicalExpr::Literal {
                value: ScalarValue::Int64(*i),
                data_type: LogicalType::Int64,
            }),
            LiteralValue::Float(f) => Ok(LogicalExpr::Literal {
                value: ScalarValue::Float64(*f),
                data_type: LogicalType::Float64,
            }),
            LiteralValue::Decimal(s) => {
                let trimmed = s.trim();
                let unsigned = trimmed.trim_start_matches(['-', '+']);
                let scale = unsigned
                    .split_once('.')
                    .map(|(_, f)| f.len() as i8)
                    .unwrap_or(0);
                let digits = unsigned.replace('.', "").len().max(1) as u8;
                let precision = digits.clamp(scale.max(0) as u8 + 1, MAX_DECIMAL_PRECISION);
                let raw = parse_decimal(trimmed, precision, scale)?;
                Ok(LogicalExpr::Literal {
                    value: ScalarValue::Decimal128 {
                        value: raw,
                        precision,
                        scale,
                    },
                    data_type: LogicalType::Decimal128 { precision, scale },
                })
            }
            LiteralValue::String(s) => Ok(LogicalExpr::Literal {
                value: ScalarValue::Utf8(s.clone()),
                data_type: LogicalType::Utf8,
            }),
            LiteralValue::Date(s) => {
                let d = parse_date32(s)?;
                Ok(LogicalExpr::Literal {
                    value: ScalarValue::Date32(d),
                    data_type: LogicalType::Date32,
                })
            }
            LiteralValue::Timestamp(s) => {
                let ts = parse_timestamp_micros(s)?;
                Ok(LogicalExpr::Literal {
                    value: ScalarValue::TimestampMicros(ts),
                    data_type: LogicalType::TimestampMicros,
                })
            }
            LiteralValue::Interval { .. } => Err(VectraError::BindError(
                "standalone INTERVAL literal must be combined with a DATE or TIMESTAMP".into(),
            )),
        }
    }

    fn build_binary_expr(
        &self,
        op: AstBinaryOp,
        lb: LogicalExpr,
        rb: LogicalExpr,
    ) -> Result<LogicalExpr> {
        let phys_op = ast_binop_to_phys(op)?;
        match phys_op {
            BinaryOperator::And | BinaryOperator::Or => Ok(LogicalExpr::BinaryOp {
                op: phys_op,
                left: Box::new(coerce_expr(lb, LogicalType::Boolean)),
                right: Box::new(coerce_expr(rb, LogicalType::Boolean)),
                return_type: LogicalType::Boolean,
            }),
            BinaryOperator::Concat => Ok(LogicalExpr::BinaryOp {
                op: BinaryOperator::Concat,
                left: Box::new(coerce_expr(lb, LogicalType::Utf8)),
                right: Box::new(coerce_expr(rb, LogicalType::Utf8)),
                return_type: LogicalType::Utf8,
            }),
            op if op.is_comparison() => {
                let common = common_supertype(lb.data_type(), rb.data_type())?;
                Ok(LogicalExpr::BinaryOp {
                    op,
                    left: Box::new(coerce_expr(lb, common)),
                    right: Box::new(coerce_expr(rb, common)),
                    return_type: LogicalType::Boolean,
                })
            }
            op if op.is_arithmetic() => {
                let (input_ty, out_ty) =
                    arithmetic_result_type(op, lb.data_type(), rb.data_type())?;
                Ok(LogicalExpr::BinaryOp {
                    op,
                    left: Box::new(coerce_expr(lb, input_ty)),
                    right: Box::new(coerce_expr(rb, input_ty)),
                    return_type: out_ty,
                })
            }
            _ => unreachable!(),
        }
    }

    fn bind_scalar_function(
        &mut self,
        name: &str,
        args: &[Expr],
        scope: &Scope,
        plan: &mut LogicalPlan,
        windows: &mut Vec<(WindowExpr, BoundCol)>,
        allow_windows: bool,
    ) -> Result<LogicalExpr> {
        let lower = name.to_ascii_lowercase();
        if lower == "coalesce" {
            let mut bargs = Vec::with_capacity(args.len());
            let mut ret = LogicalType::Null;
            for a in args {
                let b = self.bind_expr_internal(a, scope, plan, windows, allow_windows)?;
                ret = common_supertype(ret, b.data_type())?;
                bargs.push(b);
            }
            if ret == LogicalType::Null {
                ret = LogicalType::Int64;
            }
            return Ok(LogicalExpr::Coalesce {
                args: bargs.into_iter().map(|e| coerce_expr(e, ret)).collect(),
                return_type: ret,
            });
        }
        // `version()` / `current_schema()` / `current_database()` for PostgreSQL client compatibility!
        if lower == "version" && args.is_empty() {
            return Ok(LogicalExpr::Literal {
                value: ScalarValue::Utf8(
                    "PostgreSQL 16.0 (VectraDB 0.1.0 Vectorized OLAP Engine)".into(),
                ),
                data_type: LogicalType::Utf8,
            });
        }
        if lower == "current_schema" && args.is_empty() {
            return Ok(LogicalExpr::Literal {
                value: ScalarValue::Utf8("public".into()),
                data_type: LogicalType::Utf8,
            });
        }
        if lower == "current_database" && args.is_empty() {
            return Ok(LogicalExpr::Literal {
                value: ScalarValue::Utf8("vectradb".into()),
                data_type: LogicalType::Utf8,
            });
        }

        let mut bargs = Vec::with_capacity(args.len());
        for a in args {
            bargs.push(self.bind_expr_internal(a, scope, plan, windows, allow_windows)?);
        }

        match lower.as_str() {
            "lower" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Lower,
                args: vec![coerce_expr(
                    take_arg(&bargs, 0, "lower")?,
                    LogicalType::Utf8,
                )],
                return_type: LogicalType::Utf8,
            }),
            "upper" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Upper,
                args: vec![coerce_expr(
                    take_arg(&bargs, 0, "upper")?,
                    LogicalType::Utf8,
                )],
                return_type: LogicalType::Utf8,
            }),
            "length" | "char_length" | "character_length" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Length,
                args: vec![take_arg(&bargs, 0, &lower)?],
                return_type: LogicalType::Int64,
            }),
            "substring" | "substr" => {
                let s = coerce_expr(take_arg(&bargs, 0, &lower)?, LogicalType::Utf8);
                let start = coerce_expr(take_arg(&bargs, 1, &lower)?, LogicalType::Int64);
                let mut fargs = vec![s, start];
                if bargs.len() >= 3 {
                    fargs.push(coerce_expr(bargs[2].clone(), LogicalType::Int64));
                }
                Ok(LogicalExpr::ScalarFunc {
                    func: ScalarFunction::Substring,
                    args: fargs,
                    return_type: LogicalType::Utf8,
                })
            }
            "trim" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Trim,
                args: bargs
                    .into_iter()
                    .map(|e| coerce_expr(e, LogicalType::Utf8))
                    .collect(),
                return_type: LogicalType::Utf8,
            }),
            "ltrim" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Ltrim,
                args: bargs
                    .into_iter()
                    .map(|e| coerce_expr(e, LogicalType::Utf8))
                    .collect(),
                return_type: LogicalType::Utf8,
            }),
            "rtrim" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Rtrim,
                args: bargs
                    .into_iter()
                    .map(|e| coerce_expr(e, LogicalType::Utf8))
                    .collect(),
                return_type: LogicalType::Utf8,
            }),
            "concat" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Concat,
                args: bargs,
                return_type: LogicalType::Utf8,
            }),
            "concat_ws" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::ConcatWs,
                args: bargs,
                return_type: LogicalType::Utf8,
            }),
            "replace" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Replace,
                args: vec![
                    coerce_expr(take_arg(&bargs, 0, "replace")?, LogicalType::Utf8),
                    coerce_expr(take_arg(&bargs, 1, "replace")?, LogicalType::Utf8),
                    coerce_expr(take_arg(&bargs, 2, "replace")?, LogicalType::Utf8),
                ],
                return_type: LogicalType::Utf8,
            }),
            "starts_with" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::StartsWith,
                args: vec![
                    coerce_expr(take_arg(&bargs, 0, "starts_with")?, LogicalType::Utf8),
                    coerce_expr(take_arg(&bargs, 1, "starts_with")?, LogicalType::Utf8),
                ],
                return_type: LogicalType::Boolean,
            }),
            "ends_with" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::EndsWith,
                args: vec![
                    coerce_expr(take_arg(&bargs, 0, "ends_with")?, LogicalType::Utf8),
                    coerce_expr(take_arg(&bargs, 1, "ends_with")?, LogicalType::Utf8),
                ],
                return_type: LogicalType::Boolean,
            }),
            "position" | "strpos" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Position,
                args: vec![
                    coerce_expr(take_arg(&bargs, 0, &lower)?, LogicalType::Utf8),
                    coerce_expr(take_arg(&bargs, 1, &lower)?, LogicalType::Utf8),
                ],
                return_type: LogicalType::Int64,
            }),
            "left" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Left,
                args: vec![
                    coerce_expr(take_arg(&bargs, 0, "left")?, LogicalType::Utf8),
                    coerce_expr(take_arg(&bargs, 1, "left")?, LogicalType::Int64),
                ],
                return_type: LogicalType::Utf8,
            }),
            "right" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Right,
                args: vec![
                    coerce_expr(take_arg(&bargs, 0, "right")?, LogicalType::Utf8),
                    coerce_expr(take_arg(&bargs, 1, "right")?, LogicalType::Int64),
                ],
                return_type: LogicalType::Utf8,
            }),
            "abs" => {
                let a = take_arg(&bargs, 0, "abs")?;
                let ret = a.data_type();
                Ok(LogicalExpr::ScalarFunc {
                    func: ScalarFunction::Abs,
                    args: vec![a],
                    return_type: ret,
                })
            }
            "round" => {
                let a = take_arg(&bargs, 0, "round")?;
                let ret = match a.data_type() {
                    LogicalType::Decimal128 { precision, scale } => LogicalType::Decimal128 {
                        precision: (precision + 1).min(MAX_DECIMAL_PRECISION),
                        scale,
                    },
                    _ => LogicalType::Float64,
                };
                let mut fargs = vec![a];
                if bargs.len() >= 2 {
                    fargs.push(coerce_expr(bargs[1].clone(), LogicalType::Int64));
                }
                Ok(LogicalExpr::ScalarFunc {
                    func: ScalarFunction::Round,
                    args: fargs,
                    return_type: ret,
                })
            }
            "ceil" | "ceiling" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Ceil,
                args: vec![coerce_expr(
                    take_arg(&bargs, 0, &lower)?,
                    LogicalType::Float64,
                )],
                return_type: LogicalType::Float64,
            }),
            "floor" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Floor,
                args: vec![coerce_expr(
                    take_arg(&bargs, 0, "floor")?,
                    LogicalType::Float64,
                )],
                return_type: LogicalType::Float64,
            }),
            "power" | "pow" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Power,
                args: vec![
                    coerce_expr(take_arg(&bargs, 0, &lower)?, LogicalType::Float64),
                    coerce_expr(take_arg(&bargs, 1, &lower)?, LogicalType::Float64),
                ],
                return_type: LogicalType::Float64,
            }),
            "sqrt" => Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Sqrt,
                args: vec![coerce_expr(
                    take_arg(&bargs, 0, "sqrt")?,
                    LogicalType::Float64,
                )],
                return_type: LogicalType::Float64,
            }),
            "nullif" => {
                let a = take_arg(&bargs, 0, "nullif")?;
                let b = take_arg(&bargs, 1, "nullif")?;
                let common = common_supertype(a.data_type(), b.data_type())?;
                Ok(LogicalExpr::ScalarFunc {
                    func: ScalarFunction::NullIf,
                    args: vec![coerce_expr(a, common), coerce_expr(b, common)],
                    return_type: common,
                })
            }
            "greatest" | "least" => {
                let mut common = LogicalType::Null;
                for a in &bargs {
                    common = common_supertype(common, a.data_type())?;
                }
                if common == LogicalType::Null {
                    common = LogicalType::Int64;
                }
                Ok(LogicalExpr::ScalarFunc {
                    func: if lower == "greatest" {
                        ScalarFunction::Greatest
                    } else {
                        ScalarFunction::Least
                    },
                    args: bargs.into_iter().map(|e| coerce_expr(e, common)).collect(),
                    return_type: common,
                })
            }
            "date_trunc" => {
                let part_str = match &args[0] {
                    Expr::Literal(LiteralValue::String(s)) => s.to_ascii_lowercase(),
                    _ => {
                        return Err(VectraError::BindError(
                            "DATE_TRUNC first argument must be a string literal".into(),
                        ))
                    }
                };
                let part = match part_str.as_str() {
                    "year" => DatePart::Year,
                    "quarter" => DatePart::Quarter,
                    "month" => DatePart::Month,
                    "day" => DatePart::Day,
                    "hour" => DatePart::Hour,
                    "minute" => DatePart::Minute,
                    "second" => DatePart::Second,
                    _ => {
                        return Err(VectraError::BindError(format!(
                            "unknown DATE_TRUNC part `{part_str}`"
                        )))
                    }
                };
                let target = take_arg(&bargs, 1, "date_trunc")?;
                let ret = target.data_type();
                Ok(LogicalExpr::ScalarFunc {
                    func: ScalarFunction::DateTrunc(part),
                    args: vec![target],
                    return_type: ret,
                })
            }
            "date_diff" | "datediff" => {
                let a = take_arg(&bargs, 0, &lower)?;
                let b = take_arg(&bargs, 1, &lower)?;
                let common = common_supertype(a.data_type(), b.data_type())?;
                Ok(LogicalExpr::ScalarFunc {
                    func: ScalarFunction::DateDiffDays,
                    args: vec![coerce_expr(a, common), coerce_expr(b, common)],
                    return_type: LogicalType::Int64,
                })
            }
            _ => Err(VectraError::BindError(format!(
                "unknown scalar function `{name}`"
            ))),
        }
    }

    fn bind_window_function(
        &mut self,
        name: &str,
        args: &[Expr],
        wildcard_arg: bool,
        spec: &WindowSpec,
        scope: &Scope,
        plan: &mut LogicalPlan,
    ) -> Result<WindowExpr> {
        let lower = name.to_ascii_lowercase();
        let resolved_spec = if let Some(wn) = &spec.window_name {
            let base = self
                .named_windows
                .get(&wn.to_ascii_lowercase())
                .ok_or_else(|| VectraError::BindError(format!("unknown window `{wn}`")))?
                .clone();
            WindowSpec {
                window_name: None,
                partition_by: if spec.partition_by.is_empty() {
                    base.partition_by
                } else {
                    spec.partition_by.clone()
                },
                order_by: if spec.order_by.is_empty() {
                    base.order_by
                } else {
                    spec.order_by.clone()
                },
                frame: spec.frame.clone().or(base.frame),
            }
        } else {
            spec.clone()
        };

        let mut bargs = Vec::with_capacity(args.len());
        for a in args {
            bargs.push(self.bind_expr(a, scope, plan, false)?);
        }

        let (func, return_type) = match lower.as_str() {
            "row_number" => (WindowFunc::RowNumber, LogicalType::Int64),
            "rank" => (WindowFunc::Rank, LogicalType::Int64),
            "dense_rank" => (WindowFunc::DenseRank, LogicalType::Int64),
            "ntile" => (WindowFunc::Ntile, LogicalType::Int64),
            "lead" => {
                let ty = take_arg(&bargs, 0, "lead")?.data_type();
                (WindowFunc::Lead, ty)
            }
            "lag" => {
                let ty = take_arg(&bargs, 0, "lag")?.data_type();
                (WindowFunc::Lag, ty)
            }
            "first_value" => {
                let ty = take_arg(&bargs, 0, "first_value")?.data_type();
                (WindowFunc::FirstValue, ty)
            }
            "last_value" => {
                let ty = take_arg(&bargs, 0, "last_value")?.data_type();
                (WindowFunc::LastValue, ty)
            }
            _ => {
                let (af, rt) = resolve_agg_func(&lower, &bargs, wildcard_arg)?;
                (WindowFunc::Aggregate(af), rt)
            }
        };

        let mut partition_by = Vec::with_capacity(resolved_spec.partition_by.len());
        for p in &resolved_spec.partition_by {
            partition_by.push(self.bind_expr(p, scope, plan, false)?);
        }
        let mut order_by = Vec::with_capacity(resolved_spec.order_by.len());
        for o in &resolved_spec.order_by {
            let e = self.bind_expr(&o.expr, scope, plan, false)?;
            let asc = o.asc.unwrap_or(true);
            let nulls_first = o.nulls_first.unwrap_or(!asc);
            order_by.push(LogicalSortExpr {
                expr: e,
                order: SortOrder {
                    ascending: asc,
                    nulls_first,
                },
            });
        }

        Ok(WindowExpr {
            func,
            args: bargs,
            partition_by,
            order_by,
            frame: resolved_spec.frame,
            return_type,
        })
    }

    fn bind_post_agg_expr(
        &mut self,
        expr: &Expr,
        ctx: &mut AggRewriteCtx<'_>,
        plan: &mut LogicalPlan,
    ) -> Result<LogicalExpr> {
        // 1. Check if `expr` matches a GROUP BY expression directly!
        if let Some(pos) = ctx.group_ast_exprs.iter().position(|g| g == expr) {
            return Ok(ctx.group_by_bound[pos].1.to_expr());
        }

        match expr {
            Expr::Nested(inner) => self.bind_post_agg_expr(inner, ctx, plan),
            Expr::Literal(l) => self.bind_literal(l),
            Expr::Param(_) => self.bind_expr(expr, ctx.from_scope, plan, false),
            Expr::Identifier(name) => {
                // Match against GROUP BY columns by name/ColId
                if let Some(from_col) = ctx.from_scope.resolve(None, name)? {
                    for (ge, gc) in ctx.group_by_bound {
                        if matches!(ge, LogicalExpr::Col { id, .. } if *id == from_col.id) {
                            return Ok(gc.to_expr());
                        }
                    }
                    return Err(VectraError::BindError(format!(
                        "column `{name}` must appear in the GROUP BY clause or be used in an aggregate function"
                    )));
                }
                // Check outer correlated scope
                self.resolve_column_ref(None, name, ctx.from_scope)
            }
            Expr::CompoundIdentifier(parts) => {
                let col_name = parts.last().unwrap();
                let qual = parts.get(parts.len() - 2).map(String::as_str);
                if let Some(from_col) = ctx.from_scope.resolve(qual, col_name)? {
                    for (ge, gc) in ctx.group_by_bound {
                        if matches!(ge, LogicalExpr::Col { id, .. } if *id == from_col.id) {
                            return Ok(gc.to_expr());
                        }
                    }
                    return Err(VectraError::BindError(format!(
                        "column `{}.{}` must appear in GROUP BY or be aggregated",
                        qual.unwrap_or(""),
                        col_name
                    )));
                }
                self.resolve_column_ref(qual, col_name, ctx.from_scope)
            }
            Expr::Function {
                name,
                args,
                distinct,
                wildcard_arg,
                filter,
                over,
            } => {
                let lower = name.to_ascii_lowercase();
                if lower == "grouping" {
                    // GROUPING(col) checks bit inside __grouping_id
                    if let Some(gid_col) = ctx.grouping_id_col {
                        let bit_pos = args
                            .first()
                            .and_then(|a| ctx.group_ast_exprs.iter().position(|g| g == a))
                            .unwrap_or(0);
                        // Extract bit via integer division/mod: (__grouping_id / 2^bit) % 2
                        let div = LogicalExpr::BinaryOp {
                            op: BinaryOperator::Div,
                            left: Box::new(gid_col.to_expr()),
                            right: Box::new(LogicalExpr::Literal {
                                value: ScalarValue::Int64(1i64 << bit_pos),
                                data_type: LogicalType::Int64,
                            }),
                            return_type: LogicalType::Int64,
                        };
                        return Ok(LogicalExpr::BinaryOp {
                            op: BinaryOperator::Mod,
                            left: Box::new(div),
                            right: Box::new(LogicalExpr::Literal {
                                value: ScalarValue::Int64(2),
                                data_type: LogicalType::Int64,
                            }),
                            return_type: LogicalType::Int64,
                        });
                    }
                    return Ok(LogicalExpr::Literal {
                        value: ScalarValue::Int64(0),
                        data_type: LogicalType::Int64,
                    });
                }

                if let Some(wspec) = over {
                    if !ctx.allow_windows {
                        return Err(VectraError::BindError(
                            "window functions not allowed in HAVING".into(),
                        ));
                    }
                    let mut bargs = Vec::with_capacity(args.len());
                    for a in args {
                        bargs.push(self.bind_post_agg_expr(a, ctx, plan)?);
                    }
                    let (wfunc, ret_ty) = match lower.as_str() {
                        "row_number" => (WindowFunc::RowNumber, LogicalType::Int64),
                        "rank" => (WindowFunc::Rank, LogicalType::Int64),
                        "dense_rank" => (WindowFunc::DenseRank, LogicalType::Int64),
                        "ntile" => (WindowFunc::Ntile, LogicalType::Int64),
                        "lead" => (WindowFunc::Lead, take_arg(&bargs, 0, "lead")?.data_type()),
                        "lag" => (WindowFunc::Lag, take_arg(&bargs, 0, "lag")?.data_type()),
                        "first_value" => (
                            WindowFunc::FirstValue,
                            take_arg(&bargs, 0, "first_value")?.data_type(),
                        ),
                        "last_value" => (
                            WindowFunc::LastValue,
                            take_arg(&bargs, 0, "last_value")?.data_type(),
                        ),
                        _ => {
                            let (af, rt) = resolve_agg_func(&lower, &bargs, *wildcard_arg)?;
                            (WindowFunc::Aggregate(af), rt)
                        }
                    };
                    let mut pby = Vec::new();
                    for p in &wspec.partition_by {
                        pby.push(self.bind_post_agg_expr(p, ctx, plan)?);
                    }
                    let mut oby = Vec::new();
                    for o in &wspec.order_by {
                        let e = self.bind_post_agg_expr(&o.expr, ctx, plan)?;
                        let asc = o.asc.unwrap_or(true);
                        let nf = o.nulls_first.unwrap_or(!asc);
                        oby.push(LogicalSortExpr {
                            expr: e,
                            order: SortOrder {
                                ascending: asc,
                                nulls_first: nf,
                            },
                        });
                    }
                    let wexpr = WindowExpr {
                        func: wfunc,
                        args: bargs,
                        partition_by: pby,
                        order_by: oby,
                        frame: wspec.frame.clone(),
                        return_type: ret_ty,
                    };
                    let wcol = BoundCol::new(
                        self.col_gen.next_id(),
                        format!("__win_{}_{}", lower, ctx.windows.len()),
                        ret_ty,
                    );
                    ctx.windows.push((wexpr, wcol.clone()));
                    return Ok(wcol.to_expr());
                }

                if is_aggregate_name(&lower) {
                    let mut bargs = Vec::with_capacity(args.len());
                    for a in args {
                        if expr_contains_aggregate(a) {
                            return Err(VectraError::BindError(
                                "nested aggregate functions are not allowed".into(),
                            ));
                        }
                        bargs.push(self.bind_expr(a, ctx.from_scope, plan, false)?);
                    }
                    let bound_filter = match filter {
                        Some(f) => Some(coerce_expr(
                            self.bind_expr(f, ctx.from_scope, plan, false)?,
                            LogicalType::Boolean,
                        )),
                        None => None,
                    };
                    let (func, return_type) = resolve_agg_func(&lower, &bargs, *wildcard_arg)?;
                    let agg_expr = AggregateExpr {
                        func,
                        args: bargs,
                        distinct: *distinct,
                        filter: bound_filter,
                        return_type,
                    };
                    // Deduplicate identical aggregates!
                    if let Some((_, existing_col)) =
                        ctx.aggregates.iter().find(|(ae, _)| ae == &agg_expr)
                    {
                        return Ok(existing_col.to_expr());
                    }
                    let col = BoundCol::new(
                        self.col_gen.next_id(),
                        format!("__agg_{}_{}", lower, ctx.aggregates.len()),
                        return_type,
                    );
                    ctx.aggregates.push((agg_expr, col.clone()));
                    return Ok(col.to_expr());
                }

                // Scalar function over post-aggregate arguments
                if lower == "coalesce" {
                    let mut bargs = Vec::with_capacity(args.len());
                    let mut ret = LogicalType::Null;
                    for a in args {
                        let b = self.bind_post_agg_expr(a, ctx, plan)?;
                        ret = common_supertype(ret, b.data_type())?;
                        bargs.push(b);
                    }
                    if ret == LogicalType::Null {
                        ret = LogicalType::Int64;
                    }
                    return Ok(LogicalExpr::Coalesce {
                        args: bargs.into_iter().map(|e| coerce_expr(e, ret)).collect(),
                        return_type: ret,
                    });
                }
                let mut bargs = Vec::with_capacity(args.len());
                for a in args {
                    bargs.push(self.bind_post_agg_expr(a, ctx, plan)?);
                }
                bind_scalar_func_from_bound_args(&lower, bargs)
            }
            Expr::UnaryOp { op, expr: inner } => {
                let b = self.bind_post_agg_expr(inner, ctx, plan)?;
                match op {
                    AstUnaryOp::Pos => Ok(b),
                    AstUnaryOp::Neg => {
                        let ty = b.data_type();
                        Ok(LogicalExpr::UnaryOp {
                            op: UnaryOperator::Neg,
                            expr: Box::new(b),
                            return_type: ty,
                        })
                    }
                    AstUnaryOp::Not => Ok(LogicalExpr::UnaryOp {
                        op: UnaryOperator::Not,
                        expr: Box::new(coerce_expr(b, LogicalType::Boolean)),
                        return_type: LogicalType::Boolean,
                    }),
                }
            }
            Expr::IsNull {
                expr: inner,
                negated,
            } => {
                let b = self.bind_post_agg_expr(inner, ctx, plan)?;
                Ok(LogicalExpr::UnaryOp {
                    op: if *negated {
                        UnaryOperator::IsNotNull
                    } else {
                        UnaryOperator::IsNull
                    },
                    expr: Box::new(b),
                    return_type: LogicalType::Boolean,
                })
            }
            Expr::BinaryOp { left, op, right } => {
                let lb = self.bind_post_agg_expr(left, ctx, plan)?;
                let rb = self.bind_post_agg_expr(right, ctx, plan)?;
                self.build_binary_expr(*op, lb, rb)
            }
            Expr::Cast {
                expr: inner,
                data_type,
                try_cast,
            } => {
                let b = self.bind_post_agg_expr(inner, ctx, plan)?;
                Ok(LogicalExpr::Cast {
                    expr: Box::new(b),
                    target_type: *data_type,
                    try_cast: *try_cast,
                })
            }
            Expr::Case {
                operand,
                branches,
                else_result,
            } => {
                let op_b = match operand {
                    Some(o) => Some(self.bind_post_agg_expr(o, ctx, plan)?),
                    None => None,
                };
                let mut bb = Vec::with_capacity(branches.len());
                let mut ret = LogicalType::Null;
                for (w, t) in branches {
                    let wb = self.bind_post_agg_expr(w, ctx, plan)?;
                    let cond = match &op_b {
                        Some(ob) => {
                            let c = common_supertype(ob.data_type(), wb.data_type())?;
                            LogicalExpr::BinaryOp {
                                op: BinaryOperator::Eq,
                                left: Box::new(coerce_expr(ob.clone(), c)),
                                right: Box::new(coerce_expr(wb, c)),
                                return_type: LogicalType::Boolean,
                            }
                        }
                        None => coerce_expr(wb, LogicalType::Boolean),
                    };
                    let tb = self.bind_post_agg_expr(t, ctx, plan)?;
                    ret = common_supertype(ret, tb.data_type())?;
                    bb.push((cond, tb));
                }
                let eb = match else_result {
                    Some(e) => {
                        let b = self.bind_post_agg_expr(e, ctx, plan)?;
                        ret = common_supertype(ret, b.data_type())?;
                        Some(b)
                    }
                    None => None,
                };
                if ret == LogicalType::Null {
                    ret = LogicalType::Int64;
                }
                Ok(LogicalExpr::CaseWhen {
                    branches: bb
                        .into_iter()
                        .map(|(c, t)| (c, coerce_expr(t, ret)))
                        .collect(),
                    else_expr: eb.map(|e| Box::new(coerce_expr(e, ret))),
                    return_type: ret,
                })
            }
            Expr::Between {
                expr: target,
                low,
                high,
                negated,
            } => {
                let tb = self.bind_post_agg_expr(target, ctx, plan)?;
                let lb = self.bind_post_agg_expr(low, ctx, plan)?;
                let hb = self.bind_post_agg_expr(high, ctx, plan)?;
                let c1 = common_supertype(tb.data_type(), lb.data_type())?;
                let c = common_supertype(c1, hb.data_type())?;
                Ok(LogicalExpr::Between {
                    expr: Box::new(coerce_expr(tb, c)),
                    low: Box::new(coerce_expr(lb, c)),
                    high: Box::new(coerce_expr(hb, c)),
                    negated: *negated,
                })
            }
            Expr::InList {
                expr: target,
                list,
                negated,
            } => {
                let tb = self.bind_post_agg_expr(target, ctx, plan)?;
                let mut c = tb.data_type();
                let mut bl = Vec::with_capacity(list.len());
                for item in list {
                    let ib = self.bind_post_agg_expr(item, ctx, plan)?;
                    c = common_supertype(c, ib.data_type())?;
                    bl.push(ib);
                }
                Ok(LogicalExpr::InList {
                    expr: Box::new(coerce_expr(tb, c)),
                    list: bl.into_iter().map(|e| coerce_expr(e, c)).collect(),
                    negated: *negated,
                })
            }
            Expr::Like {
                expr: target,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => {
                let tb = self.bind_post_agg_expr(target, ctx, plan)?;
                let pb = self.bind_post_agg_expr(pattern, ctx, plan)?;
                Ok(LogicalExpr::Like {
                    expr: Box::new(coerce_expr(tb, LogicalType::Utf8)),
                    pattern: Box::new(coerce_expr(pb, LogicalType::Utf8)),
                    escape: *escape,
                    case_insensitive: *case_insensitive,
                    negated: *negated,
                })
            }
            Expr::Extract { part, expr: inner } => {
                let b = self.bind_post_agg_expr(inner, ctx, plan)?;
                Ok(LogicalExpr::ScalarFunc {
                    func: ScalarFunction::Extract(*part),
                    args: vec![b],
                    return_type: LogicalType::Int64,
                })
            }
            Expr::Subquery(_)
            | Expr::Exists { .. }
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. } => self.bind_expr(expr, ctx.from_scope, plan, false),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_having_with_subquery(
        &mut self,
        expr: &Expr,
        from_scope: &Scope,
        post_agg_scope: &Scope,
        group_by_bound: &[(LogicalExpr, BoundCol)],
        group_ast_exprs: &[Expr],
        aggregates: &[(AggregateExpr, BoundCol)],
        post_agg_plan: &mut LogicalPlan,
    ) -> Result<LogicalExpr> {
        // Check group_by match
        if let Some(pos) = group_ast_exprs.iter().position(|g| g == expr) {
            return Ok(group_by_bound[pos].1.to_expr());
        }
        match expr {
            Expr::Nested(inner) => self.bind_having_with_subquery(
                inner,
                from_scope,
                post_agg_scope,
                group_by_bound,
                group_ast_exprs,
                aggregates,
                post_agg_plan,
            ),
            Expr::Function {
                name,
                args,
                distinct,
                wildcard_arg,
                filter,
                ..
            } if is_aggregate_name(name) => {
                let mut dummy = LogicalPlan::EmptyRelation {
                    produce_one_row: true,
                    columns: Vec::new(),
                };
                let mut bargs = Vec::with_capacity(args.len());
                for a in args {
                    bargs.push(self.bind_expr(a, from_scope, &mut dummy, false)?);
                }
                let bound_filter = match filter {
                    Some(f) => Some(coerce_expr(
                        self.bind_expr(f, from_scope, &mut dummy, false)?,
                        LogicalType::Boolean,
                    )),
                    None => None,
                };
                let (func, return_type) =
                    resolve_agg_func(&name.to_ascii_lowercase(), &bargs, *wildcard_arg)?;
                let target = AggregateExpr {
                    func,
                    args: bargs,
                    distinct: *distinct,
                    filter: bound_filter,
                    return_type,
                };
                aggregates
                    .iter()
                    .find(|(ae, _)| ae == &target)
                    .map(|(_, col)| col.to_expr())
                    .ok_or_else(|| {
                        VectraError::BindError(format!("aggregate `{name}` in HAVING not resolved"))
                    })
            }
            Expr::BinaryOp { left, op, right } => {
                let lb = self.bind_having_with_subquery(
                    left,
                    from_scope,
                    post_agg_scope,
                    group_by_bound,
                    group_ast_exprs,
                    aggregates,
                    post_agg_plan,
                )?;
                let rb = self.bind_having_with_subquery(
                    right,
                    from_scope,
                    post_agg_scope,
                    group_by_bound,
                    group_ast_exprs,
                    aggregates,
                    post_agg_plan,
                )?;
                self.build_binary_expr(*op, lb, rb)
            }
            Expr::UnaryOp { op, expr: inner } => {
                let b = self.bind_having_with_subquery(
                    inner,
                    from_scope,
                    post_agg_scope,
                    group_by_bound,
                    group_ast_exprs,
                    aggregates,
                    post_agg_plan,
                )?;
                match op {
                    AstUnaryOp::Pos => Ok(b),
                    AstUnaryOp::Neg => {
                        let ty = b.data_type();
                        Ok(LogicalExpr::UnaryOp {
                            op: UnaryOperator::Neg,
                            expr: Box::new(b),
                            return_type: ty,
                        })
                    }
                    AstUnaryOp::Not => Ok(LogicalExpr::UnaryOp {
                        op: UnaryOperator::Not,
                        expr: Box::new(coerce_expr(b, LogicalType::Boolean)),
                        return_type: LogicalType::Boolean,
                    }),
                }
            }
            _ => self.bind_expr(expr, post_agg_scope, post_agg_plan, false),
        }
    }
}

struct AggRewriteCtx<'a> {
    from_scope: &'a Scope,
    group_by_bound: &'a [(LogicalExpr, BoundCol)],
    group_ast_exprs: &'a [Expr],
    grouping_id_col: Option<&'a BoundCol>,
    aggregates: &'a mut Vec<(AggregateExpr, BoundCol)>,
    windows: &'a mut Vec<(WindowExpr, BoundCol)>,
    allow_windows: bool,
}

fn collect_aggs_from_ast(
    binder: &mut Binder<'_>,
    expr: &Expr,
    ctx: &mut AggRewriteCtx<'_>,
    plan: &mut LogicalPlan,
) -> Result<()> {
    match expr {
        Expr::Function {
            name, over: None, ..
        } if is_aggregate_name(name) => {
            let _ = binder.bind_post_agg_expr(expr, ctx, plan)?;
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_aggs_from_ast(binder, left, ctx, plan)?;
            collect_aggs_from_ast(binder, right, ctx, plan)
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull { expr: inner, .. }
        | Expr::Cast { expr: inner, .. }
        | Expr::Nested(inner) => collect_aggs_from_ast(binder, inner, ctx, plan),
        Expr::Between {
            expr: e, low, high, ..
        } => {
            collect_aggs_from_ast(binder, e, ctx, plan)?;
            collect_aggs_from_ast(binder, low, ctx, plan)?;
            collect_aggs_from_ast(binder, high, ctx, plan)
        }
        Expr::Case {
            operand,
            branches,
            else_result,
        } => {
            if let Some(o) = operand {
                collect_aggs_from_ast(binder, o, ctx, plan)?;
            }
            for (w, t) in branches {
                collect_aggs_from_ast(binder, w, ctx, plan)?;
                collect_aggs_from_ast(binder, t, ctx, plan)?;
            }
            if let Some(e) = else_result {
                collect_aggs_from_ast(binder, e, ctx, plan)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn extract_local_correlated_cols(sub_plan: &LogicalPlan, scope: &Scope) -> Vec<BoundCol> {
    let free = sub_plan.free_correlated_vars();
    scope
        .cols
        .iter()
        .filter(|c| free.contains(&c.id))
        .cloned()
        .collect()
}

fn attach_subquery_join(
    plan: &mut LogicalPlan,
    sub_plan: LogicalPlan,
    join_type: JoinType,
    condition: Option<LogicalExpr>,
    corr_cols: Vec<BoundCol>,
) {
    let left = std::mem::replace(
        plan,
        LogicalPlan::EmptyRelation {
            produce_one_row: false,
            columns: Vec::new(),
        },
    );
    *plan = if corr_cols.is_empty() {
        LogicalPlan::Join {
            left: Box::new(left),
            right: Box::new(sub_plan),
            join_type,
            condition,
        }
    } else {
        LogicalPlan::DependentJoin {
            left: Box::new(left),
            right: Box::new(sub_plan),
            join_type,
            condition,
            correlated_cols: corr_cols,
        }
    };
}

fn bind_date_interval_op(
    lhs: LogicalExpr,
    signed_val: i64,
    unit: IntervalUnit,
) -> Result<LogicalExpr> {
    let lty = lhs.data_type();
    if !matches!(lty, LogicalType::Date32 | LogicalType::TimestampMicros) {
        return Err(VectraError::TypeMismatch {
            expected: "DATE or TIMESTAMP".into(),
            actual: lty.to_string(),
        });
    }
    let (func, delta) = match unit {
        IntervalUnit::Day => (ScalarFunction::DateAddDays, signed_val),
        IntervalUnit::Month => (ScalarFunction::DateAddMonths, signed_val),
        IntervalUnit::Year => (ScalarFunction::DateAddYears, signed_val),
        IntervalUnit::Hour => (
            ScalarFunction::TimestampAddMicros,
            signed_val.saturating_mul(MICROS_PER_HOUR),
        ),
        IntervalUnit::Minute => (
            ScalarFunction::TimestampAddMicros,
            signed_val.saturating_mul(MICROS_PER_MINUTE),
        ),
        IntervalUnit::Second => (
            ScalarFunction::TimestampAddMicros,
            signed_val.saturating_mul(MICROS_PER_SECOND),
        ),
    };
    let lhs_eff =
        if matches!(func, ScalarFunction::TimestampAddMicros) && lty == LogicalType::Date32 {
            coerce_expr(lhs, LogicalType::TimestampMicros)
        } else {
            lhs
        };
    let ret = lhs_eff.data_type();
    Ok(LogicalExpr::ScalarFunc {
        func,
        args: vec![
            lhs_eff,
            LogicalExpr::Literal {
                value: ScalarValue::Int64(delta),
                data_type: LogicalType::Int64,
            },
        ],
        return_type: ret,
    })
}

fn bind_scalar_func_from_bound_args(lower: &str, bargs: Vec<LogicalExpr>) -> Result<LogicalExpr> {
    match lower {
        "lower" => Ok(LogicalExpr::ScalarFunc {
            func: ScalarFunction::Lower,
            args: vec![coerce_expr(
                take_arg(&bargs, 0, "lower")?,
                LogicalType::Utf8,
            )],
            return_type: LogicalType::Utf8,
        }),
        "upper" => Ok(LogicalExpr::ScalarFunc {
            func: ScalarFunction::Upper,
            args: vec![coerce_expr(
                take_arg(&bargs, 0, "upper")?,
                LogicalType::Utf8,
            )],
            return_type: LogicalType::Utf8,
        }),
        "length" | "char_length" => Ok(LogicalExpr::ScalarFunc {
            func: ScalarFunction::Length,
            args: vec![take_arg(&bargs, 0, lower)?],
            return_type: LogicalType::Int64,
        }),
        "abs" => {
            let a = take_arg(&bargs, 0, "abs")?;
            let ty = a.data_type();
            Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Abs,
                args: vec![a],
                return_type: ty,
            })
        }
        "round" => {
            let a = take_arg(&bargs, 0, "round")?;
            let ty = match a.data_type() {
                LogicalType::Decimal128 { precision, scale } => LogicalType::Decimal128 {
                    precision: (precision + 1).min(MAX_DECIMAL_PRECISION),
                    scale,
                },
                _ => LogicalType::Float64,
            };
            Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::Round,
                args: bargs,
                return_type: ty,
            })
        }
        "ceil" | "ceiling" => Ok(LogicalExpr::ScalarFunc {
            func: ScalarFunction::Ceil,
            args: vec![coerce_expr(
                take_arg(&bargs, 0, lower)?,
                LogicalType::Float64,
            )],
            return_type: LogicalType::Float64,
        }),
        "floor" => Ok(LogicalExpr::ScalarFunc {
            func: ScalarFunction::Floor,
            args: vec![coerce_expr(
                take_arg(&bargs, 0, "floor")?,
                LogicalType::Float64,
            )],
            return_type: LogicalType::Float64,
        }),
        "nullif" => {
            let a = take_arg(&bargs, 0, "nullif")?;
            let b = take_arg(&bargs, 1, "nullif")?;
            let c = common_supertype(a.data_type(), b.data_type())?;
            Ok(LogicalExpr::ScalarFunc {
                func: ScalarFunction::NullIf,
                args: vec![coerce_expr(a, c), coerce_expr(b, c)],
                return_type: c,
            })
        }
        "substring" | "substr" => Ok(LogicalExpr::ScalarFunc {
            func: ScalarFunction::Substring,
            args: bargs,
            return_type: LogicalType::Utf8,
        }),
        "concat" => Ok(LogicalExpr::ScalarFunc {
            func: ScalarFunction::Concat,
            args: bargs,
            return_type: LogicalType::Utf8,
        }),
        _ => Err(VectraError::BindError(format!(
            "unsupported scalar function `{lower}` in post-aggregate context"
        ))),
    }
}

fn take_arg(args: &[LogicalExpr], idx: usize, fn_name: &str) -> Result<LogicalExpr> {
    args.get(idx).cloned().ok_or_else(|| {
        VectraError::BindError(format!(
            "function `{fn_name}` missing required argument #{}",
            idx + 1
        ))
    })
}

fn ast_binop_to_phys(op: AstBinaryOp) -> Result<BinaryOperator> {
    Ok(match op {
        AstBinaryOp::Add => BinaryOperator::Add,
        AstBinaryOp::Sub => BinaryOperator::Sub,
        AstBinaryOp::Mul => BinaryOperator::Mul,
        AstBinaryOp::Div => BinaryOperator::Div,
        AstBinaryOp::Mod => BinaryOperator::Mod,
        AstBinaryOp::Eq => BinaryOperator::Eq,
        AstBinaryOp::NotEq => BinaryOperator::NotEq,
        AstBinaryOp::Lt => BinaryOperator::Lt,
        AstBinaryOp::LtEq => BinaryOperator::LtEq,
        AstBinaryOp::Gt => BinaryOperator::Gt,
        AstBinaryOp::GtEq => BinaryOperator::GtEq,
        AstBinaryOp::And => BinaryOperator::And,
        AstBinaryOp::Or => BinaryOperator::Or,
        AstBinaryOp::Concat => BinaryOperator::Concat,
        AstBinaryOp::IsDistinctFrom => BinaryOperator::IsDistinctFrom,
        AstBinaryOp::IsNotDistinctFrom => BinaryOperator::IsNotDistinctFrom,
    })
}

fn negate_cmp_op(op: BinaryOperator) -> BinaryOperator {
    match op {
        BinaryOperator::Eq => BinaryOperator::NotEq,
        BinaryOperator::NotEq => BinaryOperator::Eq,
        BinaryOperator::Lt => BinaryOperator::GtEq,
        BinaryOperator::LtEq => BinaryOperator::Gt,
        BinaryOperator::Gt => BinaryOperator::LtEq,
        BinaryOperator::GtEq => BinaryOperator::Lt,
        other => other,
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "stddev_samp"
            | "stddev"
            | "stddev_pop"
            | "bool_and"
            | "bool_or"
            | "string_agg"
            | "approx_count_distinct"
    )
}

fn resolve_agg_func(
    lower: &str,
    bargs: &[LogicalExpr],
    wildcard_arg: bool,
) -> Result<(AggregateFunc, LogicalType)> {
    match lower {
        "count" => {
            if wildcard_arg || bargs.is_empty() {
                Ok((AggregateFunc::CountStar, LogicalType::Int64))
            } else {
                Ok((AggregateFunc::Count, LogicalType::Int64))
            }
        }
        "sum" => {
            let arg = take_arg(bargs, 0, "sum")?;
            let ret = match arg.data_type() {
                LogicalType::Int8
                | LogicalType::Int16
                | LogicalType::Int32
                | LogicalType::Int64 => LogicalType::Int64,
                LogicalType::Decimal128 { scale, .. } => LogicalType::Decimal128 {
                    precision: MAX_DECIMAL_PRECISION,
                    scale,
                },
                LogicalType::Float64 => LogicalType::Float64,
                LogicalType::Null => LogicalType::Int64,
                other => {
                    return Err(VectraError::TypeMismatch {
                        expected: "numeric".into(),
                        actual: other.to_string(),
                    })
                }
            };
            Ok((AggregateFunc::Sum, ret))
        }
        "avg" => {
            let arg = take_arg(bargs, 0, "avg")?;
            let ret = match arg.data_type() {
                LogicalType::Decimal128 { scale, .. } => LogicalType::Decimal128 {
                    precision: MAX_DECIMAL_PRECISION,
                    scale: scale.clamp(4, 10),
                },
                ty if ty.is_numeric() || ty == LogicalType::Null => LogicalType::Float64,
                other => {
                    return Err(VectraError::TypeMismatch {
                        expected: "numeric".into(),
                        actual: other.to_string(),
                    })
                }
            };
            Ok((AggregateFunc::Avg, ret))
        }
        "min" => {
            let arg = take_arg(bargs, 0, "min")?;
            Ok((AggregateFunc::Min, arg.data_type()))
        }
        "max" => {
            let arg = take_arg(bargs, 0, "max")?;
            Ok((AggregateFunc::Max, arg.data_type()))
        }
        "stddev_samp" | "stddev" => {
            let _ = take_arg(bargs, 0, lower)?;
            Ok((AggregateFunc::StddevSamp, LogicalType::Float64))
        }
        "stddev_pop" => {
            let _ = take_arg(bargs, 0, lower)?;
            Ok((AggregateFunc::StddevPop, LogicalType::Float64))
        }
        "bool_and" => {
            let _ = take_arg(bargs, 0, "bool_and")?;
            Ok((AggregateFunc::BoolAnd, LogicalType::Boolean))
        }
        "bool_or" => {
            let _ = take_arg(bargs, 0, "bool_or")?;
            Ok((AggregateFunc::BoolOr, LogicalType::Boolean))
        }
        "string_agg" => {
            let _ = take_arg(bargs, 0, "string_agg")?;
            Ok((AggregateFunc::StringAgg, LogicalType::Utf8))
        }
        "approx_count_distinct" => {
            let _ = take_arg(bargs, 0, "approx_count_distinct")?;
            Ok((AggregateFunc::ApproxCountDistinct, LogicalType::Int64))
        }
        _ => Err(VectraError::BindError(format!(
            "unknown aggregate function `{lower}`"
        ))),
    }
}

fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function {
            name,
            args,
            over,
            filter,
            ..
        } => {
            if over.is_none() && is_aggregate_name(name) {
                return true;
            }
            args.iter().any(expr_contains_aggregate)
                || filter.as_deref().is_some_and(expr_contains_aggregate)
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_aggregate(left) || expr_contains_aggregate(right)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Extract { expr, .. }
        | Expr::Nested(expr) => expr_contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_contains_aggregate(expr)
                || expr_contains_aggregate(low)
                || expr_contains_aggregate(high)
        }
        Expr::InList { expr, list, .. } => {
            expr_contains_aggregate(expr) || list.iter().any(expr_contains_aggregate)
        }
        Expr::Like { expr, pattern, .. } => {
            expr_contains_aggregate(expr) || expr_contains_aggregate(pattern)
        }
        Expr::Case {
            operand,
            branches,
            else_result,
        } => {
            operand.as_deref().is_some_and(expr_contains_aggregate)
                || branches
                    .iter()
                    .any(|(w, t)| expr_contains_aggregate(w) || expr_contains_aggregate(t))
                || else_result.as_deref().is_some_and(expr_contains_aggregate)
        }
        // Do not descend into subqueries (their aggregates belong to their own SELECT!)
        Expr::Subquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Literal(_)
        | Expr::Param(_) => false,
    }
}

fn expr_contains_window(expr: &Expr) -> bool {
    match expr {
        Expr::Function { args, over, .. } => {
            over.is_some() || args.iter().any(expr_contains_window)
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_window(left) || expr_contains_window(right)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Nested(expr) => expr_contains_window(expr),
        _ => false,
    }
}

fn split_ast_and<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::BinaryOp {
            left,
            op: AstBinaryOp::And,
            right,
        } => {
            split_ast_and(left, out);
            split_ast_and(right, out);
        }
        Expr::Nested(inner) => split_ast_and(inner, out),
        other => out.push(other),
    }
}

fn expr_contains_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::Subquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. } => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_subquery(left) || expr_contains_subquery(right)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Nested(expr) => expr_contains_subquery(expr),
        Expr::Case {
            operand,
            branches,
            else_result,
        } => {
            operand.as_deref().is_some_and(expr_contains_subquery)
                || branches
                    .iter()
                    .any(|(w, t)| expr_contains_subquery(w) || expr_contains_subquery(t))
                || else_result.as_deref().is_some_and(expr_contains_subquery)
        }
        _ => false,
    }
}

fn derive_expr_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(s) => s.clone(),
        Expr::CompoundIdentifier(parts) => parts.last().cloned().unwrap_or_else(|| "col".into()),
        Expr::Function { name, .. } => name.to_ascii_lowercase(),
        Expr::Nested(inner) => derive_expr_name(inner),
        _ => "?column?".to_string(),
    }
}

fn infer_parquet_schema_from_path(path: &str) -> Result<Schema> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let bytes = std::fs::read(path)?;
        let pool = vectra_types::BoundedMemoryPool::unbounded().as_dyn();
        let (schema, _, _) = vectra_storage::read_parquet_bytes(&bytes, Some(&[]), None, &pool)?;
        // Re-read full schema without empty projection by reading metadata on full projection with AlwaysFalse
        let _ = schema;
        let (full_schema, _, _) = vectra_storage::read_parquet_bytes(&bytes, None, None, &pool)?;
        Ok(full_schema)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        Err(VectraError::BindError(
            "filesystem read_parquet unavailable in wasm32; use import_parquet".into(),
        ))
    }
}

fn infer_csv_schema_from_path(path: &str, delimiter: u8) -> Result<Schema> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let text = std::fs::read_to_string(path)?;
        let opts = vectra_storage::CsvOptions {
            delimiter,
            ..Default::default()
        };
        vectra_storage::infer_csv_schema(&text, &opts)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (path, delimiter);
        Err(VectraError::BindError(
            "filesystem read_csv unavailable in wasm32; use import_csv".into(),
        ))
    }
}
