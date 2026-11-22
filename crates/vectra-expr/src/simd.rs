//! Quarantined SIMD kernels with runtime CPU feature detection (`AVX2` / `NEON`)
//! and 100% safe scalar fallbacks (automatically selected under `cfg(miri)` or older CPUs).

use vectra_types::{Result, VectraError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Returns true if runtime AVX2 acceleration is active on this CPU.
#[inline]
pub fn has_avx2() -> bool {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(all(target_arch = "x86_64", not(miri))))]
    {
        false
    }
}

/// Returns true if runtime NEON acceleration is active on this CPU.
#[inline]
pub fn has_neon() -> bool {
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    {
        true
    }
    #[cfg(not(all(target_arch = "aarch64", not(miri))))]
    {
        false
    }
}

// ============================================================================
// Safe Portable Scalar Fallbacks (Ground Truth Reference for SIMD Parity Gate)
// ============================================================================

pub fn filter_i64_scalar_ref(data: &[i64], rhs: i64, op: CmpOp, out: &mut Vec<u16>) {
    out.clear();
    for (i, &v) in data.iter().enumerate() {
        let hit = match op {
            CmpOp::Eq => v == rhs,
            CmpOp::Ne => v != rhs,
            CmpOp::Lt => v < rhs,
            CmpOp::Le => v <= rhs,
            CmpOp::Gt => v > rhs,
            CmpOp::Ge => v >= rhs,
        };
        if hit {
            out.push(i as u16);
        }
    }
}

pub fn filter_i32_scalar_ref(data: &[i32], rhs: i32, op: CmpOp, out: &mut Vec<u16>) {
    out.clear();
    for (i, &v) in data.iter().enumerate() {
        let hit = match op {
            CmpOp::Eq => v == rhs,
            CmpOp::Ne => v != rhs,
            CmpOp::Lt => v < rhs,
            CmpOp::Le => v <= rhs,
            CmpOp::Gt => v > rhs,
            CmpOp::Ge => v >= rhs,
        };
        if hit {
            out.push(i as u16);
        }
    }
}

pub fn filter_f64_scalar_ref(data: &[f64], rhs: f64, op: CmpOp, out: &mut Vec<u16>) {
    out.clear();
    for (i, &v) in data.iter().enumerate() {
        let hit = match op {
            CmpOp::Eq => v == rhs,
            CmpOp::Ne => v != rhs,
            CmpOp::Lt => v < rhs,
            CmpOp::Le => v <= rhs,
            CmpOp::Gt => v > rhs,
            CmpOp::Ge => v >= rhs,
        };
        if hit {
            out.push(i as u16);
        }
    }
}

pub fn add_f64_scalar_ref(a: &[f64], b: &[f64], out: &mut [f64]) {
    for ((o, &x), &y) in out.iter_mut().zip(a.iter()).zip(b.iter()) {
        *o = x + y;
    }
}

pub fn sub_f64_scalar_ref(a: &[f64], b: &[f64], out: &mut [f64]) {
    for ((o, &x), &y) in out.iter_mut().zip(a.iter()).zip(b.iter()) {
        *o = x - y;
    }
}

pub fn mul_f64_scalar_ref(a: &[f64], b: &[f64], out: &mut [f64]) {
    for ((o, &x), &y) in out.iter_mut().zip(a.iter()).zip(b.iter()) {
        *o = x * y;
    }
}

pub fn add_i64_checked_scalar_ref(a: &[i64], b: &[i64], out: &mut [i64]) -> Result<()> {
    for ((o, &x), &y) in out.iter_mut().zip(a.iter()).zip(b.iter()) {
        *o = x
            .checked_add(y)
            .ok_or_else(|| VectraError::ArithmeticOverflow(format!("{x} + {y} overflows i64")))?;
    }
    Ok(())
}

