//! Deterministic SQL AST Pretty-Printer guaranteeing round-trip idempotence:
//! `parse(pretty_print(parse(sql))) == parse(sql)`.

use crate::ast::*;
use vectra_expr::DatePart;

pub fn pretty_print_statement(stmt: &Statement) -> String {
    let mut out = String::new();
    fmt_statement(stmt, &mut out);
    out
}

pub fn pretty_print_query(q: &Query) -> String {
    let mut out = String::new();
    fmt_query(q, &mut out);
    out
}

pub fn pretty_print_expr(e: &Expr) -> String {
    let mut out = String::new();
    fmt_expr(e, &mut out);
    out
}

fn quote_ident_if_needed(name: &str) -> String {
    // Always safe to quote identifiers with special chars or reserved words; keep simple lowercase unquoted.
    if name.is_empty()
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || name.as_bytes()[0].is_ascii_digit()
        || crate::lexer::Keyword::lookup(name).is_some()
    {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_string()
    }
}

fn fmt_statement(stmt: &Statement, out: &mut String) {
    match stmt {
        Statement::Query(q) => fmt_query(q, out),
        Statement::CreateTable {
            name,
            if_not_exists,
            or_replace,
            columns,
            as_query,
        } => {
            out.push_str("CREATE ");
            if *or_replace {
                out.push_str("OR REPLACE ");
            }
            out.push_str("TABLE ");
            if *if_not_exists {
                out.push_str("IF NOT EXISTS ");
            }
            out.push_str(&quote_ident_if_needed(name));
            if let Some(q) = as_query {
                out.push_str(" AS ");
                fmt_query(q, out);
            } else {
                out.push_str(" (");
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&quote_ident_if_needed(&c.name));
                    out.push(' ');
                    out.push_str(&c.data_type.to_string());
                    if !c.nullable {
                        out.push_str(" NOT NULL");
                    }
                }
                out.push(')');
            }
        }
        Statement::CreateView {
            name,
            or_replace,
            column_aliases,
            query,
        } => {
            out.push_str("CREATE ");
            if *or_replace {
                out.push_str("OR REPLACE ");
            }
            out.push_str("VIEW ");
            out.push_str(&quote_ident_if_needed(name));
            if !column_aliases.is_empty() {
                out.push_str(" (");
                for (i, a) in column_aliases.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&quote_ident_if_needed(a));
                }
                out.push(')');
            }
            out.push_str(" AS ");
            fmt_query(query, out);
        }
        Statement::DropTable { name, if_exists } => {
            out.push_str("DROP TABLE ");
            if *if_exists {
                out.push_str("IF EXISTS ");
            }
            out.push_str(&quote_ident_if_needed(name));
        }
        Statement::DropView { name, if_exists } => {
            out.push_str("DROP VIEW ");
            if *if_exists {
                out.push_str("IF EXISTS ");
            }
            out.push_str(&quote_ident_if_needed(name));
        }
        Statement::Insert {
            table_name,
            columns,
            source,
        } => {
            out.push_str("INSERT INTO ");
            out.push_str(&quote_ident_if_needed(table_name));
            if !columns.is_empty() {
                out.push_str(" (");
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&quote_ident_if_needed(c));
                }
                out.push(')');
            }
            out.push(' ');
            match source {
                InsertSource::Values(rows) => {
                    out.push_str("VALUES ");
                    fmt_values_rows(rows, out);
                }
                InsertSource::Query(q) => fmt_query(q, out),
            }
        }
        Statement::Delete {
            table_name,
            selection,
        } => {
            out.push_str("DELETE FROM ");
            out.push_str(&quote_ident_if_needed(table_name));
            if let Some(sel) = selection {
                out.push_str(" WHERE ");
                fmt_expr(sel, out);
            }
        }
        Statement::Update {
            table_name,
            assignments,
            selection,
        } => {
            out.push_str("UPDATE ");
            out.push_str(&quote_ident_if_needed(table_name));
            out.push_str(" SET ");
            for (i, (c, e)) in assignments.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&quote_ident_if_needed(c));
                out.push_str(" = ");
                fmt_expr(e, out);
            }
            if let Some(sel) = selection {
                out.push_str(" WHERE ");
                fmt_expr(sel, out);
            }
        }
        Statement::Copy {
            target,
            path,
            to_file,
            format,
            delimiter,
            header,
        } => {
            out.push_str("COPY ");
            match target {
                CopyTarget::Table(t) => out.push_str(&quote_ident_if_needed(t)),
                CopyTarget::Query(q) => {
                    out.push('(');
                    fmt_query(q, out);
                    out.push(')');
                }
            }
            out.push_str(if *to_file { " TO '" } else { " FROM '" });
            out.push_str(&path.replace('\'', "''"));
            out.push('\'');
            let mut opts = Vec::new();
            opts.push(format!(
                "FORMAT {}",
                match format {
                    CopyFormat::Csv => "CSV",
                    CopyFormat::Parquet => "PARQUET",
                }
            ));
            if let Some(d) = delimiter {
                opts.push(format!("DELIMITER '{d}'"));
            }
            if let Some(h) = header {
                opts.push(format!("HEADER {}", if *h { "TRUE" } else { "FALSE" }));
            }
            out.push_str(" WITH (");
            out.push_str(&opts.join(", "));
            out.push(')');
        }
        Statement::Analyze { table_name } => {
            out.push_str("ANALYZE");
            if let Some(t) = table_name {
                out.push(' ');
                out.push_str(&quote_ident_if_needed(t));
            }
        }
        Statement::Begin => out.push_str("BEGIN"),
        Statement::Commit => out.push_str("COMMIT"),
        Statement::Rollback => out.push_str("ROLLBACK"),
        Statement::Explain { format, stmt } => {
            out.push_str("EXPLAIN ");
            out.push_str(match format {
                ExplainFormat::Physical => "PHYSICAL ",
                ExplainFormat::Logical => "LOGICAL ",
                ExplainFormat::Unnested => "UNNESTED ",
                ExplainFormat::Analyze => "ANALYZE ",
                ExplainFormat::Json => "JSON ",
            });
            fmt_statement(stmt, out);
        }
        Statement::Set { variable, value } => {
            out.push_str(&format!("SET {variable} = '{value}'"));
        }
        Statement::Show { variable } => {
            out.push_str(&format!("SHOW {variable}"));
        }
    }
}

