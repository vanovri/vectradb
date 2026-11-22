use crate::context::ExecutionContext;
use crate::metrics::WallTimer;
use std::cmp::Ordering;
use std::sync::Arc;
use vectra_expr::{eval_expr, hash_combine, hash_vector, PhysicalExpr};
use vectra_types::{
    canonicalize_f64, decimal_rescale, DataChunk, FlatVector, LogicalType, MemoryPool, Result,
    ScalarValue, SelectionVector, Vector, VectraError, VECTOR_CAPACITY,
};

/// Pull-based vectorized chunk stream yielding `<= VECTOR_CAPACITY` (2,048) row `DataChunk`s.
pub type ChunkStream = Box<dyn Iterator<Item = Result<DataChunk>> + Send>;

/// Wraps a `ChunkStream` to record per-operator output row counts, chunk counts,
/// elapsed time, and cooperative cancellation checks.
pub fn instrumented_stream(
    plan_id: usize,
    inner: ChunkStream,
    ctx: ExecutionContext,
) -> ChunkStream {
    Box::new(InstrumentedStream {
        plan_id,
        inner,
        ctx,
    })
}

struct InstrumentedStream {
    plan_id: usize,
    inner: ChunkStream,
    ctx: ExecutionContext,
}

impl Iterator for InstrumentedStream {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Err(e) = self.ctx.check_cancelled() {
            return Some(Err(e));
        }
        let timer = WallTimer::start();
        let item = self.inner.next()?;
        let elapsed = timer.elapsed_nanos();
        match &item {
            Ok(chunk) => {
                self.ctx
                    .metrics
                    .record_output(self.plan_id, chunk.len() as u64, 1, elapsed);
                self.ctx
                    .metrics
                    .record_peak_memory(self.plan_id, self.ctx.pool.peak_bytes());
            }
            Err(_) => {
                self.ctx.metrics.add_elapsed(self.plan_id, elapsed);
            }
        }
        Some(item)
    }
}

/// Converts a slice of scalar rows into one or more `DataChunk`s bounded by `VECTOR_CAPACITY`.
pub fn chunks_from_scalar_rows(
    types: &[LogicalType],
    rows: &[Vec<ScalarValue>],
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vec<DataChunk>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut chunks = Vec::with_capacity(rows.len().div_ceil(VECTOR_CAPACITY));
    let mut off = 0;
    while off < rows.len() {
        let take = (rows.len() - off).min(VECTOR_CAPACITY);
        chunks.push(chunk_from_scalar_rows(types, &rows[off..off + take], pool)?);
        off += take;
    }
    Ok(chunks)
}

/// Converts up to `VECTOR_CAPACITY` scalar rows into a single columnar `DataChunk`.
pub fn chunk_from_scalar_rows(
    types: &[LogicalType],
    rows: &[Vec<ScalarValue>],
    pool: &Arc<dyn MemoryPool>,
) -> Result<DataChunk> {
    let n = rows.len();
    if n == 0 {
        return Ok(DataChunk::empty(types, pool));
    }
    let mut columns = Vec::with_capacity(types.len());
    let mut col_buf = Vec::with_capacity(n);
    for (c, &ty) in types.iter().enumerate() {
        col_buf.clear();
        for row in rows {
            col_buf.push(row[c].clone());
        }
        columns.push(Vector::Flat(FlatVector::from_scalars(&col_buf, ty, pool)?));
    }
    Ok(DataChunk::new(columns, n))
}

