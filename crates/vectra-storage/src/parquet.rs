//! From-scratch Apache Parquet v1/v2 reader and writer implementing:
//! - Thrift Compact Protocol (`TCompactProtocol`) metadata & page headers
//! - Parquet Physical Types + ConvertedTypes (UTF8, DATE, TIMESTAMP_MICROS, DECIMAL, INT_8/16/32/64)
//! - Encodings: `PLAIN`, `RLE_DICTIONARY` / `PLAIN_DICTIONARY`, `RLE` hybrid definition levels
//! - Compressions: `UNCOMPRESSED`, `SNAPPY`, `LZ4_RAW`
//! - RowGroup `Statistics` (`min_value` / `max_value` / `null_count`) pruning!

use crate::index::{BlockPruningResult, PruningPredicate, ZoneMap};
use crate::lz4::{lz4_compress_block, lz4_decompress_block};
use crate::snappy::{snappy_compress, snappy_decompress};
use std::collections::HashMap;
use std::sync::Arc;
use vectra_types::{
    DataChunk, Field, LogicalType, MemoryPool, Result, ScalarValue, Schema, Vector, VectraError,
};

const PARQUET_MAGIC: &[u8; 4] = b"PAR1";

// Parquet physical types
const PT_BOOLEAN: i32 = 0;
const PT_INT32: i32 = 1;
const PT_INT64: i32 = 2;
const PT_DOUBLE: i32 = 5;
const PT_BYTE_ARRAY: i32 = 6;
const PT_FIXED_LEN_BYTE_ARRAY: i32 = 7;

// Parquet converted types
const CT_UTF8: i32 = 0;
const CT_DECIMAL: i32 = 5;
const CT_DATE: i32 = 6;
const CT_TIMESTAMP_MICROS: i32 = 10;
const CT_INT_8: i32 = 15;
const CT_INT_16: i32 = 16;

// Parquet encodings
const ENC_PLAIN: i32 = 0;
const ENC_PLAIN_DICTIONARY: i32 = 2;
const ENC_RLE: i32 = 3;
const ENC_RLE_DICTIONARY: i32 = 8;

// Parquet page types
const PAGE_DATA_V1: i32 = 0;
const PAGE_DICTIONARY: i32 = 2;
const PAGE_DATA_V2: i32 = 3;

/// Supported Parquet compression codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParquetCompression {
    Uncompressed = 0,
    Snappy = 1,
    Lz4Raw = 7,
}

impl ParquetCompression {
    fn from_i32(v: i32) -> Result<Self> {
        match v {
            0 => Ok(Self::Uncompressed),
            1 => Ok(Self::Snappy),
            7 => Ok(Self::Lz4Raw),
            other => Err(VectraError::CorruptFormat(format!(
                "unsupported Parquet compression codec {other}"
            ))),
        }
    }

    fn compress(self, src: &[u8]) -> Vec<u8> {
        match self {
            Self::Uncompressed => src.to_vec(),
            Self::Snappy => snappy_compress(src),
            Self::Lz4Raw => lz4_compress_block(src),
        }
    }

    fn decompress(self, src: &[u8], uncompressed_len: usize) -> Result<Vec<u8>> {
        match self {
            Self::Uncompressed => {
                if src.len() < uncompressed_len {
                    return Err(VectraError::CorruptFormat(
                        "Parquet uncompressed page truncated".into(),
                    ));
                }
                Ok(src[..uncompressed_len].to_vec())
            }
            Self::Snappy => snappy_decompress(src),
            Self::Lz4Raw => lz4_decompress_block(src, uncompressed_len),
        }
    }
}

// ============================================================================
// Thrift Compact Protocol Writer & Reader
// ============================================================================

const T_BOOL_TRUE: u8 = 1;
const T_BOOL_FALSE: u8 = 2;
const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BINARY: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

struct ThriftWriter {
    buf: Vec<u8>,
    last_field_stack: Vec<i16>,
}

impl ThriftWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            last_field_stack: vec![0],
        }
    }

    fn write_uvarint(&mut self, mut n: u64) {
        while n >= 0x80 {
            self.buf.push(((n & 0x7F) | 0x80) as u8);
            n >>= 7;
        }
        self.buf.push(n as u8);
    }

    fn write_zigzag_i64(&mut self, v: i64) {
        let zz = ((v << 1) ^ (v >> 63)) as u64;
        self.write_uvarint(zz);
    }

    fn write_zigzag_i32(&mut self, v: i32) {
        let zz = ((v << 1) ^ (v >> 31)) as u32 as u64;
        self.write_uvarint(zz);
    }

    fn write_field_header(&mut self, field_id: i16, type_id: u8) {
        let last = *self.last_field_stack.last().expect("struct stack");
        let delta = field_id - last;
        if delta > 0 && delta <= 15 {
            self.buf.push(((delta as u8) << 4) | type_id);
        } else {
            self.buf.push(type_id);
            self.write_zigzag_i32(field_id as i32);
        }
        *self.last_field_stack.last_mut().expect("struct stack") = field_id;
    }

    fn begin_struct(&mut self) {
        self.last_field_stack.push(0);
    }

    fn end_struct(&mut self) {
        self.buf.push(0);
        self.last_field_stack.pop();
    }

    fn write_bool_field(&mut self, field_id: i16, v: bool) {
        self.write_field_header(field_id, if v { T_BOOL_TRUE } else { T_BOOL_FALSE });
    }

    fn write_i32_field(&mut self, field_id: i16, v: i32) {
        self.write_field_header(field_id, T_I32);
        self.write_zigzag_i32(v);
    }

    fn write_i64_field(&mut self, field_id: i16, v: i64) {
        self.write_field_header(field_id, T_I64);
        self.write_zigzag_i64(v);
    }

    fn write_binary_field(&mut self, field_id: i16, bytes: &[u8]) {
        self.write_field_header(field_id, T_BINARY);
        self.write_uvarint(bytes.len() as u64);
        self.buf.extend_from_slice(bytes);
    }

    fn write_list_header(&mut self, field_id: i16, elem_type: u8, len: usize) {
        self.write_field_header(field_id, T_LIST);
        if len < 15 {
            self.buf.push(((len as u8) << 4) | elem_type);
        } else {
            self.buf.push(0xF0 | elem_type);
            self.write_uvarint(len as u64);
        }
    }
}

