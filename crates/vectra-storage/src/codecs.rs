use crate::lz4::{lz4_compress_block, lz4_decompress_block};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use vectra_types::{
    AlignedBufferMut, ByteView, FlatVector, LogicalType, MemoryPool, Result, StringHeapBuilder,
    ValidityMask, VectraError,
};

/// Exact byte length of every `ColumnPage` header.
pub const COLUMN_PAGE_HEADER_SIZE: usize = 32;

/// Column compression codec identifier stored in byte 0 of `ColumnPage` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum CodecId {
    Uncompressed = 0,
    FrameOfReferenceBitPack = 1,
    DeltaBinaryPacked = 2,
    Rle = 3,
    DictionaryLz4 = 4,
}

impl CodecId {
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Uncompressed),
            1 => Ok(Self::FrameOfReferenceBitPack),
            2 => Ok(Self::DeltaBinaryPacked),
            3 => Ok(Self::Rle),
            4 => Ok(Self::DictionaryLz4),
            _ => Err(VectraError::CorruptFormat(format!("unknown codec_id {v}"))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Uncompressed => "UNCOMPRESSED",
            Self::FrameOfReferenceBitPack => "FOR_BITPACK",
            Self::DeltaBinaryPacked => "DELTA_BP",
            Self::Rle => "RLE",
            Self::DictionaryLz4 => "DICT_LZ4",
        }
    }
}

/// Encoded columnar page containing its 32-byte verified header and compressed bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnPage {
    pub codec: CodecId,
    pub logical_type: LogicalType,
    pub row_count: u32,
    pub null_count: u32,
    pub uncompressed_len: u32,
    pub compressed_len: u32,
    /// Full serialized binary page (32-byte header + validity bytes + data payload).
    pub raw_bytes: Vec<u8>,
}

impl ColumnPage {
    pub fn compression_ratio(&self) -> f64 {
        if self.raw_bytes.is_empty() {
            1.0
        } else {
            (self.uncompressed_len.max(1) as f64) / (self.raw_bytes.len() as f64)
        }
    }
}

fn type_to_tag(ty: LogicalType) -> (u8, u8, i8) {
    match ty {
        LogicalType::Null => (0, 0, 0),
        LogicalType::Boolean => (1, 0, 0),
        LogicalType::Int8 => (2, 0, 0),
        LogicalType::Int16 => (3, 0, 0),
        LogicalType::Int32 => (4, 0, 0),
        LogicalType::Int64 => (5, 0, 0),
        LogicalType::Float64 => (6, 0, 0),
        LogicalType::Decimal128 { precision, scale } => (7, precision, scale),
        LogicalType::Date32 => (8, 0, 0),
        LogicalType::TimestampMicros => (9, 0, 0),
        LogicalType::Utf8 => (10, 0, 0),
        LogicalType::Binary => (11, 0, 0),
    }
}

fn tag_to_type(tag: u8, prec: u8, scale: i8) -> Result<LogicalType> {
    match tag {
        0 => Ok(LogicalType::Null),
        1 => Ok(LogicalType::Boolean),
        2 => Ok(LogicalType::Int8),
        3 => Ok(LogicalType::Int16),
        4 => Ok(LogicalType::Int32),
        5 => Ok(LogicalType::Int64),
        6 => Ok(LogicalType::Float64),
        7 => Ok(LogicalType::Decimal128 {
            precision: prec,
            scale,
        }),
        8 => Ok(LogicalType::Date32),
        9 => Ok(LogicalType::TimestampMicros),
        10 => Ok(LogicalType::Utf8),
        11 => Ok(LogicalType::Binary),
        _ => Err(VectraError::CorruptFormat(format!(
            "unknown type tag {tag}"
        ))),
    }
}

// ============================================================================
// Bit-Packing & ZigZag Utilities (0..=64 bits per integer)
// ============================================================================

#[inline]
fn bits_needed(max_val: u64) -> u8 {
    if max_val == 0 {
        0
    } else {
        (64 - max_val.leading_zeros()) as u8
    }
}