/// Evaluates `key_exprs` over `chunk`, returning `(key_vectors, row_hashes)`.
/// Guarantees identical hashes to `hash_scalars`.
pub fn eval_and_hash_keys(
    chunk: &DataChunk,
    key_exprs: &[PhysicalExpr],
    pool: &Arc<dyn MemoryPool>,
) -> Result<(Vec<Vector>, Vec<u64>)> {
    let n = chunk.len();
    let mut vecs = Vec::with_capacity(key_exprs.len());
    for expr in key_exprs {
        vecs.push(eval_expr(expr, chunk, pool)?);
    }
    let mut hashes = vec![0u64; n];
    if vecs.is_empty() {
        hashes.fill(0x517c_c1b7_2722_0a95);
    } else {
        let id = SelectionVector::identity(n);
        for (k, v) in vecs.iter().enumerate() {
            hash_vector(v, &id, n, &mut hashes, k > 0);
        }
    }
    Ok((vecs, hashes))
}

/// Computes the exact same deterministic multi-column hash as `eval_and_hash_keys` / `hash_chunk`.
#[inline]
pub fn hash_scalars(vals: &[ScalarValue]) -> u64 {
    if vals.is_empty() {
        return 0x517c_c1b7_2722_0a95;
    }
    let mut h = vals[0].canonical_hash();
    for v in &vals[1..] {
        h = hash_combine(h, v.canonical_hash());
    }
    h
}

/// Deterministic `IS NOT DISTINCT FROM` equality for `ScalarValue`s (`NULL == NULL`, `-0.0 == 0.0`, `NaN == NaN`).
#[inline]
pub fn scalar_eq_not_distinct(a: &ScalarValue, b: &ScalarValue) -> bool {
    match (a, b) {
        (ScalarValue::Null, ScalarValue::Null) => true,
        (ScalarValue::Null, _) | (_, ScalarValue::Null) => false,
        (ScalarValue::Boolean(x), ScalarValue::Boolean(y)) => x == y,
        (ScalarValue::Int8(x), ScalarValue::Int8(y)) => x == y,
        (ScalarValue::Int16(x), ScalarValue::Int16(y)) => x == y,
        (ScalarValue::Int32(x), ScalarValue::Int32(y)) => x == y,
        (ScalarValue::Int64(x), ScalarValue::Int64(y)) => x == y,
        (ScalarValue::Date32(x), ScalarValue::Date32(y)) => x == y,
        (ScalarValue::TimestampMicros(x), ScalarValue::TimestampMicros(y)) => x == y,
        (ScalarValue::Float64(x), ScalarValue::Float64(y)) => {
            canonicalize_f64(*x).to_bits() == canonicalize_f64(*y).to_bits()
        }
        (
            ScalarValue::Decimal128 {
                value: vx,
                scale: sx,
                ..
            },
            ScalarValue::Decimal128 {
                value: vy,
                scale: sy,
                ..
            },
        ) => {
            if sx == sy {
                vx == vy
            } else {
                let target_scale = (*sx).max(*sy);
                match (
                    decimal_rescale(*vx, *sx, target_scale, 38),
                    decimal_rescale(*vy, *sy, target_scale, 38),
                ) {
                    (Ok(rx), Ok(ry)) => rx == ry,
                    _ => false,
                }
            }
        }
        (ScalarValue::Utf8(x), ScalarValue::Utf8(y)) => x == y,
        (ScalarValue::Binary(x), ScalarValue::Binary(y)) => x == y,
        _ => false,
    }
}