fn fmt_query(q: &Query, out: &mut String) {
    if let Some(with) = &q.with {
        out.push_str("WITH ");
        if with.recursive {
            out.push_str("RECURSIVE ");
        }
        for (i, cte) in with.ctes.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&quote_ident_if_needed(&cte.name));
            if !cte.column_aliases.is_empty() {
                out.push_str(" (");
                for (j, a) in cte.column_aliases.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&quote_ident_if_needed(a));
                }
                out.push(')');
            }
            out.push_str(" AS (");
            fmt_query(&cte.query, out);
            out.push(')');
        }
        out.push(' ');
    }
    fmt_set_expr(&q.body, out);
    if !q.order_by.is_empty() {
        out.push_str(" ORDER BY ");
        fmt_order_by(&q.order_by, out);
    }
    if let Some(lim) = &q.limit {
        out.push_str(" LIMIT ");
        fmt_expr(lim, out);
    }
    if let Some(off) = &q.offset {
        out.push_str(" OFFSET ");
        fmt_expr(off, out);
    }
}

fn fmt_set_expr(se: &SetExpr, out: &mut String) {
    match se {
        SetExpr::Select(s) => fmt_select(s, out),
        SetExpr::Query(q) => {
            out.push('(');
            fmt_query(q, out);
            out.push(')');
        }
        SetExpr::SetOperation {
            op,
            all,
            left,
            right,
        } => {
            fmt_set_expr(left, out);
            out.push(' ');
            out.push_str(match op {
                SetOperator::Union => "UNION",
                SetOperator::Intersect => "INTERSECT",
                SetOperator::Except => "EXCEPT",
            });
            if *all {
                out.push_str(" ALL");
            }
            out.push(' ');
            fmt_set_expr(right, out);
        }
        SetExpr::Values(rows) => {
            out.push_str("VALUES ");
            fmt_values_rows(rows, out);
        }
    }
}

fn fmt_values_rows(rows: &[Vec<Expr>], out: &mut String) {
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('(');
        for (j, e) in row.iter().enumerate() {
            if j > 0 {
                out.push_str(", ");
            }
            fmt_expr(e, out);
        }
        out.push(')');
    }
}