// ============================================================================
// Architecture-Specific SIMD Fast Paths (x86_64 AVX2 & aarch64 NEON)
// ============================================================================

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn filter_i64_avx2(data: &[i64], rhs: i64, op: CmpOp, out: &mut Vec<u16>) {
    use std::arch::x86_64::*;

    out.clear();
    let len = data.len();
    let vrhs = _mm256_set1_epi64x(rhs);
    let mut i = 0usize;

    while i + 4 <= len {
        // SAFETY: `i + 4 <= len`, so `data.as_ptr().add(i)` is valid for 32 bytes (unaligned load).
        let v = unsafe { _mm256_loadu_si256(data.as_ptr().add(i).cast::<__m256i>()) };
        let cmp = match op {
            CmpOp::Eq => _mm256_cmpeq_epi64(v, vrhs),
            CmpOp::Ne => _mm256_xor_si256(_mm256_cmpeq_epi64(v, vrhs), _mm256_set1_epi64x(-1)),
            CmpOp::Gt => _mm256_cmpgt_epi64(v, vrhs),
            CmpOp::Lt => _mm256_cmpgt_epi64(vrhs, v),
            CmpOp::Ge => _mm256_or_si256(_mm256_cmpgt_epi64(v, vrhs), _mm256_cmpeq_epi64(v, vrhs)),
            CmpOp::Le => _mm256_or_si256(_mm256_cmpgt_epi64(vrhs, v), _mm256_cmpeq_epi64(v, vrhs)),
        };
        let mut mask = _mm256_movemask_pd(_mm256_castsi256_pd(cmp)) as u32;
        while mask != 0 {
            let lane = mask.trailing_zeros() as usize;
            out.push((i + lane) as u16);
            mask &= mask - 1;
        }
        i += 4;
    }

    while i < len {
        let v = data[i];
        let hit = match op {
            CmpOp::Eq => v == rhs,
            CmpOp::Ne => v != rhs,
            CmpOp::Lt => v < rhs,
            CmpOp::Le => v <= rhs,
            CmpOp::Gt => v > rhs,
            CmpOp::Ge => v >= rhs,
        };
        if hit {
            out.push(i as u16);
        }
        i += 1;
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn filter_i32_avx2(data: &[i32], rhs: i32, op: CmpOp, out: &mut Vec<u16>) {
    use std::arch::x86_64::*;

    out.clear();
    let len = data.len();
    let vrhs = _mm256_set1_epi32(rhs);
    let mut i = 0usize;

    while i + 8 <= len {
        // SAFETY: `i + 8 <= len`, valid 32-byte unaligned load.
        let v = unsafe { _mm256_loadu_si256(data.as_ptr().add(i).cast::<__m256i>()) };
        let cmp = match op {
            CmpOp::Eq => _mm256_cmpeq_epi32(v, vrhs),
            CmpOp::Ne => _mm256_xor_si256(_mm256_cmpeq_epi32(v, vrhs), _mm256_set1_epi32(-1)),
            CmpOp::Gt => _mm256_cmpgt_epi32(v, vrhs),
            CmpOp::Lt => _mm256_cmpgt_epi32(vrhs, v),
            CmpOp::Ge => _mm256_or_si256(_mm256_cmpgt_epi32(v, vrhs), _mm256_cmpeq_epi32(v, vrhs)),
            CmpOp::Le => _mm256_or_si256(_mm256_cmpgt_epi32(vrhs, v), _mm256_cmpeq_epi32(v, vrhs)),
        };
        let mut mask = _mm256_movemask_ps(_mm256_castsi256_ps(cmp)) as u32;
        while mask != 0 {
            let lane = mask.trailing_zeros() as usize;
            out.push((i + lane) as u16);
            mask &= mask - 1;
        }
        i += 8;
    }

    while i < len {
        let v = data[i];
        let hit = match op {
            CmpOp::Eq => v == rhs,
            CmpOp::Ne => v != rhs,
            CmpOp::Lt => v < rhs,
            CmpOp::Le => v <= rhs,
            CmpOp::Gt => v > rhs,
            CmpOp::Ge => v >= rhs,
        };
        if hit {
            out.push(i as u16);
        }
        i += 1;
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn filter_f64_avx2(data: &[f64], rhs: f64, op: CmpOp, out: &mut Vec<u16>) {
    use std::arch::x86_64::*;

    out.clear();
    let len = data.len();
    let vrhs = _mm256_set1_pd(rhs);
    let mut i = 0usize;

    while i + 4 <= len {
        // SAFETY: `i + 4 <= len`, valid 32-byte unaligned load of 4 f64 lanes.
        let v = unsafe { _mm256_loadu_pd(data.as_ptr().add(i)) };
        let cmp = match op {
            CmpOp::Eq => _mm256_cmp_pd::<_CMP_EQ_OQ>(v, vrhs),
            CmpOp::Ne => _mm256_cmp_pd::<_CMP_NEQ_UQ>(v, vrhs),
            CmpOp::Lt => _mm256_cmp_pd::<_CMP_LT_OQ>(v, vrhs),
            CmpOp::Le => _mm256_cmp_pd::<_CMP_LE_OQ>(v, vrhs),
            CmpOp::Gt => _mm256_cmp_pd::<_CMP_GT_OQ>(v, vrhs),
            CmpOp::Ge => _mm256_cmp_pd::<_CMP_GE_OQ>(v, vrhs),
        };
        let mut mask = _mm256_movemask_pd(cmp) as u32;
        while mask != 0 {
            let lane = mask.trailing_zeros() as usize;
            out.push((i + lane) as u16);
            mask &= mask - 1;
        }
        i += 4;
    }

    while i < len {
        let v = data[i];
        let hit = match op {
            CmpOp::Eq => v == rhs,
            CmpOp::Ne => v != rhs,
            CmpOp::Lt => v < rhs,
            CmpOp::Le => v <= rhs,
            CmpOp::Gt => v > rhs,
            CmpOp::Ge => v >= rhs,
        };
        if hit {
            out.push(i as u16);
        }
        i += 1;
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn arith_f64_avx2(a: &[f64], b: &[f64], out: &mut [f64], mode: u8) {
    use std::arch::x86_64::*;

    let len = out.len();
    let mut i = 0usize;
    while i + 4 <= len {
        // SAFETY: `i + 4 <= len`, and caller guarantees `a.len() == b.len() == out.len() == len`.
        unsafe {
            let va = _mm256_loadu_pd(a.as_ptr().add(i));
            let vb = _mm256_loadu_pd(b.as_ptr().add(i));
            let res = match mode {
                0 => _mm256_add_pd(va, vb),
                1 => _mm256_sub_pd(va, vb),
                _ => _mm256_mul_pd(va, vb),
            };
            _mm256_storeu_pd(out.as_mut_ptr().add(i), res);
        }
        i += 4;
    }
    while i < len {
        out[i] = match mode {
            0 => a[i] + b[i],
            1 => a[i] - b[i],
            _ => a[i] * b[i],
        };
        i += 1;
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn add_i64_checked_avx2(a: &[i64], b: &[i64], out: &mut [i64]) -> Result<()> {
    use std::arch::x86_64::*;

    let len = out.len();
    let mut i = 0usize;
    while i + 4 <= len {
        // SAFETY: `i + 4 <= len`, and `a.len() == b.len() == out.len()`.
        let overflow_mask = unsafe {
            let va = _mm256_loadu_si256(a.as_ptr().add(i).cast::<__m256i>());
            let vb = _mm256_loadu_si256(b.as_ptr().add(i).cast::<__m256i>());
            let sum = _mm256_add_epi64(va, vb);
            // Signed overflow occurs iff (sum ^ a) & (sum ^ b) has sign bit set.
            let ov = _mm256_and_si256(_mm256_xor_si256(sum, va), _mm256_xor_si256(sum, vb));
            _mm256_storeu_si256(out.as_mut_ptr().add(i).cast::<__m256i>(), sum);
            _mm256_movemask_pd(_mm256_castsi256_pd(ov))
        };
        if overflow_mask != 0 {
            let lane = (overflow_mask as u32).trailing_zeros() as usize;
            return Err(VectraError::ArithmeticOverflow(format!(
                "{} + {} overflows i64",
                a[i + lane],
                b[i + lane]
            )));
        }
        i += 4;
    }
    while i < len {
        out[i] = a[i].checked_add(b[i]).ok_or_else(|| {
            VectraError::ArithmeticOverflow(format!("{} + {} overflows i64", a[i], b[i]))
        })?;
        i += 1;
    }
    Ok(())
}

// ============================================================================
// Public Runtime-Dispatched SIMD Entry Points
// ============================================================================

pub fn filter_i64(data: &[i64], rhs: i64, op: CmpOp, out: &mut Vec<u16>) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: Runtime check verified AVX2 support on this CPU.
            unsafe {
                filter_i64_avx2(data, rhs, op, out);
            }
            return;
        }
    }
    filter_i64_scalar_ref(data, rhs, op, out);
}

pub fn filter_i32(data: &[i32], rhs: i32, op: CmpOp, out: &mut Vec<u16>) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: Runtime check verified AVX2 support on this CPU.
            unsafe {
                filter_i32_avx2(data, rhs, op, out);
            }
            return;
        }
    }
    filter_i32_scalar_ref(data, rhs, op, out);
}

pub fn filter_f64(data: &[f64], rhs: f64, op: CmpOp, out: &mut Vec<u16>) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: Runtime check verified AVX2 support on this CPU.
            unsafe {
                filter_f64_avx2(data, rhs, op, out);
            }
            return;
        }
    }
    filter_f64_scalar_ref(data, rhs, op, out);
}