#[inline]
fn zigzag_encode(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn zigzag_decode(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn bitpack_u64(vals: &[u64], bit_width: u8, out: &mut Vec<u8>) {
    if bit_width == 0 || vals.is_empty() {
        return;
    }
    if bit_width == 64 {
        for &v in vals {
            out.extend_from_slice(&v.to_le_bytes());
        }
        return;
    }
    let bw = bit_width as u32;
    let mask = (1u64 << bw) - 1;
    let mut acc: u128 = 0;
    let mut acc_bits: u32 = 0;
    for &v in vals {
        acc |= ((v & mask) as u128) << acc_bits;
        acc_bits += bw;
        while acc_bits >= 8 {
            out.push((acc & 0xFF) as u8);
            acc >>= 8;
            acc_bits -= 8;
        }
    }
    if acc_bits > 0 {
        out.push((acc & 0xFF) as u8);
    }
}

fn bitunpack_u64(src: &[u8], count: usize, bit_width: u8) -> Result<Vec<u64>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if bit_width == 0 {
        return Ok(vec![0u64; count]);
    }
    if bit_width > 64 {
        return Err(VectraError::CorruptFormat(format!(
            "invalid bit_width {bit_width}"
        )));
    }
    if bit_width == 64 {
        if src.len() < count * 8 {
            return Err(VectraError::CorruptFormat(
                "truncated 64-bit packed data".into(),
            ));
        }
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let off = i * 8;
            out.push(u64::from_le_bytes(src[off..off + 8].try_into().unwrap()));
        }
        return Ok(out);
    }
    let needed_bytes = (count * bit_width as usize).div_ceil(8);
    if src.len() < needed_bytes {
        return Err(VectraError::CorruptFormat(
            "truncated bitpacked stream".into(),
        ));
    }
    let bw = bit_width as u32;
    let mask = (1u64 << bw) - 1;
    let mut out = Vec::with_capacity(count);
    let mut acc: u128 = 0;
    let mut acc_bits: u32 = 0;
    let mut byte_idx = 0usize;

    for _ in 0..count {
        while acc_bits < bw {
            acc |= (src[byte_idx] as u128) << acc_bits;
            byte_idx += 1;
            acc_bits += 8;
        }
        out.push((acc as u64) & mask);
        acc >>= bw;
        acc_bits -= bw;
    }
    Ok(out)
}

/// RLE compresses a validity bitmap (returns empty Vec when all valid -> zero bytes!).
fn encode_validity(mask: &ValidityMask) -> Vec<u8> {
    if mask.is_all_valid() {
        return Vec::new();
    }
    let raw = mask.to_bytes();
    let lz = lz4_compress_block(&raw);
    let mut out = Vec::with_capacity(5 + lz.len().min(raw.len()));
    if lz.len() + 4 < raw.len() {
        out.push(1); // LZ4 compressed validity
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&lz);
    } else {
        out.push(0); // raw validity bytes
        out.extend_from_slice(&raw);
    }
    out
}

fn decode_validity(bytes: &[u8], row_count: usize) -> Result<ValidityMask> {
    if bytes.is_empty() {
        return Ok(ValidityMask::all_valid(row_count));
    }
    match bytes[0] {
        0 => Ok(ValidityMask::from_bytes(&bytes[1..], row_count)),
        1 => {
            if bytes.len() < 5 {
                return Err(VectraError::CorruptFormat("truncated LZ4 validity".into()));
            }
            let raw_len = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
            let raw = lz4_decompress_block(&bytes[5..], raw_len)?;
            Ok(ValidityMask::from_bytes(&raw, row_count))
        }
        other => Err(VectraError::CorruptFormat(format!(
            "unknown validity encoding {other}"
        ))),
    }
}

// ============================================================================
// Integer Extraction Helpers (for FoR & Delta codecs)
// ============================================================================

fn extract_i64s(flat: &FlatVector) -> Option<Vec<i64>> {
    match flat.logical_type {
        LogicalType::Int8 => Some(flat.as_i8_slice().iter().map(|&v| v as i64).collect()),
        LogicalType::Int16 => Some(flat.as_i16_slice().iter().map(|&v| v as i64).collect()),
        LogicalType::Int32 | LogicalType::Date32 => {
            Some(flat.as_i32_slice().iter().map(|&v| v as i64).collect())
        }
        LogicalType::Int64 | LogicalType::TimestampMicros => Some(flat.as_i64_slice().to_vec()),
        _ => None,
    }
}

