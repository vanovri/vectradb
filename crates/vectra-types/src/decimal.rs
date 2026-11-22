use crate::error::{Result, VectraError};

pub const MAX_DECIMAL_PRECISION: u8 = 38;

/// Precomputed powers of 10 from `10^0` through `10^38`.
pub const POW10_I128: [i128; 39] = {
    let mut table = [0i128; 39];
    let mut v = 1i128;
    let mut i = 0;
    while i <= 38 {
        table[i] = v;
        if i < 38 {
            v *= 10;
        }
        i += 1;
    }
    table
};

#[inline]
pub fn pow10_i128(exp: u8) -> Result<i128> {
    POW10_I128.get(exp as usize).copied().ok_or_else(|| {
        VectraError::ArithmeticOverflow(format!("decimal power-of-10 exponent {exp} > 38"))
    })
}

/// Validates that `val` fits within `precision` base-10 digits.
#[inline]
pub fn validate_decimal_precision(val: i128, precision: u8) -> Result<i128> {
    if precision == 0 || precision > MAX_DECIMAL_PRECISION {
        return Err(VectraError::ArithmeticOverflow(format!(
            "invalid decimal precision {precision}"
        )));
    }
    if precision == MAX_DECIMAL_PRECISION {
        // 10^38 - 1 < i128::MAX (~1.7e38)
        let bound = POW10_I128[38] - 1;
        if val < -bound || val > bound {
            return Err(VectraError::ArithmeticOverflow(format!(
                "decimal value {val} exceeds precision {precision}"
            )));
        }
        return Ok(val);
    }
    let bound = POW10_I128[precision as usize] - 1;
    if val < -bound || val > bound {
        return Err(VectraError::ArithmeticOverflow(format!(
            "decimal value {val} exceeds precision {precision}"
        )));
    }
    Ok(val)
}

/// Rescales a `Decimal128` raw `i128` value from `from_scale` to `to_scale`.
/// Rounds half-away-from-zero when reducing scale; checks overflow when increasing scale.
pub fn decimal_rescale(
    val: i128,
    from_scale: i8,
    to_scale: i8,
    target_precision: u8,
) -> Result<i128> {
    let diff = (to_scale as i32) - (from_scale as i32);
    let rescaled = if diff == 0 {
        val
    } else if diff > 0 {
        if diff > 38 {
            if val == 0 {
                0
            } else {
                return Err(VectraError::ArithmeticOverflow(
                    "decimal scale-up overflow".into(),
                ));
            }
        } else {
            let factor = pow10_i128(diff as u8)?;
            val.checked_mul(factor).ok_or_else(|| {
                VectraError::ArithmeticOverflow("decimal rescale multiply overflow".into())
            })?
        }
    } else {
        let down = (-diff) as u32;
        if down > 38 {
            0
        } else {
            let divisor = pow10_i128(down as u8)?;
            let q = val / divisor;
            let r = val % divisor;
            let half = divisor / 2;
            if r.unsigned_abs() >= (half as u128) {
                if val >= 0 {
                    q + 1
                } else {
                    q - 1
                }
            } else {
                q
            }
        }
    };
    validate_decimal_precision(rescaled, target_precision)
}

/// Unsigned 256-bit helper (`[u64; 4]` little-endian limbs) for exact intermediate
/// Decimal128 multiplication and division without premature 128-bit overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct U256([u64; 4]);

impl U256 {
    fn mul_u128(a: u128, b: u128) -> Self {
        let a0 = a as u64 as u128;
        let a1 = (a >> 64) as u64 as u128;
        let b0 = b as u64 as u128;
        let b1 = (b >> 64) as u64 as u128;

        let p00 = a0 * b0;
        let p01 = a0 * b1;
        let p10 = a1 * b0;
        let p11 = a1 * b1;

        let l0 = p00 as u64;
        let mid1 = (p00 >> 64) + (p01 as u64 as u128) + (p10 as u64 as u128);
        let l1 = mid1 as u64;
        let mid2 = (mid1 >> 64) + (p01 >> 64) + (p10 >> 64) + (p11 as u64 as u128);
        let l2 = mid2 as u64;
        let l3 = ((mid2 >> 64) + (p11 >> 64)) as u64;

        Self([l0, l1, l2, l3])
    }

    fn div_rem_u128(self, divisor: u128) -> (Self, u128) {
        debug_assert!(divisor != 0);
        let mut q = [0u64; 4];
        let mut rem: u128 = 0;
        // Bit-by-bit long division over 256 bits (fast, exact, zero edge cases).
        for limb_idx in (0..4).rev() {
            let word = self.0[limb_idx];
            let mut q_word = 0u64;
            for bit in (0..64).rev() {
                let b = ((word >> bit) & 1) as u128;
                // Handle rem shift overflow when high bit of rem is set
                let carry = rem >> 127;
                rem = (rem << 1) | b;
                if carry != 0 || rem >= divisor {
                    rem = rem.wrapping_sub(divisor);
                    q_word |= 1u64 << bit;
                }
            }
            q[limb_idx] = q_word;
        }
        (Self(q), rem)
    }

