use crate::ast::*;
use crate::diagnostics::{parse_error, Span};
use crate::lexer::{tokenize, Keyword, Token, TokenKind};
use vectra_expr::DatePart;
use vectra_types::{LogicalType, Result};

/// Parses one or more semicolon-separated SQL statements.
pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let tokens = tokenize(sql)?;
    let mut parser = Parser::new(sql, tokens);
    let mut stmts = Vec::new();
    while !parser.at_eof() {
        while parser.consume_if(&TokenKind::Semicolon) {}
        if parser.at_eof() {
            break;
        }
        stmts.push(parser.parse_statement()?);
        while parser.consume_if(&TokenKind::Semicolon) {}
    }
    Ok(stmts)
}

/// Parses a single SQL statement (erroring if empty or trailing tokens remain).
pub fn parse_single_statement(sql: &str) -> Result<Statement> {
    let mut stmts = parse_sql(sql)?;
    if stmts.is_empty() {
        return Err(parse_error(sql, Span::new(0, 0), "empty SQL input", None));
    }
    if stmts.len() > 1 {
        return Err(parse_error(
            sql,
            Span::new(0, sql.len() as u32),
            "expected a single SQL statement",
            None,
        ));
    }
    Ok(stmts.pop().unwrap())
}

struct Parser<'a> {
    sql: &'a str,
    tokens: Vec<Token<'a>>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(sql: &'a str, tokens: Vec<Token<'a>>) -> Self {
        Self {
            sql,
            tokens,
            pos: 0,
        }
    }

    #[inline]
    fn peek(&self) -> &Token<'a> {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    #[inline]
    fn peek_n(&self, offset: usize) -> &Token<'a> {
        &self.tokens[(self.pos + offset).min(self.tokens.len() - 1)]
    }

    #[inline]
    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    #[inline]
    fn advance(&mut self) -> Token<'a> {
        let tok = self.peek().clone();
        if !self.at_eof() {
            self.pos += 1;
        }
        tok
    }

    #[inline]
    fn check_kw(&self, kw: Keyword) -> bool {
        matches!(self.peek().kind, TokenKind::Keyword(k, _) if k == kw)
    }

    #[inline]
    fn check_kw_at(&self, offset: usize, kw: Keyword) -> bool {
        matches!(self.peek_n(offset).kind, TokenKind::Keyword(k, _) if k == kw)
    }

    #[inline]
    fn consume_kw(&mut self, kw: Keyword) -> bool {
        if self.check_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    #[inline]
    fn consume_if(&mut self, kind: &TokenKind<'_>) -> bool {
        if &self.peek().kind == kind {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: Keyword) -> Result<Span> {
        if self.check_kw(kw) {
            Ok(self.advance().span)
        } else {
            Err(parse_error(
                self.sql,
                self.peek().span,
                format!("expected keyword `{kw:?}`, found `{:?}`", self.peek().kind),
                None,
            ))
        }
    }

    fn expect_token(&mut self, expected: &TokenKind<'_>, label: &str) -> Result<Span> {
        if &self.peek().kind == expected {
            Ok(self.advance().span)
        } else {
            Err(parse_error(
                self.sql,
                self.peek().span,
                format!("expected {label}, found `{:?}`", self.peek().kind),
                None,
            ))
        }
    }

    fn parse_ident(&mut self) -> Result<String> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Ident(s) => {
                self.advance();
                Ok(s.to_ascii_lowercase())
            }
            TokenKind::QuotedIdent(s) => {
                self.advance();
                Ok(s)
            }
            TokenKind::Keyword(kw, raw) if is_ident_allowed_keyword(kw) => {
                self.advance();
                Ok(raw.to_ascii_lowercase())
            }
            _ => Err(parse_error(
                self.sql,
                tok.span,
                format!("expected identifier, found `{:?}`", tok.kind),
                None,
            )),
        }
    }

    fn parse_alias_ident(&mut self) -> Result<String> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Ident(s) => {
                self.advance();
                Ok(s.to_ascii_lowercase())
            }
            TokenKind::QuotedIdent(s) => {
                self.advance();
                Ok(s)
            }
            TokenKind::Keyword(kw, raw) if is_alias_allowed_keyword(kw) => {
                self.advance();
                Ok(raw.to_ascii_lowercase())
            }
            _ => Err(parse_error(
                self.sql,
                tok.span,
                format!("expected alias identifier, found `{:?}`", tok.kind),
                None,
            )),
        }
    }

    fn parse_string_lit(&mut self) -> Result<String> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(s)
            }
            _ => Err(parse_error(
                self.sql,
                tok.span,
                format!("expected string literal, found `{:?}`", tok.kind),
                None,
            )),
        }
    }

    // ========================================================================
    // Statements
    // ========================================================================

    fn parse_statement(&mut self) -> Result<Statement> {
        if self.check_kw(Keyword::Explain) {
            return self.parse_explain();
        }
        if self.check_kw(Keyword::Create) {
            return self.parse_create();
        }
        if self.check_kw(Keyword::Drop) {
            return self.parse_drop();
        }
        if self.check_kw(Keyword::Insert) {
            return self.parse_insert();
        }
        if self.check_kw(Keyword::Delete) {
            return self.parse_delete();
        }
        if self.check_kw(Keyword::Update) {
            return self.parse_update();
        }
        if self.check_kw(Keyword::Copy) {
            return self.parse_copy();
        }
        if self.check_kw(Keyword::Analyze) {
            self.advance();
            let table_name = if !self.at_eof() && !matches!(self.peek().kind, TokenKind::Semicolon)
            {
                Some(self.parse_ident()?)
            } else {
                None
            };
            return Ok(Statement::Analyze { table_name });
        }
        if self.consume_kw(Keyword::Begin) {
            let _ = self.consume_kw(Keyword::Transaction);
            return Ok(Statement::Begin);
        }
        if self.consume_kw(Keyword::Commit) {
            let _ = self.consume_kw(Keyword::Transaction);
            return Ok(Statement::Commit);
        }
        if self.consume_kw(Keyword::Rollback) {
            let _ = self.consume_kw(Keyword::Transaction);
            return Ok(Statement::Rollback);
        }
        if self.consume_kw(Keyword::Set) {
            let variable = self.parse_ident()?;
            let _ = self.consume_kw(Keyword::To) || self.consume_if(&TokenKind::Eq);
            let mut val_parts = Vec::new();
            while !self.at_eof() && !matches!(self.peek().kind, TokenKind::Semicolon) {
                let tok = self.advance();
                match tok.kind {
                    TokenKind::StringLit(s) | TokenKind::QuotedIdent(s) => val_parts.push(s),
                    TokenKind::Ident(s) | TokenKind::Number(s) => val_parts.push(s.to_string()),
                    TokenKind::Keyword(_, s) => val_parts.push(s.to_string()),
                    _ => {}
                }
            }
            return Ok(Statement::Set {
                variable,
                value: val_parts.join(" "),
            });
        }
        if self.consume_kw(Keyword::Show) {
            let variable = self.parse_ident()?;
            return Ok(Statement::Show { variable });
        }
        let q = self.parse_query()?;
        Ok(Statement::Query(Box::new(q)))
    }

    fn parse_explain(&mut self) -> Result<Statement> {
        self.expect_kw(Keyword::Explain)?;
        let mut format = ExplainFormat::Physical;
        if self.consume_kw(Keyword::Analyze) {
            format = ExplainFormat::Analyze;
        } else if self.consume_kw(Keyword::Logical) {
            format = ExplainFormat::Logical;
        } else if self.consume_kw(Keyword::Unnested) {
            format = ExplainFormat::Unnested;
        } else if self.consume_kw(Keyword::Physical) {
            format = ExplainFormat::Physical;
        } else if self.consume_kw(Keyword::Json) {
            format = ExplainFormat::Json;
        }
        let stmt = self.parse_statement()?;
        Ok(Statement::Explain {
            format,
            stmt: Box::new(stmt),
        })
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_kw(Keyword::Create)?;
        let or_replace = if self.consume_kw(Keyword::Or) {
            self.expect_kw(Keyword::Replace)?;
            true
        } else {
            false
        };
        if self.consume_kw(Keyword::View) {
            let name = self.parse_ident()?;
            let mut column_aliases = Vec::new();
            if self.consume_if(&TokenKind::LParen) {
                loop {
                    column_aliases.push(self.parse_ident()?);
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect_token(&TokenKind::RParen, "`)`")?;
            }
            self.expect_kw(Keyword::As)?;
            let query = self.parse_query()?;
            return Ok(Statement::CreateView {
                name,
                or_replace,
                column_aliases,
                query: Box::new(query),
            });
        }

        self.expect_kw(Keyword::Table)?;
        let if_not_exists = if self.consume_kw(Keyword::If) {
            self.expect_kw(Keyword::Not)?;
            self.expect_kw(Keyword::Exists)?;
            true
        } else {
            false
        };
        let name = self.parse_ident()?;
        if self.consume_kw(Keyword::As) {
            let query = self.parse_query()?;
            return Ok(Statement::CreateTable {
                name,
                if_not_exists,
                or_replace,
                columns: Vec::new(),
                as_query: Some(Box::new(query)),
            });
        }

        self.expect_token(&TokenKind::LParen, "`(`")?;
        let mut columns = Vec::new();
        loop {
            if self.consume_kw(Keyword::Primary) {
                self.expect_kw(Keyword::Key)?;
                self.expect_token(&TokenKind::LParen, "`(`")?;
                while !self.consume_if(&TokenKind::RParen) && !self.at_eof() {
                    self.advance();
                }
            } else {
                let col_name = self.parse_ident()?;
                let data_type = self.parse_data_type()?;
                let mut nullable = true;
                loop {
                    if self.consume_kw(Keyword::Not) {
                        self.expect_kw(Keyword::Null)?;
                        nullable = false;
                    } else if self.consume_kw(Keyword::Null) {
                        nullable = true;
                    } else if self.consume_kw(Keyword::Primary) {
                        self.expect_kw(Keyword::Key)?;
                        nullable = false;
                    } else if self.consume_kw(Keyword::Default) {
                        let _ = self.parse_expr(0)?;
                    } else {
                        break;
                    }
                }
                columns.push(ColumnDef {
                    name: col_name,
                    data_type,
                    nullable,
                });
            }
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_token(&TokenKind::RParen, "`)`")?;
        Ok(Statement::CreateTable {
            name,
            if_not_exists,
            or_replace,
            columns,
            as_query: None,
        })
    }

    fn parse_drop(&mut self) -> Result<Statement> {
        self.expect_kw(Keyword::Drop)?;
        let is_view = if self.consume_kw(Keyword::View) {
            true
        } else {
            self.expect_kw(Keyword::Table)?;
            false
        };
        let if_exists = if self.consume_kw(Keyword::If) {
            self.expect_kw(Keyword::Exists)?;
            true
        } else {
            false
        };
        let name = self.parse_ident()?;
        if is_view {
            Ok(Statement::DropView { name, if_exists })
        } else {
            Ok(Statement::DropTable { name, if_exists })
        }
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_kw(Keyword::Insert)?;
        self.expect_kw(Keyword::Into)?;
        let table_name = self.parse_ident()?;
        let mut columns = Vec::new();
        if self.consume_if(&TokenKind::LParen) {
            loop {
                columns.push(self.parse_ident()?);
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect_token(&TokenKind::RParen, "`)`")?;
        }
        let source = if self.consume_kw(Keyword::Values) {
            InsertSource::Values(self.parse_values_rows()?)
        } else {
            InsertSource::Query(Box::new(self.parse_query()?))
        };
        Ok(Statement::Insert {
            table_name,
            columns,
            source,
        })
    }

    fn parse_values_rows(&mut self) -> Result<Vec<Vec<Expr>>> {
        let mut rows = Vec::new();
        loop {
            self.expect_token(&TokenKind::LParen, "`(`")?;
            let mut row = Vec::new();
            if !matches!(self.peek().kind, TokenKind::RParen) {
                loop {
                    row.push(self.parse_expr(0)?);
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect_token(&TokenKind::RParen, "`)`")?;
            rows.push(row);
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        Ok(rows)
    }

    fn parse_delete(&mut self) -> Result<Statement> {
        self.expect_kw(Keyword::Delete)?;
        self.expect_kw(Keyword::From)?;
        let table_name = self.parse_ident()?;
        let selection = if self.consume_kw(Keyword::Where) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Delete {
            table_name,
            selection,
        })
    }

    fn parse_update(&mut self) -> Result<Statement> {
        self.expect_kw(Keyword::Update)?;
        let table_name = self.parse_ident()?;
        self.expect_kw(Keyword::Set)?;
        let mut assignments = Vec::new();
        loop {
            let col = self.parse_ident()?;
            self.expect_token(&TokenKind::Eq, "`=`")?;
            let val = self.parse_expr(0)?;
            assignments.push((col, val));
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        let selection = if self.consume_kw(Keyword::Where) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Update {
            table_name,
            assignments,
            selection,
        })
    }

    fn parse_copy(&mut self) -> Result<Statement> {
        self.expect_kw(Keyword::Copy)?;
        let target = if self.consume_if(&TokenKind::LParen) {
            let q = self.parse_query()?;
            self.expect_token(&TokenKind::RParen, "`)`")?;
            CopyTarget::Query(Box::new(q))
        } else {
            CopyTarget::Table(self.parse_ident()?)
        };
        let to_file = if self.consume_kw(Keyword::To) {
            true
        } else {
            self.expect_kw(Keyword::From)?;
            false
        };
        let path = self.parse_string_lit()?;
        let mut format = if path.to_ascii_lowercase().ends_with(".parquet") {
            CopyFormat::Parquet
        } else {
            CopyFormat::Csv
        };
        let mut delimiter = None;
        let mut header = None;

        let _ = self.consume_kw(Keyword::With);
        if self.consume_if(&TokenKind::LParen) {
            loop {
                let opt = self.parse_ident()?;
                match opt.as_str() {
                    "format" => {
                        let f = self.parse_ident()?;
                        format = match f.as_str() {
                            "parquet" => CopyFormat::Parquet,
                            _ => CopyFormat::Csv,
                        };
                    }
                    "delimiter" => {
                        let d = self.parse_string_lit()?;
                        delimiter = d.chars().next();
                    }
                    "header" => {
                        if self.consume_kw(Keyword::True) {
                            header = Some(true);
                        } else if self.consume_kw(Keyword::False) {
                            header = Some(false);
                        } else {
                            header = Some(true);
                        }
                    }
                    _ => {
                        if !matches!(self.peek().kind, TokenKind::Comma | TokenKind::RParen) {
                            self.advance();
                        }
                    }
                }
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect_token(&TokenKind::RParen, "`)`")?;
        }
        Ok(Statement::Copy {
            target,
            path,
            to_file,
            format,
            delimiter,
            header,
        })
    }

    fn parse_data_type(&mut self) -> Result<LogicalType> {
        let tok = self.advance();
        match tok.kind {
            TokenKind::Keyword(Keyword::Boolean | Keyword::Bool, _) => Ok(LogicalType::Boolean),
            TokenKind::Keyword(Keyword::Tinyint, _) => Ok(LogicalType::Int8),
            TokenKind::Keyword(Keyword::Smallint, _) => Ok(LogicalType::Int16),
            TokenKind::Keyword(Keyword::Int | Keyword::Integer, _) => Ok(LogicalType::Int32),
            TokenKind::Keyword(Keyword::Bigint, _) => Ok(LogicalType::Int64),
            TokenKind::Keyword(Keyword::Float | Keyword::Real, _) => Ok(LogicalType::Float64),
            TokenKind::Keyword(Keyword::Double, _) => {
                let _ = self.consume_kw(Keyword::Precision);
                Ok(LogicalType::Float64)
            }
            TokenKind::Keyword(Keyword::Decimal | Keyword::Numeric, _) => {
                let mut precision = 18u8;
                let mut scale = 2i8;
                if self.consume_if(&TokenKind::LParen) {
                    precision = self.parse_u64_lit()? as u8;
                    if self.consume_if(&TokenKind::Comma) {
                        let neg = self.consume_if(&TokenKind::Minus);
                        let s = self.parse_u64_lit()? as i8;
                        scale = if neg { -s } else { s };
                    } else {
                        scale = 0;
                    }
                    self.expect_token(&TokenKind::RParen, "`)`")?;
                }
                Ok(LogicalType::Decimal128 { precision, scale })
            }
            TokenKind::Keyword(Keyword::Date, _) => Ok(LogicalType::Date32),
            TokenKind::Keyword(Keyword::Timestamp, _) => Ok(LogicalType::TimestampMicros),
            TokenKind::Keyword(
                Keyword::Varchar | Keyword::Char | Keyword::Character | Keyword::Text,
                _,
            ) => {
                let _ = self.consume_kw(Keyword::Varying);
                if self.consume_if(&TokenKind::LParen) {
                    let _ = self.parse_u64_lit()?;
                    self.expect_token(&TokenKind::RParen, "`)`")?;
                }
                Ok(LogicalType::Utf8)
            }
            TokenKind::Keyword(Keyword::Varbinary | Keyword::Blob | Keyword::Bytea, _) => {
                if self.consume_if(&TokenKind::LParen) {
                    let _ = self.parse_u64_lit()?;
                    self.expect_token(&TokenKind::RParen, "`)`")?;
                }
                Ok(LogicalType::Binary)
            }
            _ => Err(parse_error(
                self.sql,
                tok.span,
                format!("expected SQL data type, found `{:?}`", tok.kind),
                None,
            )),
        }
    }

    fn parse_u64_lit(&mut self) -> Result<u64> {
        let tok = self.advance();
        match tok.kind {
            TokenKind::Number(n) => n.parse::<u64>().map_err(|_| {
                parse_error(
                    self.sql,
                    tok.span,
                    format!("expected unsigned int, got `{n}`"),
                    None,
                )
            }),
            _ => Err(parse_error(
                self.sql,
                tok.span,
                format!("expected integer literal, found `{:?}`", tok.kind),
                None,
            )),
        }
    }

    // ========================================================================
    // Queries & Set Operations (INTERSECT binds tighter than UNION / EXCEPT)
    // ========================================================================

    fn parse_query(&mut self) -> Result<Query> {
        let with = if self.consume_kw(Keyword::With) {
            let recursive = self.consume_kw(Keyword::Recursive);
            let mut ctes = Vec::new();
            loop {
                let name = self.parse_ident()?;
                let mut column_aliases = Vec::new();
                if self.consume_if(&TokenKind::LParen) {
                    loop {
                        column_aliases.push(self.parse_ident()?);
                        if !self.consume_if(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect_token(&TokenKind::RParen, "`)`")?;
                }
                self.expect_kw(Keyword::As)?;
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let q = self.parse_query()?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                ctes.push(Cte {
                    name,
                    column_aliases,
                    query: Box::new(q),
                });
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
            }
            Some(With { recursive, ctes })
        } else {
            None
        };

        let body = self.parse_set_expr(0)?;
        let order_by = self.parse_optional_order_by()?;

        let mut limit = None;
        let mut offset = None;
        loop {
            if self.consume_kw(Keyword::Limit) {
                limit = Some(self.parse_expr(0)?);
            } else if self.consume_kw(Keyword::Offset) {
                offset = Some(self.parse_expr(0)?);
                let _ = self.consume_kw(Keyword::Row) || self.consume_kw(Keyword::Rows);
            } else if self.consume_kw(Keyword::Fetch) {
                let _ = self.consume_kw(Keyword::First) || self.consume_kw(Keyword::Next);
                limit = Some(self.parse_expr(0)?);
                let _ = self.consume_kw(Keyword::Row) || self.consume_kw(Keyword::Rows);
                let _ = self.consume_kw(Keyword::Only);
            } else {
                break;
            }
        }

        Ok(Query {
            with,
            body: Box::new(body),
            order_by,
            limit,
            offset,
        })
    }

    fn parse_set_expr(&mut self, min_prec: u8) -> Result<SetExpr> {
        let mut left = self.parse_set_atom()?;
        loop {
            let (op, prec) = if self.check_kw(Keyword::Intersect) {
                (SetOperator::Intersect, 20u8)
            } else if self.check_kw(Keyword::Union) {
                (SetOperator::Union, 10u8)
            } else if self.check_kw(Keyword::Except) {
                (SetOperator::Except, 10u8)
            } else {
                break;
            };
            if prec < min_prec {
                break;
            }
            self.advance();
            let all = if self.consume_kw(Keyword::All) {
                true
            } else {
                let _ = self.consume_kw(Keyword::Distinct);
                false
            };
            let right = self.parse_set_expr(prec + 1)?;
            left = SetExpr::SetOperation {
                op,
                all,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_set_atom(&mut self) -> Result<SetExpr> {
        if self.consume_if(&TokenKind::LParen) {
            let q = self.parse_query()?;
            self.expect_token(&TokenKind::RParen, "`)`")?;
            return Ok(SetExpr::Query(Box::new(q)));
        }
        if self.consume_kw(Keyword::Values) {
            return Ok(SetExpr::Values(self.parse_values_rows()?));
        }
        Ok(SetExpr::Select(Box::new(self.parse_select()?)))
    }

    fn parse_select(&mut self) -> Result<Select> {
        self.expect_kw(Keyword::Select)?;
        let distinct = if self.consume_kw(Keyword::Distinct) {
            true
        } else {
            let _ = self.consume_kw(Keyword::All);
            false
        };

        let mut projection = Vec::new();
        loop {
            projection.push(self.parse_select_item()?);
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }

        let from = if self.consume_kw(Keyword::From) {
            self.parse_from_list()?
        } else {
            Vec::new()
        };

        let selection = if self.consume_kw(Keyword::Where) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let group_by = if self.consume_kw(Keyword::Group) {
            self.expect_kw(Keyword::By)?;
            self.parse_group_by()?
        } else {
            GroupByClause::Expressions(Vec::new())
        };

        let having = if self.consume_kw(Keyword::Having) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let mut named_windows = Vec::new();
        if self.consume_kw(Keyword::Window) {
            loop {
                let wname = self.parse_ident()?;
                self.expect_kw(Keyword::As)?;
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let spec = self.parse_window_spec_inner()?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                named_windows.push((wname, spec));
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
            }
        }

        let qualify = if self.consume_kw(Keyword::Qualify) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        Ok(Select {
            distinct,
            projection,
            from,
            selection,
            group_by,
            having,
            qualify,
            named_windows,
        })
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        if self.consume_if(&TokenKind::Star) {
            return Ok(SelectItem::Wildcard);
        }
        // Check `table.*`
        if matches!(self.peek_n(1).kind, TokenKind::Dot)
            && matches!(self.peek_n(2).kind, TokenKind::Star)
        {
            let qualifier = self.parse_ident()?;
            self.advance(); // .
            self.advance(); // *
            return Ok(SelectItem::QualifiedWildcard(qualifier));
        }

        let expr = self.parse_expr(0)?;
        if self.consume_kw(Keyword::As) || self.starts_implicit_alias() {
            let alias = self.parse_alias_ident()?;
            Ok(SelectItem::ExprWithAlias { expr, alias })
        } else {
            Ok(SelectItem::UnnamedExpr(expr))
        }
    }

    fn starts_implicit_alias(&self) -> bool {
        match self.peek().kind {
            TokenKind::Ident(_) | TokenKind::QuotedIdent(_) => true,
            TokenKind::Keyword(kw, _) => is_implicit_alias_keyword(kw),
            _ => false,
        }
    }

    fn parse_from_list(&mut self) -> Result<Vec<TableWithJoins>> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_table_with_joins()?);
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_table_with_joins(&mut self) -> Result<TableWithJoins> {
        let relation = self.parse_table_factor()?;
        let mut joins = Vec::new();
        loop {
            let natural = self.consume_kw(Keyword::Natural);
            let kind = if self.consume_kw(Keyword::Cross) {
                self.expect_kw(Keyword::Join)?;
                AstJoinKind::Cross
            } else if self.consume_kw(Keyword::Inner) {
                self.expect_kw(Keyword::Join)?;
                AstJoinKind::Inner
            } else if self.consume_kw(Keyword::Left) {
                if self.consume_kw(Keyword::Semi) {
                    self.expect_kw(Keyword::Join)?;
                    AstJoinKind::LeftSemi
                } else if self.consume_kw(Keyword::Anti) {
                    self.expect_kw(Keyword::Join)?;
                    AstJoinKind::LeftAnti
                } else {
                    let _ = self.consume_kw(Keyword::Outer);
                    self.expect_kw(Keyword::Join)?;
                    AstJoinKind::LeftOuter
                }
            } else if self.consume_kw(Keyword::Right) {
                let _ = self.consume_kw(Keyword::Outer);
                self.expect_kw(Keyword::Join)?;
                AstJoinKind::RightOuter
            } else if self.consume_kw(Keyword::Full) {
                let _ = self.consume_kw(Keyword::Outer);
                self.expect_kw(Keyword::Join)?;
                AstJoinKind::FullOuter
            } else if self.consume_kw(Keyword::Join) {
                AstJoinKind::Inner
            } else if natural {
                return Err(parse_error(
                    self.sql,
                    self.peek().span,
                    "expected JOIN after NATURAL",
                    None,
                ));
            } else {
                break;
            };

            let right = self.parse_table_factor()?;
            let constraint = if natural {
                JoinConstraint::Natural
            } else if self.consume_kw(Keyword::On) {
                JoinConstraint::On(self.parse_expr(0)?)
            } else if self.consume_kw(Keyword::Using) {
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let mut cols = Vec::new();
                loop {
                    cols.push(self.parse_ident()?);
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect_token(&TokenKind::RParen, "`)`")?;
                JoinConstraint::Using(cols)
            } else {
                JoinConstraint::None
            };

            joins.push(Join {
                relation: right,
                kind,
                constraint,
            });
        }
        Ok(TableWithJoins { relation, joins })
    }

    fn parse_table_factor(&mut self) -> Result<TableFactor> {
        if self.consume_if(&TokenKind::LParen) {
            if self.check_kw(Keyword::Select)
                || self.check_kw(Keyword::With)
                || self.check_kw(Keyword::Values)
                || matches!(self.peek().kind, TokenKind::LParen) && self.lookahead_is_subquery()
            {
                let sub = self.parse_query()?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                let alias = self.parse_optional_table_alias()?;
                return Ok(TableFactor::Derived {
                    subquery: Box::new(sub),
                    alias,
                });
            }
            let nested = self.parse_table_with_joins()?;
            self.expect_token(&TokenKind::RParen, "`)`")?;
            return Ok(TableFactor::NestedJoin(Box::new(nested)));
        }

        let name = self.parse_ident()?;
        if self.consume_if(&TokenKind::LParen) {
            let mut args = Vec::new();
            if !matches!(self.peek().kind, TokenKind::RParen) {
                loop {
                    args.push(self.parse_expr(0)?);
                    if !self.consume_if(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect_token(&TokenKind::RParen, "`)`")?;
            let alias = self.parse_optional_table_alias()?;
            return Ok(TableFactor::TableFunction { name, args, alias });
        }
        // Handle optional schema prefix `schema.table` (e.g., `information_schema.tables`)
        let full_name = if self.consume_if(&TokenKind::Dot) {
            let tbl = self.parse_ident()?;
            format!("{name}.{tbl}")
        } else {
            name
        };
        let alias = self.parse_optional_table_alias()?;
        Ok(TableFactor::Table {
            name: full_name,
            alias,
        })
    }

    fn lookahead_is_subquery(&self) -> bool {
        let mut off = 0usize;
        while matches!(self.peek_n(off).kind, TokenKind::LParen) {
            off += 1;
        }
        self.check_kw_at(off, Keyword::Select)
            || self.check_kw_at(off, Keyword::With)
            || self.check_kw_at(off, Keyword::Values)
    }

    fn parse_optional_table_alias(&mut self) -> Result<Option<TableAlias>> {
        let has_as = self.consume_kw(Keyword::As);
        if !has_as && !self.starts_implicit_alias() {
            return Ok(None);
        }
        let name = self.parse_alias_ident()?;
        let mut columns = Vec::new();
        if self.consume_if(&TokenKind::LParen) {
            loop {
                columns.push(self.parse_ident()?);
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect_token(&TokenKind::RParen, "`)`")?;
        }
        Ok(Some(TableAlias { name, columns }))
    }

    fn parse_group_by(&mut self) -> Result<GroupByClause> {
        if self.consume_kw(Keyword::Rollup) {
            self.expect_token(&TokenKind::LParen, "`(`")?;
            let exprs = self.parse_expr_list()?;
            self.expect_token(&TokenKind::RParen, "`)`")?;
            return Ok(GroupByClause::Rollup(exprs));
        }
        if self.consume_kw(Keyword::Cube) {
            self.expect_token(&TokenKind::LParen, "`(`")?;
            let exprs = self.parse_expr_list()?;
            self.expect_token(&TokenKind::RParen, "`)`")?;
            return Ok(GroupByClause::Cube(exprs));
        }
        if self.check_kw(Keyword::Grouping) && self.check_kw_at(1, Keyword::Sets) {
            self.advance();
            self.advance();
            self.expect_token(&TokenKind::LParen, "`(`")?;
            let mut sets = Vec::new();
            loop {
                if self.consume_if(&TokenKind::LParen) {
                    let s = if matches!(self.peek().kind, TokenKind::RParen) {
                        Vec::new()
                    } else {
                        self.parse_expr_list()?
                    };
                    self.expect_token(&TokenKind::RParen, "`)`")?;
                    sets.push(s);
                } else {
                    sets.push(vec![self.parse_expr(0)?]);
                }
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect_token(&TokenKind::RParen, "`)`")?;
            return Ok(GroupByClause::GroupingSets(sets));
        }
        Ok(GroupByClause::Expressions(self.parse_expr_list()?))
    }

    fn parse_optional_order_by(&mut self) -> Result<Vec<OrderByExpr>> {
        if !self.consume_kw(Keyword::Order) {
            return Ok(Vec::new());
        }
        self.expect_kw(Keyword::By)?;
        self.parse_order_by_list()
    }

    fn parse_order_by_list(&mut self) -> Result<Vec<OrderByExpr>> {
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expr(0)?;
            let asc = if self.consume_kw(Keyword::Asc) {
                Some(true)
            } else if self.consume_kw(Keyword::Desc) {
                Some(false)
            } else {
                None
            };
            let nulls_first = if self.consume_kw(Keyword::Nulls) {
                if self.consume_kw(Keyword::First) {
                    Some(true)
                } else {
                    self.expect_kw(Keyword::Last)?;
                    Some(false)
                }
            } else {
                None
            };
            items.push(OrderByExpr {
                expr,
                asc,
                nulls_first,
            });
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut exprs = Vec::new();
        loop {
            exprs.push(self.parse_expr(0)?);
            if !self.consume_if(&TokenKind::Comma) {
                break;
            }
        }
        Ok(exprs)
    }

    // ========================================================================
    // Pratt Expression Parser
    // ========================================================================

    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.parse_prefix_expr()?;

        loop {
            // Postfix `::TYPE` cast (bp 90)
            if matches!(self.peek().kind, TokenKind::DoubleColon) {
                if 90 < min_bp {
                    break;
                }
                self.advance();
                let data_type = self.parse_data_type()?;
                lhs = Expr::Cast {
                    expr: Box::new(lhs),
                    data_type,
                    try_cast: false,
                };
                continue;
            }

            // `IS [NOT] NULL / TRUE / FALSE / DISTINCT FROM` (bp 40)
            if self.check_kw(Keyword::Is) {
                if 40 < min_bp {
                    break;
                }
                self.advance();
                let negated = self.consume_kw(Keyword::Not);
                if self.consume_kw(Keyword::Null) {
                    lhs = Expr::IsNull {
                        expr: Box::new(lhs),
                        negated,
                    };
                } else if self.consume_kw(Keyword::Distinct) {
                    self.expect_kw(Keyword::From)?;
                    let rhs = self.parse_expr(41)?;
                    lhs = Expr::BinaryOp {
                        left: Box::new(lhs),
                        op: if negated {
                            AstBinaryOp::IsNotDistinctFrom
                        } else {
                            AstBinaryOp::IsDistinctFrom
                        },
                        right: Box::new(rhs),
                    };
                } else if self.consume_kw(Keyword::True) {
                    lhs = Expr::BinaryOp {
                        left: Box::new(lhs),
                        op: if negated {
                            AstBinaryOp::IsDistinctFrom
                        } else {
                            AstBinaryOp::IsNotDistinctFrom
                        },
                        right: Box::new(Expr::Literal(LiteralValue::Boolean(true))),
                    };
                } else if self.consume_kw(Keyword::False) {
                    lhs = Expr::BinaryOp {
                        left: Box::new(lhs),
                        op: if negated {
                            AstBinaryOp::IsDistinctFrom
                        } else {
                            AstBinaryOp::IsNotDistinctFrom
                        },
                        right: Box::new(Expr::Literal(LiteralValue::Boolean(false))),
                    };
                } else {
                    return Err(parse_error(
                        self.sql,
                        self.peek().span,
                        "expected NULL, TRUE, FALSE, or DISTINCT FROM after IS",
                        None,
                    ));
                }
                continue;
            }

            // `[NOT] BETWEEN / IN / LIKE / ILIKE` (bp 40)
            let is_negated_pred = self.check_kw(Keyword::Not)
                && (self.check_kw_at(1, Keyword::Between)
                    || self.check_kw_at(1, Keyword::In)
                    || self.check_kw_at(1, Keyword::Like)
                    || self.check_kw_at(1, Keyword::Ilike));

            if is_negated_pred
                || self.check_kw(Keyword::Between)
                || self.check_kw(Keyword::In)
                || self.check_kw(Keyword::Like)
                || self.check_kw(Keyword::Ilike)
            {
                if 40 < min_bp {
                    break;
                }
                let negated = self.consume_kw(Keyword::Not);
                if self.consume_kw(Keyword::Between) {
                    // Parse `low` with bp 41 so `AND` isn't consumed as boolean AND!
                    let low = self.parse_expr(41)?;
                    self.expect_kw(Keyword::And)?;
                    let high = self.parse_expr(41)?;
                    lhs = Expr::Between {
                        expr: Box::new(lhs),
                        low: Box::new(low),
                        high: Box::new(high),
                        negated,
                    };
                } else if self.consume_kw(Keyword::In) {
                    self.expect_token(&TokenKind::LParen, "`(`")?;
                    if self.check_kw(Keyword::Select) || self.check_kw(Keyword::With) {
                        let sub = self.parse_query()?;
                        self.expect_token(&TokenKind::RParen, "`)`")?;
                        lhs = Expr::InSubquery {
                            expr: Box::new(lhs),
                            subquery: Box::new(sub),
                            negated,
                        };
                    } else {
                        let list = if matches!(self.peek().kind, TokenKind::RParen) {
                            Vec::new()
                        } else {
                            self.parse_expr_list()?
                        };
                        self.expect_token(&TokenKind::RParen, "`)`")?;
                        lhs = Expr::InList {
                            expr: Box::new(lhs),
                            list,
                            negated,
                        };
                    }
                } else {
                    let case_insensitive = self.consume_kw(Keyword::Ilike);
                    if !case_insensitive {
                        self.expect_kw(Keyword::Like)?;
                    }
                    let pattern = self.parse_expr(41)?;
                    let escape = if self.consume_kw(Keyword::Escape) {
                        let s = self.parse_string_lit()?;
                        s.chars().next()
                    } else {
                        None
                    };
                    lhs = Expr::Like {
                        expr: Box::new(lhs),
                        pattern: Box::new(pattern),
                        escape,
                        case_insensitive,
                        negated,
                    };
                }
                continue;
            }

            // Standard binary infix operators
            let Some((op, lbp, rbp)) = self.infix_op() else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            self.advance();

            // Check quantified subquery: `expr op ANY|SOME|ALL (SELECT ...)`
            if matches!(
                op,
                AstBinaryOp::Eq
                    | AstBinaryOp::NotEq
                    | AstBinaryOp::Lt
                    | AstBinaryOp::LtEq
                    | AstBinaryOp::Gt
                    | AstBinaryOp::GtEq
            ) && (self.check_kw(Keyword::Any)
                || self.check_kw(Keyword::Some)
                || self.check_kw(Keyword::All))
            {
                let quant = if self.consume_kw(Keyword::All) {
                    Quantifier::All
                } else {
                    self.advance();
                    Quantifier::Any
                };
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let sub = self.parse_query()?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                lhs = Expr::QuantifiedSubquery {
                    expr: Box::new(lhs),
                    op,
                    quantifier: quant,
                    subquery: Box::new(sub),
                };
                continue;
            }

            let rhs = self.parse_expr(rbp)?;
            lhs = Expr::BinaryOp {
                left: Box::new(lhs),
                op,
                right: Box::new(rhs),
            };
        }

        Ok(lhs)
    }

    fn infix_op(&self) -> Option<(AstBinaryOp, u8, u8)> {
        match self.peek().kind {
            TokenKind::Keyword(Keyword::Or, _) => Some((AstBinaryOp::Or, 10, 11)),
            TokenKind::Keyword(Keyword::And, _) => Some((AstBinaryOp::And, 20, 21)),
            TokenKind::Eq => Some((AstBinaryOp::Eq, 40, 41)),
            TokenKind::NotEq => Some((AstBinaryOp::NotEq, 40, 41)),
            TokenKind::Lt => Some((AstBinaryOp::Lt, 40, 41)),
            TokenKind::LtEq => Some((AstBinaryOp::LtEq, 40, 41)),
            TokenKind::Gt => Some((AstBinaryOp::Gt, 40, 41)),
            TokenKind::GtEq => Some((AstBinaryOp::GtEq, 40, 41)),
            TokenKind::Concat => Some((AstBinaryOp::Concat, 50, 51)),
            TokenKind::Plus => Some((AstBinaryOp::Add, 60, 61)),
            TokenKind::Minus => Some((AstBinaryOp::Sub, 60, 61)),
            TokenKind::Star => Some((AstBinaryOp::Mul, 70, 71)),
            TokenKind::Slash => Some((AstBinaryOp::Div, 70, 71)),
            TokenKind::Percent => Some((AstBinaryOp::Mod, 70, 71)),
            _ => None,
        }
    }

    fn parse_prefix_expr(&mut self) -> Result<Expr> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Keyword(Keyword::Not, _) => {
                self.advance();
                if self.consume_kw(Keyword::Exists) {
                    self.expect_token(&TokenKind::LParen, "`(`")?;
                    let sub = self.parse_query()?;
                    self.expect_token(&TokenKind::RParen, "`)`")?;
                    return Ok(Expr::Exists {
                        subquery: Box::new(sub),
                        negated: true,
                    });
                }
                let inner = self.parse_expr(30)?;
                Ok(Expr::UnaryOp {
                    op: AstUnaryOp::Not,
                    expr: Box::new(inner),
                })
            }
            TokenKind::Minus => {
                self.advance();
                let inner = self.parse_expr(80)?;
                Ok(Expr::UnaryOp {
                    op: AstUnaryOp::Neg,
                    expr: Box::new(inner),
                })
            }
            TokenKind::Plus => {
                self.advance();
                let inner = self.parse_expr(80)?;
                Ok(Expr::UnaryOp {
                    op: AstUnaryOp::Pos,
                    expr: Box::new(inner),
                })
            }
            TokenKind::Keyword(Keyword::Null, _) => {
                self.advance();
                Ok(Expr::Literal(LiteralValue::Null))
            }
            TokenKind::Keyword(Keyword::True, _) => {
                self.advance();
                Ok(Expr::Literal(LiteralValue::Boolean(true)))
            }
            TokenKind::Keyword(Keyword::False, _) => {
                self.advance();
                Ok(Expr::Literal(LiteralValue::Boolean(false)))
            }
            TokenKind::Number(s) => {
                self.advance();
                if s.contains('e') || s.contains('E') {
                    let f: f64 = s.parse().map_err(|_| {
                        parse_error(self.sql, tok.span, format!("invalid float `{s}`"), None)
                    })?;
                    Ok(Expr::Literal(LiteralValue::Float(f)))
                } else if s.contains('.') {
                    Ok(Expr::Literal(LiteralValue::Decimal(s.to_string())))
                } else if let Ok(i) = s.parse::<i64>() {
                    Ok(Expr::Literal(LiteralValue::Integer(i)))
                } else {
                    Ok(Expr::Literal(LiteralValue::Decimal(s.to_string())))
                }
            }
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(Expr::Literal(LiteralValue::String(s)))
            }
            TokenKind::Param(idx) => {
                self.advance();
                Ok(Expr::Param(idx))
            }
            TokenKind::Keyword(Keyword::Date, _)
                if matches!(self.peek_n(1).kind, TokenKind::StringLit(_)) =>
            {
                self.advance();
                let s = self.parse_string_lit()?;
                Ok(Expr::Literal(LiteralValue::Date(s)))
            }
            TokenKind::Keyword(Keyword::Timestamp, _)
                if matches!(self.peek_n(1).kind, TokenKind::StringLit(_)) =>
            {
                self.advance();
                let s = self.parse_string_lit()?;
                Ok(Expr::Literal(LiteralValue::Timestamp(s)))
            }
            TokenKind::Keyword(Keyword::Interval, _) => self.parse_interval_literal(),
            TokenKind::Keyword(Keyword::Case, _) => self.parse_case_expr(),
            TokenKind::Keyword(Keyword::Cast | Keyword::TryCast, _) => {
                let try_cast = matches!(tok.kind, TokenKind::Keyword(Keyword::TryCast, _));
                self.advance();
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let expr = self.parse_expr(0)?;
                self.expect_kw(Keyword::As)?;
                let data_type = self.parse_data_type()?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                Ok(Expr::Cast {
                    expr: Box::new(expr),
                    data_type,
                    try_cast,
                })
            }
            TokenKind::Keyword(Keyword::Extract, _) => {
                self.advance();
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let part = self.parse_date_part()?;
                self.expect_kw(Keyword::From)?;
                let expr = self.parse_expr(0)?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                Ok(Expr::Extract {
                    part,
                    expr: Box::new(expr),
                })
            }
            TokenKind::Keyword(Keyword::Substring, _)
                if matches!(self.peek_n(1).kind, TokenKind::LParen) =>
            {
                self.advance();
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let s = self.parse_expr(0)?;
                let mut args = vec![s];
                if self.consume_kw(Keyword::From) {
                    args.push(self.parse_expr(0)?);
                    if self.consume_kw(Keyword::For) {
                        args.push(self.parse_expr(0)?);
                    }
                } else {
                    while self.consume_if(&TokenKind::Comma) {
                        args.push(self.parse_expr(0)?);
                    }
                }
                self.expect_token(&TokenKind::RParen, "`)`")?;
                Ok(Expr::Function {
                    name: "substring".into(),
                    args,
                    distinct: false,
                    wildcard_arg: false,
                    filter: None,
                    over: None,
                })
            }
            TokenKind::Keyword(Keyword::Position, _)
                if matches!(self.peek_n(1).kind, TokenKind::LParen) =>
            {
                self.advance();
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let needle = self.parse_expr(41)?;
                self.expect_kw(Keyword::In)?;
                let hay = self.parse_expr(0)?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                Ok(Expr::Function {
                    name: "position".into(),
                    args: vec![needle, hay],
                    distinct: false,
                    wildcard_arg: false,
                    filter: None,
                    over: None,
                })
            }
            TokenKind::Keyword(Keyword::Trim, _)
                if matches!(self.peek_n(1).kind, TokenKind::LParen) =>
            {
                self.advance();
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let func_name = if self.consume_kw(Keyword::Leading) {
                    "ltrim"
                } else if self.consume_kw(Keyword::Trailing) {
                    "rtrim"
                } else {
                    let _ = self.consume_kw(Keyword::Both);
                    "trim"
                };
                let first = self.parse_expr(0)?;
                let args = if self.consume_kw(Keyword::From) {
                    let target = self.parse_expr(0)?;
                    vec![target, first]
                } else if self.consume_if(&TokenKind::Comma) {
                    let chars = self.parse_expr(0)?;
                    vec![first, chars]
                } else {
                    vec![first]
                };
                self.expect_token(&TokenKind::RParen, "`)`")?;
                Ok(Expr::Function {
                    name: func_name.into(),
                    args,
                    distinct: false,
                    wildcard_arg: false,
                    filter: None,
                    over: None,
                })
            }
            TokenKind::Keyword(Keyword::Exists, _) => {
                self.advance();
                self.expect_token(&TokenKind::LParen, "`(`")?;
                let sub = self.parse_query()?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                Ok(Expr::Exists {
                    subquery: Box::new(sub),
                    negated: false,
                })
            }
            TokenKind::LParen => {
                self.advance();
                if self.check_kw(Keyword::Select) || self.check_kw(Keyword::With) {
                    let sub = self.parse_query()?;
                    self.expect_token(&TokenKind::RParen, "`)`")?;
                    return Ok(Expr::Subquery(Box::new(sub)));
                }
                let inner = self.parse_expr(0)?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
                Ok(Expr::Nested(Box::new(inner)))
            }
            TokenKind::Ident(_) | TokenKind::QuotedIdent(_) | TokenKind::Keyword(_, _) => {
                let name = self.parse_ident()?;
                // Function call: `name(...) [FILTER (WHERE ...)] [OVER (...)]`
                if self.consume_if(&TokenKind::LParen) {
                    let mut distinct = false;
                    let mut wildcard_arg = false;
                    let mut args = Vec::new();
                    if !matches!(self.peek().kind, TokenKind::RParen) {
                        if self.consume_if(&TokenKind::Star) {
                            wildcard_arg = true;
                        } else {
                            distinct = self.consume_kw(Keyword::Distinct);
                            if self.consume_if(&TokenKind::Star) {
                                wildcard_arg = true;
                            } else {
                                args = self.parse_expr_list()?;
                            }
                        }
                    }
                    self.expect_token(&TokenKind::RParen, "`)`")?;
                    let filter = if self.consume_kw(Keyword::Filter) {
                        self.expect_token(&TokenKind::LParen, "`(`")?;
                        self.expect_kw(Keyword::Where)?;
                        let f = self.parse_expr(0)?;
                        self.expect_token(&TokenKind::RParen, "`)`")?;
                        Some(Box::new(f))
                    } else {
                        None
                    };
                    let over = if self.consume_kw(Keyword::Over) {
                        Some(self.parse_window_spec()?)
                    } else {
                        None
                    };
                    return Ok(Expr::Function {
                        name,
                        args,
                        distinct,
                        wildcard_arg,
                        filter,
                        over,
                    });
                }

                // Compound identifier `a.b` or `a.b.c`
                if self.consume_if(&TokenKind::Dot) {
                    let mut parts = vec![name];
                    loop {
                        parts.push(self.parse_ident()?);
                        if !self.consume_if(&TokenKind::Dot) {
                            break;
                        }
                    }
                    return Ok(Expr::CompoundIdentifier(parts));
                }
                Ok(Expr::Identifier(name))
            }
            _ => Err(parse_error(
                self.sql,
                tok.span,
                format!("unexpected token `{:?}` in expression", tok.kind),
                None,
            )),
        }
    }

    fn parse_interval_literal(&mut self) -> Result<Expr> {
        let span = self.expect_kw(Keyword::Interval)?;
        let (val, inline_unit) = match self.peek().kind.clone() {
            TokenKind::StringLit(s) => {
                self.advance();
                let mut parts = s.split_whitespace();
                let num_s = parts
                    .next()
                    .ok_or_else(|| parse_error(self.sql, span, "empty INTERVAL literal", None))?;
                let v: i64 = num_s.parse().map_err(|_| {
                    parse_error(
                        self.sql,
                        span,
                        format!("invalid INTERVAL value `{num_s}`"),
                        None,
                    )
                })?;
                let u = parts.next().map(|u| u.to_ascii_lowercase());
                (v, u)
            }
            TokenKind::Number(s) => {
                self.advance();
                let v: i64 = s.parse().map_err(|_| {
                    parse_error(
                        self.sql,
                        span,
                        format!("invalid INTERVAL number `{s}`"),
                        None,
                    )
                })?;
                (v, None)
            }
            _ => {
                return Err(parse_error(
                    self.sql,
                    self.peek().span,
                    "expected string or integer after INTERVAL",
                    None,
                ))
            }
        };

        let unit = if let Some(u) = inline_unit {
            parse_interval_unit_str(&u).ok_or_else(|| {
                parse_error(self.sql, span, format!("unknown INTERVAL unit `{u}`"), None)
            })?
        } else {
            let utok = self.advance();
            let u = match utok.kind {
                TokenKind::Keyword(Keyword::Year, _) => Some(IntervalUnit::Year),
                TokenKind::Keyword(Keyword::Month, _) => Some(IntervalUnit::Month),
                TokenKind::Keyword(Keyword::Day, _) => Some(IntervalUnit::Day),
                TokenKind::Keyword(Keyword::Hour, _) => Some(IntervalUnit::Hour),
                TokenKind::Keyword(Keyword::Minute, _) => Some(IntervalUnit::Minute),
                TokenKind::Keyword(Keyword::Second, _) => Some(IntervalUnit::Second),
                TokenKind::Ident(s) => parse_interval_unit_str(&s.to_ascii_lowercase()),
                _ => None,
            };
            let resolved = u.ok_or_else(|| {
                parse_error(
                    self.sql,
                    utok.span,
                    "expected INTERVAL unit (DAY, MONTH, YEAR, ...)",
                    None,
                )
            })?;
            // Optional precision like `DAY (3)` in ANSI/TPC-H SQL!
            if self.consume_if(&TokenKind::LParen) {
                let _ = self.parse_u64_lit()?;
                self.expect_token(&TokenKind::RParen, "`)`")?;
            }
            resolved
        };
        Ok(Expr::Literal(LiteralValue::Interval { value: val, unit }))
    }

    fn parse_date_part(&mut self) -> Result<DatePart> {
        let tok = self.advance();
        match tok.kind {
            TokenKind::Keyword(Keyword::Year, _) => Ok(DatePart::Year),
            TokenKind::Keyword(Keyword::Quarter, _) => Ok(DatePart::Quarter),
            TokenKind::Keyword(Keyword::Month, _) => Ok(DatePart::Month),
            TokenKind::Keyword(Keyword::Day, _) => Ok(DatePart::Day),
            TokenKind::Keyword(Keyword::Hour, _) => Ok(DatePart::Hour),
            TokenKind::Keyword(Keyword::Minute, _) => Ok(DatePart::Minute),
            TokenKind::Keyword(Keyword::Second, _) => Ok(DatePart::Second),
            _ => Err(parse_error(
                self.sql,
                tok.span,
                "expected date part (YEAR, QUARTER, MONTH, DAY, HOUR, MINUTE, SECOND)",
                None,
            )),
        }
    }

    fn parse_case_expr(&mut self) -> Result<Expr> {
        self.expect_kw(Keyword::Case)?;
        let operand = if !self.check_kw(Keyword::When) {
            Some(Box::new(self.parse_expr(0)?))
        } else {
            None
        };
        let mut branches = Vec::new();
        while self.consume_kw(Keyword::When) {
            let cond = self.parse_expr(0)?;
            self.expect_kw(Keyword::Then)?;
            let res = self.parse_expr(0)?;
            branches.push((cond, res));
        }
        let else_result = if self.consume_kw(Keyword::Else) {
            Some(Box::new(self.parse_expr(0)?))
        } else {
            None
        };
        self.expect_kw(Keyword::End)?;
        Ok(Expr::Case {
            operand,
            branches,
            else_result,
        })
    }

    fn parse_window_spec(&mut self) -> Result<WindowSpec> {
        if self.consume_if(&TokenKind::LParen) {
            let spec = self.parse_window_spec_inner()?;
            self.expect_token(&TokenKind::RParen, "`)`")?;
            Ok(spec)
        } else {
            let name = self.parse_ident()?;
            Ok(WindowSpec {
                window_name: Some(name),
                partition_by: Vec::new(),
                order_by: Vec::new(),
                frame: None,
            })
        }
    }

    fn parse_window_spec_inner(&mut self) -> Result<WindowSpec> {
        let window_name = if !self.check_kw(Keyword::Partition)
            && !self.check_kw(Keyword::Order)
            && !self.check_kw(Keyword::Rows)
            && !self.check_kw(Keyword::Range)
            && !matches!(self.peek().kind, TokenKind::RParen)
        {
            Some(self.parse_ident()?)
        } else {
            None
        };
        let partition_by = if self.consume_kw(Keyword::Partition) {
            self.expect_kw(Keyword::By)?;
            self.parse_expr_list()?
        } else {
            Vec::new()
        };
        let order_by = self.parse_optional_order_by()?;
        let frame = if self.check_kw(Keyword::Rows) || self.check_kw(Keyword::Range) {
            let units = if self.consume_kw(Keyword::Rows) {
                WindowFrameUnits::Rows
            } else {
                self.advance();
                WindowFrameUnits::Range
            };
            let (start, end) = if self.consume_kw(Keyword::Between) {
                let s = self.parse_frame_bound()?;
                self.expect_kw(Keyword::And)?;
                let e = self.parse_frame_bound()?;
                (s, e)
            } else {
                let s = self.parse_frame_bound()?;
                (s, WindowFrameBound::CurrentRow)
            };
            Some(WindowFrame { units, start, end })
        } else {
            None
        };
        Ok(WindowSpec {
            window_name,
            partition_by,
            order_by,
            frame,
        })
    }

    fn parse_frame_bound(&mut self) -> Result<WindowFrameBound> {
        if self.consume_kw(Keyword::Unbounded) {
            if self.consume_kw(Keyword::Preceding) {
                Ok(WindowFrameBound::UnboundedPreceding)
            } else {
                self.expect_kw(Keyword::Following)?;
                Ok(WindowFrameBound::UnboundedFollowing)
            }
        } else if self.consume_kw(Keyword::Current) {
            self.expect_kw(Keyword::Row)?;
            Ok(WindowFrameBound::CurrentRow)
        } else {
            let n = self.parse_u64_lit()?;
            if self.consume_kw(Keyword::Preceding) {
                Ok(WindowFrameBound::Preceding(n))
            } else {
                self.expect_kw(Keyword::Following)?;
                Ok(WindowFrameBound::Following(n))
            }
        }
    }
}

fn parse_interval_unit_str(u: &str) -> Option<IntervalUnit> {
    match u {
        "year" | "years" => Some(IntervalUnit::Year),
        "month" | "months" => Some(IntervalUnit::Month),
        "day" | "days" => Some(IntervalUnit::Day),
        "hour" | "hours" => Some(IntervalUnit::Hour),
        "minute" | "minutes" => Some(IntervalUnit::Minute),
        "second" | "seconds" => Some(IntervalUnit::Second),
        _ => None,
    }
}

fn is_ident_allowed_keyword(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::Date
            | Keyword::Timestamp
            | Keyword::Year
            | Keyword::Quarter
            | Keyword::Month
            | Keyword::Day
            | Keyword::Hour
            | Keyword::Minute
            | Keyword::Second
            | Keyword::Key
            | Keyword::Replace
            | Keyword::Filter
            | Keyword::Row
            | Keyword::Rows
            | Keyword::Range
            | Keyword::First
            | Keyword::Last
            | Keyword::Next
            | Keyword::Only
            | Keyword::Sets
            | Keyword::Grouping
            | Keyword::Logical
            | Keyword::Unnested
            | Keyword::Physical
            | Keyword::Json
            | Keyword::Show
            | Keyword::Left
            | Keyword::Right
            | Keyword::Trim
            | Keyword::Substring
            | Keyword::Position
            | Keyword::Text
            | Keyword::Double
            | Keyword::Float
            | Keyword::Real
            | Keyword::Precision
            | Keyword::View
            | Keyword::Table
    )
}

fn is_alias_allowed_keyword(kw: Keyword) -> bool {
    is_ident_allowed_keyword(kw) || matches!(kw, Keyword::Values | Keyword::Default)
}

fn is_implicit_alias_keyword(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::Date
            | Keyword::Timestamp
            | Keyword::Year
            | Keyword::Quarter
            | Keyword::Month
            | Keyword::Day
            | Keyword::Hour
            | Keyword::Minute
            | Keyword::Second
            | Keyword::Key
            | Keyword::Text
            | Keyword::Logical
            | Keyword::Unnested
            | Keyword::Physical
            | Keyword::Json
    )
}