/// Total ordering across non-null `ScalarValue`s of the same type (`NULL` sorts greater than non-null).
pub fn scalar_cmp_total(a: &ScalarValue, b: &ScalarValue) -> Ordering {
    match (a, b) {
        (ScalarValue::Null, ScalarValue::Null) => Ordering::Equal,
        (ScalarValue::Null, _) => Ordering::Greater,
        (_, ScalarValue::Null) => Ordering::Less,
        (ScalarValue::Boolean(x), ScalarValue::Boolean(y)) => x.cmp(y),
        (ScalarValue::Int8(x), ScalarValue::Int8(y)) => x.cmp(y),
        (ScalarValue::Int16(x), ScalarValue::Int16(y)) => x.cmp(y),
        (ScalarValue::Int32(x), ScalarValue::Int32(y)) => x.cmp(y),
        (ScalarValue::Int64(x), ScalarValue::Int64(y)) => x.cmp(y),
        (ScalarValue::Date32(x), ScalarValue::Date32(y)) => x.cmp(y),
        (ScalarValue::TimestampMicros(x), ScalarValue::TimestampMicros(y)) => x.cmp(y),
        (ScalarValue::Float64(x), ScalarValue::Float64(y)) => {
            let cx = canonicalize_f64(*x);
            let cy = canonicalize_f64(*y);
            cx.total_cmp(&cy)
        }
        (
            ScalarValue::Decimal128 {
                value: vx,
                scale: sx,
                ..
            },
            ScalarValue::Decimal128 {
                value: vy,
                scale: sy,
                ..
            },
        ) => {
            if sx == sy {
                vx.cmp(vy)
            } else {
                let ts = (*sx).max(*sy);
                match (
                    decimal_rescale(*vx, *sx, ts, 38),
                    decimal_rescale(*vy, *sy, ts, 38),
                ) {
                    (Ok(rx), Ok(ry)) => rx.cmp(&ry),
                    _ => vx.cmp(vy),
                }
            }
        }
        (ScalarValue::Utf8(x), ScalarValue::Utf8(y)) => x.cmp(y),
        (ScalarValue::Binary(x), ScalarValue::Binary(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Wrapper implementing `Eq + Hash` over `Vec<ScalarValue>` using canonical SQL `IS NOT DISTINCT FROM` semantics.
#[derive(Debug, Clone)]
pub struct CanonicalKey(pub Vec<ScalarValue>);

impl PartialEq for CanonicalKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        if self.0.len() != other.0.len() {
            return false;
        }
        self.0
            .iter()
            .zip(other.0.iter())
            .all(|(a, b)| scalar_eq_not_distinct(a, b))
    }
}

impl Eq for CanonicalKey {}

impl std::hash::Hash for CanonicalKey {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(hash_scalars(&self.0));
    }
}

/// Wrapper implementing `Eq + Hash` over a single `ScalarValue` (`IS NOT DISTINCT FROM`).
#[derive(Debug, Clone)]
pub struct CanonicalScalar(pub ScalarValue);

impl PartialEq for CanonicalScalar {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        scalar_eq_not_distinct(&self.0, &other.0)
    }
}

impl Eq for CanonicalScalar {}

impl std::hash::Hash for CanonicalScalar {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.0.canonical_hash());
    }
}

