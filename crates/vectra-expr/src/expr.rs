use serde::{Deserialize, Serialize};
use vectra_types::{LogicalType, ScalarValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BinaryOperator {
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
    IsDistinctFrom,
    IsNotDistinctFrom,
    And,
    Or,
    Concat,
}

impl BinaryOperator {
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq
                | Self::NotEq
                | Self::Lt
                | Self::LtEq
                | Self::Gt
                | Self::GtEq
                | Self::IsDistinctFrom
                | Self::IsNotDistinctFrom
        )
    }

    pub fn is_arithmetic(self) -> bool {
        matches!(
            self,
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod
        )
    }

    pub fn flip(self) -> Option<Self> {
        match self {
            Self::Eq => Some(Self::Eq),
            Self::NotEq => Some(Self::NotEq),
            Self::Lt => Some(Self::Gt),
            Self::LtEq => Some(Self::GtEq),
            Self::Gt => Some(Self::Lt),
            Self::GtEq => Some(Self::LtEq),
            Self::IsDistinctFrom => Some(Self::IsDistinctFrom),
            Self::IsNotDistinctFrom => Some(Self::IsNotDistinctFrom),
            Self::Add => Some(Self::Add),
            Self::Mul => Some(Self::Mul),
            Self::And => Some(Self::And),
            Self::Or => Some(Self::Or),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnaryOperator {
    Neg,
    Not,
    IsNull,
    IsNotNull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DatePart {
    Year,
    Quarter,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScalarFunction {
    // String functions
    Lower,
    Upper,
    Length,
    CharLength,
    Substring,
    Trim,
    Ltrim,
    Rtrim,
    Concat,
    ConcatWs,
    Replace,
    StartsWith,
    EndsWith,
    Position,
    Left,
    Right,
    // Math functions
    Abs,
    Round,
    Ceil,
    Floor,
    Power,
    Sqrt,
    NullIf,
    Greatest,
    Least,
    // Date / Time functions
    Extract(DatePart),
    DateTrunc(DatePart),
    DateAddDays,
    DateAddMonths,
    DateAddYears,
    TimestampAddMicros,
    DateDiffDays,
}

/// Physical expression DAG evaluated directly on columnar `DataChunk`s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PhysicalExpr {
    ColumnRef {
        index: usize,
        name: String,
        data_type: LogicalType,
    },
    Literal {
        value: ScalarValue,
        data_type: LogicalType,
    },
    BinaryOp {
        op: BinaryOperator,
        left: Box<PhysicalExpr>,
        right: Box<PhysicalExpr>,
        return_type: LogicalType,
    },
    UnaryOp {
        op: UnaryOperator,
        expr: Box<PhysicalExpr>,
        return_type: LogicalType,
    },
    Cast {
        expr: Box<PhysicalExpr>,
        target_type: LogicalType,
        try_cast: bool,
    },
    CaseWhen {
        branches: Vec<(PhysicalExpr, PhysicalExpr)>,
        else_expr: Option<Box<PhysicalExpr>>,
        return_type: LogicalType,
    },
    Coalesce {
        args: Vec<PhysicalExpr>,
        return_type: LogicalType,
    },
    InList {
        expr: Box<PhysicalExpr>,
        list: Vec<PhysicalExpr>,
        negated: bool,
    },
    Like {
        expr: Box<PhysicalExpr>,
        pattern: Box<PhysicalExpr>,
        escape: Option<char>,
        case_insensitive: bool,
        negated: bool,
    },
    Between {
        expr: Box<PhysicalExpr>,
        low: Box<PhysicalExpr>,
        high: Box<PhysicalExpr>,
        negated: bool,
    },
    ScalarFunc {
        func: ScalarFunction,
        args: Vec<PhysicalExpr>,
        return_type: LogicalType,
    },
}

impl PhysicalExpr {
    pub fn data_type(&self) -> LogicalType {
        match self {
            Self::ColumnRef { data_type, .. } => *data_type,
            Self::Literal { data_type, .. } => *data_type,
            Self::BinaryOp { return_type, .. } => *return_type,
            Self::UnaryOp { return_type, .. } => *return_type,
            Self::Cast { target_type, .. } => *target_type,
            Self::CaseWhen { return_type, .. } => *return_type,
            Self::Coalesce { return_type, .. } => *return_type,
            Self::InList { .. } | Self::Like { .. } | Self::Between { .. } => LogicalType::Boolean,
            Self::ScalarFunc { return_type, .. } => *return_type,
        }
    }

    pub fn col(index: usize, name: impl Into<String>, data_type: LogicalType) -> Self {
        Self::ColumnRef {
            index,
            name: name.into(),
            data_type,
        }
    }

    pub fn lit(value: ScalarValue, data_type: LogicalType) -> Self {
        Self::Literal { value, data_type }
    }

    pub fn binary(
        op: BinaryOperator,
        left: PhysicalExpr,
        right: PhysicalExpr,
        return_type: LogicalType,
    ) -> Self {
        Self::BinaryOp {
            op,
            left: Box::new(left),
            right: Box::new(right),
            return_type,
        }
    }

    pub fn unary(op: UnaryOperator, expr: PhysicalExpr, return_type: LogicalType) -> Self {
        Self::UnaryOp {
            op,
            expr: Box::new(expr),
            return_type,
        }
    }
}