fn rebuild_from_i64s(
    vals: &[i64],
    logical_type: LogicalType,
    validity: ValidityMask,
    pool: &Arc<dyn MemoryPool>,
) -> Result<FlatVector> {
    let n = vals.len();
    let width = logical_type.fixed_byte_width();
    let mut buf = AlignedBufferMut::with_capacity(n * width, pool)?;
    match logical_type {
        LogicalType::Int8 => {
            for &v in vals {
                buf.push_typed(v as i8)?;
            }
        }
        LogicalType::Int16 => {
            for &v in vals {
                buf.push_typed(v as i16)?;
            }
        }
        LogicalType::Int32 | LogicalType::Date32 => {
            for &v in vals {
                buf.push_typed(v as i32)?;
            }
        }
        LogicalType::Int64 | LogicalType::TimestampMicros => {
            buf.extend_from_typed_slice(vals)?;
        }
        _ => {
            return Err(VectraError::CorruptFormat(format!(
                "integer codec invalid for type {logical_type}"
            )))
        }
    }
    Ok(FlatVector {
        logical_type,
        data: buf.freeze(),
        validity,
        string_heap: Vec::new(),
        len: n,
    })
}

/// Encodes raw uncompressed value payload (independent of validity mask).
fn encode_uncompressed_payload(flat: &FlatVector) -> Vec<u8> {
    if flat.logical_type == LogicalType::Null {
        return Vec::new();
    }
    if flat.logical_type.is_varlen() {
        let views = flat.as_byte_views();
        let total: usize = views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                4 + if flat.validity.is_valid(i) {
                    v.len()
                } else {
                    0
                }
            })
            .sum();
        let mut out = Vec::with_capacity(total);
        for (i, bv) in views.iter().enumerate() {
            if flat.validity.is_null(i) {
                out.extend_from_slice(&0u32.to_le_bytes());
            } else {
                let bytes = bv.as_bytes(&flat.string_heap);
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
        out
    } else {
        flat.data.as_slice().to_vec()
    }
}

fn decode_uncompressed_payload(
    payload: &[u8],
    logical_type: LogicalType,
    row_count: usize,
    validity: ValidityMask,
    pool: &Arc<dyn MemoryPool>,
) -> Result<FlatVector> {
    if logical_type == LogicalType::Null {
        return Ok(FlatVector {
            logical_type,
            data: AlignedBufferMut::with_capacity(0, pool)?.freeze(),
            validity,
            string_heap: Vec::new(),
            len: row_count,
        });
    }
    if logical_type.is_varlen() {
        let mut views = AlignedBufferMut::with_capacity(row_count * 16, pool)?;
        let mut heap = StringHeapBuilder::new(pool);
        let mut pos = 0usize;
        for i in 0..row_count {
            if pos + 4 > payload.len() {
                return Err(VectraError::CorruptFormat("truncated varlen length".into()));
            }
            let len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + len > payload.len() {
                return Err(VectraError::CorruptFormat("truncated varlen bytes".into()));
            }
            if validity.is_null(i) {
                views.push_typed(ByteView::empty())?;
            } else {
                let bv = heap.append(&payload[pos..pos + len])?;
                views.push_typed(bv)?;
            }
            pos += len;
        }
        return Ok(FlatVector {
            logical_type,
            data: views.freeze(),
            validity,
            string_heap: heap.finish(),
            len: row_count,
        });
    }
    let expected_bytes = row_count * logical_type.fixed_byte_width();
    if payload.len() != expected_bytes {
        return Err(VectraError::CorruptFormat(format!(
            "uncompressed payload len {} != expected {expected_bytes}",
            payload.len()
        )));
    }
    let mut buf = AlignedBufferMut::with_capacity(expected_bytes, pool)?;
    buf.extend_from_slice(payload)?;
    Ok(FlatVector {
        logical_type,
        data: buf.freeze(),
        validity,
        string_heap: Vec::new(),
        len: row_count,
    })
}

fn encode_for_bitpack(flat: &FlatVector) -> Option<Vec<u8>> {
    let vals = extract_i64s(flat)?;
    if vals.is_empty() {
        return Some(Vec::new());
    }
    let min_v = vals.iter().copied().min().unwrap();
    let mut deltas = Vec::with_capacity(vals.len());
    let mut max_delta = 0u64;
    for &v in &vals {
        let d = (v as u64).wrapping_sub(min_v as u64);
        max_delta = max_delta.max(d);
        deltas.push(d);
    }
    let bw = bits_needed(max_delta);
    let mut out = Vec::with_capacity(9 + (vals.len() * bw as usize).div_ceil(8));
    out.extend_from_slice(&min_v.to_le_bytes());
    out.push(bw);
    bitpack_u64(&deltas, bw, &mut out);
    Some(out)
}

