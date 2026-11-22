use vectra_types::{
    decimal_rescale, format_date32, format_timestamp_micros, parse_date32, parse_decimal,
    parse_timestamp_micros, LogicalType, Result, ScalarValue, VectraError, MICROS_PER_DAY,
};

/// Casts a single `ScalarValue` to `target_type`.
/// When `try_cast == true`, conversion failures return `Ok(ScalarValue::Null)` instead of `Err`.
pub fn cast_scalar(
    val: &ScalarValue,
    target_type: LogicalType,
    try_cast: bool,
) -> Result<ScalarValue> {
    if val.is_null() {
        return Ok(ScalarValue::Null);
    }
    if val.logical_type() == target_type {
        return Ok(val.clone());
    }
    let res = cast_scalar_strict(val, target_type);
    match res {
        Ok(v) => Ok(v),
        Err(_) if try_cast => Ok(ScalarValue::Null),
        Err(e) => Err(e),
    }
}

fn bad_cast(val: &ScalarValue, target: LogicalType) -> VectraError {
    VectraError::InvalidCast {
        from: val.logical_type().to_string(),
        to: target.to_string(),
        value: val.to_display_string(),
    }
}

fn cast_scalar_strict(val: &ScalarValue, target: LogicalType) -> Result<ScalarValue> {
    match target {
        LogicalType::Null => Ok(ScalarValue::Null),
        LogicalType::Utf8 => Ok(ScalarValue::Utf8(val.to_display_string())),
        LogicalType::Binary => match val {
            ScalarValue::Utf8(s) => Ok(ScalarValue::Binary(s.as_bytes().to_vec())),
            ScalarValue::Binary(b) => Ok(ScalarValue::Binary(b.clone())),
            _ => Err(bad_cast(val, target)),
        },
        LogicalType::Boolean => match val {
            ScalarValue::Boolean(b) => Ok(ScalarValue::Boolean(*b)),
            ScalarValue::Int8(v) => Ok(ScalarValue::Boolean(*v != 0)),
            ScalarValue::Int16(v) => Ok(ScalarValue::Boolean(*v != 0)),
            ScalarValue::Int32(v) => Ok(ScalarValue::Boolean(*v != 0)),
            ScalarValue::Int64(v) => Ok(ScalarValue::Boolean(*v != 0)),
            ScalarValue::Float64(v) => Ok(ScalarValue::Boolean(*v != 0.0 && !v.is_nan())),
            ScalarValue::Decimal128 { value, .. } => Ok(ScalarValue::Boolean(*value != 0)),
            ScalarValue::Utf8(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" | "y" | "on" => Ok(ScalarValue::Boolean(true)),
                "false" | "f" | "0" | "no" | "n" | "off" => Ok(ScalarValue::Boolean(false)),
                _ => Err(bad_cast(val, target)),
            },
            _ => Err(bad_cast(val, target)),
        },
        LogicalType::Int8 => {
            let v = to_i128_trunc(val, target)?;
            i8::try_from(v)
                .map(ScalarValue::Int8)
                .map_err(|_| bad_cast(val, target))
        }
        LogicalType::Int16 => {
            let v = to_i128_trunc(val, target)?;
            i16::try_from(v)
                .map(ScalarValue::Int16)
                .map_err(|_| bad_cast(val, target))
        }
        LogicalType::Int32 => {
            let v = to_i128_trunc(val, target)?;
            i32::try_from(v)
                .map(ScalarValue::Int32)
                .map_err(|_| bad_cast(val, target))
        }
        LogicalType::Int64 => {
            let v = to_i128_trunc(val, target)?;
            i64::try_from(v)
                .map(ScalarValue::Int64)
                .map_err(|_| bad_cast(val, target))
        }
        LogicalType::Float64 => match val {
            ScalarValue::Boolean(b) => Ok(ScalarValue::Float64(if *b { 1.0 } else { 0.0 })),
            ScalarValue::Int8(v) => Ok(ScalarValue::Float64(*v as f64)),
            ScalarValue::Int16(v) => Ok(ScalarValue::Float64(*v as f64)),
            ScalarValue::Int32(v) => Ok(ScalarValue::Float64(*v as f64)),
            ScalarValue::Int64(v) => Ok(ScalarValue::Float64(*v as f64)),
            ScalarValue::Float64(v) => Ok(ScalarValue::Float64(*v)),
            ScalarValue::Decimal128 { value, scale, .. } => {
                let f = (*value as f64) / 10f64.powi(*scale as i32);
                Ok(ScalarValue::Float64(f))
            }
            ScalarValue::Utf8(s) => s
                .trim()
                .parse::<f64>()
                .map(ScalarValue::Float64)
                .map_err(|_| bad_cast(val, target)),
            _ => Err(bad_cast(val, target)),
        },
        LogicalType::Decimal128 { precision, scale } => match val {
            ScalarValue::Int8(v) => Ok(ScalarValue::Decimal128 {
                value: decimal_rescale(*v as i128, 0, scale, precision)?,
                precision,
                scale,
            }),
            ScalarValue::Int16(v) => Ok(ScalarValue::Decimal128 {
                value: decimal_rescale(*v as i128, 0, scale, precision)?,
                precision,
                scale,
            }),
            ScalarValue::Int32(v) => Ok(ScalarValue::Decimal128 {
                value: decimal_rescale(*v as i128, 0, scale, precision)?,
                precision,
                scale,
            }),
            ScalarValue::Int64(v) => Ok(ScalarValue::Decimal128 {
                value: decimal_rescale(*v as i128, 0, scale, precision)?,
                precision,
                scale,
            }),
            ScalarValue::Decimal128 {
                value,
                scale: from_scale,
                ..
            } => Ok(ScalarValue::Decimal128 {
                value: decimal_rescale(*value, *from_scale, scale, precision)?,
                precision,
                scale,
            }),
            ScalarValue::Float64(v) => {
                if !v.is_finite() {
                    return Err(bad_cast(val, target));
                }
                let scaled = (*v * 10f64.powi(scale as i32)).round();
                if scaled < (i128::MIN as f64) || scaled > (i128::MAX as f64) {
                    return Err(bad_cast(val, target));
                }
                let raw = scaled as i128;
                let checked = vectra_types::validate_decimal_precision(raw, precision)?;
                Ok(ScalarValue::Decimal128 {
                    value: checked,
                    precision,
                    scale,
                })
            }
            ScalarValue::Utf8(s) => {
                let raw = parse_decimal(s, precision, scale)?;
                Ok(ScalarValue::Decimal128 {
                    value: raw,
                    precision,
                    scale,
                })
            }
            _ => Err(bad_cast(val, target)),
        },
        LogicalType::Date32 => match val {
            ScalarValue::Date32(d) => Ok(ScalarValue::Date32(*d)),
            ScalarValue::TimestampMicros(us) => {
                Ok(ScalarValue::Date32(us.div_euclid(MICROS_PER_DAY) as i32))
            }
            ScalarValue::Utf8(s) => Ok(ScalarValue::Date32(parse_date32(s)?)),
            _ => Err(bad_cast(val, target)),
        },
        LogicalType::TimestampMicros => match val {
            ScalarValue::TimestampMicros(us) => Ok(ScalarValue::TimestampMicros(*us)),
            ScalarValue::Date32(d) => {
                Ok(ScalarValue::TimestampMicros((*d as i64) * MICROS_PER_DAY))
            }
            ScalarValue::Utf8(s) => Ok(ScalarValue::TimestampMicros(parse_timestamp_micros(s)?)),
            _ => Err(bad_cast(val, target)),
        },
    }
}

