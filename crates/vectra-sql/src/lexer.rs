use crate::diagnostics::{parse_error, Span};
use vectra_types::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    Select,
    Distinct,
    All,
    From,
    Where,
    Group,
    By,
    Having,
    Qualify,
    Order,
    Asc,
    Desc,
    Nulls,
    First,
    Last,
    Limit,
    Offset,
    Fetch,
    Next,
    Row,
    Rows,
    Only,
    As,
    Join,
    Inner,
    Left,
    Right,
    Full,
    Outer,
    Cross,
    Natural,
    Semi,
    Anti,
    On,
    Using,
    And,
    Or,
    Not,
    Is,
    Null,
    True,
    False,
    In,
    Between,
    Like,
    Ilike,
    Escape,
    Case,
    When,
    Then,
    Else,
    End,
    Cast,
    TryCast,
    Exists,
    Any,
    Some,
    Union,
    Intersect,
    Except,
    With,
    Recursive,
    Over,
    Partition,
    Range,
    Unbounded,
    Preceding,
    Following,
    Current,
    Window,
    Filter,
    Rollup,
    Cube,
    Grouping,
    Sets,
    Create,
    Replace,
    Table,
    View,
    Drop,
    If,
    Insert,
    Into,
    Values,
    Delete,
    Update,
    Set,
    Copy,
    To,
    Analyze,
    Begin,
    Transaction,
    Commit,
    Rollback,
    Explain,
    Logical,
    Unnested,
    Physical,
    Json,
    Show,
    Primary,
    Key,
    Default,
    Date,
    Timestamp,
    Interval,
    Extract,
    Year,
    Quarter,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Substring,
    For,
    Position,
    Trim,
    Leading,
    Trailing,
    Both,
    // Type keywords
    Boolean,
    Bool,
    Tinyint,
    Smallint,
    Int,
    Integer,
    Bigint,
    Float,
    Double,
    Real,
    Decimal,
    Numeric,
    Varchar,
    Char,
    Character,
    Varying,
    Text,
    Varbinary,
    Blob,
    Bytea,
    Precision,
}