fn fmt_select(s: &Select, out: &mut String) {
    out.push_str("SELECT ");
    if s.distinct {
        out.push_str("DISTINCT ");
    }
    for (i, item) in s.projection.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        match item {
            SelectItem::Wildcard => out.push('*'),
            SelectItem::QualifiedWildcard(q) => {
                out.push_str(&quote_ident_if_needed(q));
                out.push_str(".*");
            }
            SelectItem::UnnamedExpr(e) => fmt_expr(e, out),
            SelectItem::ExprWithAlias { expr, alias } => {
                fmt_expr(expr, out);
                out.push_str(" AS ");
                out.push_str(&quote_ident_if_needed(alias));
            }
        }
    }
    if !s.from.is_empty() {
        out.push_str(" FROM ");
        for (i, twj) in s.from.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            fmt_table_with_joins(twj, out);
        }
    }
    if let Some(w) = &s.selection {
        out.push_str(" WHERE ");
        fmt_expr(w, out);
    }
    match &s.group_by {
        GroupByClause::Expressions(v) if !v.is_empty() => {
            out.push_str(" GROUP BY ");
            fmt_expr_list(v, out);
        }
        GroupByClause::Rollup(v) => {
            out.push_str(" GROUP BY ROLLUP(");
            fmt_expr_list(v, out);
            out.push(')');
        }
        GroupByClause::Cube(v) => {
            out.push_str(" GROUP BY CUBE(");
            fmt_expr_list(v, out);
            out.push(')');
        }
        GroupByClause::GroupingSets(sets) => {
            out.push_str(" GROUP BY GROUPING SETS (");
            for (i, set) in sets.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('(');
                fmt_expr_list(set, out);
                out.push(')');
            }
            out.push(')');
        }
        _ => {}
    }
    if let Some(h) = &s.having {
        out.push_str(" HAVING ");
        fmt_expr(h, out);
    }
    if !s.named_windows.is_empty() {
        out.push_str(" WINDOW ");
        for (i, (wname, spec)) in s.named_windows.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&quote_ident_if_needed(wname));
            out.push_str(" AS (");
            fmt_window_spec_inner(spec, out);
            out.push(')');
        }
    }
    if let Some(q) = &s.qualify {
        out.push_str(" QUALIFY ");
        fmt_expr(q, out);
    }
}

fn fmt_table_with_joins(twj: &TableWithJoins, out: &mut String) {
    fmt_table_factor(&twj.relation, out);
    for j in &twj.joins {
        out.push(' ');
        if matches!(j.constraint, JoinConstraint::Natural) {
            out.push_str("NATURAL ");
        }
        out.push_str(match j.kind {
            AstJoinKind::Inner => "INNER JOIN ",
            AstJoinKind::LeftOuter => "LEFT JOIN ",
            AstJoinKind::RightOuter => "RIGHT JOIN ",
            AstJoinKind::FullOuter => "FULL JOIN ",
            AstJoinKind::Cross => "CROSS JOIN ",
            AstJoinKind::LeftSemi => "LEFT SEMI JOIN ",
            AstJoinKind::LeftAnti => "LEFT ANTI JOIN ",
        });
        fmt_table_factor(&j.relation, out);
        match &j.constraint {
            JoinConstraint::On(e) => {
                out.push_str(" ON ");
                fmt_expr(e, out);
            }
            JoinConstraint::Using(cols) => {
                out.push_str(" USING (");
                for (i, c) in cols.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&quote_ident_if_needed(c));
                }
                out.push(')');
            }
            JoinConstraint::Natural | JoinConstraint::None => {}
        }
    }
}

fn fmt_table_factor(tf: &TableFactor, out: &mut String) {
    match tf {
        TableFactor::Table { name, alias } => {
            if let Some((s, t)) = name.split_once('.') {
                out.push_str(&quote_ident_if_needed(s));
                out.push('.');
                out.push_str(&quote_ident_if_needed(t));
            } else {
                out.push_str(&quote_ident_if_needed(name));
            }
            fmt_table_alias(alias, out);
        }
        TableFactor::TableFunction { name, args, alias } => {
            out.push_str(name);
            out.push('(');
            fmt_expr_list(args, out);
            out.push(')');
            fmt_table_alias(alias, out);
        }
        TableFactor::Derived { subquery, alias } => {
            out.push('(');
            fmt_query(subquery, out);
            out.push(')');
            fmt_table_alias(alias, out);
        }
        TableFactor::NestedJoin(twj) => {
            out.push('(');
            fmt_table_with_joins(twj, out);
            out.push(')');
        }
    }
}

fn fmt_table_alias(alias: &Option<TableAlias>, out: &mut String) {
    if let Some(a) = alias {
        out.push_str(" AS ");
        out.push_str(&quote_ident_if_needed(&a.name));
        if !a.columns.is_empty() {
            out.push_str(" (");
            for (i, c) in a.columns.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&quote_ident_if_needed(c));
            }
            out.push(')');
        }
    }
}