fn decode_for_bitpack(
    payload: &[u8],
    logical_type: LogicalType,
    row_count: usize,
    validity: ValidityMask,
    pool: &Arc<dyn MemoryPool>,
) -> Result<FlatVector> {
    if row_count == 0 {
        return Ok(FlatVector::empty(logical_type, pool));
    }
    if payload.len() < 9 {
        return Err(VectraError::CorruptFormat("truncated FoR header".into()));
    }
    let min_v = i64::from_le_bytes(payload[0..8].try_into().unwrap());
    let bw = payload[8];
    let deltas = bitunpack_u64(&payload[9..], row_count, bw)?;
    let vals: Vec<i64> = deltas
        .into_iter()
        .map(|d| (min_v as u64).wrapping_add(d) as i64)
        .collect();
    rebuild_from_i64s(&vals, logical_type, validity, pool)
}

fn encode_delta_bp(flat: &FlatVector) -> Option<Vec<u8>> {
    let vals = extract_i64s(flat)?;
    if vals.is_empty() {
        return Some(Vec::new());
    }
    let first = vals[0];
    let mut zz_deltas = Vec::with_capacity(vals.len().saturating_sub(1));
    let mut prev = first;
    let mut max_zz = 0u64;
    for &v in &vals[1..] {
        let d = v.wrapping_sub(prev);
        let zz = zigzag_encode(d);
        max_zz = max_zz.max(zz);
        zz_deltas.push(zz);
        prev = v;
    }
    let bw = bits_needed(max_zz);
    let mut out = Vec::with_capacity(9 + (zz_deltas.len() * bw as usize).div_ceil(8));
    out.extend_from_slice(&first.to_le_bytes());
    out.push(bw);
    bitpack_u64(&zz_deltas, bw, &mut out);
    Some(out)
}

fn decode_delta_bp(
    payload: &[u8],
    logical_type: LogicalType,
    row_count: usize,
    validity: ValidityMask,
    pool: &Arc<dyn MemoryPool>,
) -> Result<FlatVector> {
    if row_count == 0 {
        return Ok(FlatVector::empty(logical_type, pool));
    }
    if payload.len() < 9 {
        return Err(VectraError::CorruptFormat(
            "truncated DeltaBP header".into(),
        ));
    }
    let first = i64::from_le_bytes(payload[0..8].try_into().unwrap());
    let bw = payload[8];
    let zz = bitunpack_u64(&payload[9..], row_count - 1, bw)?;
    let mut vals = Vec::with_capacity(row_count);
    let mut cur = first;
    vals.push(cur);
    for z in zz {
        cur = cur.wrapping_add(zigzag_decode(z));
        vals.push(cur);
    }
    rebuild_from_i64s(&vals, logical_type, validity, pool)
}

fn encode_rle(flat: &FlatVector) -> Vec<u8> {
    let n = flat.len;
    if n == 0 || flat.logical_type == LogicalType::Null {
        return Vec::new();
    }
    let mut out = Vec::new();
    if flat.logical_type.is_varlen() {
        let views = flat.as_byte_views();
        let mut i = 0usize;
        while i < n {
            let cur: &[u8] = if flat.validity.is_null(i) {
                &[]
            } else {
                views[i].as_bytes(&flat.string_heap)
            };
            let mut run = 1u32;
            while i + (run as usize) < n {
                let next_idx = i + run as usize;
                let next: &[u8] = if flat.validity.is_null(next_idx) {
                    &[]
                } else {
                    views[next_idx].as_bytes(&flat.string_heap)
                };
                if next != cur {
                    break;
                }
                run += 1;
            }
            out.extend_from_slice(&run.to_le_bytes());
            out.extend_from_slice(&(cur.len() as u32).to_le_bytes());
            out.extend_from_slice(cur);
            i += run as usize;
        }
    } else {
        let w = flat.logical_type.fixed_byte_width();
        let bytes = flat.data.as_slice();
        let mut i = 0usize;
        while i < n {
            let cur = &bytes[i * w..(i + 1) * w];
            let mut run = 1u32;
            while i + (run as usize) < n {
                let next_idx = i + run as usize;
                if &bytes[next_idx * w..(next_idx + 1) * w] != cur {
                    break;
                }
                run += 1;
            }
            out.extend_from_slice(&run.to_le_bytes());
            out.extend_from_slice(cur);
            i += run as usize;
        }
    }
    out
}

