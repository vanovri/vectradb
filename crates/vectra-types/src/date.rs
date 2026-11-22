use crate::error::{Result, VectraError};

pub const MICROS_PER_SECOND: i64 = 1_000_000;
pub const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
pub const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
pub const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;

/// Howard Hinnant's O(1) proleptic Gregorian algorithm: civil `(year, month, day)` -> days since 1970-01-01.
#[inline]
pub const fn days_from_civil(year: i32, month: u32, day: u32) -> i32 {
    let y = (year as i64) - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let m = month as u64;
    let d = day as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    (era * 146_097 + doe as i64 - 719_468) as i32
}

/// Howard Hinnant's O(1) inverse: days since 1970-01-01 -> `(year, month, day)`.
#[inline]
pub const fn civil_from_days(days: i32) -> (i32, u32, u32) {
    let z = (days as i64) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

#[inline]
pub const fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[inline]
pub const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Parses `YYYY-MM-DD` into `Date32` (days since 1970-01-01).
pub fn parse_date32(s: &str) -> Result<i32> {
    let trimmed = s.trim();
    let mut parts = trimmed.split('-');
    let bad = || VectraError::InvalidCast {
        from: "Utf8".into(),
        to: "Date32".into(),
        value: s.to_string(),
    };
    let y_str = parts.next().ok_or_else(bad)?;
    let m_str = parts.next().ok_or_else(bad)?;
    let d_str = parts.next().ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }
    let year: i32 = y_str.parse().map_err(|_| bad())?;
    let month: u32 = m_str.parse().map_err(|_| bad())?;
    let day: u32 = d_str.parse().map_err(|_| bad())?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(bad());
    }
    Ok(days_from_civil(year, month, day))
}

/// Formats `Date32` as ISO-8601 `YYYY-MM-DD`.
pub fn format_date32(days: i32) -> String {
    let (y, m, d) = civil_from_days(days);
    if y < 0 {
        format!("-{:04}-{:02}-{:02}", y.unsigned_abs(), m, d)
    } else {
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// Parses ISO-8601 timestamp (`YYYY-MM-DD[T| ]HH:MM:SS[.ffffff][Z]`) or date-only into UTC micros.
pub fn parse_timestamp_micros(s: &str) -> Result<i64> {
    let trimmed = s.trim().trim_end_matches('Z');
    let bad = || VectraError::InvalidCast {
        from: "Utf8".into(),
        to: "TimestampMicros".into(),
        value: s.to_string(),
    };
    let (date_part, time_part) = if let Some(idx) = trimmed.find(['T', ' ']) {
        (&trimmed[..idx], Some(&trimmed[idx + 1..]))
    } else {
        (trimmed, None)
    };
    let days = parse_date32(date_part)?;
    let mut total = (days as i64).checked_mul(MICROS_PER_DAY).ok_or_else(bad)?;
    if let Some(t) = time_part {
        let (hms, frac) = match t.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (t, None),
        };
        let mut tparts = hms.split(':');
        let hh: i64 = tparts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
        let mm: i64 = tparts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
        let ss: i64 = match tparts.next() {
            Some(v) => v.parse().map_err(|_| bad())?,
            None => 0,
        };
        if tparts.next().is_some()
            || !(0..24).contains(&hh)
            || !(0..60).contains(&mm)
            || !(0..60).contains(&ss)
        {
            return Err(bad());
        }
        let mut micros = 0i64;
        if let Some(f) = frac {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            let mut scale = 100_000i64;
            for b in f.bytes().take(6) {
                micros += ((b - b'0') as i64) * scale;
                scale /= 10;
            }
        }
        total = total
            .checked_add(
                hh * MICROS_PER_HOUR + mm * MICROS_PER_MINUTE + ss * MICROS_PER_SECOND + micros,
            )
            .ok_or_else(bad)?;
    }
    Ok(total)
}

/// Decomposes UTC microseconds since Unix epoch into `(year, month, day, hour, minute, second, micro)`.
pub fn timestamp_parts(micros: i64) -> (i32, u32, u32, u32, u32, u32, u32) {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let rem = micros.rem_euclid(MICROS_PER_DAY);
    let (y, m, d) = civil_from_days(days as i32);
    let hour = (rem / MICROS_PER_HOUR) as u32;
    let rem = rem % MICROS_PER_HOUR;
    let minute = (rem / MICROS_PER_MINUTE) as u32;
    let rem = rem % MICROS_PER_MINUTE;
    let second = (rem / MICROS_PER_SECOND) as u32;
    let micro = (rem % MICROS_PER_SECOND) as u32;
    (y, m, d, hour, minute, second, micro)
}

/// Formats `TimestampMicros` as `YYYY-MM-DD HH:MM:SS[.ffffff]`.
pub fn format_timestamp_micros(micros: i64) -> String {
    let (y, m, d, hh, mm, ss, us) = timestamp_parts(micros);
    if us == 0 {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{us:06}")
    }
}

/// Adds `months` (positive or negative) to a `Date32`, clamping day-of-month to end of month.
pub fn add_months_date32(days: i32, delta_months: i32) -> i32 {
    let (y, m, d) = civil_from_days(days);
    let total_m = (y as i64) * 12 + (m as i64 - 1) + (delta_months as i64);
    let new_y = total_m.div_euclid(12) as i32;
    let new_m = (total_m.rem_euclid(12) + 1) as u32;
    let new_d = d.min(days_in_month(new_y, new_m));
    days_from_civil(new_y, new_m, new_d)
}