impl Keyword {
    pub fn lookup(ident: &str) -> Option<Self> {
        // Fast case-insensitive keyword dispatch without heap allocation.
        if ident.len() > 16 {
            return None;
        }
        let mut buf = [0u8; 16];
        for (i, b) in ident.bytes().enumerate() {
            buf[i] = b.to_ascii_uppercase();
        }
        let upper = std::str::from_utf8(&buf[..ident.len()]).ok()?;
        Some(match upper {
            "SELECT" => Self::Select,
            "DISTINCT" => Self::Distinct,
            "ALL" => Self::All,
            "FROM" => Self::From,
            "WHERE" => Self::Where,
            "GROUP" => Self::Group,
            "BY" => Self::By,
            "HAVING" => Self::Having,
            "QUALIFY" => Self::Qualify,
            "ORDER" => Self::Order,
            "ASC" => Self::Asc,
            "DESC" => Self::Desc,
            "NULLS" => Self::Nulls,
            "FIRST" => Self::First,
            "LAST" => Self::Last,
            "LIMIT" => Self::Limit,
            "OFFSET" => Self::Offset,
            "FETCH" => Self::Fetch,
            "NEXT" => Self::Next,
            "ROW" => Self::Row,
            "ROWS" => Self::Rows,
            "ONLY" => Self::Only,
            "AS" => Self::As,
            "JOIN" => Self::Join,
            "INNER" => Self::Inner,
            "LEFT" => Self::Left,
            "RIGHT" => Self::Right,
            "FULL" => Self::Full,
            "OUTER" => Self::Outer,
            "CROSS" => Self::Cross,
            "NATURAL" => Self::Natural,
            "SEMI" => Self::Semi,
            "ANTI" => Self::Anti,
            "ON" => Self::On,
            "USING" => Self::Using,
            "AND" => Self::And,
            "OR" => Self::Or,
            "NOT" => Self::Not,
            "IS" => Self::Is,
            "NULL" => Self::Null,
            "TRUE" => Self::True,
            "FALSE" => Self::False,
            "IN" => Self::In,
            "BETWEEN" => Self::Between,
            "LIKE" => Self::Like,
            "ILIKE" => Self::Ilike,
            "ESCAPE" => Self::Escape,
            "CASE" => Self::Case,
            "WHEN" => Self::When,
            "THEN" => Self::Then,
            "ELSE" => Self::Else,
            "END" => Self::End,
            "CAST" => Self::Cast,
            "TRY_CAST" => Self::TryCast,
            "EXISTS" => Self::Exists,
            "ANY" => Self::Any,
            "SOME" => Self::Some,
            "UNION" => Self::Union,
            "INTERSECT" => Self::Intersect,
            "EXCEPT" => Self::Except,
            "WITH" => Self::With,
            "RECURSIVE" => Self::Recursive,
            "OVER" => Self::Over,
            "PARTITION" => Self::Partition,
            "RANGE" => Self::Range,
            "UNBOUNDED" => Self::Unbounded,
            "PRECEDING" => Self::Preceding,
            "FOLLOWING" => Self::Following,
            "CURRENT" => Self::Current,
            "WINDOW" => Self::Window,
            "FILTER" => Self::Filter,
            "ROLLUP" => Self::Rollup,
            "CUBE" => Self::Cube,
            "GROUPING" => Self::Grouping,
            "SETS" => Self::Sets,
            "CREATE" => Self::Create,
            "REPLACE" => Self::Replace,
            "TABLE" => Self::Table,
            "VIEW" => Self::View,
            "DROP" => Self::Drop,
            "IF" => Self::If,
            "INSERT" => Self::Insert,
            "INTO" => Self::Into,
            "VALUES" => Self::Values,
            "DELETE" => Self::Delete,
            "UPDATE" => Self::Update,
            "SET" => Self::Set,
            "COPY" => Self::Copy,
            "TO" => Self::To,
            "ANALYZE" => Self::Analyze,
            "BEGIN" => Self::Begin,
            "TRANSACTION" => Self::Transaction,
            "COMMIT" => Self::Commit,
            "ROLLBACK" => Self::Rollback,
            "EXPLAIN" => Self::Explain,
            "LOGICAL" => Self::Logical,
            "UNNESTED" => Self::Unnested,
            "PHYSICAL" => Self::Physical,
            "JSON" => Self::Json,
            "SHOW" => Self::Show,
            "PRIMARY" => Self::Primary,
            "KEY" => Self::Key,
            "DEFAULT" => Self::Default,
            "DATE" => Self::Date,
            "TIMESTAMP" => Self::Timestamp,
            "INTERVAL" => Self::Interval,
            "EXTRACT" => Self::Extract,
            "YEAR" | "YEARS" => Self::Year,
            "QUARTER" | "QUARTERS" => Self::Quarter,
            "MONTH" | "MONTHS" => Self::Month,
            "DAY" | "DAYS" => Self::Day,
            "HOUR" | "HOURS" => Self::Hour,
            "MINUTE" | "MINUTES" => Self::Minute,
            "SECOND" | "SECONDS" => Self::Second,
            "SUBSTRING" => Self::Substring,
            "FOR" => Self::For,
            "POSITION" => Self::Position,
            "TRIM" => Self::Trim,
            "LEADING" => Self::Leading,
            "TRAILING" => Self::Trailing,
            "BOTH" => Self::Both,
            "BOOLEAN" => Self::Boolean,
            "BOOL" => Self::Bool,
            "TINYINT" => Self::Tinyint,
            "SMALLINT" => Self::Smallint,
            "INT" => Self::Int,
            "INTEGER" => Self::Integer,
            "BIGINT" => Self::Bigint,
            "FLOAT" => Self::Float,
            "DOUBLE" => Self::Double,
            "REAL" => Self::Real,
            "DECIMAL" => Self::Decimal,
            "NUMERIC" => Self::Numeric,
            "VARCHAR" => Self::Varchar,
            "CHAR" => Self::Char,
            "CHARACTER" => Self::Character,
            "VARYING" => Self::Varying,
            "TEXT" => Self::Text,
            "VARBINARY" => Self::Varbinary,
            "BLOB" => Self::Blob,
            "BYTEA" => Self::Bytea,
            "PRECISION" => Self::Precision,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind<'a> {
    Keyword(Keyword, &'a str),
    Ident(&'a str),
    QuotedIdent(String),
    Number(&'a str),
    StringLit(String),
    Param(u32),
    LParen,
    RParen,
    Comma,
    Dot,
    Semicolon,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Concat,
    DoubleColon,
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokenKind<'a>,
    pub span: Span,
}

/// Zero-copy SQL lexer producing spanned tokens.
pub fn tokenize(sql: &str) -> Result<Vec<Token<'_>>> {
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    let mut tokens = Vec::with_capacity(n / 4 + 4);

    while i < n {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Single-line comment `-- ...`
        if b == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
            i += 2;
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment `/* ... */` (supports nesting)
        if b == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i + 1 < n && depth > 0 {
                if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                return Err(parse_error(
                    sql,
                    Span::new(start as u32, n as u32),
                    "unterminated block comment",
                    Some("close with `*/`"),
                ));
            }
            continue;
        }

        let start = i;
        match b {
            b'(' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::LParen,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b')' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::RParen,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b',' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Comma,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b';' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Semicolon,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'+' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Plus,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'-' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Minus,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'*' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Star,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'/' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Slash,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'%' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Percent,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'=' => {
                i += 1;
                tokens.push(Token {
                    kind: TokenKind::Eq,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'!' if i + 1 < n && bytes[i + 1] == b'=' => {
                i += 2;
                tokens.push(Token {
                    kind: TokenKind::NotEq,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'<' => {
                if i + 1 < n && bytes[i + 1] == b'=' {
                    i += 2;
                    tokens.push(Token {
                        kind: TokenKind::LtEq,
                        span: Span::new(start as u32, i as u32),
                    });
                } else if i + 1 < n && bytes[i + 1] == b'>' {
                    i += 2;
                    tokens.push(Token {
                        kind: TokenKind::NotEq,
                        span: Span::new(start as u32, i as u32),
                    });
                } else {
                    i += 1;
                    tokens.push(Token {
                        kind: TokenKind::Lt,
                        span: Span::new(start as u32, i as u32),
                    });
                }
            }
            b'>' => {
                if i + 1 < n && bytes[i + 1] == b'=' {
                    i += 2;
                    tokens.push(Token {
                        kind: TokenKind::GtEq,
                        span: Span::new(start as u32, i as u32),
                    });
                } else {
                    i += 1;
                    tokens.push(Token {
                        kind: TokenKind::Gt,
                        span: Span::new(start as u32, i as u32),
                    });
                }
            }
            b'|' if i + 1 < n && bytes[i + 1] == b'|' => {
                i += 2;
                tokens.push(Token {
                    kind: TokenKind::Concat,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b':' if i + 1 < n && bytes[i + 1] == b':' => {
                i += 2;
                tokens.push(Token {
                    kind: TokenKind::DoubleColon,
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'.' => {
                if i + 1 < n && bytes[i + 1].is_ascii_digit() {
                    i += 1;
                    while i < n && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
                        i += 1;
                        if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
                            i += 1;
                        }
                        while i < n && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    tokens.push(Token {
                        kind: TokenKind::Number(&sql[start..i]),
                        span: Span::new(start as u32, i as u32),
                    });
                } else {
                    i += 1;
                    tokens.push(Token {
                        kind: TokenKind::Dot,
                        span: Span::new(start as u32, i as u32),
                    });
                }
            }
            b'\'' => {
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < n {
                    if bytes[i] == b'\'' {
                        if i + 1 < n && bytes[i + 1] == b'\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            closed = true;
                            break;
                        }
                    } else {
                        let ch_start = i;
                        while i < n && bytes[i] != b'\'' {
                            i += 1;
                        }
                        s.push_str(&sql[ch_start..i]);
                    }
                }
                if !closed {
                    return Err(parse_error(
                        sql,
                        Span::new(start as u32, n as u32),
                        "unterminated string literal",
                        Some("add closing single quote `'`"),
                    ));
                }
                tokens.push(Token {
                    kind: TokenKind::StringLit(s),
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'"' => {
                i += 1;
                let mut ident = String::new();
                let mut closed = false;
                while i < n {
                    if bytes[i] == b'"' {
                        if i + 1 < n && bytes[i + 1] == b'"' {
                            ident.push('"');
                            i += 2;
                        } else {
                            i += 1;
                            closed = true;
                            break;
                        }
                    } else {
                        let ch_start = i;
                        while i < n && bytes[i] != b'"' {
                            i += 1;
                        }
                        ident.push_str(&sql[ch_start..i]);
                    }
                }
                if !closed {
                    return Err(parse_error(
                        sql,
                        Span::new(start as u32, n as u32),
                        "unterminated quoted identifier",
                        Some("add closing double quote `\"`"),
                    ));
                }
                tokens.push(Token {
                    kind: TokenKind::QuotedIdent(ident),
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'$' if i + 1 < n && bytes[i + 1].is_ascii_digit() => {
                i += 1;
                let num_start = i;
                while i < n && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let param_idx: u32 = sql[num_start..i].parse().map_err(|_| {
                    parse_error(
                        sql,
                        Span::new(start as u32, i as u32),
                        "invalid parameter placeholder index",
                        None,
                    )
                })?;
                if param_idx == 0 {
                    return Err(parse_error(
                        sql,
                        Span::new(start as u32, i as u32),
                        "parameter placeholders are 1-indexed ($1, $2, ...)",
                        None,
                    ));
                }
                tokens.push(Token {
                    kind: TokenKind::Param(param_idx),
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'0'..=b'9' => {
                while i < n && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i < n && bytes[i] == b'.' && !(i + 1 < n && bytes[i + 1] == b'.') {
                    i += 1;
                    while i < n && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
                    let save = i;
                    i += 1;
                    if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
                        i += 1;
                    }
                    if i < n && bytes[i].is_ascii_digit() {
                        while i < n && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    } else {
                        i = save;
                    }
                }
                tokens.push(Token {
                    kind: TokenKind::Number(&sql[start..i]),
                    span: Span::new(start as u32, i as u32),
                });
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &sql[start..i];
                let kind = match Keyword::lookup(word) {
                    Some(kw) => TokenKind::Keyword(kw, word),
                    None => TokenKind::Ident(word),
                };
                tokens.push(Token {
                    kind,
                    span: Span::new(start as u32, i as u32),
                });
            }
            _ => {
                return Err(parse_error(
                    sql,
                    Span::new(start as u32, (start + 1) as u32),
                    format!(
                        "unexpected character `{}`",
                        sql[start..].chars().next().unwrap()
                    ),
                    None,
                ));
            }
        }
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        span: Span::new(n as u32, n as u32),
    });
    Ok(tokens)
}
