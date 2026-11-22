use crate::logical_plan::LogicalExpr;
use vectra_expr::BinaryOperator;
use vectra_types::{LogicalType, Result, VectraError, MAX_DECIMAL_PRECISION};

/// Least-Upper-Bound (LUB) implicit type coercion lattice matching SQL / DuckDB semantics:
/// `Null < Boolean < Int8 < Int16 < Int32 < Int64 < Decimal128 < Float64`
/// plus `Date32 -> TimestampMicros` and string-literal coercion to Date/Timestamp/Decimal.
pub fn common_supertype(a: LogicalType, b: LogicalType) -> Result<LogicalType> {
    if a == b {
        return Ok(a);
    }
    match (a, b) {
        (LogicalType::Null, t) | (t, LogicalType::Null) => Ok(t),

        // Integer promotion hierarchy
        (LogicalType::Int8, LogicalType::Int16) | (LogicalType::Int16, LogicalType::Int8) => {
            Ok(LogicalType::Int16)
        }
        (LogicalType::Int8 | LogicalType::Int16, LogicalType::Int32)
        | (LogicalType::Int32, LogicalType::Int8 | LogicalType::Int16) => Ok(LogicalType::Int32),
        (LogicalType::Int8 | LogicalType::Int16 | LogicalType::Int32, LogicalType::Int64)
        | (LogicalType::Int64, LogicalType::Int8 | LogicalType::Int16 | LogicalType::Int32) => {
            Ok(LogicalType::Int64)
        }

        // Integer + Decimal -> Decimal (widened to fit integer digits + decimal scale)
        (int_ty, LogicalType::Decimal128 { precision, scale })
        | (LogicalType::Decimal128 { precision, scale }, int_ty)
            if int_ty.is_integer() =>
        {
            let int_digits: u8 = match int_ty {
                LogicalType::Int8 => 3,
                LogicalType::Int16 => 5,
                LogicalType::Int32 => 10,
                LogicalType::Int64 => 19,
                _ => 19,
            };
            let s = scale.max(0) as u8;
            let dec_int = precision.saturating_sub(s);
            let new_p = (dec_int.max(int_digits) + s).min(MAX_DECIMAL_PRECISION);
            Ok(LogicalType::Decimal128 {
                precision: new_p,
                scale,
            })
        }

        // Decimal + Decimal -> unified scale and precision
        (
            LogicalType::Decimal128 {
                precision: p1,
                scale: s1,
            },
            LogicalType::Decimal128 {
                precision: p2,
                scale: s2,
            },
        ) => {
            let out_s = s1.max(s2);
            let i1 = (p1 as i32) - (s1 as i32);
            let i2 = (p2 as i32) - (s2 as i32);
            let out_p = (i1.max(i2) + (out_s as i32)).clamp(1, MAX_DECIMAL_PRECISION as i32) as u8;
            Ok(LogicalType::Decimal128 {
                precision: out_p,
                scale: out_s,
            })
        }

        // Any numeric + Float64 -> Float64
        (x, LogicalType::Float64) | (LogicalType::Float64, x) if x.is_numeric() => {
            Ok(LogicalType::Float64)
        }

        // Date32 + TimestampMicros -> TimestampMicros
        (LogicalType::Date32, LogicalType::TimestampMicros)
        | (LogicalType::TimestampMicros, LogicalType::Date32) => Ok(LogicalType::TimestampMicros),

        // String <-> Date / Timestamp implicit literal coercion
        (LogicalType::Date32, LogicalType::Utf8) | (LogicalType::Utf8, LogicalType::Date32) => {
            Ok(LogicalType::Date32)
        }
        (LogicalType::TimestampMicros, LogicalType::Utf8)
        | (LogicalType::Utf8, LogicalType::TimestampMicros) => Ok(LogicalType::TimestampMicros),

        // Boolean + Numeric in SQLite-compatible expressions -> Int64/Float64
        (LogicalType::Boolean, x) | (x, LogicalType::Boolean) if x.is_numeric() => Ok(x),

        // String + Numeric fallback -> Utf8 for comparisons/concat if needed, otherwise error
        _ => Err(VectraError::TypeMismatch {
            expected: a.to_string(),
            actual: b.to_string(),
        }),
    }
}

