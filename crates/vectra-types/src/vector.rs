use crate::byte_view::{ByteView, StringHeapBuilder};
use crate::error::{Result, VectraError};
use crate::mask::{ValidityMask, ValidityMaskBuilder};
use crate::memory::{AlignedBuffer, AlignedBufferMut, MemoryPool};
use crate::selection::SelectionVector;
use crate::types::{LogicalType, ScalarValue};
use std::sync::Arc;

/// Contiguous columnar storage for `len` elements of a single `LogicalType`.
#[derive(Debug, Clone)]
pub struct FlatVector {
    pub logical_type: LogicalType,
    pub data: AlignedBuffer,
    pub validity: ValidityMask,
    pub string_heap: Vec<AlignedBuffer>,
    pub len: usize,
}

impl FlatVector {
    pub fn empty(logical_type: LogicalType, pool: &Arc<dyn MemoryPool>) -> Self {
        Self {
            logical_type,
            data: AlignedBuffer::empty(pool),
            validity: ValidityMask::all_valid(0),
            string_heap: Vec::new(),
            len: 0,
        }
    }

    pub fn from_scalars(
        values: &[ScalarValue],
        logical_type: LogicalType,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<Self> {
        let len = values.len();
        if len == 0 {
            return Ok(Self::empty(logical_type, pool));
        }
        let mut mask = ValidityMaskBuilder::with_capacity(len);
        let width = logical_type.fixed_byte_width();
        let mut data = AlignedBufferMut::with_capacity(len * width, pool)?;
        let mut heap = StringHeapBuilder::new(pool);

        for v in values {
            match (logical_type, v) {
                (LogicalType::Null, ScalarValue::Null) => {
                    mask.push(false);
                }
                (_, ScalarValue::Null) => {
                    mask.push(false);
                    push_zero_slot(&mut data, logical_type)?;
                }
                (LogicalType::Boolean, ScalarValue::Boolean(b)) => {
                    mask.push(true);
                    data.push_typed(u8::from(*b))?;
                }
                (LogicalType::Int8, ScalarValue::Int8(x)) => {
                    mask.push(true);
                    data.push_typed(*x)?;
                }
                (LogicalType::Int16, ScalarValue::Int16(x)) => {
                    mask.push(true);
                    data.push_typed(*x)?;
                }
                (LogicalType::Int32, ScalarValue::Int32(x)) => {
                    mask.push(true);
                    data.push_typed(*x)?;
                }
                (LogicalType::Int64, ScalarValue::Int64(x)) => {
                    mask.push(true);
                    data.push_typed(*x)?;
                }
                (LogicalType::Float64, ScalarValue::Float64(x)) => {
                    mask.push(true);
                    data.push_typed(*x)?;
                }
                (LogicalType::Decimal128 { .. }, ScalarValue::Decimal128 { value, .. }) => {
                    mask.push(true);
                    data.push_typed(*value)?;
                }
                (LogicalType::Date32, ScalarValue::Date32(x)) => {
                    mask.push(true);
                    data.push_typed(*x)?;
                }
                (LogicalType::TimestampMicros, ScalarValue::TimestampMicros(x)) => {
                    mask.push(true);
                    data.push_typed(*x)?;
                }
                (LogicalType::Utf8, ScalarValue::Utf8(s)) => {
                    mask.push(true);
                    let bv = heap.append(s.as_bytes())?;
                    data.push_typed(bv)?;
                }
                (LogicalType::Binary, ScalarValue::Binary(b)) => {
                    mask.push(true);
                    let bv = heap.append(b)?;
                    data.push_typed(bv)?;
                }
                (expected, actual) => {
                    return Err(VectraError::TypeMismatch {
                        expected: expected.to_string(),
                        actual: actual.logical_type().to_string(),
                    });
                }
            }
        }

        Ok(Self {
            logical_type,
            data: data.freeze(),
            validity: mask.finish(),
            string_heap: heap.finish(),
            len,
        })
    }

    pub fn from_i64_slice(values: &[i64], pool: &Arc<dyn MemoryPool>) -> Result<Self> {
        let data = AlignedBuffer::from_typed_slice(values, pool)?;
        Ok(Self {
            logical_type: LogicalType::Int64,
            data,
            validity: ValidityMask::all_valid(values.len()),
            string_heap: Vec::new(),
            len: values.len(),
        })
    }

    pub fn from_i32_slice(
        values: &[i32],
        logical_type: LogicalType,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<Self> {
        debug_assert!(matches!(
            logical_type,
            LogicalType::Int32 | LogicalType::Date32
        ));
        let data = AlignedBuffer::from_typed_slice(values, pool)?;
        Ok(Self {
            logical_type,
            data,
            validity: ValidityMask::all_valid(values.len()),
            string_heap: Vec::new(),
            len: values.len(),
        })
    }

    pub fn from_f64_slice(values: &[f64], pool: &Arc<dyn MemoryPool>) -> Result<Self> {
        let data = AlignedBuffer::from_typed_slice(values, pool)?;
        Ok(Self {
            logical_type: LogicalType::Float64,
            data,
            validity: ValidityMask::all_valid(values.len()),
            string_heap: Vec::new(),
            len: values.len(),
        })
    }

    pub fn from_bool_bytes(
        values: &[u8],
        validity: ValidityMask,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<Self> {
        assert_eq!(values.len(), validity.len());
        let data = AlignedBuffer::from_slice(values, pool)?;
        Ok(Self {
            logical_type: LogicalType::Boolean,
            data,
            validity,
            string_heap: Vec::new(),
            len: values.len(),
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn as_bool_bytes(&self) -> &[u8] {
        debug_assert_eq!(self.logical_type, LogicalType::Boolean);
        self.data.as_slice()
    }

    #[inline]
    pub fn as_i8_slice(&self) -> &[i8] {
        debug_assert_eq!(self.logical_type, LogicalType::Int8);
        self.data.as_typed_slice::<i8>()
    }

    #[inline]
    pub fn as_i16_slice(&self) -> &[i16] {
        debug_assert_eq!(self.logical_type, LogicalType::Int16);
        self.data.as_typed_slice::<i16>()
    }

    #[inline]
    pub fn as_i32_slice(&self) -> &[i32] {
        debug_assert!(matches!(
            self.logical_type,
            LogicalType::Int32 | LogicalType::Date32
        ));
        self.data.as_typed_slice::<i32>()
    }

    #[inline]
    pub fn as_i64_slice(&self) -> &[i64] {
        debug_assert!(matches!(
            self.logical_type,
            LogicalType::Int64 | LogicalType::TimestampMicros
        ));
        self.data.as_typed_slice::<i64>()
    }

    #[inline]
    pub fn as_f64_slice(&self) -> &[f64] {
        debug_assert_eq!(self.logical_type, LogicalType::Float64);
        self.data.as_typed_slice::<f64>()
    }

    #[inline]
    pub fn as_i128_slice(&self) -> &[i128] {
        debug_assert!(matches!(self.logical_type, LogicalType::Decimal128 { .. }));
        self.data.as_typed_slice::<i128>()
    }

    #[inline]
    pub fn as_byte_views(&self) -> &[ByteView] {
        debug_assert!(self.logical_type.is_varlen());
        self.data.as_typed_slice::<ByteView>()
    }

    /// Reads the scalar value at `row` (for testing, CLI display, and fallback paths).
    pub fn get_scalar(&self, row: usize) -> ScalarValue {
        assert!(row < self.len, "FlatVector row {row} >= len {}", self.len);
        if self.validity.is_null(row) {
            return ScalarValue::Null;
        }
        match self.logical_type {
            LogicalType::Null => ScalarValue::Null,
            LogicalType::Boolean => ScalarValue::Boolean(self.as_bool_bytes()[row] != 0),
            LogicalType::Int8 => ScalarValue::Int8(self.as_i8_slice()[row]),
            LogicalType::Int16 => ScalarValue::Int16(self.as_i16_slice()[row]),
            LogicalType::Int32 => ScalarValue::Int32(self.as_i32_slice()[row]),
            LogicalType::Int64 => ScalarValue::Int64(self.as_i64_slice()[row]),
            LogicalType::Float64 => ScalarValue::Float64(self.as_f64_slice()[row]),
            LogicalType::Decimal128 { precision, scale } => ScalarValue::Decimal128 {
                value: self.as_i128_slice()[row],
                precision,
                scale,
            },
            LogicalType::Date32 => ScalarValue::Date32(self.as_i32_slice()[row]),
            LogicalType::TimestampMicros => ScalarValue::TimestampMicros(self.as_i64_slice()[row]),
            LogicalType::Utf8 => {
                let bytes = self.as_byte_views()[row].as_bytes(&self.string_heap);
                let s = String::from_utf8_lossy(bytes).into_owned();
                ScalarValue::Utf8(s)
            }
            LogicalType::Binary => {
                let bytes = self.as_byte_views()[row].as_bytes(&self.string_heap);
                ScalarValue::Binary(bytes.to_vec())
            }
        }
    }

    /// Zero-copy slice of `[offset..offset + len)`.
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        assert!(
            offset <= self.len && len <= self.len - offset,
            "FlatVector::slice({offset}, {len}) OOB for len {}",
            self.len
        );
        let width = self.logical_type.fixed_byte_width();
        let data = if width == 0 {
            self.data.clone()
        } else {
            self.data.slice(offset * width, len * width)
        };
        Self {
            logical_type: self.logical_type,
            data,
            validity: self.validity.slice(offset, len),
            string_heap: self.string_heap.clone(),
            len,
        }
    }

    /// Gathers `count` elements from `self` via `sel` into a new contiguous `FlatVector`.
    pub fn gather(
        &self,
        sel: &SelectionVector,
        count: usize,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<Self> {
        assert!(
            count <= sel.len(),
            "gather count {count} > sel.len {}",
            sel.len()
        );
        if count == 0 {
            return Ok(Self::empty(self.logical_type, pool));
        }
        if sel.is_identity() && count <= self.len {
            return Ok(self.slice(0, count));
        }
        let width = self.logical_type.fixed_byte_width();
        let mut out_data = AlignedBufferMut::with_capacity(count * width, pool)?;
        let mut mask = ValidityMaskBuilder::with_capacity(count);
        let mut heap = StringHeapBuilder::new(pool);

        match self.logical_type {
            LogicalType::Null => {
                for _ in 0..count {
                    mask.push(false);
                }
            }
            LogicalType::Boolean => {
                let src = self.as_bool_bytes();
                for pos in 0..count {
                    let i = sel.get(pos);
                    let valid = self.validity.is_valid(i);
                    mask.push(valid);
                    out_data.push_typed(if valid { src[i] } else { 0u8 })?;
                }
            }
            LogicalType::Int8 => {
                let src = self.as_i8_slice();
                for pos in 0..count {
                    let i = sel.get(pos);
                    let valid = self.validity.is_valid(i);
                    mask.push(valid);
                    out_data.push_typed(if valid { src[i] } else { 0i8 })?;
                }
            }
            LogicalType::Int16 => {
                let src = self.as_i16_slice();
                for pos in 0..count {
                    let i = sel.get(pos);
                    let valid = self.validity.is_valid(i);
                    mask.push(valid);
                    out_data.push_typed(if valid { src[i] } else { 0i16 })?;
                }
            }
            LogicalType::Int32 | LogicalType::Date32 => {
                let src = self.as_i32_slice();
                for pos in 0..count {
                    let i = sel.get(pos);
                    let valid = self.validity.is_valid(i);
                    mask.push(valid);
                    out_data.push_typed(if valid { src[i] } else { 0i32 })?;
                }
            }
            LogicalType::Int64 | LogicalType::TimestampMicros => {
                let src = self.as_i64_slice();
                for pos in 0..count {
                    let i = sel.get(pos);
                    let valid = self.validity.is_valid(i);
                    mask.push(valid);
                    out_data.push_typed(if valid { src[i] } else { 0i64 })?;
                }
            }
            LogicalType::Float64 => {
                let src = self.as_f64_slice();
                for pos in 0..count {
                    let i = sel.get(pos);
                    let valid = self.validity.is_valid(i);
                    mask.push(valid);
                    out_data.push_typed(if valid { src[i] } else { 0.0f64 })?;
                }
            }
            LogicalType::Decimal128 { .. } => {
                let src = self.as_i128_slice();
                for pos in 0..count {
                    let i = sel.get(pos);
                    let valid = self.validity.is_valid(i);
                    mask.push(valid);
                    out_data.push_typed(if valid { src[i] } else { 0i128 })?;
                }
            }
            LogicalType::Utf8 | LogicalType::Binary => {
                let src = self.as_byte_views();
                for pos in 0..count {
                    let i = sel.get(pos);
                    let valid = self.validity.is_valid(i);
                    mask.push(valid);
                    if valid {
                        let bv = &src[i];
                        if bv.is_inline() {
                            out_data.push_typed(*bv)?;
                        } else {
                            let bytes = bv.as_bytes(&self.string_heap);
                            let new_bv = heap.append(bytes)?;
                            out_data.push_typed(new_bv)?;
                        }
                    } else {
                        out_data.push_typed(ByteView::empty())?;
                    }
                }
            }
        }

        Ok(Self {
            logical_type: self.logical_type,
            data: out_data.freeze(),
            validity: mask.finish(),
            string_heap: heap.finish(),
            len: count,
        })
    }

    /// Concatenates multiple `FlatVector`s of the same `LogicalType` into one contiguous `FlatVector`.
    pub fn concat(
        parts: &[&FlatVector],
        logical_type: LogicalType,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<Self> {
        let total: usize = parts.iter().map(|p| p.len).sum();
        if total == 0 {
            return Ok(Self::empty(logical_type, pool));
        }
        let width = logical_type.fixed_byte_width();
        let mut out_data = AlignedBufferMut::with_capacity(total * width, pool)?;
        let mut mask = ValidityMaskBuilder::with_capacity(total);
        let mut heap = StringHeapBuilder::new(pool);

        for part in parts {
            if part.logical_type != logical_type {
                return Err(VectraError::TypeMismatch {
                    expected: logical_type.to_string(),
                    actual: part.logical_type.to_string(),
                });
            }
            for i in 0..part.len {
                mask.push(part.validity.is_valid(i));
            }
            if logical_type.is_varlen() {
                let views = part.as_byte_views();
                for (i, bv) in views.iter().enumerate() {
                    if part.validity.is_null(i) {
                        out_data.push_typed(ByteView::empty())?;
                    } else if bv.is_inline() {
                        out_data.push_typed(*bv)?;
                    } else {
                        let new_bv = heap.append(bv.as_bytes(&part.string_heap))?;
                        out_data.push_typed(new_bv)?;
                    }
                }
            } else if width > 0 {
                out_data.extend_from_slice(part.data.as_slice())?;
            }
        }

        Ok(Self {
            logical_type,
            data: out_data.freeze(),
            validity: mask.finish(),
            string_heap: heap.finish(),
            len: total,
        })
    }

    pub fn memory_bytes(&self) -> usize {
        let heap_bytes: usize = self.string_heap.iter().map(|b| b.len()).sum();
        let mask_bytes = self.validity.raw_words().map_or(0, |w| w.len() * 8);
        self.data.len() + heap_bytes + mask_bytes
    }
}

fn push_zero_slot(buf: &mut AlignedBufferMut, ty: LogicalType) -> Result<()> {
    match ty {
        LogicalType::Null => Ok(()),
        LogicalType::Boolean | LogicalType::Int8 => buf.push_typed(0u8),
        LogicalType::Int16 => buf.push_typed(0i16),
        LogicalType::Int32 | LogicalType::Date32 => buf.push_typed(0i32),
        LogicalType::Int64 | LogicalType::TimestampMicros => buf.push_typed(0i64),
        LogicalType::Float64 => buf.push_typed(0.0f64),
        LogicalType::Decimal128 { .. } => buf.push_typed(0i128),
        LogicalType::Utf8 | LogicalType::Binary => buf.push_typed(ByteView::empty()),
    }
}

/// Polymorphic columnar vector supporting Flat, Constant (O(1) memory), and Dictionary encodings.
#[derive(Debug, Clone)]
pub enum Vector {
    Flat(FlatVector),
    Constant {
        value: ScalarValue,
        logical_type: LogicalType,
        len: usize,
    },
    Dictionary {
        dict: Arc<Vector>,
        indices: SelectionVector,
        validity: ValidityMask,
    },
}

impl Vector {
    pub fn constant(value: ScalarValue, logical_type: LogicalType, len: usize) -> Self {
        Self::Constant {
            value,
            logical_type,
            len,
        }
    }

    pub fn dictionary(dict: Arc<Vector>, indices: SelectionVector, validity: ValidityMask) -> Self {
        assert_eq!(indices.len(), validity.len());
        Self::Dictionary {
            dict,
            indices,
            validity,
        }
    }

    pub fn from_scalars(
        values: &[ScalarValue],
        logical_type: LogicalType,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<Self> {
        Ok(Self::Flat(FlatVector::from_scalars(
            values,
            logical_type,
            pool,
        )?))
    }

    #[inline]
    pub fn logical_type(&self) -> LogicalType {
        match self {
            Self::Flat(f) => f.logical_type,
            Self::Constant { logical_type, .. } => *logical_type,
            Self::Dictionary { dict, .. } => dict.logical_type(),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Self::Flat(f) => f.len,
            Self::Constant { len, .. } => *len,
            Self::Dictionary { indices, .. } => indices.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get_scalar(&self, row: usize) -> ScalarValue {
        match self {
            Self::Flat(f) => f.get_scalar(row),
            Self::Constant { value, len, .. } => {
                assert!(row < *len, "Constant vector row {row} >= len {len}");
                value.clone()
            }
            Self::Dictionary {
                dict,
                indices,
                validity,
            } => {
                assert!(
                    row < indices.len(),
                    "Dictionary row {row} >= len {}",
                    indices.len()
                );
                if validity.is_null(row) {
                    ScalarValue::Null
                } else {
                    dict.get_scalar(indices.get(row))
                }
            }
        }
    }

    pub fn slice(&self, offset: usize, len: usize) -> Self {
        assert!(offset <= self.len() && len <= self.len() - offset);
        match self {
            Self::Flat(f) => Self::Flat(f.slice(offset, len)),
            Self::Constant {
                value,
                logical_type,
                ..
            } => Self::Constant {
                value: value.clone(),
                logical_type: *logical_type,
                len,
            },
            Self::Dictionary {
                dict,
                indices,
                validity,
            } => Self::Dictionary {
                dict: Arc::clone(dict),
                indices: indices.slice(offset, len),
                validity: validity.slice(offset, len),
            },
        }
    }

    /// Materializes `count` rows indexed by `sel` into a contiguous `FlatVector`,
    /// recursively resolving nested `Dictionary` and `Constant` encodings.
    pub fn flatten(
        &self,
        sel: &SelectionVector,
        count: usize,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<FlatVector> {
        assert!(count <= sel.len());
        if count == 0 {
            return Ok(FlatVector::empty(self.logical_type(), pool));
        }
        match self {
            Self::Flat(f) => f.gather(sel, count, pool),
            Self::Constant {
                value,
                logical_type,
                ..
            } => {
                if value.is_null() {
                    let width = logical_type.fixed_byte_width();
                    let data = AlignedBufferMut::zeroed(count * width, pool)?.freeze();
                    return Ok(FlatVector {
                        logical_type: *logical_type,
                        data,
                        validity: ValidityMask::all_null(count),
                        string_heap: Vec::new(),
                        len: count,
                    });
                }
                let vals = vec![value.clone(); count];
                FlatVector::from_scalars(&vals, *logical_type, pool)
            }
            Self::Dictionary {
                dict,
                indices,
                validity,
            } => {
                let mut mask = ValidityMaskBuilder::with_capacity(count);
                let mut child_indices = Vec::with_capacity(count);
                let mut has_outer_null = false;
                for pos in 0..count {
                    let row = sel.get(pos);
                    if validity.is_null(row) {
                        mask.push(false);
                        child_indices.push(0u16);
                        has_outer_null = true;
                    } else {
                        mask.push(true);
                        let ci = indices.get(row);
                        child_indices.push(u16::try_from(ci).map_err(|_| {
                            VectraError::Internal(format!("dictionary index {ci} exceeds u16::MAX"))
                        })?);
                    }
                }
                let child_sel = SelectionVector::from_vec(child_indices);
                let mut flat = if dict.is_empty() {
                    // Only possible when all selected rows were outer-null.
                    let width = dict.logical_type().fixed_byte_width();
                    FlatVector {
                        logical_type: dict.logical_type(),
                        data: AlignedBufferMut::zeroed(count * width, pool)?.freeze(),
                        validity: ValidityMask::all_null(count),
                        string_heap: Vec::new(),
                        len: count,
                    }
                } else {
                    dict.flatten(&child_sel, count, pool)?
                };
                if has_outer_null {
                    let outer_mask = mask.finish();
                    flat.validity = flat.validity.bitand(&outer_mask);
                }
                Ok(flat)
            }
        }
    }

    pub fn memory_bytes(&self) -> usize {
        match self {
            Self::Flat(f) => f.memory_bytes(),
            Self::Constant { value, .. } => match value {
                ScalarValue::Utf8(s) => s.len() + 32,
                ScalarValue::Binary(b) => b.len() + 32,
                _ => 32,
            },
            Self::Dictionary {
                dict,
                indices,
                validity,
            } => {
                let idx_bytes = indices.as_indices().map_or(0, |s| s.len() * 2);
                let mask_bytes = validity.raw_words().map_or(0, |w| w.len() * 8);
                dict.memory_bytes() + idx_bytes + mask_bytes
            }
        }
    }
}
