use serde::{Deserialize, Serialize};
use vectra_expr::DatePart;
use vectra_types::LogicalType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExplainFormat {
    Physical,
    Logical,
    Unnested,
    Analyze,
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Statement {
    Query(Box<Query>),
    CreateTable {
        name: String,
        if_not_exists: bool,
        or_replace: bool,
        columns: Vec<ColumnDef>,
        as_query: Option<Box<Query>>,
    },
    CreateView {
        name: String,
        or_replace: bool,
        column_aliases: Vec<String>,
        query: Box<Query>,
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
        columns: Vec<String>,
        source: InsertSource,
    },
    Delete {
        table_name: String,
        selection: Option<Expr>,
    },
    Update {
        table_name: String,
        assignments: Vec<(String, Expr)>,
        selection: Option<Expr>,
    },
    Copy {
        target: CopyTarget,
        path: String,
        to_file: bool,
        format: CopyFormat,
        delimiter: Option<char>,
        header: Option<bool>,
    },
    Analyze {
        table_name: Option<String>,
    },
    Begin,
    Commit,
    Rollback,
    Explain {
        format: ExplainFormat,
        stmt: Box<Statement>,
    },
    Set {
        variable: String,
        value: String,
    },
    Show {
        variable: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CopyTarget {
    Table(String),
    Query(Box<Query>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyFormat {
    Csv,
    Parquet,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: LogicalType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Query {
    pub with: Option<With>,
    pub body: Box<SetExpr>,
    pub order_by: Vec<OrderByExpr>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct With {
    pub recursive: bool,
    pub ctes: Vec<Cte>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cte {
    pub name: String,
    pub column_aliases: Vec<String>,
    pub query: Box<Query>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetOperator {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SetExpr {
    Select(Box<Select>),
    Query(Box<Query>),
    SetOperation {
        op: SetOperator,
        all: bool,
        left: Box<SetExpr>,
        right: Box<SetExpr>,
    },
    Values(Vec<Vec<Expr>>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Select {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub from: Vec<TableWithJoins>,
    pub selection: Option<Expr>,
    pub group_by: GroupByClause,
    pub having: Option<Expr>,
    pub qualify: Option<Expr>,
    pub named_windows: Vec<(String, WindowSpec)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GroupByClause {
    Expressions(Vec<Expr>),
    Rollup(Vec<Expr>),
    Cube(Vec<Expr>),
    GroupingSets(Vec<Vec<Expr>>),
}

impl GroupByClause {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Expressions(v) | Self::Rollup(v) | Self::Cube(v) => v.is_empty(),
            Self::GroupingSets(s) => s.is_empty(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SelectItem {
    UnnamedExpr(Expr),
    ExprWithAlias { expr: Expr, alias: String },
    Wildcard,
    QualifiedWildcard(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableWithJoins {
    pub relation: TableFactor,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableAlias {
    pub name: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TableFactor {
    Table {
        name: String,
        alias: Option<TableAlias>,
    },
    TableFunction {
        name: String,
        args: Vec<Expr>,
        alias: Option<TableAlias>,
    },
    Derived {
        subquery: Box<Query>,
        alias: Option<TableAlias>,
    },
    NestedJoin(Box<TableWithJoins>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AstJoinKind {
    Inner,
    LeftOuter,
    RightOuter,
    FullOuter,
    Cross,
    LeftSemi,
    LeftAnti,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JoinConstraint {
    On(Expr),
    Using(Vec<String>),
    Natural,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Join {
    pub relation: TableFactor,
    pub kind: AstJoinKind,
    pub constraint: JoinConstraint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderByExpr {
    pub expr: Expr,
    pub asc: Option<bool>,
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AstBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Concat,
    IsDistinctFrom,
    IsNotDistinctFrom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AstUnaryOp {
    Neg,
    Pos,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Quantifier {
    Any,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IntervalUnit {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LiteralValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Decimal(String),
    String(String),
    Date(String),
    Timestamp(String),
    Interval { value: i64, unit: IntervalUnit },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowFrameUnits {
    Rows,
    Range,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowFrameBound {
    UnboundedPreceding,
    Preceding(u64),
    CurrentRow,
    Following(u64),
    UnboundedFollowing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowFrame {
    pub units: WindowFrameUnits,
    pub start: WindowFrameBound,
    pub end: WindowFrameBound,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSpec {
    pub window_name: Option<String>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderByExpr>,
    pub frame: Option<WindowFrame>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Identifier(String),
    CompoundIdentifier(Vec<String>),
    Literal(LiteralValue),
    Param(u32),
    BinaryOp {
        left: Box<Expr>,
        op: AstBinaryOp,
        right: Box<Expr>,
    },
    UnaryOp {
        op: AstUnaryOp,
        expr: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<Query>,
        negated: bool,
    },
    QuantifiedSubquery {
        expr: Box<Expr>,
        op: AstBinaryOp,
        quantifier: Quantifier,
        subquery: Box<Query>,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        escape: Option<char>,
        case_insensitive: bool,
        negated: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        data_type: LogicalType,
        try_cast: bool,
    },
    Extract {
        part: DatePart,
        expr: Box<Expr>,
    },
    Function {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
        wildcard_arg: bool,
        filter: Option<Box<Expr>>,
        over: Option<WindowSpec>,
    },
    Exists {
        subquery: Box<Query>,
        negated: bool,
    },
    Subquery(Box<Query>),
    Nested(Box<Expr>),
}