fn fmt_order_by(items: &[OrderByExpr], out: &mut String) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        fmt_expr(&item.expr, out);
        if let Some(asc) = item.asc {
            out.push_str(if asc { " ASC" } else { " DESC" });
        }
        if let Some(nf) = item.nulls_first {
            out.push_str(if nf { " NULLS FIRST" } else { " NULLS LAST" });
        }
    }
}

fn fmt_expr_list(exprs: &[Expr], out: &mut String) {
    for (i, e) in exprs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        fmt_expr(e, out);
    }
}

fn fmt_expr(expr: &Expr, out: &mut String) {
    match expr {
        Expr::Identifier(s) => out.push_str(&quote_ident_if_needed(s)),
        Expr::CompoundIdentifier(parts) => {
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    out.push('.');
                }
                out.push_str(&quote_ident_if_needed(p));
            }
        }
        Expr::Literal(lit) => match lit {
            LiteralValue::Null => out.push_str("NULL"),
            LiteralValue::Boolean(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
            LiteralValue::Integer(i) => out.push_str(&i.to_string()),
            LiteralValue::Float(f) => out.push_str(&format!("{f:e}")),
            LiteralValue::Decimal(d) => out.push_str(d),
            LiteralValue::String(s) => {
                out.push('\'');
                out.push_str(&s.replace('\'', "''"));
                out.push('\'');
            }
            LiteralValue::Date(d) => out.push_str(&format!("DATE '{d}'")),
            LiteralValue::Timestamp(t) => out.push_str(&format!("TIMESTAMP '{t}'")),
            LiteralValue::Interval { value, unit } => {
                let u = match unit {
                    IntervalUnit::Year => "YEAR",
                    IntervalUnit::Month => "MONTH",
                    IntervalUnit::Day => "DAY",
                    IntervalUnit::Hour => "HOUR",
                    IntervalUnit::Minute => "MINUTE",
                    IntervalUnit::Second => "SECOND",
                };
                out.push_str(&format!("INTERVAL '{value}' {u}"));
            }
        },
        Expr::Param(idx) => out.push_str(&format!("${idx}")),
        Expr::BinaryOp { left, op, right } => {
            fmt_expr(left, out);
            out.push(' ');
            out.push_str(match op {
                AstBinaryOp::Add => "+",
                AstBinaryOp::Sub => "-",
                AstBinaryOp::Mul => "*",
                AstBinaryOp::Div => "/",
                AstBinaryOp::Mod => "%",
                AstBinaryOp::Eq => "=",
                AstBinaryOp::NotEq => "!=",
                AstBinaryOp::Lt => "<",
                AstBinaryOp::LtEq => "<=",
                AstBinaryOp::Gt => ">",
                AstBinaryOp::GtEq => ">=",
                AstBinaryOp::And => "AND",
                AstBinaryOp::Or => "OR",
                AstBinaryOp::Concat => "||",
                AstBinaryOp::IsDistinctFrom => "IS DISTINCT FROM",
                AstBinaryOp::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
            });
            out.push(' ');
            fmt_expr(right, out);
        }
        Expr::UnaryOp { op, expr } => {
            match op {
                AstUnaryOp::Neg => out.push('-'),
                AstUnaryOp::Pos => out.push('+'),
                AstUnaryOp::Not => out.push_str("NOT "),
            }
            fmt_expr(expr, out);
        }
        Expr::IsNull { expr, negated } => {
            fmt_expr(expr, out);
            out.push_str(if *negated { " IS NOT NULL" } else { " IS NULL" });
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            fmt_expr(expr, out);
            out.push_str(if *negated {
                " NOT BETWEEN "
            } else {
                " BETWEEN "
            });
            fmt_expr(low, out);
            out.push_str(" AND ");
            fmt_expr(high, out);
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            fmt_expr(expr, out);
            out.push_str(if *negated { " NOT IN (" } else { " IN (" });
            fmt_expr_list(list, out);
            out.push(')');
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            fmt_expr(expr, out);
            out.push_str(if *negated { " NOT IN (" } else { " IN (" });
            fmt_query(subquery, out);
            out.push(')');
        }
        Expr::QuantifiedSubquery {
            expr,
            op,
            quantifier,
            subquery,
        } => {
            fmt_expr(expr, out);
            out.push(' ');
            out.push_str(match op {
                AstBinaryOp::Eq => "=",
                AstBinaryOp::NotEq => "!=",
                AstBinaryOp::Lt => "<",
                AstBinaryOp::LtEq => "<=",
                AstBinaryOp::Gt => ">",
                AstBinaryOp::GtEq => ">=",
                _ => "=",
            });
            out.push_str(match quantifier {
                Quantifier::Any => " ANY (",
                Quantifier::All => " ALL (",
            });
            fmt_query(subquery, out);
            out.push(')');
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => {
            fmt_expr(expr, out);
            if *negated {
                out.push_str(" NOT");
            }
            out.push_str(if *case_insensitive {
                " ILIKE "
            } else {
                " LIKE "
            });
            fmt_expr(pattern, out);
            if let Some(esc) = escape {
                out.push_str(&format!(" ESCAPE '{esc}'"));
            }
        }
        Expr::Case {
            operand,
            branches,
            else_result,
        } => {
            out.push_str("CASE");
            if let Some(op) = operand {
                out.push(' ');
                fmt_expr(op, out);
            }
            for (w, t) in branches {
                out.push_str(" WHEN ");
                fmt_expr(w, out);
                out.push_str(" THEN ");
                fmt_expr(t, out);
            }
            if let Some(e) = else_result {
                out.push_str(" ELSE ");
                fmt_expr(e, out);
            }
            out.push_str(" END");
        }
        Expr::Cast {
            expr,
            data_type,
            try_cast,
        } => {
            out.push_str(if *try_cast { "TRY_CAST(" } else { "CAST(" });
            fmt_expr(expr, out);
            out.push_str(" AS ");
            out.push_str(&data_type.to_string());
            out.push(')');
        }
        Expr::Extract { part, expr } => {
            let p = match part {
                DatePart::Year => "YEAR",
                DatePart::Quarter => "QUARTER",
                DatePart::Month => "MONTH",
                DatePart::Day => "DAY",
                DatePart::Hour => "HOUR",
                DatePart::Minute => "MINUTE",
                DatePart::Second => "SECOND",
            };
            out.push_str(&format!("EXTRACT({p} FROM "));
            fmt_expr(expr, out);
            out.push(')');
        }
        Expr::Function {
            name,
            args,
            distinct,
            wildcard_arg,
            filter,
            over,
        } => {
            out.push_str(name);
            out.push('(');
            if *distinct {
                out.push_str("DISTINCT ");
            }
            if *wildcard_arg {
                out.push('*');
            } else {
                fmt_expr_list(args, out);
            }
            out.push(')');
            if let Some(f) = filter {
                out.push_str(" FILTER (WHERE ");
                fmt_expr(f, out);
                out.push(')');
            }
            if let Some(w) = over {
                out.push_str(" OVER (");
                fmt_window_spec_inner(w, out);
                out.push(')');
            }
        }
        Expr::Exists { subquery, negated } => {
            if *negated {
                out.push_str("NOT ");
            }
            out.push_str("EXISTS (");
            fmt_query(subquery, out);
            out.push(')');
        }
        Expr::Subquery(q) => {
            out.push('(');
            fmt_query(q, out);
            out.push(')');
        }
        Expr::Nested(e) => {
            out.push('(');
            fmt_expr(e, out);
            out.push(')');
        }
    }
}