/// Derives SQL-standard arithmetic result type for `a <op> b` (including Decimal128 precision/scale).
pub fn arithmetic_result_type(
    op: BinaryOperator,
    a: LogicalType,
    b: LogicalType,
) -> Result<(LogicalType, LogicalType)> {
    let common = common_supertype(a, b)?;
    if common == LogicalType::Null {
        return Ok((LogicalType::Int64, LogicalType::Int64));
    }
    match (a, b) {
        (
            LogicalType::Decimal128 {
                precision: p1,
                scale: s1,
            },
            LogicalType::Decimal128 {
                precision: p2,
                scale: s2,
            },
        ) => {
            let out = match op {
                BinaryOperator::Add | BinaryOperator::Sub => {
                    let s = s1.max(s2);
                    let i1 = (p1 as i32) - (s1 as i32);
                    let i2 = (p2 as i32) - (s2 as i32);
                    let p =
                        (i1.max(i2) + 1 + s as i32).clamp(1, MAX_DECIMAL_PRECISION as i32) as u8;
                    LogicalType::Decimal128 {
                        precision: p,
                        scale: s,
                    }
                }
                BinaryOperator::Mul => {
                    let s = ((s1 as i32) + (s2 as i32)).min(18) as i8;
                    let p =
                        ((p1 as i32) + (p2 as i32)).clamp(1, MAX_DECIMAL_PRECISION as i32) as u8;
                    LogicalType::Decimal128 {
                        precision: p,
                        scale: s,
                    }
                }
                BinaryOperator::Div => {
                    // DuckDB/SQL standard: division preserves generous fractional scale (at least 6)
                    let s = s1.max(s2).clamp(6, 12);
                    LogicalType::Decimal128 {
                        precision: MAX_DECIMAL_PRECISION,
                        scale: s,
                    }
                }
                BinaryOperator::Mod => {
                    let s = s1.max(s2);
                    let p = p1.max(p2).min(MAX_DECIMAL_PRECISION);
                    LogicalType::Decimal128 {
                        precision: p,
                        scale: s,
                    }
                }
                _ => common,
            };
            Ok((common, out))
        }
        _ => {
            if let LogicalType::Decimal128 { precision, scale } = common {
                let out = match op {
                    BinaryOperator::Add | BinaryOperator::Sub => LogicalType::Decimal128 {
                        precision: (precision + 1).min(MAX_DECIMAL_PRECISION),
                        scale,
                    },
                    BinaryOperator::Mul => LogicalType::Decimal128 {
                        precision: MAX_DECIMAL_PRECISION,
                        scale: (scale * 2).min(12),
                    },
                    BinaryOperator::Div => LogicalType::Decimal128 {
                        precision: MAX_DECIMAL_PRECISION,
                        scale: scale.clamp(6, 12),
                    },
                    _ => common,
                };
                Ok((common, out))
            } else if common.is_integer() {
                // Promote narrow integers in arithmetic to Int64 to avoid unexpected i8/i16 overflows
                Ok((LogicalType::Int64, LogicalType::Int64))
            } else {
                Ok((common, common))
            }
        }
    }
}

/// Wraps `expr` in an implicit `Cast` if its type differs from `target`.
pub fn coerce_expr(expr: LogicalExpr, target: LogicalType) -> LogicalExpr {
    if expr.data_type() == target || target == LogicalType::Null {
        expr
    } else {
        LogicalExpr::Cast {
            expr: Box::new(expr),
            target_type: target,
            try_cast: false,
        }
    }
}