    fn to_u128(self) -> Option<u128> {
        if self.0[2] != 0 || self.0[3] != 0 {
            None
        } else {
            Some((self.0[0] as u128) | ((self.0[1] as u128) << 64))
        }
    }
}

pub fn decimal_add(
    a: i128,
    a_scale: i8,
    b: i128,
    b_scale: i8,
    out_precision: u8,
    out_scale: i8,
) -> Result<i128> {
    let a_s = decimal_rescale(a, a_scale, out_scale, MAX_DECIMAL_PRECISION)?;
    let b_s = decimal_rescale(b, b_scale, out_scale, MAX_DECIMAL_PRECISION)?;
    let sum = a_s
        .checked_add(b_s)
        .ok_or_else(|| VectraError::ArithmeticOverflow("decimal addition overflow".into()))?;
    validate_decimal_precision(sum, out_precision)
}

pub fn decimal_sub(
    a: i128,
    a_scale: i8,
    b: i128,
    b_scale: i8,
    out_precision: u8,
    out_scale: i8,
) -> Result<i128> {
    let a_s = decimal_rescale(a, a_scale, out_scale, MAX_DECIMAL_PRECISION)?;
    let b_s = decimal_rescale(b, b_scale, out_scale, MAX_DECIMAL_PRECISION)?;
    let diff = a_s
        .checked_sub(b_s)
        .ok_or_else(|| VectraError::ArithmeticOverflow("decimal subtraction overflow".into()))?;
    validate_decimal_precision(diff, out_precision)
}

/// Exact `Decimal128` multiplication using 256-bit intermediate product before scaling to `out_scale`.
pub fn decimal_mul(
    a: i128,
    a_scale: i8,
    b: i128,
    b_scale: i8,
    out_precision: u8,
    out_scale: i8,
) -> Result<i128> {
    if a == 0 || b == 0 {
        return Ok(0);
    }
    let neg = (a < 0) ^ (b < 0);
    let prod = U256::mul_u128(a.unsigned_abs(), b.unsigned_abs());
    let prod_scale = (a_scale as i32) + (b_scale as i32);
    let diff = (out_scale as i32) - prod_scale;

    let mag_u128 = if diff == 0 {
        prod.to_u128()
            .ok_or_else(|| VectraError::ArithmeticOverflow("decimal multiply overflow".into()))?
    } else if diff > 0 {
        let mag = prod
            .to_u128()
            .ok_or_else(|| VectraError::ArithmeticOverflow("decimal multiply overflow".into()))?;
        let factor = pow10_i128(diff as u8)? as u128;
        mag.checked_mul(factor).ok_or_else(|| {
            VectraError::ArithmeticOverflow("decimal multiply scale-up overflow".into())
        })?
    } else {
        let down = (-diff) as u32;
        if down > 76 {
            0
        } else if down <= 38 {
            let div = pow10_i128(down as u8)? as u128;
            let (q, r) = prod.div_rem_u128(div);
            let mut m = q.to_u128().ok_or_else(|| {
                VectraError::ArithmeticOverflow("decimal multiply scaled overflow".into())
            })?;
            if r >= div / 2 {
                m = m.checked_add(1).ok_or_else(|| {
                    VectraError::ArithmeticOverflow("decimal rounding overflow".into())
                })?;
            }
            m
        } else {
            let d1 = pow10_i128(38)? as u128;
            let (q1, _) = prod.div_rem_u128(d1);
            let d2 = pow10_i128((down - 38) as u8)? as u128;
            let (q2, r2) = q1.div_rem_u128(d2);
            let mut m = q2.to_u128().ok_or_else(|| {
                VectraError::ArithmeticOverflow("decimal multiply scaled overflow".into())
            })?;
            if r2 >= d2 / 2 {
                m = m.checked_add(1).ok_or_else(|| {
                    VectraError::ArithmeticOverflow("decimal rounding overflow".into())
                })?;
            }
            m
        }
    };

    let signed = if neg {
        let abs = i128::try_from(mag_u128)
            .map_err(|_| VectraError::ArithmeticOverflow("decimal multiply overflow".into()))?;
        -abs
    } else {
        i128::try_from(mag_u128)
            .map_err(|_| VectraError::ArithmeticOverflow("decimal multiply overflow".into()))?
    };
    validate_decimal_precision(signed, out_precision)
}