fn decode_rle(
    payload: &[u8],
    logical_type: LogicalType,
    row_count: usize,
    validity: ValidityMask,
    pool: &Arc<dyn MemoryPool>,
) -> Result<FlatVector> {
    if row_count == 0 || logical_type == LogicalType::Null {
        return Ok(FlatVector {
            logical_type,
            data: AlignedBufferMut::with_capacity(0, pool)?.freeze(),
            validity,
            string_heap: Vec::new(),
            len: row_count,
        });
    }
    let mut pos = 0usize;
    let mut decoded_rows = 0usize;
    if logical_type.is_varlen() {
        let mut views = AlignedBufferMut::with_capacity(row_count * 16, pool)?;
        let mut heap = StringHeapBuilder::new(pool);
        while decoded_rows < row_count {
            if pos + 8 > payload.len() {
                return Err(VectraError::CorruptFormat(
                    "truncated varlen RLE run".into(),
                ));
            }
            let run = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(payload[pos + 4..pos + 8].try_into().unwrap()) as usize;
            pos += 8;
            if pos + len > payload.len() || decoded_rows + run > row_count {
                return Err(VectraError::CorruptFormat("RLE varlen bounds error".into()));
            }
            let slice = &payload[pos..pos + len];
            pos += len;
            // Store outline string ONCE in heap and reuse ByteView for entire run!
            let shared_bv = if slice.is_empty() {
                ByteView::empty()
            } else {
                heap.append(slice)?
            };
            for _ in 0..run {
                if validity.is_null(decoded_rows) {
                    views.push_typed(ByteView::empty())?;
                } else {
                    views.push_typed(shared_bv)?;
                }
                decoded_rows += 1;
            }
        }
        return Ok(FlatVector {
            logical_type,
            data: views.freeze(),
            validity,
            string_heap: heap.finish(),
            len: row_count,
        });
    }

    let w = logical_type.fixed_byte_width();
    let mut buf = AlignedBufferMut::with_capacity(row_count * w, pool)?;
    while decoded_rows < row_count {
        if pos + 4 + w > payload.len() {
            return Err(VectraError::CorruptFormat("truncated fixed RLE run".into()));
        }
        let run = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if decoded_rows + run > row_count {
            return Err(VectraError::CorruptFormat(
                "RLE run exceeds row_count".into(),
            ));
        }
        let item = &payload[pos..pos + w];
        pos += w;
        for _ in 0..run {
            buf.extend_from_slice(item)?;
        }
        decoded_rows += run;
    }
    Ok(FlatVector {
        logical_type,
        data: buf.freeze(),
        validity,
        string_heap: Vec::new(),
        len: row_count,
    })
}

/// Mode 0 = raw LZ4 block over uncompressed payload; Mode 1 = per-page Dictionary + bitpacked codes + LZ4 dictionary blob.
fn encode_dict_lz4(flat: &FlatVector) -> Vec<u8> {
    let raw = encode_uncompressed_payload(flat);
    let raw_lz4 = lz4_compress_block(&raw);
    let mut best = Vec::with_capacity(5 + raw_lz4.len());
    best.push(0u8); // mode 0: plain LZ4 over raw payload
    best.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    best.extend_from_slice(&raw_lz4);

    if flat.len == 0 || flat.logical_type == LogicalType::Null {
        return best;
    }

    // Also try Dictionary + bit-packed codes + LZ4-compressed dictionary!
    let mut dict_map: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut dict_entries: Vec<Vec<u8>> = Vec::new();
    let mut codes: Vec<u64> = Vec::with_capacity(flat.len);

    if flat.logical_type.is_varlen() {
        let views = flat.as_byte_views();
        for (i, bv) in views.iter().enumerate() {
            let slice: &[u8] = if flat.validity.is_null(i) {
                &[]
            } else {
                bv.as_bytes(&flat.string_heap)
            };
            if let Some(&id) = dict_map.get(slice) {
                codes.push(id as u64);
            } else {
                let id = dict_entries.len() as u32;
                let owned = slice.to_vec();
                dict_map.insert(owned.clone(), id);
                dict_entries.push(owned);
                codes.push(id as u64);
            }
        }
    } else {
        let w = flat.logical_type.fixed_byte_width();
        let bytes = flat.data.as_slice();
        for i in 0..flat.len {
            let slice = &bytes[i * w..(i + 1) * w];
            if let Some(&id) = dict_map.get(slice) {
                codes.push(id as u64);
            } else {
                let id = dict_entries.len() as u32;
                let owned = slice.to_vec();
                dict_map.insert(owned.clone(), id);
                dict_entries.push(owned);
                codes.push(id as u64);
            }
        }
    }

    // Only build dictionary encoding if cardinality is meaningfully lower than row count.
    if dict_entries.len() <= (flat.len / 2).max(1) {
        let mut dict_raw = Vec::new();
        for entry in &dict_entries {
            if flat.logical_type.is_varlen() {
                dict_raw.extend_from_slice(&(entry.len() as u32).to_le_bytes());
            }
            dict_raw.extend_from_slice(entry);
        }
        let dict_lz4 = lz4_compress_block(&dict_raw);
        let bw = bits_needed(dict_entries.len().saturating_sub(1) as u64);
        let mut packed_codes = Vec::new();
        bitpack_u64(&codes, bw, &mut packed_codes);

        let mut cand = Vec::with_capacity(14 + dict_lz4.len() + packed_codes.len());
        cand.push(1u8); // mode 1: Dictionary + LZ4 + bitpacked codes
        cand.extend_from_slice(&(dict_entries.len() as u32).to_le_bytes());
        cand.extend_from_slice(&(dict_raw.len() as u32).to_le_bytes());
        cand.extend_from_slice(&(dict_lz4.len() as u32).to_le_bytes());
        cand.push(bw);
        cand.extend_from_slice(&dict_lz4);
        cand.extend_from_slice(&packed_codes);

        if cand.len() < best.len() {
            return cand;
        }
    }
    best
}

