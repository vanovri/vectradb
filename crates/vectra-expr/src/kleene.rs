use std::sync::Arc;
use vectra_types::mask::{mask_word_count, trailing_word_mask};
use vectra_types::{FlatVector, MemoryPool, Result, SelectionVector, ValidityMask, Vector};

/// Word-level 64-lane Kleene three-valued AND:
/// - `FALSE AND NULL -> FALSE (valid)`
/// - `TRUE AND NULL -> NULL (invalid)`
#[inline]
pub const fn kleene_and_word(a_val: u64, a_valid: u64, b_val: u64, b_valid: u64) -> (u64, u64) {
    let a_true = a_valid & a_val;
    let b_true = b_valid & b_val;
    let a_false = a_valid & !a_val;
    let b_false = b_valid & !b_val;
    let out_val = a_true & b_true;
    let out_valid = out_val | a_false | b_false;
    (out_val, out_valid)
}

/// Word-level 64-lane Kleene three-valued OR:
/// - `TRUE OR NULL -> TRUE (valid)`
/// - `FALSE OR NULL -> NULL (invalid)`
#[inline]
pub const fn kleene_or_word(a_val: u64, a_valid: u64, b_val: u64, b_valid: u64) -> (u64, u64) {
    let a_true = a_valid & a_val;
    let b_true = b_valid & b_val;
    let a_false = a_valid & !a_val;
    let b_false = b_valid & !b_val;
    let out_val = a_true | b_true;
    let out_valid = out_val | (a_false & b_false);
    (out_val, out_valid)
}

/// Word-level 64-lane Kleene three-valued NOT:
/// - `NOT TRUE -> FALSE`, `NOT FALSE -> TRUE`, `NOT NULL -> NULL`.
#[inline]
pub const fn kleene_not_word(a_val: u64, a_valid: u64) -> (u64, u64) {
    (!a_val & a_valid, a_valid)
}

/// Packs a boolean `FlatVector` into `(value_words, valid_words)` of 64-bit bitmasks.
fn pack_bool_flat(flat: &FlatVector) -> (Vec<u64>, Vec<u64>) {
    let len = flat.len;
    let nwords = mask_word_count(len);
    let mut vals = vec![0u64; nwords];
    let bytes = flat.as_bool_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != 0 {
            vals[i >> 6] |= 1u64 << (i & 63);
        }
    }
    let valids = match flat.validity.raw_words() {
        Some(w) => w.to_vec(),
        None => {
            let mut v = vec![u64::MAX; nwords];
            if let Some(last) = v.last_mut() {
                *last &= trailing_word_mask(len);
            }
            v
        }
    };
    (vals, valids)
}

/// Unpacks `(value_words, valid_words)` into a boolean `FlatVector`.
fn unpack_bool_flat(
    val_words: &[u64],
    valid_words: Vec<u64>,
    len: usize,
    pool: &Arc<dyn MemoryPool>,
) -> Result<FlatVector> {
    let mut bytes = vec![0u8; len];
    for (w_idx, &word) in val_words.iter().enumerate() {
        let mut w = word;
        let base = w_idx << 6;
        while w != 0 {
            let tz = w.trailing_zeros() as usize;
            let idx = base + tz;
            if idx < len {
                bytes[idx] = 1;
            }
            w &= w - 1;
        }
    }
    let validity = ValidityMask::from_words(valid_words, len);
    FlatVector::from_bool_bytes(&bytes, validity, pool)
}

/// Evaluates vectorized Kleene `AND` over two boolean `Vector`s of logical length `count`.
pub fn eval_kleene_and(
    left: &Vector,
    right: &Vector,
    count: usize,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    if count == 0 {
        return Ok(Vector::Flat(FlatVector::empty(
            vectra_types::LogicalType::Boolean,
            pool,
        )));
    }
    let id = SelectionVector::identity(count);
    let lf = left.flatten(&id, count, pool)?;
    let rf = right.flatten(&id, count, pool)?;
    let (lv, lm) = pack_bool_flat(&lf);
    let (rv, rm) = pack_bool_flat(&rf);
    let nwords = lv.len();
    let mut ov = Vec::with_capacity(nwords);
    let mut om = Vec::with_capacity(nwords);
    for i in 0..nwords {
        let (v, m) = kleene_and_word(lv[i], lm[i], rv[i], rm[i]);
        ov.push(v);
        om.push(m);
    }
    Ok(Vector::Flat(unpack_bool_flat(&ov, om, count, pool)?))
}

/// Evaluates vectorized Kleene `OR` over two boolean `Vector`s of logical length `count`.
pub fn eval_kleene_or(
    left: &Vector,
    right: &Vector,
    count: usize,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    if count == 0 {
        return Ok(Vector::Flat(FlatVector::empty(
            vectra_types::LogicalType::Boolean,
            pool,
        )));
    }
    let id = SelectionVector::identity(count);
    let lf = left.flatten(&id, count, pool)?;
    let rf = right.flatten(&id, count, pool)?;
    let (lv, lm) = pack_bool_flat(&lf);
    let (rv, rm) = pack_bool_flat(&rf);
    let nwords = lv.len();
    let mut ov = Vec::with_capacity(nwords);
    let mut om = Vec::with_capacity(nwords);
    for i in 0..nwords {
        let (v, m) = kleene_or_word(lv[i], lm[i], rv[i], rm[i]);
        ov.push(v);
        om.push(m);
    }
    Ok(Vector::Flat(unpack_bool_flat(&ov, om, count, pool)?))
}

/// Evaluates vectorized Kleene `NOT` over a boolean `Vector` of logical length `count`.
pub fn eval_kleene_not(input: &Vector, count: usize, pool: &Arc<dyn MemoryPool>) -> Result<Vector> {
    if count == 0 {
        return Ok(Vector::Flat(FlatVector::empty(
            vectra_types::LogicalType::Boolean,
            pool,
        )));
    }
    let id = SelectionVector::identity(count);
    let f = input.flatten(&id, count, pool)?;
    let (lv, lm) = pack_bool_flat(&f);
    let nwords = lv.len();
    let mut ov = Vec::with_capacity(nwords);
    let mut om = Vec::with_capacity(nwords);
    for i in 0..nwords {
        let (v, m) = kleene_not_word(lv[i], lm[i]);
        ov.push(v);
        om.push(m);
    }
    Ok(Vector::Flat(unpack_bool_flat(&ov, om, count, pool)?))
}