struct ThriftReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ThriftReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.buf.len() {
            return Err(VectraError::CorruptFormat("Thrift unexpected EOF".into()));
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_uvarint(&mut self) -> Result<u64> {
        let mut val = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.read_u8()?;
            val |= ((b & 0x7F) as u64) << shift;
            if (b & 0x80) == 0 {
                return Ok(val);
            }
            shift += 7;
            if shift >= 70 {
                return Err(VectraError::CorruptFormat("Thrift varint overflow".into()));
            }
        }
    }

    fn read_zigzag_i64(&mut self) -> Result<i64> {
        let u = self.read_uvarint()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }

    fn read_zigzag_i32(&mut self) -> Result<i32> {
        Ok(self.read_zigzag_i64()? as i32)
    }

    fn read_binary(&mut self) -> Result<Vec<u8>> {
        let len = self.read_uvarint()? as usize;
        if self.pos + len > self.buf.len() {
            return Err(VectraError::CorruptFormat("Thrift binary field OOB".into()));
        }
        let out = self.buf[self.pos..self.pos + len].to_vec();
        self.pos += len;
        Ok(out)
    }

    fn read_string(&mut self) -> Result<String> {
        let b = self.read_binary()?;
        String::from_utf8(b)
            .map_err(|e| VectraError::CorruptFormat(format!("Thrift invalid UTF-8: {e}")))
    }

    /// Reads next field `(field_id, type_id)`, or `None` on end-of-struct `0x00`.
    fn read_field_header(&mut self, last_field_id: &mut i16) -> Result<Option<(i16, u8)>> {
        let b = self.read_u8()?;
        if b == 0 {
            return Ok(None);
        }
        let type_id = b & 0x0F;
        let delta = (b >> 4) as i16;
        let field_id = if delta == 0 {
            self.read_zigzag_i32()? as i16
        } else {
            *last_field_id + delta
        };
        *last_field_id = field_id;
        Ok(Some((field_id, type_id)))
    }

    fn read_list_header(&mut self) -> Result<(u8, usize)> {
        let b = self.read_u8()?;
        let elem_type = b & 0x0F;
        let size_nibble = (b >> 4) as usize;
        let len = if size_nibble == 15 {
            self.read_uvarint()? as usize
        } else {
            size_nibble
        };
        Ok((elem_type, len))
    }

    fn skip_value(&mut self, type_id: u8) -> Result<()> {
        match type_id {
            1 | 2 => Ok(()),
            3 => {
                let _ = self.read_u8()?;
                Ok(())
            }
            4..=6 => {
                let _ = self.read_uvarint()?;
                Ok(())
            }
            7 => {
                if self.pos + 8 > self.buf.len() {
                    return Err(VectraError::CorruptFormat("Thrift double EOF".into()));
                }
                self.pos += 8;
                Ok(())
            }
            8 => {
                let _ = self.read_binary()?;
                Ok(())
            }
            9 | 10 => {
                let (et, len) = self.read_list_header()?;
                for _ in 0..len {
                    self.skip_value(et)?;
                }
                Ok(())
            }
            11 => {
                let len = self.read_uvarint()? as usize;
                if len > 0 {
                    let kv = self.read_u8()?;
                    let kt = kv >> 4;
                    let vt = kv & 0x0F;
                    for _ in 0..len {
                        self.skip_value(kt)?;
                        self.skip_value(vt)?;
                    }
                }
                Ok(())
            }
            12 => {
                let mut last = 0i16;
                while let Some((_, ft)) = self.read_field_header(&mut last)? {
                    self.skip_value(ft)?;
                }
                Ok(())
            }
            other => Err(VectraError::CorruptFormat(format!(
                "unknown Thrift type {other}"
            ))),
        }
    }
}

// ============================================================================
// Parquet RLE / Bit-Packing Hybrid Codec (Definition Levels & Dictionary Codes)
// ============================================================================

fn encode_rle_hybrid(vals: &[u32], bit_width: u8) -> Vec<u8> {
    let mut out = Vec::new();
    if vals.is_empty() || bit_width == 0 {
        return out;
    }
    let byte_width = (bit_width as usize).div_ceil(8);
    let mut i = 0usize;
    while i < vals.len() {
        let v = vals[i];
        let mut run = 1usize;
        while i + run < vals.len() && vals[i + run] == v {
            run += 1;
        }
        // Emit RLE run (header = (run << 1) | 0)
        write_uvarint_vec((run as u64) << 1, &mut out);
        let vb = v.to_le_bytes();
        out.extend_from_slice(&vb[..byte_width]);
        i += run;
    }
    out
}

fn decode_rle_hybrid(src: &[u8], bit_width: u8, count: usize) -> Result<Vec<u32>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if bit_width == 0 {
        return Ok(vec![0u32; count]);
    }
    let bw = bit_width as u32;
    let byte_width = (bit_width as usize).div_ceil(8);
    let mask = if bw == 32 { u32::MAX } else { (1u32 << bw) - 1 };

    let mut out = Vec::with_capacity(count);
    let mut pos = 0usize;

    while out.len() < count {
        let header = read_uvarint_slice(src, &mut pos)?;
        if (header & 1) == 0 {
            // RLE run
            let run = (header >> 1) as usize;
            if pos + byte_width > src.len() {
                return Err(VectraError::CorruptFormat(
                    "Parquet RLE run truncated".into(),
                ));
            }
            let mut vb = [0u8; 4];
            vb[..byte_width].copy_from_slice(&src[pos..pos + byte_width]);
            pos += byte_width;
            let val = u32::from_le_bytes(vb) & mask;
            let take = run.min(count - out.len());
            out.extend(std::iter::repeat_n(val, take));
        } else {
            // Bit-packed run: header >> 1 is number of 8-value groups
            let num_groups = (header >> 1) as usize;
            let total_vals = num_groups * 8;
            let bytes_needed = (total_vals * bit_width as usize).div_ceil(8);
            if pos + bytes_needed > src.len() {
                return Err(VectraError::CorruptFormat(
                    "Parquet bit-packed run truncated".into(),
                ));
            }
            let chunk = &src[pos..pos + bytes_needed];
            pos += bytes_needed;
            let mut acc = 0u64;
            let mut acc_bits = 0u32;
            let mut bidx = 0usize;
            for _ in 0..total_vals {
                while acc_bits < bw && bidx < chunk.len() {
                    acc |= (chunk[bidx] as u64) << acc_bits;
                    bidx += 1;
                    acc_bits += 8;
                }
                if out.len() < count {
                    out.push((acc as u32) & mask);
                }
                acc >>= bw;
                acc_bits = acc_bits.saturating_sub(bw);
            }
        }
    }
    Ok(out)
}

fn write_uvarint_vec(mut n: u64, out: &mut Vec<u8>) {
    while n >= 0x80 {
        out.push(((n & 0x7F) | 0x80) as u8);
        n >>= 7;
    }
    out.push(n as u8);
}

fn read_uvarint_slice(src: &[u8], pos: &mut usize) -> Result<u64> {
    let mut val = 0u64;
    let mut shift = 0u32;
    loop {
        if *pos >= src.len() || shift >= 70 {
            return Err(VectraError::CorruptFormat("Parquet uvarint EOF".into()));
        }
        let b = src[*pos];
        *pos += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            return Ok(val);
        }
        shift += 7;
    }
}

// ============================================================================
// Parquet Physical Type <-> VectraDB LogicalType Mapping & Plain Codec
// ============================================================================