fn decode_dict_lz4(
    payload: &[u8],
    logical_type: LogicalType,
    row_count: usize,
    validity: ValidityMask,
    pool: &Arc<dyn MemoryPool>,
) -> Result<FlatVector> {
    if payload.is_empty() {
        return Ok(FlatVector::empty(logical_type, pool));
    }
    match payload[0] {
        0 => {
            if payload.len() < 5 {
                return Err(VectraError::CorruptFormat("truncated LZ4 page".into()));
            }
            let raw_len = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
            let raw = lz4_decompress_block(&payload[5..], raw_len)?;
            decode_uncompressed_payload(&raw, logical_type, row_count, validity, pool)
        }
        1 => {
            if payload.len() < 14 {
                return Err(VectraError::CorruptFormat(
                    "truncated DictLZ4 header".into(),
                ));
            }
            let dict_len = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
            let dict_raw_len = u32::from_le_bytes(payload[5..9].try_into().unwrap()) as usize;
            let dict_lz4_len = u32::from_le_bytes(payload[9..13].try_into().unwrap()) as usize;
            let bw = payload[13];
            if 14 + dict_lz4_len > payload.len() {
                return Err(VectraError::CorruptFormat(
                    "truncated DictLZ4 dictionary block".into(),
                ));
            }
            let dict_raw = lz4_decompress_block(&payload[14..14 + dict_lz4_len], dict_raw_len)?;
            let codes = bitunpack_u64(&payload[14 + dict_lz4_len..], row_count, bw)?;

            if logical_type.is_varlen() {
                let mut heap = StringHeapBuilder::new(pool);
                let mut dict_views = Vec::with_capacity(dict_len);
                let mut pos = 0usize;
                for _ in 0..dict_len {
                    if pos + 4 > dict_raw.len() {
                        return Err(VectraError::CorruptFormat(
                            "truncated varlen dict entry".into(),
                        ));
                    }
                    let slen =
                        u32::from_le_bytes(dict_raw[pos..pos + 4].try_into().unwrap()) as usize;
                    pos += 4;
                    if pos + slen > dict_raw.len() {
                        return Err(VectraError::CorruptFormat(
                            "truncated varlen dict bytes".into(),
                        ));
                    }
                    dict_views.push(heap.append(&dict_raw[pos..pos + slen])?);
                    pos += slen;
                }
                let mut views = AlignedBufferMut::with_capacity(row_count * 16, pool)?;
                for (i, &code) in codes.iter().enumerate() {
                    if validity.is_null(i) {
                        views.push_typed(ByteView::empty())?;
                    } else {
                        let bv = *dict_views.get(code as usize).ok_or_else(|| {
                            VectraError::CorruptFormat(format!("dict code {code} OOB"))
                        })?;
                        views.push_typed(bv)?;
                    }
                }
                Ok(FlatVector {
                    logical_type,
                    data: views.freeze(),
                    validity,
                    string_heap: heap.finish(),
                    len: row_count,
                })
            } else {
                let w = logical_type.fixed_byte_width();
                if dict_raw.len() != dict_len * w {
                    return Err(VectraError::CorruptFormat(
                        "fixed dict byte length mismatch".into(),
                    ));
                }
                let mut buf = AlignedBufferMut::with_capacity(row_count * w, pool)?;
                for &code in &codes {
                    let idx = code as usize;
                    if idx >= dict_len {
                        return Err(VectraError::CorruptFormat(format!(
                            "fixed dict code {idx} >= {dict_len}"
                        )));
                    }
                    buf.extend_from_slice(&dict_raw[idx * w..(idx + 1) * w])?;
                }
                Ok(FlatVector {
                    logical_type,
                    data: buf.freeze(),
                    validity,
                    string_heap: Vec::new(),
                    len: row_count,
                })
            }
        }
        other => Err(VectraError::CorruptFormat(format!(
            "unknown DictLZ4 mode {other}"
        ))),
    }
}