fn to_i128_trunc(val: &ScalarValue, target: LogicalType) -> Result<i128> {
    match val {
        ScalarValue::Boolean(b) => Ok(i128::from(*b)),
        ScalarValue::Int8(v) => Ok(*v as i128),
        ScalarValue::Int16(v) => Ok(*v as i128),
        ScalarValue::Int32(v) => Ok(*v as i128),
        ScalarValue::Int64(v) => Ok(*v as i128),
        ScalarValue::Float64(v) => {
            if !v.is_finite() {
                return Err(bad_cast(val, target));
            }
            let r = v.trunc();
            if r < (i128::MIN as f64) || r > (i128::MAX as f64) {
                return Err(bad_cast(val, target));
            }
            Ok(r as i128)
        }
        ScalarValue::Decimal128 { value, scale, .. } => {
            decimal_rescale(*value, *scale, 0, vectra_types::MAX_DECIMAL_PRECISION)
        }
        ScalarValue::Utf8(s) => {
            let trimmed = s.trim();
            if let Ok(i) = trimmed.parse::<i128>() {
                Ok(i)
            } else if let Ok(f) = trimmed.parse::<f64>() {
                if !f.is_finite() {
                    return Err(bad_cast(val, target));
                }
                Ok(f.trunc() as i128)
            } else {
                Err(bad_cast(val, target))
            }
        }
        ScalarValue::Date32(d) => Ok(*d as i128),
        ScalarValue::TimestampMicros(us) => Ok(*us as i128),
        _ => Err(bad_cast(val, target)),
    }
}

/// Ensures date formatting helper is linked cleanly.
pub fn format_scalar_for_debug(val: &ScalarValue) -> String {
    match val {
        ScalarValue::Date32(d) => format_date32(*d),
        ScalarValue::TimestampMicros(us) => format_timestamp_micros(*us),
        other => other.to_display_string(),
    }
}