fn fmt_window_spec_inner(spec: &WindowSpec, out: &mut String) {
    let mut need_space = false;
    if let Some(wn) = &spec.window_name {
        out.push_str(&quote_ident_if_needed(wn));
        need_space = true;
    }
    if !spec.partition_by.is_empty() {
        if need_space {
            out.push(' ');
        }
        out.push_str("PARTITION BY ");
        fmt_expr_list(&spec.partition_by, out);
        need_space = true;
    }
    if !spec.order_by.is_empty() {
        if need_space {
            out.push(' ');
        }
        out.push_str("ORDER BY ");
        fmt_order_by(&spec.order_by, out);
        need_space = true;
    }
    if let Some(f) = &spec.frame {
        if need_space {
            out.push(' ');
        }
        out.push_str(match f.units {
            WindowFrameUnits::Rows => "ROWS BETWEEN ",
            WindowFrameUnits::Range => "RANGE BETWEEN ",
        });
        fmt_frame_bound(&f.start, out);
        out.push_str(" AND ");
        fmt_frame_bound(&f.end, out);
    }
}

fn fmt_frame_bound(b: &WindowFrameBound, out: &mut String) {
    match b {
        WindowFrameBound::UnboundedPreceding => out.push_str("UNBOUNDED PRECEDING"),
        WindowFrameBound::Preceding(n) => out.push_str(&format!("{n} PRECEDING")),
        WindowFrameBound::CurrentRow => out.push_str("CURRENT ROW"),
        WindowFrameBound::Following(n) => out.push_str(&format!("{n} FOLLOWING")),
        WindowFrameBound::UnboundedFollowing => out.push_str("UNBOUNDED FOLLOWING"),
    }
}
