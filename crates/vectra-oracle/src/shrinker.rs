use std::path::{Path, PathBuf};
use vectra_sql::{
    parse_single_statement, pretty_print_query, AstBinaryOp, Expr, GroupByClause, Query, Select,
    SelectItem, SetExpr, Statement,
};
use vectra_types::Result;

/// Automated AST Delta-Debugging Shrinker.
///
/// Iteratively removes clauses (`WHERE` conjuncts, `HAVING`, `GROUP BY`, `ORDER BY`, `LIMIT`,
/// joins, `SELECT` projections, `CASE`/`OR`/`AND` branches, set-op branches) while verifying
/// via `reproduces(&sql)` that the differential bug still triggers, converging to a minimal
/// failing SQL reproducer.
pub fn shrink_failing_sql<F>(original_sql: &str, mut reproduces: F) -> String
where
    F: FnMut(&str) -> bool,
{
    let Ok(Statement::Query(mut current)) = parse_single_statement(original_sql) else {
        return original_sql.to_string();
    };
    let mut best_sql = original_sql.to_string();

    for _round in 0..16 {
        let mut improved = false;
        let candidates = generate_reduction_candidates(&current);
        for cand in candidates {
            let cand_sql = pretty_print_query(&cand);
            if cand_sql.len() < best_sql.len() && reproduces(&cand_sql) {
                *current = cand;
                best_sql = cand_sql;
                improved = true;
                break;
            }
        }
        if !improved {
            break;
        }
    }

    best_sql
}

fn generate_reduction_candidates(q: &Query) -> Vec<Query> {
    let mut out = Vec::new();

    // 1. Drop LIMIT / OFFSET / ORDER BY
    if q.offset.is_some() {
        let mut c = q.clone();
        c.offset = None;
        out.push(c);
    }
    if q.limit.is_some() {
        let mut c = q.clone();
        c.limit = None;
        out.push(c);
    }
    if !q.order_by.is_empty() {
        let mut c = q.clone();
        c.order_by.clear();
        out.push(c);
    }

    // 2. Simplify SetExpr body
    match q.body.as_ref() {
        SetExpr::SetOperation { left, right, .. } => {
            let mut cl = q.clone();
            cl.body = left.clone();
            out.push(cl);

            let mut cr = q.clone();
            cr.body = right.clone();
            out.push(cr);
        }
        SetExpr::Select(sel) => {
            for s_cand in reduce_select(sel) {
                let mut c = q.clone();
                c.body = Box::new(SetExpr::Select(Box::new(s_cand)));
                out.push(c);
            }
        }
        SetExpr::Query(inner) => {
            out.push((**inner).clone());
        }
        SetExpr::Values(_) => {}
    }

    out
}

fn reduce_select(sel: &Select) -> Vec<Select> {
    let mut out = Vec::new();

    if sel.distinct {
        let mut c = sel.clone();
        c.distinct = false;
        out.push(c);
    }

    if sel.having.is_some() {
        let mut c = sel.clone();
        c.having = None;
        out.push(c);
    }

    if !sel.group_by.is_empty() {
        let mut c = sel.clone();
        c.group_by = GroupByClause::Expressions(Vec::new());
        c.having = None;
        out.push(c);
    }

    // Reduce WHERE predicate: drop entirely OR split AND/OR conjuncts!
    if let Some(pred) = &sel.selection {
        let mut c_none = sel.clone();
        c_none.selection = None;
        out.push(c_none);

        for sub_pred in reduce_expr(pred) {
            let mut c = sel.clone();
            c.selection = Some(sub_pred);
            out.push(c);
        }
    }

    // Drop trailing joins one by one
    if let Some(first_from) = sel.from.first() {
        if !first_from.joins.is_empty() {
            for idx in 0..first_from.joins.len() {
                let mut c = sel.clone();
                c.from[0].joins.remove(idx);
                out.push(c);
            }
        }
    }

    // Drop SELECT projection items while keeping >= 1 column
    if sel.projection.len() > 1 {
        for idx in 0..sel.projection.len() {
            let mut c = sel.clone();
            c.projection.remove(idx);
            out.push(c);
        }
    }

    // Simplify projection expressions
    for (idx, item) in sel.projection.iter().enumerate() {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            for se in reduce_expr(e) {
                let mut c = sel.clone();
                c.projection[idx] = match item {
                    SelectItem::UnnamedExpr(_) => SelectItem::UnnamedExpr(se),
                    SelectItem::ExprWithAlias { alias, .. } => SelectItem::ExprWithAlias {
                        expr: se,
                        alias: alias.clone(),
                    },
                    other => other.clone(),
                };
                out.push(c);
            }
        }
    }

    out
}

fn reduce_expr(expr: &Expr) -> Vec<Expr> {
    let mut out = Vec::new();
    match expr {
        Expr::BinaryOp {
            left,
            op: AstBinaryOp::And | AstBinaryOp::Or,
            right,
        } => {
            out.push((**left).clone());
            out.push((**right).clone());
        }
        Expr::Case {
            branches,
            else_result,
            ..
        } => {
            for (_, then_e) in branches {
                out.push(then_e.clone());
            }
            if let Some(e) = else_result {
                out.push((**e).clone());
            }
        }
        Expr::Nested(inner) => {
            out.push((**inner).clone());
        }
        _ => {}
    }
    out
}

/// Writes a standalone `.slt` reproducer file for a failing SQLsmith seed into `artifacts_dir`.
pub fn write_failure_reproducer(
    artifacts_dir: &Path,
    seed: u64,
    setup_sql: &str,
    original_sql: &str,
    shrunk_sql: &str,
    diff_report: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(artifacts_dir)?;
    let path = artifacts_dir.join(format!("seed_{seed}.slt"));
    let mut content = String::new();
    content.push_str(&format!(
        "# Auto-generated SQLsmith failure reproducer for seed {seed}\n"
    ));
    for line in diff_report.lines() {
        content.push_str(&format!("# DIFF: {line}\n"));
    }
    content.push_str("# ORIGINAL SQL:\n");
    for line in original_sql.lines() {
        content.push_str(&format!("#   {line}\n"));
    }
    content.push('\n');
    for stmt in setup_sql.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            content.push_str("statement ok\n");
            content.push_str(s);
            content.push_str("\n\n");
        }
    }
    content.push_str("query T rowsort\n");
    content.push_str(shrunk_sql);
    content.push_str("\n----\n");
    std::fs::write(&path, content)?;
    Ok(path)
}
