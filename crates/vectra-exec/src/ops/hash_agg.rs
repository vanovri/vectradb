use crate::context::ExecutionContext;
use crate::spill::{SpillFile, SpillWriter};
use crate::stream::{
    chunk_from_scalar_rows, eval_and_hash_keys, hash_scalars, read_scalar_bin, scalar_cmp_total,
    scalar_heap_bytes, write_scalar_bin, CanonicalKey, CanonicalScalar, ChunkStream,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use vectra_expr::{cast_scalar, eval_expr, PhysicalExpr};
use vectra_optimizer::{HyperLogLog, PhysicalAggExpr};
use vectra_sql::AggregateFunc;
use vectra_types::{
    decimal_add, decimal_div, decimal_rescale, DataChunk, LogicalType, MemoryPoolExt,
    MemoryReservation, Result, ScalarValue, VectraError, VECTOR_CAPACITY,
};

const NUM_SPILL_PARTITIONS: usize = 64;
const SPILL_STAGING_TARGET: usize = 16 * 1024;
const MAX_SPILL_DEPTH: usize = 3;

/// Serializable, mergeable accumulator state supporting all 12 SQL aggregate functions,
/// `DISTINCT` modifiers, Welford online variance with Chan merge, and `HyperLogLog`.
#[derive(Debug, Clone)]
pub enum AggState {
    Count(i64),
    SumInt {
        sum: i128,
        has_val: bool,
    },
    SumFloat {
        sum: f64,
        has_val: bool,
    },
    SumDecimal {
        sum: i128,
        precision: u8,
        scale: i8,
        has_val: bool,
    },
    AvgFloat {
        sum: f64,
        count: i64,
    },
    AvgDecimal {
        sum: i128,
        count: i64,
        precision: u8,
        scale: i8,
    },
    Min(Option<ScalarValue>),
    Max(Option<ScalarValue>),
    /// Welford online mean/M2 state with exact Chan parallel merge formula.
    Welford {
        count: u64,
        mean: f64,
        m2: f64,
        is_pop: bool,
    },
    BoolAnd(Option<bool>),
    BoolOr(Option<bool>),
    StringAgg {
        parts: Vec<String>,
        sep: String,
    },
    ApproxDistinct(HyperLogLog),
    /// Exact `DISTINCT` wrapper supporting `COUNT/SUM/AVG/MIN/MAX/STRING_AGG(DISTINCT x)`.
    Distinct {
        func: AggregateFunc,
        return_type: LogicalType,
        sep: String,
        seen: HashSet<CanonicalScalar>,
    },
}

impl AggState {
    pub fn new(spec: &PhysicalAggExpr) -> Self {
        if spec.distinct {
            return Self::Distinct {
                func: spec.func,
                return_type: spec.return_type,
                sep: ",".to_string(),
                seen: HashSet::new(),
            };
        }
        Self::new_non_distinct(spec.func, spec.return_type)
    }

    pub fn new_non_distinct(func: AggregateFunc, return_type: LogicalType) -> Self {
        match func {
            AggregateFunc::CountStar | AggregateFunc::Count => Self::Count(0),
            AggregateFunc::Sum => match return_type {
                LogicalType::Decimal128 { precision, scale } => Self::SumDecimal {
                    sum: 0,
                    precision,
                    scale,
                    has_val: false,
                },
                LogicalType::Float64 => Self::SumFloat {
                    sum: 0.0,
                    has_val: false,
                },
                _ => Self::SumInt {
                    sum: 0,
                    has_val: false,
                },
            },
            AggregateFunc::Avg => match return_type {
                LogicalType::Decimal128 { precision, scale } => Self::AvgDecimal {
                    sum: 0,
                    count: 0,
                    precision,
                    scale,
                },
                _ => Self::AvgFloat { sum: 0.0, count: 0 },
            },
            AggregateFunc::Min => Self::Min(None),
            AggregateFunc::Max => Self::Max(None),
            AggregateFunc::StddevSamp => Self::Welford {
                count: 0,
                mean: 0.0,
                m2: 0.0,
                is_pop: false,
            },
            AggregateFunc::StddevPop => Self::Welford {
                count: 0,
                mean: 0.0,
                m2: 0.0,
                is_pop: true,
            },
            AggregateFunc::BoolAnd => Self::BoolAnd(None),
            AggregateFunc::BoolOr => Self::BoolOr(None),
            AggregateFunc::StringAgg => Self::StringAgg {
                parts: Vec::new(),
                sep: ",".to_string(),
            },
            AggregateFunc::ApproxCountDistinct => Self::ApproxDistinct(HyperLogLog::new()),
        }
    }

    /// Updates state with one row's argument values, returning estimated incremental heap bytes added.
    pub fn update(&mut self, args: &[ScalarValue]) -> Result<usize> {
        match self {
            Self::Count(c) => {
                if args.is_empty() || !args[0].is_null() {
                    *c += 1;
                }
                Ok(0)
            }
            Self::SumInt { sum, has_val } => {
                if let Some(v) = args.first() {
                    if let Some(n) = scalar_to_i128(v) {
                        *sum = sum.checked_add(n).ok_or_else(|| {
                            VectraError::ArithmeticOverflow("SUM overflow".into())
                        })?;
                        *has_val = true;
                    }
                }
                Ok(0)
            }
            Self::SumFloat { sum, has_val } => {
                if let Some(v) = args.first() {
                    if let Some(f) = scalar_to_f64(v) {
                        *sum += f;
                        *has_val = true;
                    }
                }
                Ok(0)
            }
            Self::SumDecimal {
                sum,
                precision,
                scale,
                has_val,
            } => {
                if let Some(v) = args.first() {
                    if !v.is_null() {
                        let d = match v {
                            ScalarValue::Decimal128 {
                                value, scale: vs, ..
                            } => {
                                if *vs == *scale {
                                    *value
                                } else {
                                    decimal_rescale(*value, *vs, *scale, *precision)?
                                }
                            }
                            other => {
                                let casted = cast_scalar(
                                    other,
                                    LogicalType::Decimal128 {
                                        precision: *precision,
                                        scale: *scale,
                                    },
                                    false,
                                )?;
                                match casted {
                                    ScalarValue::Decimal128 { value, .. } => value,
                                    _ => 0,
                                }
                            }
                        };
                        *sum = decimal_add(*sum, *scale, d, *scale, *precision, *scale)?;
                        *has_val = true;
                    }
                }
                Ok(0)
            }
            Self::AvgFloat { sum, count } => {
                if let Some(v) = args.first() {
                    if let Some(f) = scalar_to_f64(v) {
                        *sum += f;
                        *count += 1;
                    }
                }
                Ok(0)
            }
            Self::AvgDecimal {
                sum,
                count,
                precision,
                scale,
            } => {
                if let Some(v) = args.first() {
                    if !v.is_null() {
                        let d = match v {
                            ScalarValue::Decimal128 {
                                value, scale: vs, ..
                            } => {
                                if *vs == *scale {
                                    *value
                                } else {
                                    decimal_rescale(*value, *vs, *scale, *precision)?
                                }
                            }
                            other => {
                                let casted = cast_scalar(
                                    other,
                                    LogicalType::Decimal128 {
                                        precision: *precision,
                                        scale: *scale,
                                    },
                                    false,
                                )?;
                                match casted {
                                    ScalarValue::Decimal128 { value, .. } => value,
                                    _ => 0,
                                }
                            }
                        };
                        *sum = decimal_add(*sum, *scale, d, *scale, 38, *scale)?;
                        *count += 1;
                    }
                }
                Ok(0)
            }
            Self::Min(cur) => {
                if let Some(v) = args.first() {
                    if !v.is_null() {
                        let replace = match cur {
                            None => true,
                            Some(c) => scalar_cmp_total(v, c) == Ordering::Less,
                        };
                        if replace {
                            let added = scalar_heap_bytes(v);
                            *cur = Some(v.clone());
                            return Ok(added);
                        }
                    }
                }
                Ok(0)
            }
            Self::Max(cur) => {
                if let Some(v) = args.first() {
                    if !v.is_null() {
                        let replace = match cur {
                            None => true,
                            Some(c) => scalar_cmp_total(v, c) == Ordering::Greater,
                        };
                        if replace {
                            let added = scalar_heap_bytes(v);
                            *cur = Some(v.clone());
                            return Ok(added);
                        }
                    }
                }
                Ok(0)
            }
            Self::Welford {
                count, mean, m2, ..
            } => {
                if let Some(v) = args.first() {
                    if let Some(x) = scalar_to_f64(v) {
                        *count += 1;
                        let delta = x - *mean;
                        *mean += delta / (*count as f64);
                        let delta2 = x - *mean;
                        *m2 += delta * delta2;
                    }
                }
                Ok(0)
            }
            Self::BoolAnd(cur) => {
                if let Some(ScalarValue::Boolean(b)) = args.first() {
                    *cur = Some(cur.unwrap_or(true) && *b);
                }
                Ok(0)
            }
            Self::BoolOr(cur) => {
                if let Some(ScalarValue::Boolean(b)) = args.first() {
                    *cur = Some(cur.unwrap_or(false) || *b);
                }
                Ok(0)
            }
            Self::StringAgg { parts, sep } => {
                if let Some(v) = args.first() {
                    if !v.is_null() {
                        if let Some(ScalarValue::Utf8(s)) = args.get(1) {
                            *sep = s.clone();
                        }
                        let s = match v {
                            ScalarValue::Utf8(s) => s.clone(),
                            other => other.to_display_string(),
                        };
                        let added = 24 + s.len();
                        parts.push(s);
                        return Ok(added);
                    }
                }
                Ok(0)
            }
            Self::ApproxDistinct(hll) => {
                if let Some(v) = args.first() {
                    if !v.is_null() {
                        hll.insert_hash(v.canonical_hash());
                    }
                }
                Ok(0)
            }
            Self::Distinct { sep, seen, .. } => {
                if let Some(v) = args.first() {
                    if !v.is_null() {
                        if let Some(ScalarValue::Utf8(s)) = args.get(1) {
                            *sep = s.clone();
                        }
                        let added = scalar_heap_bytes(v) + 16;
                        if seen.insert(CanonicalScalar(v.clone())) {
                            return Ok(added);
                        }
                    }
                }
                Ok(0)
            }
        }
    }

    /// Merges another partial `AggState` of the same kind into `self` (used by out-of-core spill
    /// merge AND `ops/window.rs` Segment-Tree monoid combination!).
    pub fn merge_from(&mut self, other: Self) -> Result<usize> {
        match (self, other) {
            (Self::Count(a), Self::Count(b)) => {
                *a += b;
                Ok(0)
            }
            (
                Self::SumInt {
                    sum: sa,
                    has_val: ha,
                },
                Self::SumInt {
                    sum: sb,
                    has_val: hb,
                },
            ) => {
                if hb {
                    *sa = sa
                        .checked_add(sb)
                        .ok_or_else(|| VectraError::ArithmeticOverflow("SUM overflow".into()))?;
                    *ha = true;
                }
                Ok(0)
            }
            (
                Self::SumFloat {
                    sum: sa,
                    has_val: ha,
                },
                Self::SumFloat {
                    sum: sb,
                    has_val: hb,
                },
            ) => {
                if hb {
                    *sa += sb;
                    *ha = true;
                }
                Ok(0)
            }
            (
                Self::SumDecimal {
                    sum: sa,
                    precision,
                    scale,
                    has_val: ha,
                },
                Self::SumDecimal {
                    sum: sb,
                    has_val: hb,
                    ..
                },
            ) => {
                if hb {
                    *sa = decimal_add(*sa, *scale, sb, *scale, *precision, *scale)?;
                    *ha = true;
                }
                Ok(0)
            }
            (Self::AvgFloat { sum: sa, count: ca }, Self::AvgFloat { sum: sb, count: cb }) => {
                *sa += sb;
                *ca += cb;
                Ok(0)
            }
            (
                Self::AvgDecimal {
                    sum: sa,
                    count: ca,
                    scale,
                    ..
                },
                Self::AvgDecimal {
                    sum: sb, count: cb, ..
                },
            ) => {
                *sa = decimal_add(*sa, *scale, sb, *scale, 38, *scale)?;
                *ca += cb;
                Ok(0)
            }
            (Self::Min(a), Self::Min(b)) => {
                if let Some(bv) = b {
                    let take = match a {
                        None => true,
                        Some(av) => scalar_cmp_total(&bv, av) == Ordering::Less,
                    };
                    if take {
                        let added = scalar_heap_bytes(&bv);
                        *a = Some(bv);
                        return Ok(added);
                    }
                }
                Ok(0)
            }
            (Self::Max(a), Self::Max(b)) => {
                if let Some(bv) = b {
                    let take = match a {
                        None => true,
                        Some(av) => scalar_cmp_total(&bv, av) == Ordering::Greater,
                    };
                    if take {
                        let added = scalar_heap_bytes(&bv);
                        *a = Some(bv);
                        return Ok(added);
                    }
                }
                Ok(0)
            }
            (
                Self::Welford {
                    count: ca,
                    mean: ma,
                    m2: m2a,
                    ..
                },
                Self::Welford {
                    count: cb,
                    mean: mb,
                    m2: m2b,
                    ..
                },
            ) => {
                if cb == 0 {
                    return Ok(0);
                }
                if *ca == 0 {
                    *ca = cb;
                    *ma = mb;
                    *m2a = m2b;
                    return Ok(0);
                }
                // Chan's parallel online variance combination formula.
                let n = *ca + cb;
                let delta = mb - *ma;
                *ma += delta * (cb as f64) / (n as f64);
                *m2a += m2b + delta * delta * (*ca as f64) * (cb as f64) / (n as f64);
                *ca = n;
                Ok(0)
            }
            (Self::BoolAnd(a), Self::BoolAnd(b)) => {
                if let Some(bv) = b {
                    *a = Some(a.unwrap_or(true) && bv);
                }
                Ok(0)
            }
            (Self::BoolOr(a), Self::BoolOr(b)) => {
                if let Some(bv) = b {
                    *a = Some(a.unwrap_or(false) || bv);
                }
                Ok(0)
            }
            (Self::StringAgg { parts: pa, sep: sa }, Self::StringAgg { parts: pb, sep: sb }) => {
                let mut added = 0usize;
                if !pb.is_empty() {
                    *sa = sb;
                }
                for s in pb {
                    added += 24 + s.len();
                    pa.push(s);
                }
                Ok(added)
            }
            (Self::ApproxDistinct(a), Self::ApproxDistinct(b)) => {
                a.merge(&b);
                Ok(0)
            }
            (
                Self::Distinct {
                    seen: sa,
                    sep: sepa,
                    ..
                },
                Self::Distinct {
                    seen: sb,
                    sep: sepb,
                    ..
                },
            ) => {
                let mut added = 0usize;
                if !sb.is_empty() {
                    *sepa = sepb;
                }
                for v in sb {
                    let b = scalar_heap_bytes(&v.0) + 16;
                    if sa.insert(v) {
                        added += b;
                    }
                }
                Ok(added)
            }
            _ => Err(VectraError::Internal(
                "mismatched AggState variants during merge".into(),
            )),
        }
    }

    pub fn finalize(&self, return_type: LogicalType) -> Result<ScalarValue> {
        match self {
            Self::Count(c) => Ok(ScalarValue::Int64(*c)),
            Self::SumInt { sum, has_val } => {
                if !*has_val {
                    return Ok(ScalarValue::Null);
                }
                match return_type {
                    LogicalType::Int64 => i64::try_from(*sum)
                        .map(ScalarValue::Int64)
                        .map_err(|_| VectraError::ArithmeticOverflow("SUM Int64 overflow".into())),
                    LogicalType::Float64 => Ok(ScalarValue::Float64(*sum as f64)),
                    LogicalType::Decimal128 { precision, scale } => {
                        let scaled = decimal_rescale(*sum, 0, scale, precision)?;
                        Ok(ScalarValue::Decimal128 {
                            value: scaled,
                            precision,
                            scale,
                        })
                    }
                    _ => Ok(ScalarValue::Int64(*sum as i64)),
                }
            }
            Self::SumFloat { sum, has_val } => {
                if !*has_val {
                    Ok(ScalarValue::Null)
                } else {
                    Ok(ScalarValue::Float64(*sum))
                }
            }
            Self::SumDecimal {
                sum,
                precision,
                scale,
                has_val,
            } => {
                if !*has_val {
                    Ok(ScalarValue::Null)
                } else {
                    Ok(ScalarValue::Decimal128 {
                        value: *sum,
                        precision: *precision,
                        scale: *scale,
                    })
                }
            }
            Self::AvgFloat { sum, count } => {
                if *count == 0 {
                    Ok(ScalarValue::Null)
                } else {
                    Ok(ScalarValue::Float64(*sum / (*count as f64)))
                }
            }
            Self::AvgDecimal {
                sum,
                count,
                precision,
                scale,
            } => {
                if *count == 0 {
                    Ok(ScalarValue::Null)
                } else {
                    let denom = i128::from(*count);
                    let res = decimal_div(*sum, *scale, denom, 0, *precision, *scale)?;
                    Ok(ScalarValue::Decimal128 {
                        value: res,
                        precision: *precision,
                        scale: *scale,
                    })
                }
            }
            Self::Min(v) | Self::Max(v) => Ok(v.clone().unwrap_or(ScalarValue::Null)),
            Self::Welford {
                count, m2, is_pop, ..
            } => {
                if *is_pop {
                    if *count == 0 {
                        Ok(ScalarValue::Null)
                    } else {
                        Ok(ScalarValue::Float64(
                            (*m2 / (*count as f64)).max(0.0).sqrt(),
                        ))
                    }
                } else if *count < 2 {
                    Ok(ScalarValue::Null)
                } else {
                    Ok(ScalarValue::Float64(
                        (*m2 / ((*count - 1) as f64)).max(0.0).sqrt(),
                    ))
                }
            }
            Self::BoolAnd(v) | Self::BoolOr(v) => {
                Ok(v.map_or(ScalarValue::Null, ScalarValue::Boolean))
            }
            Self::StringAgg { parts, sep } => {
                if parts.is_empty() {
                    Ok(ScalarValue::Null)
                } else {
                    Ok(ScalarValue::Utf8(parts.join(sep)))
                }
            }
            Self::ApproxDistinct(hll) => Ok(ScalarValue::Int64(hll.estimate().round() as i64)),
            Self::Distinct {
                func,
                return_type: rt,
                sep,
                seen,
            } => {
                if *func == AggregateFunc::Count || *func == AggregateFunc::CountStar {
                    return Ok(ScalarValue::Int64(seen.len() as i64));
                }
                let mut inner = Self::new_non_distinct(*func, *rt);
                if let Self::StringAgg { sep: s, .. } = &mut inner {
                    *s = sep.clone();
                }
                let mut sorted_vals: Vec<&ScalarValue> = seen.iter().map(|c| &c.0).collect();
                if *func == AggregateFunc::StringAgg {
                    sorted_vals.sort_by(|a, b| scalar_cmp_total(a, b));
                }
                for v in sorted_vals {
                    inner.update(std::slice::from_ref(v))?;
                }
                inner.finalize(*rt)
            }
        }
    }

    pub fn serialize_bin(&self, out: &mut Vec<u8>) {
        match self {
            Self::Count(c) => {
                out.push(0);
                out.extend_from_slice(&c.to_le_bytes());
            }
            Self::SumInt { sum, has_val } => {
                out.push(1);
                out.push(u8::from(*has_val));
                out.extend_from_slice(&sum.to_le_bytes());
            }
            Self::SumFloat { sum, has_val } => {
                out.push(2);
                out.push(u8::from(*has_val));
                out.extend_from_slice(&sum.to_bits().to_le_bytes());
            }
            Self::SumDecimal {
                sum,
                precision,
                scale,
                has_val,
            } => {
                out.push(3);
                out.push(u8::from(*has_val));
                out.push(*precision);
                out.push(*scale as u8);
                out.extend_from_slice(&sum.to_le_bytes());
            }
            Self::AvgFloat { sum, count } => {
                out.push(4);
                out.extend_from_slice(&sum.to_bits().to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
            }
            Self::AvgDecimal {
                sum,
                count,
                precision,
                scale,
            } => {
                out.push(5);
                out.push(*precision);
                out.push(*scale as u8);
                out.extend_from_slice(&sum.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
            }
            Self::Min(v) => {
                out.push(6);
                write_scalar_bin(v.as_ref().unwrap_or(&ScalarValue::Null), out);
            }
            Self::Max(v) => {
                out.push(7);
                write_scalar_bin(v.as_ref().unwrap_or(&ScalarValue::Null), out);
            }
            Self::Welford {
                count,
                mean,
                m2,
                is_pop,
            } => {
                out.push(8);
                out.push(u8::from(*is_pop));
                out.extend_from_slice(&count.to_le_bytes());
                out.extend_from_slice(&mean.to_bits().to_le_bytes());
                out.extend_from_slice(&m2.to_bits().to_le_bytes());
            }
            Self::BoolAnd(v) => {
                out.push(9);
                out.push(match v {
                    None => 0,
                    Some(false) => 1,
                    Some(true) => 2,
                });
            }
            Self::BoolOr(v) => {
                out.push(10);
                out.push(match v {
                    None => 0,
                    Some(false) => 1,
                    Some(true) => 2,
                });
            }
            Self::StringAgg { parts, sep } => {
                out.push(11);
                out.extend_from_slice(&(sep.len() as u32).to_le_bytes());
                out.extend_from_slice(sep.as_bytes());
                out.extend_from_slice(&(parts.len() as u32).to_le_bytes());
                for p in parts {
                    out.extend_from_slice(&(p.len() as u32).to_le_bytes());
                    out.extend_from_slice(p.as_bytes());
                }
            }
            Self::ApproxDistinct(hll) => {
                out.push(12);
                let bytes = serde_json::to_vec(hll).unwrap_or_default();
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(&bytes);
            }
            Self::Distinct { sep, seen, .. } => {
                out.push(13);
                out.extend_from_slice(&(sep.len() as u32).to_le_bytes());
                out.extend_from_slice(sep.as_bytes());
                out.extend_from_slice(&(seen.len() as u32).to_le_bytes());
                for item in seen {
                    write_scalar_bin(&item.0, out);
                }
            }
        }
    }

    pub fn deserialize_bin(buf: &[u8], pos: &mut usize, spec: &PhysicalAggExpr) -> Result<Self> {
        if *pos >= buf.len() {
            return Err(VectraError::CorruptFormat("EOF reading AggState".into()));
        }
        let tag = buf[*pos];
        *pos += 1;
        let take = |n: usize, pos: &mut usize| -> Result<&[u8]> {
            if *pos + n > buf.len() {
                Err(VectraError::CorruptFormat("truncated AggState".into()))
            } else {
                let s = &buf[*pos..*pos + n];
                *pos += n;
                Ok(s)
            }
        };
        match tag {
            0 => Ok(Self::Count(i64::from_le_bytes(
                take(8, pos)?.try_into().unwrap(),
            ))),
            1 => {
                let hv = take(1, pos)?[0] != 0;
                let s = i128::from_le_bytes(take(16, pos)?.try_into().unwrap());
                Ok(Self::SumInt {
                    sum: s,
                    has_val: hv,
                })
            }
            2 => {
                let hv = take(1, pos)?[0] != 0;
                let s = f64::from_bits(u64::from_le_bytes(take(8, pos)?.try_into().unwrap()));
                Ok(Self::SumFloat {
                    sum: s,
                    has_val: hv,
                })
            }
            3 => {
                let hv = take(1, pos)?[0] != 0;
                let p = take(1, pos)?[0];
                let sc = take(1, pos)?[0] as i8;
                let s = i128::from_le_bytes(take(16, pos)?.try_into().unwrap());
                Ok(Self::SumDecimal {
                    sum: s,
                    precision: p,
                    scale: sc,
                    has_val: hv,
                })
            }
            4 => {
                let s = f64::from_bits(u64::from_le_bytes(take(8, pos)?.try_into().unwrap()));
                let c = i64::from_le_bytes(take(8, pos)?.try_into().unwrap());
                Ok(Self::AvgFloat { sum: s, count: c })
            }
            5 => {
                let p = take(1, pos)?[0];
                let sc = take(1, pos)?[0] as i8;
                let s = i128::from_le_bytes(take(16, pos)?.try_into().unwrap());
                let c = i64::from_le_bytes(take(8, pos)?.try_into().unwrap());
                Ok(Self::AvgDecimal {
                    sum: s,
                    count: c,
                    precision: p,
                    scale: sc,
                })
            }
            6 => {
                let v = read_scalar_bin(buf, pos)?;
                Ok(Self::Min(if v.is_null() { None } else { Some(v) }))
            }
            7 => {
                let v = read_scalar_bin(buf, pos)?;
                Ok(Self::Max(if v.is_null() { None } else { Some(v) }))
            }
            8 => {
                let is_pop = take(1, pos)?[0] != 0;
                let count = u64::from_le_bytes(take(8, pos)?.try_into().unwrap());
                let mean = f64::from_bits(u64::from_le_bytes(take(8, pos)?.try_into().unwrap()));
                let m2 = f64::from_bits(u64::from_le_bytes(take(8, pos)?.try_into().unwrap()));
                Ok(Self::Welford {
                    count,
                    mean,
                    m2,
                    is_pop,
                })
            }
            9 => {
                let b = take(1, pos)?[0];
                Ok(Self::BoolAnd(match b {
                    1 => Some(false),
                    2 => Some(true),
                    _ => None,
                }))
            }
            10 => {
                let b = take(1, pos)?[0];
                Ok(Self::BoolOr(match b {
                    1 => Some(false),
                    2 => Some(true),
                    _ => None,
                }))
            }
            11 => {
                let slen = u32::from_le_bytes(take(4, pos)?.try_into().unwrap()) as usize;
                let sep = String::from_utf8_lossy(take(slen, pos)?).into_owned();
                let plen = u32::from_le_bytes(take(4, pos)?.try_into().unwrap()) as usize;
                let mut parts = Vec::with_capacity(plen);
                for _ in 0..plen {
                    let l = u32::from_le_bytes(take(4, pos)?.try_into().unwrap()) as usize;
                    parts.push(String::from_utf8_lossy(take(l, pos)?).into_owned());
                }
                Ok(Self::StringAgg { parts, sep })
            }
            12 => {
                let len = u32::from_le_bytes(take(4, pos)?.try_into().unwrap()) as usize;
                let hll: HyperLogLog = serde_json::from_slice(take(len, pos)?)
                    .map_err(|e| VectraError::CorruptFormat(format!("HLL decode: {e}")))?;
                Ok(Self::ApproxDistinct(hll))
            }
            13 => {
                let slen = u32::from_le_bytes(take(4, pos)?.try_into().unwrap()) as usize;
                let sep = String::from_utf8_lossy(take(slen, pos)?).into_owned();
                let n = u32::from_le_bytes(take(4, pos)?.try_into().unwrap()) as usize;
                let mut seen = HashSet::with_capacity(n);
                for _ in 0..n {
                    seen.insert(CanonicalScalar(read_scalar_bin(buf, pos)?));
                }
                Ok(Self::Distinct {
                    func: spec.func,
                    return_type: spec.return_type,
                    sep,
                    seen,
                })
            }
            other => Err(VectraError::CorruptFormat(format!(
                "unknown AggState tag {other}"
            ))),
        }
    }
}

#[inline]
fn scalar_to_i128(v: &ScalarValue) -> Option<i128> {
    match v {
        ScalarValue::Int8(x) => Some(i128::from(*x)),
        ScalarValue::Int16(x) => Some(i128::from(*x)),
        ScalarValue::Int32(x) => Some(i128::from(*x)),
        ScalarValue::Int64(x) => Some(i128::from(*x)),
        _ => None,
    }
}

#[inline]
fn scalar_to_f64(v: &ScalarValue) -> Option<f64> {
    match v {
        ScalarValue::Int8(x) => Some(f64::from(*x)),
        ScalarValue::Int16(x) => Some(f64::from(*x)),
        ScalarValue::Int32(x) => Some(f64::from(*x)),
        ScalarValue::Int64(x) => Some(*x as f64),
        ScalarValue::Float64(x) => Some(*x),
        ScalarValue::Decimal128 { value, scale, .. } => {
            Some((*value as f64) / 10f64.powi(i32::from(*scale)))
        }
        _ => None,
    }
}

/// Executes `PhysicalOp::HashAggregate` with global fast path, `GROUPING SETS`/`ROLLUP`/`CUBE`,
/// and adaptive 64-partition out-of-core partial-state spilling!
#[allow(clippy::too_many_arguments)]
pub fn exec_hash_aggregate(
    plan_id: usize,
    input: ChunkStream,
    group_by: Vec<PhysicalExpr>,
    grouping_sets: Option<Vec<Vec<usize>>>,
    has_grouping_id: bool,
    aggregates: Vec<PhysicalAggExpr>,
    output_types: Vec<LogicalType>,
    ctx: ExecutionContext,
) -> Result<ChunkStream> {
    // Global aggregation fast path (no GROUP BY and no grouping sets): always emits exactly 1 row!
    if group_by.is_empty() && grouping_sets.is_none() {
        let mut states: Vec<AggState> = aggregates.iter().map(AggState::new).collect();
        let mut arg_buf = Vec::new();
        for chunk_res in input {
            ctx.check_cancelled()?;
            let chunk = chunk_res?;
            let n = chunk.len();
            if n == 0 {
                continue;
            }
            ctx.metrics.record_input(plan_id, n as u64);

            let (agg_args, agg_filters) = eval_agg_inputs(&chunk, &aggregates, &ctx)?;
            for r in 0..n {
                for (a_idx, state) in states.iter_mut().enumerate() {
                    if let Some(fv) = &agg_filters[a_idx] {
                        if !matches!(fv.get_scalar(r), ScalarValue::Boolean(true)) {
                            continue;
                        }
                    }
                    arg_buf.clear();
                    for av in &agg_args[a_idx] {
                        arg_buf.push(av.get_scalar(r));
                    }
                    state.update(&arg_buf)?;
                }
            }
        }
        let mut row = Vec::with_capacity(aggregates.len());
        for (a_idx, st) in states.iter().enumerate() {
            row.push(st.finalize(aggregates[a_idx].return_type)?);
        }
        let chunk = chunk_from_scalar_rows(&output_types, &[row], &ctx.pool)?;
        return Ok(Box::new(std::iter::once(Ok(chunk))));
    }

    // Determine effective grouping sets (default = single set `[0..group_by.len()]`).
    let sets: Vec<Vec<usize>> =
        grouping_sets.unwrap_or_else(|| vec![(0..group_by.len()).collect()]);
    let num_gb = group_by.len();
    let agg_budget = ctx.operator_budget(30, 100);

    // Key format when `has_grouping_id || sets.len() > 1`: `[group_col_0..group_col_{k-1}, grouping_id_i64]`
    let include_gid_in_key = has_grouping_id || sets.len() > 1;
    let base_entry_bytes =
        96 + (num_gb + usize::from(include_gid_in_key)) * 28 + aggregates.len() * 48;

    let mut map: HashMap<CanonicalKey, Vec<AggState>> = HashMap::new();
    let mut mem_res: MemoryReservation = ctx.pool.reserve(0)?;
    let mut spill_parts: Option<PartitionSpillWriters> = None;
    let mut arg_buf = Vec::new();
    let mut key_buf = Vec::with_capacity(num_gb + 1);

    for chunk_res in input {
        ctx.check_cancelled()?;
        let chunk = chunk_res?;
        let n = chunk.len();
        if n == 0 {
            continue;
        }
        ctx.metrics.record_input(plan_id, n as u64);

        let (gb_vecs, _hashes) = eval_and_hash_keys(&chunk, &group_by, &ctx.pool)?;
        let (agg_args, agg_filters) = eval_agg_inputs(&chunk, &aggregates, &ctx)?;

        for r in 0..n {
            for set in &sets {
                let mut gid: i64 = 0;
                key_buf.clear();
                for (col_i, gv) in gb_vecs.iter().enumerate().take(num_gb) {
                    if set.contains(&col_i) {
                        key_buf.push(gv.get_scalar(r));
                    } else {
                        gid |= 1i64 << (num_gb - 1 - col_i);
                        key_buf.push(ScalarValue::Null);
                    }
                }
                if include_gid_in_key {
                    key_buf.push(ScalarValue::Int64(gid));
                }

                let key = CanonicalKey(key_buf.clone());
                let mut added_bytes = 0usize;
                let entry_states = if let Some(existing) = map.get_mut(&key) {
                    existing
                } else {
                    added_bytes += base_entry_bytes;
                    for kv in &key.0 {
                        if let ScalarValue::Utf8(s) = kv {
                            added_bytes += s.len();
                        } else if let ScalarValue::Binary(b) = kv {
                            added_bytes += b.len();
                        }
                    }
                    let init: Vec<AggState> = aggregates.iter().map(AggState::new).collect();
                    map.entry(key).or_insert(init)
                };

                for (a_idx, state) in entry_states.iter_mut().enumerate() {
                    if let Some(fv) = &agg_filters[a_idx] {
                        if !matches!(fv.get_scalar(r), ScalarValue::Boolean(true)) {
                            continue;
                        }
                    }
                    arg_buf.clear();
                    for av in &agg_args[a_idx] {
                        arg_buf.push(av.get_scalar(r));
                    }
                    added_bytes += state.update(&arg_buf)?;
                }

                if added_bytes > 0 {
                    let over_budget = mem_res.bytes() + added_bytes >= agg_budget;
                    let grow_failed = over_budget || mem_res.try_grow(added_bytes).is_err();
                    if grow_failed {
                        let writers = match &mut spill_parts {
                            Some(w) => w,
                            None => {
                                spill_parts = Some(PartitionSpillWriters::new(
                                    plan_id,
                                    NUM_SPILL_PARTITIONS,
                                    &ctx,
                                )?);
                                spill_parts.as_mut().unwrap()
                            }
                        };
                        flush_agg_map_to_partitions(&mut map, writers, 0)?;
                        mem_res.shrink(mem_res.bytes());
                    }
                }
            }
        }
        ctx.metrics
            .record_peak_memory(plan_id, ctx.pool.peak_bytes());
    }

    // Fast path: 0 spills! Emit directly from in-memory map in 2,048-row chunks.
    if spill_parts.is_none() {
        let mut iter_entries: Vec<(Vec<ScalarValue>, Vec<AggState>)> =
            map.into_iter().map(|(k, s)| (k.0, s)).collect();
        drop(mem_res);
        // Handle edge case: grouping sets with empty input & an empty grouping set `()` -> emit 1 row per empty set.
        if iter_entries.is_empty() && sets.iter().any(|s| s.is_empty()) {
            for set in &sets {
                if set.is_empty() {
                    let mut k = vec![ScalarValue::Null; num_gb];
                    let gid = if num_gb > 0 { (1i64 << num_gb) - 1 } else { 0 };
                    if include_gid_in_key {
                        k.push(ScalarValue::Int64(gid));
                    }
                    let init: Vec<AggState> = aggregates.iter().map(AggState::new).collect();
                    iter_entries.push((k, init));
                }
            }
        }
        return Ok(Box::new(InMemoryAggEmitStream {
            entries: iter_entries.into_iter(),
            num_gb,
            has_grouping_id,
            aggregates,
            output_types,
            ctx,
        }));
    }

    // Flush any leftover in-memory groups into the 64 spill partitions and release map RAM!
    let mut writers = spill_parts.unwrap();
    if !map.is_empty() {
        flush_agg_map_to_partitions(&mut map, &mut writers, 0)?;
    }
    drop(map);
    drop(mem_res);
    let partition_files = writers.finish_all()?;

    Ok(Box::new(SpilledAggMergeStream {
        plan_id,
        pending_partitions: VecDeque::from(
            partition_files
                .into_iter()
                .filter(|f| !f.is_empty())
                .map(|f| (f, 0usize))
                .collect::<Vec<_>>(),
        ),
        current_chunks: VecDeque::new(),
        num_gb,
        include_gid_in_key,
        has_grouping_id,
        aggregates,
        output_types,
        agg_budget,
        base_entry_bytes,
        ctx,
    }))
}

type EvalAggInputs = (
    Vec<Vec<vectra_types::Vector>>,
    Vec<Option<vectra_types::Vector>>,
);

fn eval_agg_inputs(
    chunk: &DataChunk,
    aggregates: &[PhysicalAggExpr],
    ctx: &ExecutionContext,
) -> Result<EvalAggInputs> {
    let mut agg_args = Vec::with_capacity(aggregates.len());
    let mut agg_filters = Vec::with_capacity(aggregates.len());
    for agg in aggregates {
        let mut args = Vec::with_capacity(agg.args.len());
        for a in &agg.args {
            args.push(eval_expr(a, chunk, &ctx.pool)?);
        }
        let filt = match &agg.filter {
            Some(f) => Some(eval_expr(f, chunk, &ctx.pool)?),
            None => None,
        };
        agg_args.push(args);
        agg_filters.push(filt);
    }
    Ok((agg_args, agg_filters))
}

#[inline]
fn partition_for_hash(h: u64, depth: usize, num_parts: usize) -> usize {
    let shift = 58usize.saturating_sub(depth * 6);
    let mixed = if depth == 0 {
        h
    } else {
        h.wrapping_mul(0x9e37_79b9_7f4a_7c15u64.wrapping_add(depth as u64 * 17))
    };
    ((mixed >> shift) as usize) % num_parts
}

struct PartitionSpillWriters {
    writers: Vec<SpillWriter>,
    staging: Vec<Vec<u8>>,
}

impl PartitionSpillWriters {
    fn new(plan_id: usize, num_parts: usize, ctx: &ExecutionContext) -> Result<Self> {
        let mut writers = Vec::with_capacity(num_parts);
        let mut staging = Vec::with_capacity(num_parts);
        for _ in 0..num_parts {
            writers.push(
                ctx.spill
                    .create_writer(plan_id, Some(Arc::clone(&ctx.metrics)))?,
            );
            staging.push(Vec::new());
        }
        Ok(Self { writers, staging })
    }

    fn append(&mut self, part: usize, record: &[u8]) -> Result<()> {
        let buf = &mut self.staging[part];
        buf.extend_from_slice(&(record.len() as u32).to_le_bytes());
        buf.extend_from_slice(record);
        if buf.len() >= SPILL_STAGING_TARGET {
            self.writers[part].write_block(buf)?;
            buf.clear();
        }
        Ok(())
    }

    fn finish_all(self) -> Result<Vec<SpillFile>> {
        let mut files = Vec::with_capacity(self.writers.len());
        for (mut w, mut buf) in self.writers.into_iter().zip(self.staging) {
            if !buf.is_empty() {
                w.write_block(&buf)?;
                buf.clear();
            }
            files.push(w.finish()?);
        }
        Ok(files)
    }
}

fn flush_agg_map_to_partitions(
    map: &mut HashMap<CanonicalKey, Vec<AggState>>,
    writers: &mut PartitionSpillWriters,
    depth: usize,
) -> Result<()> {
    let num_parts = writers.writers.len();
    let mut rec = Vec::with_capacity(256);
    for (key, states) in map.drain() {
        let h = hash_scalars(&key.0);
        let p = partition_for_hash(h, depth, num_parts);
        rec.clear();
        rec.extend_from_slice(&(key.0.len() as u32).to_le_bytes());
        for kv in &key.0 {
            write_scalar_bin(kv, &mut rec);
        }
        for st in &states {
            st.serialize_bin(&mut rec);
        }
        writers.append(p, &rec)?;
    }
    map.shrink_to_fit();
    Ok(())
}

struct InMemoryAggEmitStream {
    entries: std::vec::IntoIter<(Vec<ScalarValue>, Vec<AggState>)>,
    num_gb: usize,
    has_grouping_id: bool,
    aggregates: Vec<PhysicalAggExpr>,
    output_types: Vec<LogicalType>,
    ctx: ExecutionContext,
}

impl Iterator for InMemoryAggEmitStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Err(e) = self.ctx.check_cancelled() {
            return Some(Err(e));
        }
        let mut rows = Vec::with_capacity(VECTOR_CAPACITY);
        for (key, states) in self.entries.by_ref().take(VECTOR_CAPACITY) {
            let mut row = Vec::with_capacity(self.output_types.len());
            for v in key.iter().take(self.num_gb) {
                row.push(v.clone());
            }
            if self.has_grouping_id {
                row.push(key[self.num_gb].clone());
            }
            for (i, st) in states.iter().enumerate() {
                match st.finalize(self.aggregates[i].return_type) {
                    Ok(val) => row.push(val),
                    Err(e) => return Some(Err(e)),
                }
            }
            rows.push(row);
        }
        if rows.is_empty() {
            return None;
        }
        Some(chunk_from_scalar_rows(
            &self.output_types,
            &rows,
            &self.ctx.pool,
        ))
    }
}

struct SpilledAggMergeStream {
    plan_id: usize,
    pending_partitions: VecDeque<(SpillFile, usize)>,
    current_chunks: VecDeque<DataChunk>,
    num_gb: usize,
    include_gid_in_key: bool,
    has_grouping_id: bool,
    aggregates: Vec<PhysicalAggExpr>,
    output_types: Vec<LogicalType>,
    agg_budget: usize,
    base_entry_bytes: usize,
    ctx: ExecutionContext,
}

impl SpilledAggMergeStream {
    fn load_next_partition(&mut self) -> Result<bool> {
        while let Some((file, depth)) = self.pending_partitions.pop_front() {
            self.ctx.check_cancelled()?;
            if file.is_empty() {
                continue;
            }
            let mut reader = file.into_reader()?;
            let mut map: HashMap<CanonicalKey, Vec<AggState>> = HashMap::new();
            let mut mem_res = self.ctx.pool.reserve(0)?;
            let mut sub_writers: Option<PartitionSpillWriters> = None;

            while let Some(block) = reader.next_block()? {
                self.ctx.check_cancelled()?;
                let mut pos = 0usize;
                while pos < block.len() {
                    if pos + 4 > block.len() {
                        return Err(VectraError::CorruptFormat(
                            "truncated agg spill record len".into(),
                        ));
                    }
                    let rec_len =
                        u32::from_le_bytes(block[pos..pos + 4].try_into().unwrap()) as usize;
                    pos += 4;
                    let rec_end = pos + rec_len;
                    if rec_end > block.len() {
                        return Err(VectraError::CorruptFormat(
                            "truncated agg spill record body".into(),
                        ));
                    }
                    let rec = &block[pos..rec_end];
                    pos = rec_end;

                    let mut rpos = 0usize;
                    let key_len = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
                    rpos += 4;
                    let mut key_vals = Vec::with_capacity(key_len);
                    for _ in 0..key_len {
                        key_vals.push(read_scalar_bin(rec, &mut rpos)?);
                    }
                    let mut states = Vec::with_capacity(self.aggregates.len());
                    for spec in &self.aggregates {
                        states.push(AggState::deserialize_bin(rec, &mut rpos, spec)?);
                    }

                    // If we already overflowed into sub-partitions, route directly to sub-partition.
                    if let Some(sw) = &mut sub_writers {
                        let h = hash_scalars(&key_vals);
                        let sp = partition_for_hash(h, depth + 1, sw.writers.len());
                        sw.append(sp, rec)?;
                        continue;
                    }

                    let key = CanonicalKey(key_vals);
                    let mut added = 0usize;
                    if let Some(existing) = map.get_mut(&key) {
                        for (dst, src) in existing.iter_mut().zip(states) {
                            added += dst.merge_from(src)?;
                        }
                    } else {
                        added += self.base_entry_bytes;
                        for kv in &key.0 {
                            if let ScalarValue::Utf8(s) = kv {
                                added += s.len();
                            } else if let ScalarValue::Binary(b) = kv {
                                added += b.len();
                            }
                        }
                        for st in &states {
                            if let AggState::Distinct { seen, .. } = st {
                                added += seen.len() * 40;
                            }
                        }
                        map.insert(key, states);
                    }

                    if added > 0
                        && (mem_res.bytes() + added >= self.agg_budget
                            || mem_res.try_grow(added).is_err())
                        && depth < MAX_SPILL_DEPTH
                    {
                        let mut sw = PartitionSpillWriters::new(self.plan_id, 16, &self.ctx)?;
                        flush_agg_map_to_partitions(&mut map, &mut sw, depth + 1)?;
                        mem_res.shrink(mem_res.bytes());
                        sub_writers = Some(sw);
                    }
                }
            }

            if let Some(sw) = sub_writers {
                let sub_files = sw.finish_all()?;
                for sf in sub_files.into_iter().rev() {
                    if !sf.is_empty() {
                        self.pending_partitions.push_front((sf, depth + 1));
                    }
                }
                continue;
            }

            // Finalize this partition into 2,048-row output chunks, then drop `map` + `mem_res`!
            let mut rows = Vec::with_capacity(VECTOR_CAPACITY);
            let _ = self.include_gid_in_key;
            for (key, states) in map.into_iter() {
                let mut row = Vec::with_capacity(self.output_types.len());
                for v in key.0.iter().take(self.num_gb) {
                    row.push(v.clone());
                }
                if self.has_grouping_id {
                    row.push(key.0[self.num_gb].clone());
                }
                for (i, st) in states.iter().enumerate() {
                    row.push(st.finalize(self.aggregates[i].return_type)?);
                }
                rows.push(row);
                if rows.len() >= VECTOR_CAPACITY {
                    self.current_chunks.push_back(chunk_from_scalar_rows(
                        &self.output_types,
                        &rows,
                        &self.ctx.pool,
                    )?);
                    rows.clear();
                }
            }
            if !rows.is_empty() {
                self.current_chunks.push_back(chunk_from_scalar_rows(
                    &self.output_types,
                    &rows,
                    &self.ctx.pool,
                )?);
            }
            drop(mem_res);
            if !self.current_chunks.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Iterator for SpilledAggMergeStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(ch) = self.current_chunks.pop_front() {
                return Some(Ok(ch));
            }
            match self.load_next_partition() {
                Ok(true) => continue,
                Ok(false) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}