/// Encodes a `FlatVector` with the specified `CodecId`, wrapping it in the verified 32-byte header.
pub fn encode_page(flat: &FlatVector, codec: CodecId) -> Result<ColumnPage> {
    let uncompressed_raw = encode_uncompressed_payload(flat);
    let uncompressed_len = u32::try_from(uncompressed_raw.len())
        .map_err(|_| VectraError::ExecutionError("page uncompressed_len > u32::MAX".into()))?;

    let (actual_codec, payload) = match codec {
        CodecId::Uncompressed => (CodecId::Uncompressed, uncompressed_raw),
        CodecId::FrameOfReferenceBitPack => match encode_for_bitpack(flat) {
            Some(p) => (CodecId::FrameOfReferenceBitPack, p),
            None => (CodecId::Uncompressed, uncompressed_raw),
        },
        CodecId::DeltaBinaryPacked => match encode_delta_bp(flat) {
            Some(p) => (CodecId::DeltaBinaryPacked, p),
            None => (CodecId::Uncompressed, uncompressed_raw),
        },
        CodecId::Rle => (CodecId::Rle, encode_rle(flat)),
        CodecId::DictionaryLz4 => (CodecId::DictionaryLz4, encode_dict_lz4(flat)),
    };

    let validity_bytes = encode_validity(&flat.validity);
    let row_count = flat.len as u32;
    let null_count = flat.validity.count_nulls() as u32;
    let compressed_len = payload.len() as u32;
    let validity_len = validity_bytes.len() as u32;
    let (type_tag, dec_prec, dec_scale) = type_to_tag(flat.logical_type);

    let mut raw_bytes =
        Vec::with_capacity(COLUMN_PAGE_HEADER_SIZE + validity_bytes.len() + payload.len());
    // Bytes 0..24
    raw_bytes.push(actual_codec as u8);
    raw_bytes.push(type_tag);
    raw_bytes.push(dec_prec);
    raw_bytes.push(dec_scale as u8);
    raw_bytes.extend_from_slice(&row_count.to_le_bytes());
    raw_bytes.extend_from_slice(&uncompressed_len.to_le_bytes());
    raw_bytes.extend_from_slice(&compressed_len.to_le_bytes());
    raw_bytes.extend_from_slice(&null_count.to_le_bytes());
    raw_bytes.extend_from_slice(&validity_len.to_le_bytes());

    // Compute CRC32C over header[0..24] + validity_bytes + payload.
    let mut crc = crc32c::crc32c(&raw_bytes[0..24]);
    crc = crc32c::crc32c_append(crc, &validity_bytes);
    crc = crc32c::crc32c_append(crc, &payload);

    // Bytes 24..32
    raw_bytes.extend_from_slice(&crc.to_le_bytes());
    raw_bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
    debug_assert_eq!(raw_bytes.len(), COLUMN_PAGE_HEADER_SIZE);

    raw_bytes.extend_from_slice(&validity_bytes);
    raw_bytes.extend_from_slice(&payload);

    Ok(ColumnPage {
        codec: actual_codec,
        logical_type: flat.logical_type,
        row_count,
        null_count,
        uncompressed_len,
        compressed_len,
        raw_bytes,
    })
}