fn logical_to_parquet_meta(
    ty: LogicalType,
) -> (i32, Option<i32>, Option<i32>, Option<i32>, Option<i32>) {
    // Returns (physical_type, converted_type, type_length, precision, scale)
    match ty {
        LogicalType::Boolean => (PT_BOOLEAN, None, None, None, None),
        LogicalType::Int8 => (PT_INT32, Some(CT_INT_8), None, None, None),
        LogicalType::Int16 => (PT_INT32, Some(CT_INT_16), None, None, None),
        LogicalType::Int32 => (PT_INT32, None, None, None, None),
        LogicalType::Date32 => (PT_INT32, Some(CT_DATE), None, None, None),
        LogicalType::Int64 => (PT_INT64, None, None, None, None),
        LogicalType::TimestampMicros => (PT_INT64, Some(CT_TIMESTAMP_MICROS), None, None, None),
        LogicalType::Float64 => (PT_DOUBLE, None, None, None, None),
        LogicalType::Decimal128 { precision, scale } => (
            PT_FIXED_LEN_BYTE_ARRAY,
            Some(CT_DECIMAL),
            Some(16),
            Some(precision as i32),
            Some(scale as i32),
        ),
        LogicalType::Utf8 => (PT_BYTE_ARRAY, Some(CT_UTF8), None, None, None),
        LogicalType::Binary => (PT_BYTE_ARRAY, None, None, None, None),
        LogicalType::Null => (PT_INT32, None, None, None, None),
    }
}

fn parquet_meta_to_logical(
    pt: i32,
    ct: Option<i32>,
    precision: Option<i32>,
    scale: Option<i32>,
) -> LogicalType {
    match (pt, ct) {
        (PT_BOOLEAN, _) => LogicalType::Boolean,
        (PT_INT32, Some(CT_INT_8)) => LogicalType::Int8,
        (PT_INT32, Some(CT_INT_16)) => LogicalType::Int16,
        (PT_INT32, Some(CT_DATE)) => LogicalType::Date32,
        (PT_INT32, _) => LogicalType::Int32,
        (PT_INT64, Some(CT_TIMESTAMP_MICROS)) => LogicalType::TimestampMicros,
        (PT_INT64, _) => LogicalType::Int64,
        (PT_DOUBLE, _) => LogicalType::Float64,
        (PT_FIXED_LEN_BYTE_ARRAY, Some(CT_DECIMAL)) => LogicalType::Decimal128 {
            precision: precision.unwrap_or(38) as u8,
            scale: scale.unwrap_or(0) as i8,
        },
        (PT_BYTE_ARRAY, Some(CT_UTF8)) => LogicalType::Utf8,
        (PT_BYTE_ARRAY, _) | (PT_FIXED_LEN_BYTE_ARRAY, _) => LogicalType::Binary,
        _ => LogicalType::Binary,
    }
}

