use crate::byte_view::{ByteView, StringHeapBuilder};
use crate::chunk::DataChunk;
use crate::error::{Result, VectraError};
use crate::mask::ValidityMaskBuilder;
use crate::memory::{AlignedBufferMut, MemoryPool};
use crate::types::{canonicalize_f64, LogicalType, ScalarValue};
use crate::vector::{FlatVector, Vector};
use std::ops::Range;
use std::sync::Arc;

/// Sort specification for a single key column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SortOrder {
    pub ascending: bool,
    pub nulls_first: bool,
}

impl SortOrder {
    pub const ASC_NULLS_LAST: Self = Self {
        ascending: true,
        nulls_first: false,
    };
    pub const ASC_NULLS_FIRST: Self = Self {
        ascending: true,
        nulls_first: true,
    };
    pub const DESC_NULLS_FIRST: Self = Self {
        ascending: false,
        nulls_first: true,
    };
    pub const DESC_NULLS_LAST: Self = Self {
        ascending: false,
        nulls_first: false,
    };
}

/// Encodes a single `ScalarValue` into a normalized, prefix-free binary sort key
/// such that `memcmp(key_a, key_b) == cmp_with_order(a, b, order)` across all types,
/// including `NaN`, `-0.0`, `Decimal128`, embedded `\0` bytes, and all 4 `ASC/DESC × NULLS FIRST/LAST` orders.
pub fn encode_sort_key(val: &ScalarValue, ty: LogicalType, order: SortOrder, out: &mut Vec<u8>) {
    if val.is_null() || ty == LogicalType::Null {
        out.push(if order.nulls_first { 0x00 } else { 0x01 });
        return;
    }
    // Non-null prefix byte (independent of ASC/DESC so NULLS FIRST/LAST is exact).
    out.push(if order.nulls_first { 0x01 } else { 0x00 });
    let start = out.len();

    match val {
        ScalarValue::Null => unreachable!(),
        ScalarValue::Boolean(b) => out.push(u8::from(*b)),
        ScalarValue::Int8(v) => out.push((*v as u8) ^ 0x80),
        ScalarValue::Int16(v) => out.extend_from_slice(&((*v as u16) ^ 0x8000).to_be_bytes()),
        ScalarValue::Int32(v) | ScalarValue::Date32(v) => {
            out.extend_from_slice(&((*v as u32) ^ 0x8000_0000).to_be_bytes());
        }
        ScalarValue::Int64(v) | ScalarValue::TimestampMicros(v) => {
            out.extend_from_slice(&((*v as u64) ^ 0x8000_0000_0000_0000).to_be_bytes());
        }
        ScalarValue::Float64(v) => {
            let c = canonicalize_f64(*v);
            let bits = c.to_bits();
            let ordered = if (bits >> 63) != 0 {
                !bits
            } else {
                bits ^ 0x8000_0000_0000_0000
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        ScalarValue::Decimal128 { value, .. } => {
            let ordered = (*value as u128) ^ (1u128 << 127);
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        ScalarValue::Utf8(s) => encode_varlen_sort_bytes(s.as_bytes(), out),
        ScalarValue::Binary(b) => encode_varlen_sort_bytes(b, out),
    }

    if !order.ascending {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

/// Prefix-free, NUL-safe lexicographical encoding for byte slices:
/// `0x00 -> [0x00, 0x01]`, other byte `b -> [b]`, terminated by `[0x00, 0x00]`.
#[inline]
fn encode_varlen_sort_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    out.reserve(bytes.len() + 2);
    for &b in bytes {
        if b == 0x00 {
            out.push(0x00);
            out.push(0x01);
        } else {
            out.push(b);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

/// Encodes a multi-column composite sort key.
pub fn encode_composite_sort_key(
    values: &[ScalarValue],
    types: &[LogicalType],
    orders: &[SortOrder],
    out: &mut Vec<u8>,
) {
    assert_eq!(values.len(), types.len());
    assert_eq!(values.len(), orders.len());
    for ((v, &ty), &ord) in values.iter().zip(types.iter()).zip(orders.iter()) {
        encode_sort_key(v, ty, ord, out);
    }
}

/// Describes the byte offsets of each column inside a fixed-width `TupleBuffer` row.
#[derive(Debug, Clone)]
pub struct RowLayout {
    pub types: Vec<LogicalType>,
    pub null_bitmap_bytes: usize,
    pub col_offsets: Vec<usize>,
    pub row_width: usize,
}

impl RowLayout {
    pub fn new(types: Vec<LogicalType>) -> Self {
        let null_bitmap_bytes = types.len().div_ceil(8);
        let mut offset = null_bitmap_bytes;
        let mut col_offsets = Vec::with_capacity(types.len());
        for &ty in &types {
            // Fixed slot width in row format: varlen stores (u32 heap_offset, u32 byte_len) = 8 bytes.
            let slot_size = Self::slot_size(ty);
            let align = Self::slot_align(ty);
            offset = (offset + align - 1) & !(align - 1);
            col_offsets.push(offset);
            offset += slot_size;
        }
        // Align whole row to 8 bytes for clean sequential access.
        let row_width = ((offset + 7) & !7).max(8);
        Self {
            types,
            null_bitmap_bytes,
            col_offsets,
            row_width,
        }
    }

    #[inline]
    const fn slot_size(ty: LogicalType) -> usize {
        match ty {
            LogicalType::Null => 0,
            LogicalType::Boolean | LogicalType::Int8 => 1,
            LogicalType::Int16 => 2,
            LogicalType::Int32 | LogicalType::Date32 => 4,
            LogicalType::Int64 | LogicalType::Float64 | LogicalType::TimestampMicros => 8,
            LogicalType::Decimal128 { .. } => 16,
            LogicalType::Utf8 | LogicalType::Binary => 8, // (offset: u32, len: u32)
        }
    }

    #[inline]
    const fn slot_align(ty: LogicalType) -> usize {
        match ty {
            LogicalType::Null | LogicalType::Boolean | LogicalType::Int8 => 1,
            LogicalType::Int16 => 2,
            LogicalType::Int32 | LogicalType::Date32 => 4,
            LogicalType::Int64
            | LogicalType::Float64
            | LogicalType::TimestampMicros
            | LogicalType::Utf8
            | LogicalType::Binary => 8,
            LogicalType::Decimal128 { .. } => 8,
        }
    }
}

/// Memory-tracked compact row store supporting HashJoin build tables,
/// HashAggregate state tables, and Out-of-Core Spill serialization.
#[derive(Debug)]
pub struct TupleBuffer {
    layout: RowLayout,
    fixed_buf: AlignedBufferMut,
    var_buf: AlignedBufferMut,
    row_count: usize,
    pool: Arc<dyn MemoryPool>,
}

impl TupleBuffer {
    pub fn new(types: Vec<LogicalType>, pool: &Arc<dyn MemoryPool>) -> Result<Self> {
        let layout = RowLayout::new(types);
        Ok(Self {
            layout,
            fixed_buf: AlignedBufferMut::with_capacity(0, pool)?,
            var_buf: AlignedBufferMut::with_capacity(0, pool)?,
            row_count: 0,
            pool: Arc::clone(pool),
        })
    }

    #[inline]
    pub fn layout(&self) -> &RowLayout {
        &self.layout
    }

    #[inline]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    #[inline]
    pub fn memory_bytes(&self) -> usize {
        self.fixed_buf.capacity() + self.var_buf.capacity()
    }

    /// Appends all active rows of `chunk` into this `TupleBuffer`, returning the assigned row-id range.
    pub fn append_chunk(&mut self, chunk: &DataChunk) -> Result<Range<usize>> {
        if chunk.num_columns() != self.layout.types.len() {
            return Err(VectraError::TypeMismatch {
                expected: format!("{} columns", self.layout.types.len()),
                actual: format!("{} columns", chunk.num_columns()),
            });
        }
        let n = chunk.len();
        let start_row = self.row_count;
        if n == 0 {
            return Ok(start_row..start_row);
        }

        // Flatten columns lazily once for fast sequential row encoding.
        let mut flats = Vec::with_capacity(chunk.num_columns());
        for (col_idx, col) in chunk.columns.iter().enumerate() {
            if col.logical_type() != self.layout.types[col_idx] {
                return Err(VectraError::TypeMismatch {
                    expected: self.layout.types[col_idx].to_string(),
                    actual: col.logical_type().to_string(),
                });
            }
            flats.push(col.flatten(&chunk.sel, n, &self.pool)?);
        }

        let row_width = self.layout.row_width;
        let old_fixed_len = self.fixed_buf.len();
        self.fixed_buf
            .resize_zeroed(old_fixed_len + n * row_width)?;

        for r in 0..n {
            let row_base = old_fixed_len + r * row_width;
            for (c, flat) in flats.iter().enumerate() {
                if flat.validity.is_null(r) {
                    continue;
                }
                // Set valid bit in row's null bitmap.
                self.fixed_buf.as_mut_slice()[row_base + (c >> 3)] |= 1u8 << (c & 7);
                let slot = row_base + self.layout.col_offsets[c];
                match flat.logical_type {
                    LogicalType::Null => {}
                    LogicalType::Boolean => {
                        self.fixed_buf.as_mut_slice()[slot] = flat.as_bool_bytes()[r];
                    }
                    LogicalType::Int8 => {
                        self.fixed_buf.as_mut_slice()[slot] = flat.as_i8_slice()[r] as u8;
                    }
                    LogicalType::Int16 => {
                        let bytes = flat.as_i16_slice()[r].to_le_bytes();
                        self.fixed_buf.as_mut_slice()[slot..slot + 2].copy_from_slice(&bytes);
                    }
                    LogicalType::Int32 | LogicalType::Date32 => {
                        let bytes = flat.as_i32_slice()[r].to_le_bytes();
                        self.fixed_buf.as_mut_slice()[slot..slot + 4].copy_from_slice(&bytes);
                    }
                    LogicalType::Int64 | LogicalType::TimestampMicros => {
                        let bytes = flat.as_i64_slice()[r].to_le_bytes();
                        self.fixed_buf.as_mut_slice()[slot..slot + 8].copy_from_slice(&bytes);
                    }
                    LogicalType::Float64 => {
                        let bytes = flat.as_f64_slice()[r].to_le_bytes();
                        self.fixed_buf.as_mut_slice()[slot..slot + 8].copy_from_slice(&bytes);
                    }
                    LogicalType::Decimal128 { .. } => {
                        let bytes = flat.as_i128_slice()[r].to_le_bytes();
                        self.fixed_buf.as_mut_slice()[slot..slot + 16].copy_from_slice(&bytes);
                    }
                    LogicalType::Utf8 | LogicalType::Binary => {
                        let bytes = flat.as_byte_views()[r].as_bytes(&flat.string_heap);
                        let var_off = u32::try_from(self.var_buf.len()).map_err(|_| {
                            VectraError::ExecutionError("TupleBuffer varlen offset overflow".into())
                        })?;
                        let var_len = u32::try_from(bytes.len()).map_err(|_| {
                            VectraError::ExecutionError("TupleBuffer varlen length overflow".into())
                        })?;
                        self.var_buf.extend_from_slice(bytes)?;
                        let dst = &mut self.fixed_buf.as_mut_slice()[slot..slot + 8];
                        dst[0..4].copy_from_slice(&var_off.to_le_bytes());
                        dst[4..8].copy_from_slice(&var_len.to_le_bytes());
                    }
                }
            }
        }

        self.row_count += n;
        Ok(start_row..self.row_count)
    }

    /// Appends a single row of `ScalarValue`s.
    pub fn append_row(&mut self, values: &[ScalarValue]) -> Result<usize> {
        if values.len() != self.layout.types.len() {
            return Err(VectraError::TypeMismatch {
                expected: format!("{} columns", self.layout.types.len()),
                actual: format!("{} columns", values.len()),
            });
        }
        let row_idx = self.row_count;
        let row_width = self.layout.row_width;
        let row_base = self.fixed_buf.len();
        self.fixed_buf.resize_zeroed(row_base + row_width)?;

        for (c, (val, &ty)) in values.iter().zip(self.layout.types.iter()).enumerate() {
            if val.is_null() {
                continue;
            }
            self.fixed_buf.as_mut_slice()[row_base + (c >> 3)] |= 1u8 << (c & 7);
            let slot = row_base + self.layout.col_offsets[c];
            match (ty, val) {
                (LogicalType::Boolean, ScalarValue::Boolean(b)) => {
                    self.fixed_buf.as_mut_slice()[slot] = u8::from(*b);
                }
                (LogicalType::Int8, ScalarValue::Int8(v)) => {
                    self.fixed_buf.as_mut_slice()[slot] = *v as u8;
                }
                (LogicalType::Int16, ScalarValue::Int16(v)) => {
                    self.fixed_buf.as_mut_slice()[slot..slot + 2].copy_from_slice(&v.to_le_bytes());
                }
                (LogicalType::Int32, ScalarValue::Int32(v))
                | (LogicalType::Date32, ScalarValue::Date32(v)) => {
                    self.fixed_buf.as_mut_slice()[slot..slot + 4].copy_from_slice(&v.to_le_bytes());
                }
                (LogicalType::Int64, ScalarValue::Int64(v))
                | (LogicalType::TimestampMicros, ScalarValue::TimestampMicros(v)) => {
                    self.fixed_buf.as_mut_slice()[slot..slot + 8].copy_from_slice(&v.to_le_bytes());
                }
                (LogicalType::Float64, ScalarValue::Float64(v)) => {
                    self.fixed_buf.as_mut_slice()[slot..slot + 8].copy_from_slice(&v.to_le_bytes());
                }
                (LogicalType::Decimal128 { .. }, ScalarValue::Decimal128 { value, .. }) => {
                    self.fixed_buf.as_mut_slice()[slot..slot + 16]
                        .copy_from_slice(&value.to_le_bytes());
                }
                (LogicalType::Utf8, ScalarValue::Utf8(s)) => {
                    let var_off = u32::try_from(self.var_buf.len()).map_err(|_| {
                        VectraError::ExecutionError("TupleBuffer varlen overflow".into())
                    })?;
                    let var_len = u32::try_from(s.len()).map_err(|_| {
                        VectraError::ExecutionError("TupleBuffer varlen overflow".into())
                    })?;
                    self.var_buf.extend_from_slice(s.as_bytes())?;
                    let dst = &mut self.fixed_buf.as_mut_slice()[slot..slot + 8];
                    dst[0..4].copy_from_slice(&var_off.to_le_bytes());
                    dst[4..8].copy_from_slice(&var_len.to_le_bytes());
                }
                (LogicalType::Binary, ScalarValue::Binary(b)) => {
                    let var_off = u32::try_from(self.var_buf.len()).map_err(|_| {
                        VectraError::ExecutionError("TupleBuffer varlen overflow".into())
                    })?;
                    let var_len = u32::try_from(b.len()).map_err(|_| {
                        VectraError::ExecutionError("TupleBuffer varlen overflow".into())
                    })?;
                    self.var_buf.extend_from_slice(b)?;
                    let dst = &mut self.fixed_buf.as_mut_slice()[slot..slot + 8];
                    dst[0..4].copy_from_slice(&var_off.to_le_bytes());
                    dst[4..8].copy_from_slice(&var_len.to_le_bytes());
                }
                (expected, actual) => {
                    return Err(VectraError::TypeMismatch {
                        expected: expected.to_string(),
                        actual: actual.logical_type().to_string(),
                    });
                }
            }
        }
        self.row_count += 1;
        Ok(row_idx)
    }

    /// Reads one scalar cell directly from the packed row buffer.
    pub fn get_scalar(&self, row_id: usize, col_idx: usize) -> ScalarValue {
        assert!(row_id < self.row_count && col_idx < self.layout.types.len());
        let fixed = self.fixed_buf.as_slice();
        let row_base = row_id * self.layout.row_width;
        let valid = (fixed[row_base + (col_idx >> 3)] & (1u8 << (col_idx & 7))) != 0;
        if !valid {
            return ScalarValue::Null;
        }
        let slot = row_base + self.layout.col_offsets[col_idx];
        match self.layout.types[col_idx] {
            LogicalType::Null => ScalarValue::Null,
            LogicalType::Boolean => ScalarValue::Boolean(fixed[slot] != 0),
            LogicalType::Int8 => ScalarValue::Int8(fixed[slot] as i8),
            LogicalType::Int16 => ScalarValue::Int16(i16::from_le_bytes(
                fixed[slot..slot + 2].try_into().unwrap(),
            )),
            LogicalType::Int32 => ScalarValue::Int32(i32::from_le_bytes(
                fixed[slot..slot + 4].try_into().unwrap(),
            )),
            LogicalType::Date32 => ScalarValue::Date32(i32::from_le_bytes(
                fixed[slot..slot + 4].try_into().unwrap(),
            )),
            LogicalType::Int64 => ScalarValue::Int64(i64::from_le_bytes(
                fixed[slot..slot + 8].try_into().unwrap(),
            )),
            LogicalType::TimestampMicros => ScalarValue::TimestampMicros(i64::from_le_bytes(
                fixed[slot..slot + 8].try_into().unwrap(),
            )),
            LogicalType::Float64 => ScalarValue::Float64(f64::from_le_bytes(
                fixed[slot..slot + 8].try_into().unwrap(),
            )),
            LogicalType::Decimal128 { precision, scale } => ScalarValue::Decimal128 {
                value: i128::from_le_bytes(fixed[slot..slot + 16].try_into().unwrap()),
                precision,
                scale,
            },
            LogicalType::Utf8 => {
                let off = u32::from_le_bytes(fixed[slot..slot + 4].try_into().unwrap()) as usize;
                let len =
                    u32::from_le_bytes(fixed[slot + 4..slot + 8].try_into().unwrap()) as usize;
                let bytes = &self.var_buf.as_slice()[off..off + len];
                ScalarValue::Utf8(String::from_utf8_lossy(bytes).into_owned())
            }
            LogicalType::Binary => {
                let off = u32::from_le_bytes(fixed[slot..slot + 4].try_into().unwrap()) as usize;
                let len =
                    u32::from_le_bytes(fixed[slot + 4..slot + 8].try_into().unwrap()) as usize;
                let bytes = &self.var_buf.as_slice()[off..off + len];
                ScalarValue::Binary(bytes.to_vec())
            }
        }
    }

    pub fn get_row(&self, row_id: usize) -> Vec<ScalarValue> {
        (0..self.layout.types.len())
            .map(|c| self.get_scalar(row_id, c))
            .collect()
    }

    /// Materializes the specified `row_ids` back into a columnar `DataChunk`.
    pub fn materialize_rows(
        &self,
        row_ids: &[usize],
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<DataChunk> {
        let n = row_ids.len();
        if n == 0 {
            return Ok(DataChunk::empty(&self.layout.types, pool));
        }
        let fixed = self.fixed_buf.as_slice();
        let var = self.var_buf.as_slice();
        let row_width = self.layout.row_width;
        let mut columns = Vec::with_capacity(self.layout.types.len());

        for (c, &ty) in self.layout.types.iter().enumerate() {
            let col_off = self.layout.col_offsets[c];
            let width = ty.fixed_byte_width();
            let mut data = AlignedBufferMut::with_capacity(n * width, pool)?;
            let mut mask = ValidityMaskBuilder::with_capacity(n);
            let mut heap = StringHeapBuilder::new(pool);

            for &row_id in row_ids {
                assert!(row_id < self.row_count);
                let row_base = row_id * row_width;
                let valid = (fixed[row_base + (c >> 3)] & (1u8 << (c & 7))) != 0;
                mask.push(valid);
                let slot = row_base + col_off;
                match ty {
                    LogicalType::Null => {}
                    LogicalType::Boolean | LogicalType::Int8 => {
                        data.push_typed(if valid { fixed[slot] } else { 0u8 })?;
                    }
                    LogicalType::Int16 => {
                        let v = if valid {
                            i16::from_le_bytes(fixed[slot..slot + 2].try_into().unwrap())
                        } else {
                            0
                        };
                        data.push_typed(v)?;
                    }
                    LogicalType::Int32 | LogicalType::Date32 => {
                        let v = if valid {
                            i32::from_le_bytes(fixed[slot..slot + 4].try_into().unwrap())
                        } else {
                            0
                        };
                        data.push_typed(v)?;
                    }
                    LogicalType::Int64 | LogicalType::TimestampMicros => {
                        let v = if valid {
                            i64::from_le_bytes(fixed[slot..slot + 8].try_into().unwrap())
                        } else {
                            0
                        };
                        data.push_typed(v)?;
                    }
                    LogicalType::Float64 => {
                        let v = if valid {
                            f64::from_le_bytes(fixed[slot..slot + 8].try_into().unwrap())
                        } else {
                            0.0
                        };
                        data.push_typed(v)?;
                    }
                    LogicalType::Decimal128 { .. } => {
                        let v = if valid {
                            i128::from_le_bytes(fixed[slot..slot + 16].try_into().unwrap())
                        } else {
                            0
                        };
                        data.push_typed(v)?;
                    }
                    LogicalType::Utf8 | LogicalType::Binary => {
                        if valid {
                            let off = u32::from_le_bytes(fixed[slot..slot + 4].try_into().unwrap())
                                as usize;
                            let len =
                                u32::from_le_bytes(fixed[slot + 4..slot + 8].try_into().unwrap())
                                    as usize;
                            let bv = heap.append(&var[off..off + len])?;
                            data.push_typed(bv)?;
                        } else {
                            data.push_typed(ByteView::empty())?;
                        }
                    }
                }
            }

            columns.push(Vector::Flat(FlatVector {
                logical_type: ty,
                data: data.freeze(),
                validity: mask.finish(),
                string_heap: heap.finish(),
                len: n,
            }));
        }

        Ok(DataChunk::new(columns, n))
    }

    /// Serializes the entire `TupleBuffer` payload to raw bytes (for M06 disk spill pages).
    pub fn serialize_to_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.row_count as u64).to_le_bytes());
        out.extend_from_slice(&(self.fixed_buf.len() as u64).to_le_bytes());
        out.extend_from_slice(&(self.var_buf.len() as u64).to_le_bytes());
        out.extend_from_slice(self.fixed_buf.as_slice());
        out.extend_from_slice(self.var_buf.as_slice());
    }

    /// Restores a `TupleBuffer` from bytes produced by `serialize_to_bytes`.
    pub fn deserialize_from_bytes(
        types: Vec<LogicalType>,
        bytes: &[u8],
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<Self> {
        if bytes.len() < 24 {
            return Err(VectraError::CorruptFormat(
                "truncated TupleBuffer header".into(),
            ));
        }
        let row_count = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
        let fixed_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let var_len = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
        if bytes.len() != 24 + fixed_len + var_len {
            return Err(VectraError::CorruptFormat(
                "TupleBuffer payload length mismatch".into(),
            ));
        }
        let layout = RowLayout::new(types);
        if row_count * layout.row_width != fixed_len {
            return Err(VectraError::CorruptFormat(
                "TupleBuffer fixed_len != row_count * row_width".into(),
            ));
        }
        let mut fixed_buf = AlignedBufferMut::with_capacity(fixed_len, pool)?;
        fixed_buf.extend_from_slice(&bytes[24..24 + fixed_len])?;
        let mut var_buf = AlignedBufferMut::with_capacity(var_len, pool)?;
        var_buf.extend_from_slice(&bytes[24 + fixed_len..])?;
        Ok(Self {
            layout,
            fixed_buf,
            var_buf,
            row_count,
            pool: Arc::clone(pool),
        })
    }
}
