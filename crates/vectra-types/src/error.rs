use thiserror::Error;

/// Unified error type for all VectraDB crates.
#[derive(Debug, Error)]
pub enum VectraError {
    #[error("out of memory: requested {requested} bytes, available {available} of {limit} limit")]
    OutOfMemory {
        requested: usize,
        available: usize,
        limit: usize,
    },

    #[error("arithmetic overflow: {0}")]
    ArithmeticOverflow(String),

    #[error("division by zero")]
    DivisionByZero,

    #[error("invalid cast from {from} to {to}: {value}")]
    InvalidCast {
        from: String,
        to: String,
        value: String,
    },

    #[error("type mismatch: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },

    #[error("checksum mismatch in {context}: expected 0x{expected:08x}, actual 0x{actual:08x}")]
    ChecksumMismatch {
        expected: u32,
        actual: u32,
        context: String,
    },

    #[error("corrupt storage/wire format: {0}")]
    CorruptFormat(String),

    #[error("SQL parse error: {message}\n{rendered}")]
    ParseError {
        message: String,
        span: Option<(u32, u32)>,
        rendered: String,
    },

    #[error("SQL bind error: {0}")]
    BindError(String),

    #[error("query planning error: {0}")]
    PlanError(String),

    #[error("execution error: {0}")]
    ExecutionError(String),

    #[error("transaction conflict: {0}")]
    TransactionConflict(String),

    #[error("query cancelled")]
    Cancelled,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal invariant violation: {0}")]
    Internal(String),
}

impl VectraError {
    /// Returns the standard 5-character PostgreSQL SQLSTATE code for this error.
    pub fn sqlstate(&self) -> &'static str {
        match self {
            Self::OutOfMemory { .. } => "53200",
            Self::ArithmeticOverflow(_) => "22003",
            Self::DivisionByZero => "22012",
            Self::InvalidCast { .. } => "22P02",
            Self::TypeMismatch { .. } => "42804",
            Self::ChecksumMismatch { .. } | Self::CorruptFormat(_) => "XX001",
            Self::ParseError { .. } => "42601",
            Self::BindError(msg) => {
                if msg.contains("does not exist") && msg.contains("table") {
                    "42P01"
                } else if msg.contains("column") {
                    "42703"
                } else {
                    "42601"
                }
            }
            Self::PlanError(_) | Self::ExecutionError(_) => "42000",
            Self::TransactionConflict(_) => "40001",
            Self::Cancelled => "57014",
            Self::Io(_) => "58030",
            Self::Internal(_) => "XX000",
        }
    }
}

pub type Result<T> = std::result::Result<T, VectraError>;