/// Exact `Decimal128` division using 256-bit scaled numerator.
pub fn decimal_div(
    a: i128,
    a_scale: i8,
    b: i128,
    b_scale: i8,
    out_precision: u8,
    out_scale: i8,
) -> Result<i128> {
    if b == 0 {
        return Err(VectraError::DivisionByZero);
    }
    if a == 0 {
        return Ok(0);
    }
    let neg = (a < 0) ^ (b < 0);
    // Need (a * 10^(out_scale - a_scale + b_scale)) / b
    let shift = (out_scale as i32) - (a_scale as i32) + (b_scale as i32);
    let b_abs = b.unsigned_abs();
    let a_abs = a.unsigned_abs();

    let (q_u256, rem) = if shift >= 0 {
        if shift > 38 {
            return Err(VectraError::ArithmeticOverflow(
                "decimal division scale shift > 38".into(),
            ));
        }
        let factor = pow10_i128(shift as u8)? as u128;
        let num = U256::mul_u128(a_abs, factor);
        num.div_rem_u128(b_abs)
    } else {
        let down = (-shift) as u32;
        if down > 38 {
            return Ok(0);
        }
        let factor = pow10_i128(down as u8)? as u128;
        let den = b_abs.checked_mul(factor).ok_or_else(|| {
            VectraError::ArithmeticOverflow("decimal division denominator overflow".into())
        })?;
        U256([a_abs as u64, (a_abs >> 64) as u64, 0, 0]).div_rem_u128(den)
    };

    let mut mag = q_u256.to_u128().ok_or_else(|| {
        VectraError::ArithmeticOverflow("decimal division quotient overflow".into())
    })?;
    // Round half away from zero: check 2*rem >= denom (avoiding overflow)
    let denom_for_round = if shift >= 0 {
        b_abs
    } else {
        b_abs * (pow10_i128((-shift) as u8)? as u128)
    };
    if rem >= denom_for_round - rem {
        mag = mag.checked_add(1).ok_or_else(|| {
            VectraError::ArithmeticOverflow("decimal division rounding overflow".into())
        })?;
    }

    let signed = if neg {
        let abs = i128::try_from(mag)
            .map_err(|_| VectraError::ArithmeticOverflow("decimal division overflow".into()))?;
        -abs
    } else {
        i128::try_from(mag)
            .map_err(|_| VectraError::ArithmeticOverflow("decimal division overflow".into()))?
    };
    validate_decimal_precision(signed, out_precision)
}

/// Formats a `Decimal128` value cleanly with fixed fractional digits when `scale > 0`.
pub fn format_decimal(val: i128, scale: i8) -> String {
    let neg = val < 0;
    let mag = val.unsigned_abs();
    if scale == 0 {
        return if neg {
            format!("-{mag}")
        } else {
            format!("{mag}")
        };
    }
    if scale > 0 {
        let s = scale as usize;
        let digits = mag.to_string();
        let mut out = String::with_capacity(digits.len() + s + 3);
        if neg {
            out.push('-');
        }
        if digits.len() <= s {
            out.push_str("0.");
            for _ in 0..(s - digits.len()) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let split = digits.len() - s;
            out.push_str(&digits[..split]);
            out.push('.');
            out.push_str(&digits[split..]);
        }
        out
    } else {
        // Negative scale: append zeros
        let zeros = (-scale) as usize;
        let mut out = if neg {
            format!("-{mag}")
        } else {
            format!("{mag}")
        };
        if mag != 0 {
            for _ in 0..zeros {
                out.push('0');
            }
        }
        out
    }
}

/// Parses a decimal string into a scaled `i128` matching `(precision, scale)`.
pub fn parse_decimal(s: &str, precision: u8, scale: i8) -> Result<i128> {
    let trimmed = s.trim();
    let bad = || VectraError::InvalidCast {
        from: "Utf8".into(),
        to: format!("Decimal({precision},{scale})"),
        value: s.to_string(),
    };
    if trimmed.is_empty() {
        return Err(bad());
    }
    let (neg, rest) = if let Some(r) = trimmed.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = trimmed.strip_prefix('+') {
        (false, r)
    } else {
        (false, trimmed)
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, f),
        None => (rest, ""),
    };
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let mut digits = String::with_capacity(int_part.len() + frac_part.len());
    digits.push_str(int_part);
    digits.push_str(frac_part);
    let raw_scale = i8::try_from(frac_part.len()).map_err(|_| bad())?;
    let mut mag: i128 = 0;
    for b in digits.bytes() {
        mag = mag
            .checked_mul(10)
            .and_then(|v| v.checked_add((b - b'0') as i128))
            .ok_or_else(bad)?;
    }
    let signed = if neg { -mag } else { mag };
    decimal_rescale(signed, raw_scale, scale, precision)
}