fn encode_plain_values(vals: &[ScalarValue], ty: LogicalType) -> Vec<u8> {
    let mut out = Vec::new();
    match ty {
        LogicalType::Boolean => {
            let mut cur = 0u8;
            let mut bit = 0u8;
            for v in vals {
                if let ScalarValue::Boolean(b) = v {
                    if *b {
                        cur |= 1 << bit;
                    }
                    bit += 1;
                    if bit == 8 {
                        out.push(cur);
                        cur = 0;
                        bit = 0;
                    }
                }
            }
            if bit > 0 {
                out.push(cur);
            }
        }
        LogicalType::Int8 => {
            for v in vals {
                if let ScalarValue::Int8(x) = v {
                    out.extend_from_slice(&(*x as i32).to_le_bytes());
                }
            }
        }
        LogicalType::Int16 => {
            for v in vals {
                if let ScalarValue::Int16(x) = v {
                    out.extend_from_slice(&(*x as i32).to_le_bytes());
                }
            }
        }
        LogicalType::Int32 | LogicalType::Date32 => {
            for v in vals {
                match v {
                    ScalarValue::Int32(x) | ScalarValue::Date32(x) => {
                        out.extend_from_slice(&x.to_le_bytes());
                    }
                    _ => {}
                }
            }
        }
        LogicalType::Int64 | LogicalType::TimestampMicros => {
            for v in vals {
                match v {
                    ScalarValue::Int64(x) | ScalarValue::TimestampMicros(x) => {
                        out.extend_from_slice(&x.to_le_bytes());
                    }
                    _ => {}
                }
            }
        }
        LogicalType::Float64 => {
            for v in vals {
                if let ScalarValue::Float64(x) = v {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        LogicalType::Decimal128 { .. } => {
            // Parquet Decimal FIXED_LEN_BYTE_ARRAY(16) is two's complement big-endian!
            for v in vals {
                if let ScalarValue::Decimal128 { value, .. } = v {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
        }
        LogicalType::Utf8 => {
            for v in vals {
                if let ScalarValue::Utf8(s) = v {
                    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
            }
        }
        LogicalType::Binary => {
            for v in vals {
                if let ScalarValue::Binary(b) = v {
                    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    out.extend_from_slice(b);
                }
            }
        }
        LogicalType::Null => {}
    }
    out
}

fn decode_plain_values(
    src: &[u8],
    count: usize,
    ty: LogicalType,
    fixed_len: usize,
) -> Result<Vec<ScalarValue>> {
    let mut out = Vec::with_capacity(count);
    let mut pos = 0usize;
    match ty {
        LogicalType::Boolean => {
            for i in 0..count {
                let byte_i = i / 8;
                if byte_i >= src.len() {
                    return Err(VectraError::CorruptFormat("Parquet PLAIN bool EOF".into()));
                }
                let bit = (src[byte_i] >> (i % 8)) & 1;
                out.push(ScalarValue::Boolean(bit != 0));
            }
        }
        LogicalType::Int8 => {
            for _ in 0..count {
                let v = i32::from_le_bytes(read_fixed::<4>(src, &mut pos)?);
                out.push(ScalarValue::Int8(v as i8));
            }
        }
        LogicalType::Int16 => {
            for _ in 0..count {
                let v = i32::from_le_bytes(read_fixed::<4>(src, &mut pos)?);
                out.push(ScalarValue::Int16(v as i16));
            }
        }
        LogicalType::Int32 => {
            for _ in 0..count {
                out.push(ScalarValue::Int32(i32::from_le_bytes(read_fixed::<4>(
                    src, &mut pos,
                )?)));
            }
        }
        LogicalType::Date32 => {
            for _ in 0..count {
                out.push(ScalarValue::Date32(i32::from_le_bytes(read_fixed::<4>(
                    src, &mut pos,
                )?)));
            }
        }
        LogicalType::Int64 => {
            for _ in 0..count {
                out.push(ScalarValue::Int64(i64::from_le_bytes(read_fixed::<8>(
                    src, &mut pos,
                )?)));
            }
        }
        LogicalType::TimestampMicros => {
            for _ in 0..count {
                out.push(ScalarValue::TimestampMicros(i64::from_le_bytes(
                    read_fixed::<8>(src, &mut pos)?,
                )));
            }
        }
        LogicalType::Float64 => {
            for _ in 0..count {
                out.push(ScalarValue::Float64(f64::from_le_bytes(read_fixed::<8>(
                    src, &mut pos,
                )?)));
            }
        }
        LogicalType::Decimal128 { precision, scale } => {
            let flen = if fixed_len == 0 { 16 } else { fixed_len };
            for _ in 0..count {
                if pos + flen > src.len() {
                    return Err(VectraError::CorruptFormat("Parquet Decimal EOF".into()));
                }
                let slice = &src[pos..pos + flen];
                pos += flen;
                let value = decode_be_signed_i128(slice);
                out.push(ScalarValue::Decimal128 {
                    value,
                    precision,
                    scale,
                });
            }
        }
        LogicalType::Utf8 => {
            for _ in 0..count {
                let len = u32::from_le_bytes(read_fixed::<4>(src, &mut pos)?) as usize;
                if pos + len > src.len() {
                    return Err(VectraError::CorruptFormat("Parquet UTF8 EOF".into()));
                }
                let s = String::from_utf8_lossy(&src[pos..pos + len]).into_owned();
                pos += len;
                out.push(ScalarValue::Utf8(s));
            }
        }
        LogicalType::Binary => {
            for _ in 0..count {
                let len = u32::from_le_bytes(read_fixed::<4>(src, &mut pos)?) as usize;
                if pos + len > src.len() {
                    return Err(VectraError::CorruptFormat("Parquet Binary EOF".into()));
                }
                out.push(ScalarValue::Binary(src[pos..pos + len].to_vec()));
                pos += len;
            }
        }
        LogicalType::Null => {
            out.resize(count, ScalarValue::Null);
        }
    }
    Ok(out)
}

fn read_fixed<const N: usize>(src: &[u8], pos: &mut usize) -> Result<[u8; N]> {
    if *pos + N > src.len() {
        return Err(VectraError::CorruptFormat("Parquet PLAIN value EOF".into()));
    }
    let arr: [u8; N] = src[*pos..*pos + N].try_into().unwrap();
    *pos += N;
    Ok(arr)
}

fn decode_be_signed_i128(bytes: &[u8]) -> i128 {
    let neg = !bytes.is_empty() && (bytes[0] & 0x80) != 0;
    let mut full = [if neg { 0xFF } else { 0x00 }; 16];
    let take = bytes.len().min(16);
    full[16 - take..].copy_from_slice(&bytes[bytes.len() - take..]);
    i128::from_be_bytes(full)
}

fn encode_stat_scalar(val: &ScalarValue, ty: LogicalType) -> Vec<u8> {
    match (ty, val) {
        (LogicalType::Boolean, ScalarValue::Boolean(b)) => vec![u8::from(*b)],
        (LogicalType::Int8, ScalarValue::Int8(x)) => (*x as i32).to_le_bytes().to_vec(),
        (LogicalType::Int16, ScalarValue::Int16(x)) => (*x as i32).to_le_bytes().to_vec(),
        (LogicalType::Int32, ScalarValue::Int32(x))
        | (LogicalType::Date32, ScalarValue::Date32(x)) => x.to_le_bytes().to_vec(),
        (LogicalType::Int64, ScalarValue::Int64(x))
        | (LogicalType::TimestampMicros, ScalarValue::TimestampMicros(x)) => {
            x.to_le_bytes().to_vec()
        }
        (LogicalType::Float64, ScalarValue::Float64(x)) => x.to_le_bytes().to_vec(),
        (LogicalType::Decimal128 { .. }, ScalarValue::Decimal128 { value, .. }) => {
            value.to_be_bytes().to_vec()
        }
        (LogicalType::Utf8, ScalarValue::Utf8(s)) => s.as_bytes().to_vec(),
        (LogicalType::Binary, ScalarValue::Binary(b)) => b.clone(),
        _ => Vec::new(),
    }
}

fn decode_stat_scalar(bytes: &[u8], ty: LogicalType) -> Option<ScalarValue> {
    match ty {
        LogicalType::Boolean if !bytes.is_empty() => Some(ScalarValue::Boolean(bytes[0] != 0)),
        LogicalType::Int8 if bytes.len() == 4 => Some(ScalarValue::Int8(i32::from_le_bytes(
            bytes.try_into().ok()?,
        ) as i8)),
        LogicalType::Int16 if bytes.len() == 4 => Some(ScalarValue::Int16(i32::from_le_bytes(
            bytes.try_into().ok()?,
        ) as i16)),
        LogicalType::Int32 if bytes.len() == 4 => Some(ScalarValue::Int32(i32::from_le_bytes(
            bytes.try_into().ok()?,
        ))),
        LogicalType::Date32 if bytes.len() == 4 => Some(ScalarValue::Date32(i32::from_le_bytes(
            bytes.try_into().ok()?,
        ))),
        LogicalType::Int64 if bytes.len() == 8 => Some(ScalarValue::Int64(i64::from_le_bytes(
            bytes.try_into().ok()?,
        ))),
        LogicalType::TimestampMicros if bytes.len() == 8 => Some(ScalarValue::TimestampMicros(
            i64::from_le_bytes(bytes.try_into().ok()?),
        )),
        LogicalType::Float64 if bytes.len() == 8 => Some(ScalarValue::Float64(f64::from_le_bytes(
            bytes.try_into().ok()?,
        ))),
        LogicalType::Decimal128 { precision, scale } if !bytes.is_empty() => {
            Some(ScalarValue::Decimal128 {
                value: decode_be_signed_i128(bytes),
                precision,
                scale,
            })
        }
        LogicalType::Utf8 => Some(ScalarValue::Utf8(
            String::from_utf8_lossy(bytes).into_owned(),
        )),
        LogicalType::Binary => Some(ScalarValue::Binary(bytes.to_vec())),
        _ => None,
    }
}

// ============================================================================
// Native Parquet v2 Writer (DATA_PAGE_V2 + RLE_DICTIONARY + Statistics)
// ============================================================================

struct WrittenColChunk {
    col_name: String,
    ty: LogicalType,
    codec: ParquetCompression,
    num_values: i64,
    total_uncomp: i64,
    total_comp: i64,
    dict_page_offset: Option<i64>,
    data_page_offset: i64,
    encodings: Vec<i32>,
    zone_map: ZoneMap,
}

/// Writes `chunks` with `schema` into a spec-compliant Apache Parquet v2 byte buffer
/// featuring `DATA_PAGE_V2`, optional `RLE_DICTIONARY`, `Statistics` min/max/null_count,
/// and selectable compression (`Uncompressed`, `Snappy`, or `Lz4Raw`).
pub fn write_parquet_bytes(
    schema: &Schema,
    chunks: &[DataChunk],
    compression: ParquetCompression,
    use_dictionary: bool,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(PARQUET_MAGIC);

    let mut total_rows = 0i64;
    let mut written_rgs: Vec<(i64, i64, Vec<WrittenColChunk>)> = Vec::new();

    for chunk in chunks {
        let n = chunk.len();
        if n == 0 {
            continue;
        }
        total_rows += n as i64;
        let mut rg_cols = Vec::with_capacity(schema.len());
        let mut rg_byte_size = 0i64;

        for (c, field) in schema.fields.iter().enumerate() {
            let ty = field.logical_type;
            let mut def_levels = Vec::with_capacity(n);
            let mut non_null_vals = Vec::new();
            let mut null_count = 0u32;
            let mut zm = ZoneMap::empty();
            zm.row_count = n as u32;

            for r in 0..n {
                let phys = chunk.sel.get(r);
                let val = chunk.column(c).get_scalar(phys);
                if val.is_null() {
                    def_levels.push(0u32);
                    null_count += 1;
                } else {
                    def_levels.push(1u32);
                    if !matches!(val, ScalarValue::Float64(f) if f.is_nan()) {
                        zm.min = Some(match zm.min.take() {
                            None => val.clone(),
                            Some(cur) => {
                                if val.total_cmp(&cur).is_lt() {
                                    val.clone()
                                } else {
                                    cur
                                }
                            }
                        });
                        zm.max = Some(match zm.max.take() {
                            None => val.clone(),
                            Some(cur) => {
                                if val.total_cmp(&cur).is_gt() {
                                    val.clone()
                                } else {
                                    cur
                                }
                            }
                        });
                    }
                    non_null_vals.push(val);
                }
            }
            zm.null_count = null_count;

            let def_bytes = encode_rle_hybrid(&def_levels, 1);

            let mut dict_page_offset = None;
            let mut total_uncomp = 0i64;
            let mut total_comp = 0i64;
            let mut encodings = vec![ENC_RLE];

            let (data_encoding, raw_data_payload) = if use_dictionary && !non_null_vals.is_empty() {
                let mut dict_map: HashMap<ScalarValue, u32> = HashMap::new();
                let mut dict_vals: Vec<ScalarValue> = Vec::new();
                let mut codes: Vec<u32> = Vec::with_capacity(non_null_vals.len());
                for v in &non_null_vals {
                    if let Some(&id) = dict_map.get(v) {
                        codes.push(id);
                    } else {
                        let id = dict_vals.len() as u32;
                        dict_map.insert(v.clone(), id);
                        dict_vals.push(v.clone());
                        codes.push(id);
                    }
                }
                if dict_vals.len() <= (non_null_vals.len() / 2).max(1) {
                    let dict_raw = encode_plain_values(&dict_vals, ty);
                    let dict_comp = compression.compress(&dict_raw);
                    let off = out.len() as i64;
                    dict_page_offset = Some(off);

                    let mut ph = ThriftWriter::new();
                    ph.write_i32_field(1, PAGE_DICTIONARY);
                    ph.write_i32_field(2, dict_raw.len() as i32);
                    ph.write_i32_field(3, dict_comp.len() as i32);
                    ph.write_field_header(7, T_STRUCT);
                    ph.begin_struct();
                    ph.write_i32_field(1, dict_vals.len() as i32);
                    ph.write_i32_field(2, ENC_PLAIN);
                    ph.end_struct();
                    ph.end_struct();

                    let hdr_len = ph.buf.len() as i64;
                    out.extend_from_slice(&ph.buf);
                    out.extend_from_slice(&dict_comp);
                    total_uncomp += hdr_len + dict_raw.len() as i64;
                    total_comp += hdr_len + dict_comp.len() as i64;

                    let bw = if dict_vals.len() <= 1 {
                        0u8
                    } else {
                        (32 - ((dict_vals.len() - 1) as u32).leading_zeros()) as u8
                    };
                    let mut encoded_codes = vec![bw];
                    encoded_codes.extend_from_slice(&encode_rle_hybrid(&codes, bw));
                    encodings.push(ENC_RLE_DICTIONARY);
                    (ENC_RLE_DICTIONARY, encoded_codes)
                } else {
                    encodings.push(ENC_PLAIN);
                    (ENC_PLAIN, encode_plain_values(&non_null_vals, ty))
                }
            } else {
                encodings.push(ENC_PLAIN);
                (ENC_PLAIN, encode_plain_values(&non_null_vals, ty))
            };

            let comp_data = compression.compress(&raw_data_payload);
            let uncomp_page_size = def_bytes.len() + raw_data_payload.len();
            let comp_page_size = def_bytes.len() + comp_data.len();

            let data_page_offset = out.len() as i64;
            let mut ph = ThriftWriter::new();
            ph.write_i32_field(1, PAGE_DATA_V2);
            ph.write_i32_field(2, uncomp_page_size as i32);
            ph.write_i32_field(3, comp_page_size as i32);
            ph.write_field_header(8, T_STRUCT);
            ph.begin_struct();
            ph.write_i32_field(1, n as i32); // num_values
            ph.write_i32_field(2, null_count as i32); // num_nulls
            ph.write_i32_field(3, n as i32); // num_rows
            ph.write_i32_field(4, data_encoding);
            ph.write_i32_field(5, def_bytes.len() as i32);
            ph.write_i32_field(6, 0); // repetition_levels_byte_length
            ph.write_bool_field(7, compression != ParquetCompression::Uncompressed);
            ph.end_struct();
            ph.end_struct();

            let hdr_len = ph.buf.len() as i64;
            out.extend_from_slice(&ph.buf);
            out.extend_from_slice(&def_bytes);
            out.extend_from_slice(&comp_data);

            total_uncomp += hdr_len + uncomp_page_size as i64;
            total_comp += hdr_len + comp_page_size as i64;
            rg_byte_size += total_uncomp;

            rg_cols.push(WrittenColChunk {
                col_name: field.name.clone(),
                ty,
                codec: compression,
                num_values: n as i64,
                total_uncomp,
                total_comp,
                dict_page_offset,
                data_page_offset,
                encodings,
                zone_map: zm,
            });
        }
        written_rgs.push((n as i64, rg_byte_size, rg_cols));
    }

    // Write Thrift FileMetaData footer
    let mut fm = ThriftWriter::new();
    fm.write_i32_field(1, 2); // Parquet version 2
                              // SchemaElement list: root + leaf columns
    fm.write_list_header(2, T_STRUCT, 1 + schema.len());
    // Root schema element
    fm.begin_struct();
    fm.write_binary_field(4, b"vectradb_schema");
    fm.write_i32_field(5, schema.len() as i32);
    fm.end_struct();
    for field in &schema.fields {
        let (pt, ct, tlen, prec, scale) = logical_to_parquet_meta(field.logical_type);
        fm.begin_struct();
        fm.write_i32_field(1, pt);
        if let Some(l) = tlen {
            fm.write_i32_field(2, l);
        }
        fm.write_i32_field(3, 1); // OPTIONAL (nullable)
        fm.write_binary_field(4, field.name.as_bytes());
        if let Some(c) = ct {
            fm.write_i32_field(6, c);
        }
        if let Some(s) = scale {
            fm.write_i32_field(7, s);
        }
        if let Some(p) = prec {
            fm.write_i32_field(8, p);
        }
        fm.end_struct();
    }
    fm.write_i64_field(3, total_rows);

    // RowGroups
    fm.write_list_header(4, T_STRUCT, written_rgs.len());
    for (rg_rows, rg_size, cols) in &written_rgs {
        fm.begin_struct();
        fm.write_list_header(1, T_STRUCT, cols.len());
        for col in cols {
            let (pt, _, _, _, _) = logical_to_parquet_meta(col.ty);
            fm.begin_struct();
            fm.write_i64_field(2, col.dict_page_offset.unwrap_or(col.data_page_offset));
            fm.write_field_header(3, T_STRUCT);
            fm.begin_struct();
            fm.write_i32_field(1, pt);
            fm.write_list_header(2, T_I32, col.encodings.len());
            for &e in &col.encodings {
                fm.write_zigzag_i32(e);
            }
            fm.write_list_header(3, T_BINARY, 1);
            fm.write_uvarint(col.col_name.len() as u64);
            fm.buf.extend_from_slice(col.col_name.as_bytes());
            fm.write_i32_field(4, col.codec as i32);
            fm.write_i64_field(5, col.num_values);
            fm.write_i64_field(6, col.total_uncomp);
            fm.write_i64_field(7, col.total_comp);
            fm.write_i64_field(9, col.data_page_offset);
            if let Some(doff) = col.dict_page_offset {
                fm.write_i64_field(11, doff);
            }
            // Statistics (field 12)
            fm.write_field_header(12, T_STRUCT);
            fm.begin_struct();
            fm.write_i64_field(3, col.zone_map.null_count as i64);
            if let Some(max_v) = &col.zone_map.max {
                let b = encode_stat_scalar(max_v, col.ty);
                if !b.is_empty() {
                    fm.write_binary_field(5, &b);
                }
            }
            if let Some(min_v) = &col.zone_map.min {
                let b = encode_stat_scalar(min_v, col.ty);
                if !b.is_empty() {
                    fm.write_binary_field(6, &b);
                }
            }
            fm.end_struct();
            fm.end_struct(); // ColumnMetaData
            fm.end_struct(); // ColumnChunk
        }
        fm.write_i64_field(2, *rg_size);
        fm.write_i64_field(3, *rg_rows);
        fm.end_struct(); // RowGroup
    }
    fm.write_binary_field(6, b"VectraDB Native Parquet v2 Writer");
    fm.end_struct(); // FileMetaData

    let footer_len = fm.buf.len() as u32;
    out.extend_from_slice(&fm.buf);
    out.extend_from_slice(&footer_len.to_le_bytes());
    out.extend_from_slice(PARQUET_MAGIC);
    Ok(out)
}

// ============================================================================
// Native Parquet v1/v2 Reader with RowGroup Statistics Pruning
// ============================================================================

#[derive(Debug, Clone)]
struct ParquetColSchema {
    name: String,
    logical_type: LogicalType,
    fixed_len: usize,
    optional: bool,
}

#[derive(Debug, Clone)]
struct ParquetColChunkMeta {
    codec: ParquetCompression,
    num_values: usize,
    data_page_offset: usize,
    dict_page_offset: Option<usize>,
    total_compressed_size: usize,
    zone_map: ZoneMap,
}

#[derive(Debug, Clone)]
struct ParquetRowGroupMeta {
    num_rows: usize,
    columns: Vec<ParquetColChunkMeta>,
}

/// Telemetry returned from a native Parquet scan.
#[derive(Debug, Default, Clone, Copy)]
pub struct ParquetScanStats {
    pub row_groups_total: usize,
    pub row_groups_pruned: usize,
    pub rows_read: usize,
}

/// Reads a Parquet byte buffer, returning `(Schema, Vec<DataChunk>, ParquetScanStats)`.
/// Supports projection pruning and RowGroup min/max statistics pruning!
pub fn read_parquet_bytes(
    bytes: &[u8],
    projection: Option<&[usize]>,
    pruning: Option<&PruningPredicate>,
    pool: &Arc<dyn MemoryPool>,
) -> Result<(Schema, Vec<DataChunk>, ParquetScanStats)> {
    if bytes.len() < 12
        || &bytes[0..4] != PARQUET_MAGIC
        || &bytes[bytes.len() - 4..] != PARQUET_MAGIC
    {
        return Err(VectraError::CorruptFormat(
            "invalid Parquet PAR1 magic header/footer".into(),
        ));
    }
    let footer_len =
        u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().unwrap()) as usize;
    if footer_len + 12 > bytes.len() {
        return Err(VectraError::CorruptFormat(
            "Parquet footer length exceeds file size".into(),
        ));
    }
    let footer_slice = &bytes[bytes.len() - 8 - footer_len..bytes.len() - 8];
    let (col_schemas, row_groups) = parse_file_metadata(footer_slice)?;

    let full_schema = Schema::new(
        col_schemas
            .iter()
            .map(|c| Field::new(c.name.clone(), c.logical_type, c.optional))
            .collect(),
    );
    let proj_cols: Vec<usize> = match projection {
        Some(p) => p.to_vec(),
        None => (0..col_schemas.len()).collect(),
    };
    let out_schema = Schema::new(
        proj_cols
            .iter()
            .map(|&idx| full_schema.field(idx).clone())
            .collect(),
    );

    let mut stats = ParquetScanStats::default();
    let mut chunks = Vec::new();

    for rg in &row_groups {
        stats.row_groups_total += 1;
        if let Some(pred) = pruning {
            let zms: Vec<ZoneMap> = rg.columns.iter().map(|c| c.zone_map.clone()).collect();
            if pred.evaluate(&zms, None) == BlockPruningResult::AlwaysFalse {
                stats.row_groups_pruned += 1;
                continue;
            }
        }
        if rg.num_rows == 0 {
            continue;
        }
        let mut vecs = Vec::with_capacity(proj_cols.len());
        for &col_idx in &proj_cols {
            let cs = &col_schemas[col_idx];
            let cm = &rg.columns[col_idx];
            let vals = decode_parquet_column_chunk(bytes, cs, cm)?;
            vecs.push(Vector::from_scalars(&vals, cs.logical_type, pool)?);
        }
        stats.rows_read += rg.num_rows;
        chunks.push(DataChunk::new(vecs, rg.num_rows));
    }

    Ok((out_schema, chunks, stats))
}

fn parse_file_metadata(footer: &[u8]) -> Result<(Vec<ParquetColSchema>, Vec<ParquetRowGroupMeta>)> {
    let mut r = ThriftReader::new(footer);
    let mut last = 0i16;
    let mut col_schemas = Vec::new();
    let mut raw_rgs = Vec::new();

    while let Some((fid, tid)) = r.read_field_header(&mut last)? {
        match fid {
            2 => {
                // schema: list<SchemaElement>
                let (_, len) = r.read_list_header()?;
                for idx in 0..len {
                    let elem = parse_schema_element(&mut r)?;
                    if idx > 0 && elem.num_children.is_none() {
                        let lt = parquet_meta_to_logical(
                            elem.phys_type.unwrap_or(PT_BYTE_ARRAY),
                            elem.converted_type,
                            elem.precision,
                            elem.scale,
                        );
                        col_schemas.push(ParquetColSchema {
                            name: elem.name,
                            logical_type: lt,
                            fixed_len: elem.type_length.unwrap_or(0) as usize,
                            optional: elem.repetition_type.unwrap_or(1) != 0,
                        });
                    }
                }
            }
            4 => {
                // row_groups: list<RowGroup>
                let (_, len) = r.read_list_header()?;
                for _ in 0..len {
                    raw_rgs.push(parse_row_group(&mut r, &col_schemas)?);
                }
            }
            _ => r.skip_value(tid)?,
        }
    }
    Ok((col_schemas, raw_rgs))
}

struct RawSchemaElement {
    phys_type: Option<i32>,
    type_length: Option<i32>,
    repetition_type: Option<i32>,
    name: String,
    num_children: Option<i32>,
    converted_type: Option<i32>,
    scale: Option<i32>,
    precision: Option<i32>,
}

fn parse_schema_element(r: &mut ThriftReader<'_>) -> Result<RawSchemaElement> {
    let mut last = 0i16;
    let mut e = RawSchemaElement {
        phys_type: None,
        type_length: None,
        repetition_type: None,
        name: String::new(),
        num_children: None,
        converted_type: None,
        scale: None,
        precision: None,
    };
    while let Some((fid, tid)) = r.read_field_header(&mut last)? {
        match fid {
            1 => e.phys_type = Some(r.read_zigzag_i32()?),
            2 => e.type_length = Some(r.read_zigzag_i32()?),
            3 => e.repetition_type = Some(r.read_zigzag_i32()?),
            4 => e.name = r.read_string()?,
            5 => e.num_children = Some(r.read_zigzag_i32()?),
            6 => e.converted_type = Some(r.read_zigzag_i32()?),
            7 => e.scale = Some(r.read_zigzag_i32()?),
            8 => e.precision = Some(r.read_zigzag_i32()?),
            _ => r.skip_value(tid)?,
        }
    }
    Ok(e)
}

fn parse_row_group(
    r: &mut ThriftReader<'_>,
    schemas: &[ParquetColSchema],
) -> Result<ParquetRowGroupMeta> {
    let mut last = 0i16;
    let mut num_rows = 0usize;
    let mut columns = Vec::new();
    while let Some((fid, tid)) = r.read_field_header(&mut last)? {
        match fid {
            1 => {
                let (_, len) = r.read_list_header()?;
                for c in 0..len {
                    let ty = schemas
                        .get(c)
                        .map(|s| s.logical_type)
                        .unwrap_or(LogicalType::Binary);
                    columns.push(parse_column_chunk(r, ty)?);
                }
            }
            3 => num_rows = r.read_zigzag_i64()? as usize,
            _ => r.skip_value(tid)?,
        }
    }
    for col in &mut columns {
        col.zone_map.row_count = num_rows as u32;
    }
    Ok(ParquetRowGroupMeta { num_rows, columns })
}

fn parse_column_chunk(r: &mut ThriftReader<'_>, ty: LogicalType) -> Result<ParquetColChunkMeta> {
    let mut last = 0i16;
    let mut meta = ParquetColChunkMeta {
        codec: ParquetCompression::Uncompressed,
        num_values: 0,
        data_page_offset: 0,
        dict_page_offset: None,
        total_compressed_size: 0,
        zone_map: ZoneMap::empty(),
    };
    while let Some((fid, tid)) = r.read_field_header(&mut last)? {
        match fid {
            3 => meta = parse_column_meta_data(r, ty)?,
            _ => r.skip_value(tid)?,
        }
    }
    Ok(meta)
}

fn parse_column_meta_data(
    r: &mut ThriftReader<'_>,
    ty: LogicalType,
) -> Result<ParquetColChunkMeta> {
    let mut last = 0i16;
    let mut codec = ParquetCompression::Uncompressed;
    let mut num_values = 0usize;
    let mut total_compressed_size = 0usize;
    let mut data_page_offset = 0usize;
    let mut dict_page_offset = None;
    let mut zm = ZoneMap::empty();

    while let Some((fid, tid)) = r.read_field_header(&mut last)? {
        match fid {
            4 => codec = ParquetCompression::from_i32(r.read_zigzag_i32()?)?,
            5 => num_values = r.read_zigzag_i64()? as usize,
            7 => total_compressed_size = r.read_zigzag_i64()? as usize,
            9 => data_page_offset = r.read_zigzag_i64()? as usize,
            11 => dict_page_offset = Some(r.read_zigzag_i64()? as usize),
            12 => zm = parse_statistics(r, ty)?,
            _ => r.skip_value(tid)?,
        }
    }
    Ok(ParquetColChunkMeta {
        codec,
        num_values,
        data_page_offset,
        dict_page_offset,
        total_compressed_size,
        zone_map: zm,
    })
}

fn parse_statistics(r: &mut ThriftReader<'_>, ty: LogicalType) -> Result<ZoneMap> {
    let mut last = 0i16;
    let mut zm = ZoneMap::empty();
    let mut legacy_min: Option<Vec<u8>> = None;
    let mut legacy_max: Option<Vec<u8>> = None;
    let mut modern_min: Option<Vec<u8>> = None;
    let mut modern_max: Option<Vec<u8>> = None;

    while let Some((fid, tid)) = r.read_field_header(&mut last)? {
        match fid {
            1 => legacy_max = Some(r.read_binary()?),
            2 => legacy_min = Some(r.read_binary()?),
            3 => zm.null_count = r.read_zigzag_i64()? as u32,
            5 => modern_max = Some(r.read_binary()?),
            6 => modern_min = Some(r.read_binary()?),
            _ => r.skip_value(tid)?,
        }
    }
    if let Some(b) = modern_min.or(legacy_min) {
        zm.min = decode_stat_scalar(&b, ty);
    }
    if let Some(b) = modern_max.or(legacy_max) {
        zm.max = decode_stat_scalar(&b, ty);
    }
    Ok(zm)
}

fn decode_parquet_column_chunk(
    file: &[u8],
    cs: &ParquetColSchema,
    cm: &ParquetColChunkMeta,
) -> Result<Vec<ScalarValue>> {
    let start = cm.dict_page_offset.unwrap_or(cm.data_page_offset);
    let end = (start + cm.total_compressed_size).min(file.len());
    let mut pos = start;
    let mut dictionary: Option<Vec<ScalarValue>> = None;
    let mut out = Vec::with_capacity(cm.num_values);

    while out.len() < cm.num_values && pos < end {
        let mut r = ThriftReader::new(&file[pos..end]);
        let mut last = 0i16;
        let mut page_type = -1i32;
        let mut uncomp_size = 0usize;
        let mut comp_size = 0usize;

        let mut dp1_num_values = 0usize;
        let mut dp1_encoding = ENC_PLAIN;
        let mut dict_num_values = 0usize;

        let mut dp2_num_values = 0usize;
        let mut dp2_encoding = ENC_PLAIN;
        let mut dp2_def_len = 0usize;
        let mut dp2_rep_len = 0usize;
        let mut dp2_is_compressed = true;

        while let Some((fid, tid)) = r.read_field_header(&mut last)? {
            match fid {
                1 => page_type = r.read_zigzag_i32()?,
                2 => uncomp_size = r.read_zigzag_i32()? as usize,
                3 => comp_size = r.read_zigzag_i32()? as usize,
                5 => {
                    // DataPageHeader (v1)
                    let mut l2 = 0i16;
                    while let Some((sf, st)) = r.read_field_header(&mut l2)? {
                        match sf {
                            1 => dp1_num_values = r.read_zigzag_i32()? as usize,
                            2 => dp1_encoding = r.read_zigzag_i32()?,
                            _ => r.skip_value(st)?,
                        }
                    }
                }
                7 => {
                    // DictionaryPageHeader
                    let mut l2 = 0i16;
                    while let Some((sf, st)) = r.read_field_header(&mut l2)? {
                        match sf {
                            1 => dict_num_values = r.read_zigzag_i32()? as usize,
                            _ => r.skip_value(st)?,
                        }
                    }
                }
                8 => {
                    // DataPageHeaderV2
                    let mut l2 = 0i16;
                    while let Some((sf, st)) = r.read_field_header(&mut l2)? {
                        match sf {
                            1 => dp2_num_values = r.read_zigzag_i32()? as usize,
                            4 => dp2_encoding = r.read_zigzag_i32()?,
                            5 => dp2_def_len = r.read_zigzag_i32()? as usize,
                            6 => dp2_rep_len = r.read_zigzag_i32()? as usize,
                            7 => dp2_is_compressed = st == T_BOOL_TRUE,
                            _ => r.skip_value(st)?,
                        }
                    }
                }
                _ => r.skip_value(tid)?,
            }
        }

        let header_size = r.pos;
        let body_start = pos + header_size;
        let body_end = body_start + comp_size;
        if body_end > file.len() {
            return Err(VectraError::CorruptFormat("Parquet page body OOB".into()));
        }
        let body = &file[body_start..body_end];
        pos = body_end;

        match page_type {
            PAGE_DICTIONARY => {
                let raw = cm.codec.decompress(body, uncomp_size)?;
                dictionary = Some(decode_plain_values(
                    &raw,
                    dict_num_values,
                    cs.logical_type,
                    cs.fixed_len,
                )?);
            }
            PAGE_DATA_V1 => {
                let raw = cm.codec.decompress(body, uncomp_size)?;
                let mut p = 0usize;
                let def_levels = if cs.optional {
                    if p + 4 > raw.len() {
                        return Err(VectraError::CorruptFormat(
                            "Parquet v1 def level len EOF".into(),
                        ));
                    }
                    let dlen = u32::from_le_bytes(raw[p..p + 4].try_into().unwrap()) as usize;
                    p += 4;
                    let dl = decode_rle_hybrid(&raw[p..p + dlen], 1, dp1_num_values)?;
                    p += dlen;
                    dl
                } else {
                    vec![1u32; dp1_num_values]
                };
                decode_page_values(
                    &raw[p..],
                    &def_levels,
                    dp1_encoding,
                    cs,
                    dictionary.as_deref(),
                    &mut out,
                )?;
            }
            PAGE_DATA_V2 => {
                let levels_len = dp2_rep_len + dp2_def_len;
                if levels_len > body.len() {
                    return Err(VectraError::CorruptFormat(
                        "Parquet v2 levels exceed page".into(),
                    ));
                }
                let def_slice = &body[dp2_rep_len..levels_len];
                let def_levels = if cs.optional && dp2_def_len > 0 {
                    decode_rle_hybrid(def_slice, 1, dp2_num_values)?
                } else {
                    vec![1u32; dp2_num_values]
                };
                let val_bytes = &body[levels_len..];
                let raw_vals = if dp2_is_compressed {
                    cm.codec
                        .decompress(val_bytes, uncomp_size.saturating_sub(levels_len))?
                } else {
                    val_bytes.to_vec()
                };
                decode_page_values(
                    &raw_vals,
                    &def_levels,
                    dp2_encoding,
                    cs,
                    dictionary.as_deref(),
                    &mut out,
                )?;
            }
            other => {
                return Err(VectraError::CorruptFormat(format!(
                    "unsupported Parquet page type {other}"
                )));
            }
        }
    }

    Ok(out)
}

fn decode_page_values(
    val_bytes: &[u8],
    def_levels: &[u32],
    encoding: i32,
    cs: &ParquetColSchema,
    dictionary: Option<&[ScalarValue]>,
    out: &mut Vec<ScalarValue>,
) -> Result<()> {
    let non_null_count = def_levels.iter().filter(|&&d| d == 1).count();
    let decoded_non_nulls = match encoding {
        ENC_PLAIN => decode_plain_values(val_bytes, non_null_count, cs.logical_type, cs.fixed_len)?,
        ENC_RLE_DICTIONARY | ENC_PLAIN_DICTIONARY => {
            let dict = dictionary.ok_or_else(|| {
                VectraError::CorruptFormat("Parquet dictionary page missing".into())
            })?;
            if non_null_count == 0 {
                Vec::new()
            } else {
                if val_bytes.is_empty() {
                    return Err(VectraError::CorruptFormat(
                        "Parquet dict indices missing bit_width".into(),
                    ));
                }
                let bw = val_bytes[0];
                let codes = decode_rle_hybrid(&val_bytes[1..], bw, non_null_count)?;
                let mut vals = Vec::with_capacity(non_null_count);
                for code in codes {
                    let v = dict.get(code as usize).ok_or_else(|| {
                        VectraError::CorruptFormat(format!("Parquet dict index {code} OOB"))
                    })?;
                    vals.push(v.clone());
                }
                vals
            }
        }
        other => {
            return Err(VectraError::CorruptFormat(format!(
                "unsupported Parquet encoding {other}"
            )));
        }
    };

    let mut nn_iter = decoded_non_nulls.into_iter();
    for &dl in def_levels {
        if dl == 0 {
            out.push(ScalarValue::Null);
        } else {
            out.push(nn_iter.next().unwrap_or(ScalarValue::Null));
        }
    }
    Ok(())
}
