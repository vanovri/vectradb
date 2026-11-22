use crate::expr::{DatePart, ScalarFunction};
use vectra_types::{
    add_months_date32, civil_from_days, days_from_civil, timestamp_parts, LogicalType, Result,
    ScalarValue, VectraError, MICROS_PER_DAY, MICROS_PER_HOUR, MICROS_PER_MINUTE,
    MICROS_PER_SECOND,
};

/// Evaluates a built-in scalar function on a single row's arguments.
pub fn eval_scalar_function(
    func: ScalarFunction,
    args: &[ScalarValue],
    return_type: LogicalType,
) -> Result<ScalarValue> {
    // Non-strict functions (handle NULL arguments specially)
    match func {
        ScalarFunction::NullIf => {
            if args.len() != 2 {
                return Err(VectraError::ExecutionError("NULLIF expects 2 args".into()));
            }
            if args[0].is_null() {
                return Ok(ScalarValue::Null);
            }
            if !args[1].is_null() && args[0] == args[1] {
                return Ok(ScalarValue::Null);
            }
            return Ok(args[0].clone());
        }
        ScalarFunction::Concat => {
            // SQL CONCAT ignores NULLs (treats NULL as empty string).
            let mut out = String::new();
            for a in args {
                if !a.is_null() {
                    out.push_str(&a.to_display_string());
                }
            }
            return Ok(ScalarValue::Utf8(out));
        }
        ScalarFunction::ConcatWs => {
            if args.is_empty() || args[0].is_null() {
                return Ok(ScalarValue::Null);
            }
            let sep = args[0].to_display_string();
            let mut out = String::new();
            let mut first = true;
            for a in &args[1..] {
                if !a.is_null() {
                    if !first {
                        out.push_str(&sep);
                    }
                    out.push_str(&a.to_display_string());
                    first = false;
                }
            }
            return Ok(ScalarValue::Utf8(out));
        }
        ScalarFunction::Greatest => {
            let mut best: Option<&ScalarValue> = None;
            for a in args {
                if !a.is_null() {
                    best = match best {
                        None => Some(a),
                        Some(cur) if a.total_cmp(cur).is_gt() => Some(a),
                        Some(cur) => Some(cur),
                    };
                }
            }
            return Ok(best.cloned().unwrap_or(ScalarValue::Null));
        }
        ScalarFunction::Least => {
            let mut best: Option<&ScalarValue> = None;
            for a in args {
                if !a.is_null() {
                    best = match best {
                        None => Some(a),
                        Some(cur) if a.total_cmp(cur).is_lt() => Some(a),
                        Some(cur) => Some(cur),
                    };
                }
            }
            return Ok(best.cloned().unwrap_or(ScalarValue::Null));
        }
        _ => {}
    }

    // Strict SQL functions: any NULL input yields NULL.
    if args.iter().any(ScalarValue::is_null) {
        return Ok(ScalarValue::Null);
    }

    match func {
        ScalarFunction::Lower => Ok(ScalarValue::Utf8(as_str(&args[0])?.to_lowercase())),
        ScalarFunction::Upper => Ok(ScalarValue::Utf8(as_str(&args[0])?.to_uppercase())),
        ScalarFunction::Length | ScalarFunction::CharLength => {
            let n = match &args[0] {
                ScalarValue::Utf8(s) => s.chars().count() as i64,
                ScalarValue::Binary(b) => b.len() as i64,
                other => {
                    return Err(VectraError::TypeMismatch {
                        expected: "Utf8 or Binary".into(),
                        actual: other.logical_type().to_string(),
                    })
                }
            };
            Ok(ScalarValue::Int64(n))
        }
        ScalarFunction::Substring => {
            let s = as_str(&args[0])?;
            let start = as_i64(&args[1])?; // SQL 1-based start
            let chars: Vec<char> = s.chars().collect();
            let len_opt = if args.len() >= 3 {
                Some(as_i64(&args[2])?)
            } else {
                None
            };
            if let Some(l) = len_opt {
                if l < 0 {
                    return Err(VectraError::ExecutionError(
                        "SUBSTRING negative length".into(),
                    ));
                }
            }
            // SQL standard: positions are 1-based. If start < 1, reduce count by (1 - start).
            let (char_start, max_take) = if start >= 1 {
                let s0 = (start - 1) as usize;
                let take = len_opt.map(|l| l as usize).unwrap_or(chars.len());
                (s0, take)
            } else {
                let skip = (1 - start) as i128;
                let take = match len_opt {
                    Some(l) => ((l as i128) - skip).max(0) as usize,
                    None => chars.len(),
                };
                (0usize, take)
            };
            if char_start >= chars.len() || max_take == 0 {
                return Ok(ScalarValue::Utf8(String::new()));
            }
            let end = char_start.saturating_add(max_take).min(chars.len());
            Ok(ScalarValue::Utf8(chars[char_start..end].iter().collect()))
        }
        ScalarFunction::Trim => {
            let s = as_str(&args[0])?;
            if args.len() >= 2 {
                let pat = as_str(&args[1])?;
                let set: Vec<char> = pat.chars().collect();
                Ok(ScalarValue::Utf8(
                    s.trim_matches(set.as_slice()).to_string(),
                ))
            } else {
                Ok(ScalarValue::Utf8(s.trim().to_string()))
            }
        }
        ScalarFunction::Ltrim => {
            let s = as_str(&args[0])?;
            if args.len() >= 2 {
                let set: Vec<char> = as_str(&args[1])?.chars().collect();
                Ok(ScalarValue::Utf8(
                    s.trim_start_matches(set.as_slice()).to_string(),
                ))
            } else {
                Ok(ScalarValue::Utf8(s.trim_start().to_string()))
            }
        }
        ScalarFunction::Rtrim => {
            let s = as_str(&args[0])?;
            if args.len() >= 2 {
                let set: Vec<char> = as_str(&args[1])?.chars().collect();
                Ok(ScalarValue::Utf8(
                    s.trim_end_matches(set.as_slice()).to_string(),
                ))
            } else {
                Ok(ScalarValue::Utf8(s.trim_end().to_string()))
            }
        }
        ScalarFunction::Replace => {
            let s = as_str(&args[0])?;
            let from = as_str(&args[1])?;
            let to = as_str(&args[2])?;
            Ok(ScalarValue::Utf8(s.replace(from, to)))
        }
        ScalarFunction::StartsWith => Ok(ScalarValue::Boolean(
            as_str(&args[0])?.starts_with(as_str(&args[1])?),
        )),
        ScalarFunction::EndsWith => Ok(ScalarValue::Boolean(
            as_str(&args[0])?.ends_with(as_str(&args[1])?),
        )),
        ScalarFunction::Position => {
            let needle = as_str(&args[0])?;
            let hay = as_str(&args[1])?;
            let pos = match hay.find(needle) {
                Some(byte_idx) => hay[..byte_idx].chars().count() as i64 + 1,
                None => 0,
            };
            Ok(ScalarValue::Int64(pos))
        }
        ScalarFunction::Left => {
            let s = as_str(&args[0])?;
            let n = as_i64(&args[1])?;
            let chars: Vec<char> = s.chars().collect();
            let take = if n >= 0 {
                (n as usize).min(chars.len())
            } else {
                chars.len().saturating_sub(n.unsigned_abs() as usize)
            };
            Ok(ScalarValue::Utf8(chars[..take].iter().collect()))
        }
        ScalarFunction::Right => {
            let s = as_str(&args[0])?;
            let n = as_i64(&args[1])?;
            let chars: Vec<char> = s.chars().collect();
            let start = if n >= 0 {
                chars.len().saturating_sub(n as usize)
            } else {
                (n.unsigned_abs() as usize).min(chars.len())
            };
            Ok(ScalarValue::Utf8(chars[start..].iter().collect()))
        }
        ScalarFunction::Abs => match &args[0] {
            ScalarValue::Int8(v) => v
                .checked_abs()
                .map(ScalarValue::Int8)
                .ok_or_else(|| VectraError::ArithmeticOverflow("ABS(i8::MIN)".into())),
            ScalarValue::Int16(v) => v
                .checked_abs()
                .map(ScalarValue::Int16)
                .ok_or_else(|| VectraError::ArithmeticOverflow("ABS(i16::MIN)".into())),
            ScalarValue::Int32(v) => v
                .checked_abs()
                .map(ScalarValue::Int32)
                .ok_or_else(|| VectraError::ArithmeticOverflow("ABS(i32::MIN)".into())),
            ScalarValue::Int64(v) => v
                .checked_abs()
                .map(ScalarValue::Int64)
                .ok_or_else(|| VectraError::ArithmeticOverflow("ABS(i64::MIN)".into())),
            ScalarValue::Float64(v) => Ok(ScalarValue::Float64(v.abs())),
            ScalarValue::Decimal128 {
                value,
                precision,
                scale,
            } => value
                .checked_abs()
                .map(|abs| ScalarValue::Decimal128 {
                    value: abs,
                    precision: *precision,
                    scale: *scale,
                })
                .ok_or_else(|| VectraError::ArithmeticOverflow("ABS(i128::MIN)".into())),
            other => Err(VectraError::TypeMismatch {
                expected: "numeric".into(),
                actual: other.logical_type().to_string(),
            }),
        },
        ScalarFunction::Round => {
            let digits = if args.len() >= 2 {
                as_i64(&args[1])? as i32
            } else {
                0
            };
            match &args[0] {
                ScalarValue::Float64(v) => {
                    let f = 10f64.powi(digits);
                    Ok(ScalarValue::Float64((v * f).round() / f))
                }
                ScalarValue::Decimal128 {
                    value,
                    precision,
                    scale,
                } => {
                    let target_scale = digits.clamp(-38, 38) as i8;
                    let r =
                        vectra_types::decimal_rescale(*value, *scale, target_scale, *precision)?;
                    let back = vectra_types::decimal_rescale(r, target_scale, *scale, *precision)?;
                    Ok(ScalarValue::Decimal128 {
                        value: back,
                        precision: *precision,
                        scale: *scale,
                    })
                }
                other => {
                    let f = as_f64(other)?;
                    let mult = 10f64.powi(digits);
                    let r = (f * mult).round() / mult;
                    if matches!(return_type, LogicalType::Int64) {
                        Ok(ScalarValue::Int64(r as i64))
                    } else {
                        Ok(ScalarValue::Float64(r))
                    }
                }
            }
        }
        ScalarFunction::Ceil => Ok(ScalarValue::Float64(as_f64(&args[0])?.ceil())),
        ScalarFunction::Floor => Ok(ScalarValue::Float64(as_f64(&args[0])?.floor())),
        ScalarFunction::Power => Ok(ScalarValue::Float64(
            as_f64(&args[0])?.powf(as_f64(&args[1])?),
        )),
        ScalarFunction::Sqrt => {
            let v = as_f64(&args[0])?;
            if v < 0.0 {
                return Err(VectraError::ExecutionError(
                    "SQRT of negative number".into(),
                ));
            }
            Ok(ScalarValue::Float64(v.sqrt()))
        }
        ScalarFunction::Extract(part) => {
            let (y, m, d, hh, mm, ss) = match &args[0] {
                ScalarValue::Date32(days) => {
                    let (y, m, d) = civil_from_days(*days);
                    (y, m, d, 0, 0, 0)
                }
                ScalarValue::TimestampMicros(us) => {
                    let (y, m, d, hh, mm, ss, _) = timestamp_parts(*us);
                    (y, m, d, hh, mm, ss)
                }
                other => {
                    return Err(VectraError::TypeMismatch {
                        expected: "Date32 or TimestampMicros".into(),
                        actual: other.logical_type().to_string(),
                    })
                }
            };
            let v = match part {
                DatePart::Year => y as i64,
                DatePart::Quarter => ((m - 1) / 3 + 1) as i64,
                DatePart::Month => m as i64,
                DatePart::Day => d as i64,
                DatePart::Hour => hh as i64,
                DatePart::Minute => mm as i64,
                DatePart::Second => ss as i64,
            };
            Ok(ScalarValue::Int64(v))
        }
        ScalarFunction::DateTrunc(part) => match &args[0] {
            ScalarValue::Date32(days) => {
                let (y, m, d) = civil_from_days(*days);
                let out_days = match part {
                    DatePart::Year => days_from_civil(y, 1, 1),
                    DatePart::Quarter => days_from_civil(y, ((m - 1) / 3) * 3 + 1, 1),
                    DatePart::Month => days_from_civil(y, m, 1),
                    DatePart::Day | DatePart::Hour | DatePart::Minute | DatePart::Second => {
                        days_from_civil(y, m, d)
                    }
                };
                Ok(ScalarValue::Date32(out_days))
            }
            ScalarValue::TimestampMicros(us) => {
                let (y, m, d, hh, mm, ss, _) = timestamp_parts(*us);
                let out_us = match part {
                    DatePart::Year => (days_from_civil(y, 1, 1) as i64) * MICROS_PER_DAY,
                    DatePart::Quarter => {
                        (days_from_civil(y, ((m - 1) / 3) * 3 + 1, 1) as i64) * MICROS_PER_DAY
                    }
                    DatePart::Month => (days_from_civil(y, m, 1) as i64) * MICROS_PER_DAY,
                    DatePart::Day => (days_from_civil(y, m, d) as i64) * MICROS_PER_DAY,
                    DatePart::Hour => {
                        (days_from_civil(y, m, d) as i64) * MICROS_PER_DAY
                            + (hh as i64) * MICROS_PER_HOUR
                    }
                    DatePart::Minute => {
                        (days_from_civil(y, m, d) as i64) * MICROS_PER_DAY
                            + (hh as i64) * MICROS_PER_HOUR
                            + (mm as i64) * MICROS_PER_MINUTE
                    }
                    DatePart::Second => {
                        (days_from_civil(y, m, d) as i64) * MICROS_PER_DAY
                            + (hh as i64) * MICROS_PER_HOUR
                            + (mm as i64) * MICROS_PER_MINUTE
                            + (ss as i64) * MICROS_PER_SECOND
                    }
                };
                Ok(ScalarValue::TimestampMicros(out_us))
            }
            other => Err(VectraError::TypeMismatch {
                expected: "Date32 or TimestampMicros".into(),
                actual: other.logical_type().to_string(),
            }),
        },
        ScalarFunction::DateAddDays => {
            let delta = as_i64(&args[1])?;
            match &args[0] {
                ScalarValue::Date32(d) => {
                    let sum = (*d as i64)
                        .checked_add(delta)
                        .and_then(|v| i32::try_from(v).ok())
                        .ok_or_else(|| VectraError::ArithmeticOverflow("date add days".into()))?;
                    Ok(ScalarValue::Date32(sum))
                }
                ScalarValue::TimestampMicros(us) => {
                    let delta_us = delta
                        .checked_mul(MICROS_PER_DAY)
                        .ok_or_else(|| VectraError::ArithmeticOverflow("ts add days".into()))?;
                    let sum = us
                        .checked_add(delta_us)
                        .ok_or_else(|| VectraError::ArithmeticOverflow("ts add days".into()))?;
                    Ok(ScalarValue::TimestampMicros(sum))
                }
                other => Err(VectraError::TypeMismatch {
                    expected: "Date32 or TimestampMicros".into(),
                    actual: other.logical_type().to_string(),
                }),
            }
        }
        ScalarFunction::DateAddMonths => {
            let delta = as_i64(&args[1])? as i32;
            match &args[0] {
                ScalarValue::Date32(d) => Ok(ScalarValue::Date32(add_months_date32(*d, delta))),
                ScalarValue::TimestampMicros(us) => {
                    let days = us.div_euclid(MICROS_PER_DAY) as i32;
                    let rem = us.rem_euclid(MICROS_PER_DAY);
                    let nd = add_months_date32(days, delta);
                    Ok(ScalarValue::TimestampMicros(
                        (nd as i64) * MICROS_PER_DAY + rem,
                    ))
                }
                other => Err(VectraError::TypeMismatch {
                    expected: "Date32 or TimestampMicros".into(),
                    actual: other.logical_type().to_string(),
                }),
            }
        }
        ScalarFunction::DateAddYears => {
            let delta = (as_i64(&args[1])? as i32).saturating_mul(12);
            match &args[0] {
                ScalarValue::Date32(d) => Ok(ScalarValue::Date32(add_months_date32(*d, delta))),
                ScalarValue::TimestampMicros(us) => {
                    let days = us.div_euclid(MICROS_PER_DAY) as i32;
                    let rem = us.rem_euclid(MICROS_PER_DAY);
                    let nd = add_months_date32(days, delta);
                    Ok(ScalarValue::TimestampMicros(
                        (nd as i64) * MICROS_PER_DAY + rem,
                    ))
                }
                other => Err(VectraError::TypeMismatch {
                    expected: "Date32 or TimestampMicros".into(),
                    actual: other.logical_type().to_string(),
                }),
            }
        }
        ScalarFunction::TimestampAddMicros => {
            let delta = as_i64(&args[1])?;
            match &args[0] {
                ScalarValue::TimestampMicros(us) => {
                    let sum = us
                        .checked_add(delta)
                        .ok_or_else(|| VectraError::ArithmeticOverflow("ts add micros".into()))?;
                    Ok(ScalarValue::TimestampMicros(sum))
                }
                other => Err(VectraError::TypeMismatch {
                    expected: "TimestampMicros".into(),
                    actual: other.logical_type().to_string(),
                }),
            }
        }
        ScalarFunction::DateDiffDays => match (&args[0], &args[1]) {
            (ScalarValue::Date32(a), ScalarValue::Date32(b)) => {
                Ok(ScalarValue::Int64((*a as i64) - (*b as i64)))
            }
            (ScalarValue::TimestampMicros(a), ScalarValue::TimestampMicros(b)) => {
                Ok(ScalarValue::Int64((*a - *b) / MICROS_PER_DAY))
            }
            (a, b) => Err(VectraError::TypeMismatch {
                expected: "matching Date32/TimestampMicros".into(),
                actual: format!("{}, {}", a.logical_type(), b.logical_type()),
            }),
        },
        ScalarFunction::NullIf
        | ScalarFunction::Concat
        | ScalarFunction::ConcatWs
        | ScalarFunction::Greatest
        | ScalarFunction::Least => unreachable!(),
    }
}