/// Binary serialization for `ScalarValue` (used by spilling HashAggregate & ExternalSort).
pub fn write_scalar_bin(val: &ScalarValue, out: &mut Vec<u8>) {
    match val {
        ScalarValue::Null => out.push(0),
        ScalarValue::Boolean(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        ScalarValue::Int8(v) => {
            out.push(2);
            out.push(*v as u8);
        }
        ScalarValue::Int16(v) => {
            out.push(3);
            out.extend_from_slice(&v.to_le_bytes());
        }
        ScalarValue::Int32(v) => {
            out.push(4);
            out.extend_from_slice(&v.to_le_bytes());
        }
        ScalarValue::Int64(v) => {
            out.push(5);
            out.extend_from_slice(&v.to_le_bytes());
        }
        ScalarValue::Float64(v) => {
            out.push(6);
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        ScalarValue::Decimal128 {
            value,
            precision,
            scale,
        } => {
            out.push(7);
            out.push(*precision);
            out.push(*scale as u8);
            out.extend_from_slice(&value.to_le_bytes());
        }
        ScalarValue::Date32(v) => {
            out.push(8);
            out.extend_from_slice(&v.to_le_bytes());
        }
        ScalarValue::TimestampMicros(v) => {
            out.push(9);
            out.extend_from_slice(&v.to_le_bytes());
        }
        ScalarValue::Utf8(s) => {
            out.push(10);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        ScalarValue::Binary(b) => {
            out.push(11);
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
    }
}

/// Binary deserialization counterpart to `write_scalar_bin`.
pub fn read_scalar_bin(buf: &[u8], pos: &mut usize) -> Result<ScalarValue> {
    if *pos >= buf.len() {
        return Err(VectraError::CorruptFormat(
            "unexpected EOF reading scalar tag".into(),
        ));
    }
    let tag = buf[*pos];
    *pos += 1;
    let need = |n: usize, pos: &usize| -> Result<()> {
        if *pos + n > buf.len() {
            Err(VectraError::CorruptFormat(
                "unexpected EOF reading scalar payload".into(),
            ))
        } else {
            Ok(())
        }
    };
    match tag {
        0 => Ok(ScalarValue::Null),
        1 => {
            need(1, pos)?;
            let b = buf[*pos] != 0;
            *pos += 1;
            Ok(ScalarValue::Boolean(b))
        }
        2 => {
            need(1, pos)?;
            let v = buf[*pos] as i8;
            *pos += 1;
            Ok(ScalarValue::Int8(v))
        }
        3 => {
            need(2, pos)?;
            let v = i16::from_le_bytes(buf[*pos..*pos + 2].try_into().unwrap());
            *pos += 2;
            Ok(ScalarValue::Int16(v))
        }
        4 => {
            need(4, pos)?;
            let v = i32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            Ok(ScalarValue::Int32(v))
        }
        5 => {
            need(8, pos)?;
            let v = i64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(ScalarValue::Int64(v))
        }
        6 => {
            need(8, pos)?;
            let bits = u64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(ScalarValue::Float64(f64::from_bits(bits)))
        }
        7 => {
            need(18, pos)?;
            let precision = buf[*pos];
            let scale = buf[*pos + 1] as i8;
            let value = i128::from_le_bytes(buf[*pos + 2..*pos + 18].try_into().unwrap());
            *pos += 18;
            Ok(ScalarValue::Decimal128 {
                value,
                precision,
                scale,
            })
        }
        8 => {
            need(4, pos)?;
            let v = i32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            Ok(ScalarValue::Date32(v))
        }
        9 => {
            need(8, pos)?;
            let v = i64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(ScalarValue::TimestampMicros(v))
        }
        10 => {
            need(4, pos)?;
            let len = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap()) as usize;
            *pos += 4;
            need(len, pos)?;
            let s = String::from_utf8(buf[*pos..*pos + len].to_vec())
                .map_err(|e| VectraError::CorruptFormat(format!("invalid UTF-8: {e}")))?;
            *pos += len;
            Ok(ScalarValue::Utf8(s))
        }
        11 => {
            need(4, pos)?;
            let len = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap()) as usize;
            *pos += 4;
            need(len, pos)?;
            let b = buf[*pos..*pos + len].to_vec();
            *pos += len;
            Ok(ScalarValue::Binary(b))
        }
        other => Err(VectraError::CorruptFormat(format!(
            "unknown scalar tag {other}"
        ))),
    }
}

/// Estimates heap byte footprint of a `ScalarValue` for accurate `MemoryReservation` accounting.
#[inline]
pub fn scalar_heap_bytes(v: &ScalarValue) -> usize {
    match v {
        ScalarValue::Utf8(s) => 32 + s.len(),
        ScalarValue::Binary(b) => 32 + b.len(),
        _ => 24,
    }
}

/// Ensures any oversized chunk is yielded in slices of `<= VECTOR_CAPACITY`.
pub fn split_chunk_to_capacity(chunk: DataChunk) -> Vec<DataChunk> {
    if chunk.is_empty() {
        return Vec::new();
    }
    if chunk.len() <= VECTOR_CAPACITY {
        return vec![chunk];
    }
    let mut out = Vec::with_capacity(chunk.len().div_ceil(VECTOR_CAPACITY));
    let mut off = 0;
    while off < chunk.len() {
        let take = (chunk.len() - off).min(VECTOR_CAPACITY);
        out.push(chunk.slice(off, take));
        off += take;
    }
    out
}