pub fn add_f64(a: &[f64], b: &[f64], out: &mut [f64]) {
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len(), out.len());
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: Runtime check verified AVX2 support and equal slice lengths.
            unsafe {
                arith_f64_avx2(a, b, out, 0);
            }
            return;
        }
    }
    add_f64_scalar_ref(a, b, out);
}

pub fn sub_f64(a: &[f64], b: &[f64], out: &mut [f64]) {
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len(), out.len());
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: Runtime check verified AVX2 support and equal slice lengths.
            unsafe {
                arith_f64_avx2(a, b, out, 1);
            }
            return;
        }
    }
    sub_f64_scalar_ref(a, b, out);
}

pub fn mul_f64(a: &[f64], b: &[f64], out: &mut [f64]) {
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len(), out.len());
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: Runtime check verified AVX2 support and equal slice lengths.
            unsafe {
                arith_f64_avx2(a, b, out, 2);
            }
            return;
        }
    }
    mul_f64_scalar_ref(a, b, out);
}

pub fn add_i64_checked(a: &[i64], b: &[i64], out: &mut [i64]) -> Result<()> {
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len(), out.len());
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: Runtime check verified AVX2 support and equal slice lengths.
            return unsafe { add_i64_checked_avx2(a, b, out) };
        }
    }
    add_i64_checked_scalar_ref(a, b, out)
}
