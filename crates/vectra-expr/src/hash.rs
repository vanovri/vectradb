use vectra_types::{
    canonicalize_f64, hash_bytes_xx64, DataChunk, LogicalType, SelectionVector, Vector,
};

const NULL_HASH: u64 = 0x0000_0000_dead_beef;
const SEED: u64 = 0x517c_c1b7_2722_0a95;

#[inline]
fn mix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(SEED);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Deterministic order-dependent hash combiner for multi-column join/group keys.
#[inline]
pub fn hash_combine(h1: u64, h2: u64) -> u64 {
    h1 ^ h2
        .wrapping_add(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(h1 << 6)
        .wrapping_add(h1 >> 2)
}

/// Vectorized hashing of a single `Vector` indexed by `sel` into `out_hashes[..count]`.
/// When `combine == false`, overwrites `out_hashes`; when `combine == true`, combines into existing hashes.
/// Handles `ConstantVector` in O(1) hash evaluations and normalizes `-0.0`/`NaN` identically to `ScalarValue::canonical_hash`.
pub fn hash_vector(
    vec: &Vector,
    sel: &SelectionVector,
    count: usize,
    out_hashes: &mut [u64],
    combine: bool,
) {
    assert!(out_hashes.len() >= count);
    if count == 0 {
        return;
    }

    match vec {
        Vector::Constant { value, .. } => {
            let h = value.canonical_hash();
            if combine {
                for slot in &mut out_hashes[..count] {
                    *slot = hash_combine(*slot, h);
                }
            } else {
                out_hashes[..count].fill(h);
            }
        }
        Vector::Flat(flat) => {
            let valid = &flat.validity;
            match flat.logical_type {
                LogicalType::Null => {
                    if combine {
                        for slot in &mut out_hashes[..count] {
                            *slot = hash_combine(*slot, NULL_HASH);
                        }
                    } else {
                        out_hashes[..count].fill(NULL_HASH);
                    }
                }
                LogicalType::Boolean => {
                    let data = flat.as_bool_bytes();
                    for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                        let i = sel.get(pos);
                        let h = if valid.is_null(i) {
                            NULL_HASH
                        } else {
                            mix64(u64::from(data[i] != 0))
                        };
                        *dst = if combine { hash_combine(*dst, h) } else { h };
                    }
                }
                LogicalType::Int8 => {
                    let data = flat.as_i8_slice();
                    for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                        let i = sel.get(pos);
                        let h = if valid.is_null(i) {
                            NULL_HASH
                        } else {
                            mix64(data[i] as u64)
                        };
                        *dst = if combine { hash_combine(*dst, h) } else { h };
                    }
                }
                LogicalType::Int16 => {
                    let data = flat.as_i16_slice();
                    for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                        let i = sel.get(pos);
                        let h = if valid.is_null(i) {
                            NULL_HASH
                        } else {
                            mix64(data[i] as u64)
                        };
                        *dst = if combine { hash_combine(*dst, h) } else { h };
                    }
                }
                LogicalType::Int32 | LogicalType::Date32 => {
                    let data = flat.as_i32_slice();
                    for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                        let i = sel.get(pos);
                        let h = if valid.is_null(i) {
                            NULL_HASH
                        } else {
                            mix64(data[i] as u64)
                        };
                        *dst = if combine { hash_combine(*dst, h) } else { h };
                    }
                }
                LogicalType::Int64 | LogicalType::TimestampMicros => {
                    let data = flat.as_i64_slice();
                    for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                        let i = sel.get(pos);
                        let h = if valid.is_null(i) {
                            NULL_HASH
                        } else {
                            mix64(data[i] as u64)
                        };
                        *dst = if combine { hash_combine(*dst, h) } else { h };
                    }
                }
                LogicalType::Float64 => {
                    let data = flat.as_f64_slice();
                    for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                        let i = sel.get(pos);
                        let h = if valid.is_null(i) {
                            NULL_HASH
                        } else {
                            mix64(canonicalize_f64(data[i]).to_bits())
                        };
                        *dst = if combine { hash_combine(*dst, h) } else { h };
                    }
                }
                LogicalType::Decimal128 { .. } => {
                    let data = flat.as_i128_slice();
                    for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                        let i = sel.get(pos);
                        let h = if valid.is_null(i) {
                            NULL_HASH
                        } else {
                            let v = data[i];
                            let lo = v as u64;
                            let hi = (v >> 64) as u64;
                            mix64(lo ^ mix64(hi))
                        };
                        *dst = if combine { hash_combine(*dst, h) } else { h };
                    }
                }
                LogicalType::Utf8 | LogicalType::Binary => {
                    let views = flat.as_byte_views();
                    for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                        let i = sel.get(pos);
                        let h = if valid.is_null(i) {
                            NULL_HASH
                        } else {
                            hash_bytes_xx64(views[i].as_bytes(&flat.string_heap))
                        };
                        *dst = if combine { hash_combine(*dst, h) } else { h };
                    }
                }
            }
        }
        Vector::Dictionary {
            dict,
            indices,
            validity,
        } => {
            for (pos, dst) in out_hashes[..count].iter_mut().enumerate() {
                let row = sel.get(pos);
                let h = if validity.is_null(row) {
                    NULL_HASH
                } else {
                    dict.get_scalar(indices.get(row)).canonical_hash()
                };
                *dst = if combine { hash_combine(*dst, h) } else { h };
            }
        }
    }
}

/// Computes deterministic multi-column row hashes for `col_indices` of `chunk` into `out_hashes[..chunk.len()]`.
pub fn hash_chunk(chunk: &DataChunk, col_indices: &[usize], out_hashes: &mut [u64]) {
    let n = chunk.len();
    assert!(out_hashes.len() >= n);
    if n == 0 {
        return;
    }
    if col_indices.is_empty() {
        out_hashes[..n].fill(SEED);
        return;
    }
    for (k, &col_idx) in col_indices.iter().enumerate() {
        hash_vector(&chunk.columns[col_idx], &chunk.sel, n, out_hashes, k > 0);
    }
}