fn as_str(v: &ScalarValue) -> Result<&str> {
    match v {
        ScalarValue::Utf8(s) => Ok(s.as_str()),
        other => Err(VectraError::TypeMismatch {
            expected: "Utf8".into(),
            actual: other.logical_type().to_string(),
        }),
    }
}

fn as_i64(v: &ScalarValue) -> Result<i64> {
    match v {
        ScalarValue::Int8(x) => Ok(*x as i64),
        ScalarValue::Int16(x) => Ok(*x as i64),
        ScalarValue::Int32(x) => Ok(*x as i64),
        ScalarValue::Int64(x) => Ok(*x),
        other => Err(VectraError::TypeMismatch {
            expected: "integer".into(),
            actual: other.logical_type().to_string(),
        }),
    }
}

fn as_f64(v: &ScalarValue) -> Result<f64> {
    match v {
        ScalarValue::Int8(x) => Ok(*x as f64),
        ScalarValue::Int16(x) => Ok(*x as f64),
        ScalarValue::Int32(x) => Ok(*x as f64),
        ScalarValue::Int64(x) => Ok(*x as f64),
        ScalarValue::Float64(x) => Ok(*x),
        ScalarValue::Decimal128 { value, scale, .. } => {
            Ok((*value as f64) / 10f64.powi(*scale as i32))
        }
        other => Err(VectraError::TypeMismatch {
            expected: "numeric".into(),
            actual: other.logical_type().to_string(),
        }),
    }
}