/// Automatic `CodecSelector`: trials applicable codecs for `flat`'s logical type and
/// picks the encoding with minimal serialized byte footprint.
pub fn encode_page_auto(flat: &FlatVector) -> Result<ColumnPage> {
    let candidates: &[CodecId] = match flat.logical_type {
        LogicalType::Null => &[CodecId::Uncompressed],
        LogicalType::Int8
        | LogicalType::Int16
        | LogicalType::Int32
        | LogicalType::Int64
        | LogicalType::Date32
        | LogicalType::TimestampMicros => &[
            CodecId::Uncompressed,
            CodecId::FrameOfReferenceBitPack,
            CodecId::DeltaBinaryPacked,
            CodecId::Rle,
            CodecId::DictionaryLz4,
        ],
        LogicalType::Boolean | LogicalType::Float64 | LogicalType::Decimal128 { .. } => {
            &[CodecId::Uncompressed, CodecId::Rle, CodecId::DictionaryLz4]
        }
        LogicalType::Utf8 | LogicalType::Binary => {
            &[CodecId::Uncompressed, CodecId::Rle, CodecId::DictionaryLz4]
        }
    };

    let mut best: Option<ColumnPage> = None;
    for &codec in candidates {
        let page = encode_page(flat, codec)?;
        let take = match &best {
            None => true,
            Some(cur) => page.raw_bytes.len() < cur.raw_bytes.len(),
        };
        if take {
            best = Some(page);
        }
    }
    Ok(best.expect("at least one codec candidate"))
}

/// Validates CRC32C and decodes a serialized `ColumnPage` back into a `FlatVector`.
pub fn decode_page(raw_bytes: &[u8], pool: &Arc<dyn MemoryPool>) -> Result<FlatVector> {
    if raw_bytes.len() < COLUMN_PAGE_HEADER_SIZE {
        return Err(VectraError::CorruptFormat(
            "ColumnPage shorter than 32-byte header".into(),
        ));
    }
    let codec = CodecId::from_u8(raw_bytes[0])?;
    let logical_type = tag_to_type(raw_bytes[1], raw_bytes[2], raw_bytes[3] as i8)?;
    let row_count = u32::from_le_bytes(raw_bytes[4..8].try_into().unwrap()) as usize;
    let _uncompressed_len = u32::from_le_bytes(raw_bytes[8..12].try_into().unwrap()) as usize;
    let compressed_len = u32::from_le_bytes(raw_bytes[12..16].try_into().unwrap()) as usize;
    let _null_count = u32::from_le_bytes(raw_bytes[16..20].try_into().unwrap()) as usize;
    let validity_len = u32::from_le_bytes(raw_bytes[20..24].try_into().unwrap()) as usize;
    let expected_crc = u32::from_le_bytes(raw_bytes[24..28].try_into().unwrap());

    if COLUMN_PAGE_HEADER_SIZE + validity_len + compressed_len != raw_bytes.len() {
        return Err(VectraError::CorruptFormat(
            "ColumnPage length mismatch with header".into(),
        ));
    }

    let val_start = COLUMN_PAGE_HEADER_SIZE;
    let val_end = val_start + validity_len;
    let validity_slice = &raw_bytes[val_start..val_end];
    let payload_slice = &raw_bytes[val_end..];

    let mut actual_crc = crc32c::crc32c(&raw_bytes[0..24]);
    actual_crc = crc32c::crc32c_append(actual_crc, validity_slice);
    actual_crc = crc32c::crc32c_append(actual_crc, payload_slice);
    if actual_crc != expected_crc {
        return Err(VectraError::ChecksumMismatch {
            expected: expected_crc,
            actual: actual_crc,
            context: format!("ColumnPage({:?}, {})", codec, logical_type),
        });
    }

    let validity = decode_validity(validity_slice, row_count)?;
    match codec {
        CodecId::Uncompressed => {
            decode_uncompressed_payload(payload_slice, logical_type, row_count, validity, pool)
        }
        CodecId::FrameOfReferenceBitPack => {
            decode_for_bitpack(payload_slice, logical_type, row_count, validity, pool)
        }
        CodecId::DeltaBinaryPacked => {
            decode_delta_bp(payload_slice, logical_type, row_count, validity, pool)
        }
        CodecId::Rle => decode_rle(payload_slice, logical_type, row_count, validity, pool),
        CodecId::DictionaryLz4 => {
            decode_dict_lz4(payload_slice, logical_type, row_count, validity, pool)
        }
    }
}
