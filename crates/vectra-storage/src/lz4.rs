//! Pure-Rust, safe standard LZ4 raw block compressor and decompressor.
//! Compatible with Apache Parquet `LZ4_RAW` and VectraDB internal page/spill compression.

use vectra_types::{Result, VectraError};

const MIN_MATCH: usize = 4;
const HASH_LOG: usize = 12;
const HASH_TABLE_SIZE: usize = 1 << HASH_LOG;
const MAX_DISTANCE: usize = 65_535;

#[inline]
fn hash4(v: u32) -> usize {
    ((v.wrapping_mul(2_654_435_761)) >> (32 - HASH_LOG)) as usize
}

#[inline]
fn read_u32_le(src: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([src[pos], src[pos + 1], src[pos + 2], src[pos + 3]])
}

/// Compresses `src` into standard LZ4 raw block format.
pub fn lz4_compress_block(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + src.len() / 255 + 16);
    if src.is_empty() {
        return out;
    }
    if src.len() < 13 {
        write_literals_only(&mut out, src);
        return out;
    }

    let mut table = [usize::MAX; HASH_TABLE_SIZE];
    let mut anchor = 0usize;
    let mut ip = 0usize;
    let mflimit = src.len().saturating_sub(12);
    let match_limit = src.len().saturating_sub(5);

    while ip <= mflimit {
        let h = hash4(read_u32_le(src, ip));
        let candidate = table[h];
        table[h] = ip;

        if candidate != usize::MAX
            && ip > candidate
            && ip - candidate <= MAX_DISTANCE
            && read_u32_le(src, candidate) == read_u32_le(src, ip)
        {
            let offset = (ip - candidate) as u16;
            let mut match_len = MIN_MATCH;
            while ip + match_len < match_limit && src[candidate + match_len] == src[ip + match_len]
            {
                match_len += 1;
            }

            let lit_len = ip - anchor;
            let ml_code = match_len - MIN_MATCH;
            let token_lit = lit_len.min(15) as u8;
            let token_ml = ml_code.min(15) as u8;
            out.push((token_lit << 4) | token_ml);

            if lit_len >= 15 {
                let mut rem = lit_len - 15;
                while rem >= 255 {
                    out.push(255);
                    rem -= 255;
                }
                out.push(rem as u8);
            }
            out.extend_from_slice(&src[anchor..ip]);
            out.extend_from_slice(&offset.to_le_bytes());

            if ml_code >= 15 {
                let mut rem = ml_code - 15;
                while rem >= 255 {
                    out.push(255);
                    rem -= 255;
                }
                out.push(rem as u8);
            }

            ip += match_len;
            anchor = ip;
        } else {
            ip += 1;
        }
    }

    // Trailing literals
    if anchor < src.len() {
        write_literals_only(&mut out, &src[anchor..]);
    }
    out
}

fn write_literals_only(out: &mut Vec<u8>, lits: &[u8]) {
    let lit_len = lits.len();
    let token_lit = lit_len.min(15) as u8;
    out.push(token_lit << 4);
    if lit_len >= 15 {
        let mut rem = lit_len - 15;
        while rem >= 255 {
            out.push(255);
            rem -= 255;
        }
        out.push(rem as u8);
    }
    out.extend_from_slice(lits);
}

/// Decompresses a standard LZ4 raw block of exact expected `uncompressed_len`.
pub fn lz4_decompress_block(src: &[u8], uncompressed_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(uncompressed_len);
    if uncompressed_len == 0 && src.is_empty() {
        return Ok(out);
    }
    let mut ip = 0usize;

    while ip < src.len() {
        let token = src[ip];
        ip += 1;

        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            loop {
                if ip >= src.len() {
                    return Err(VectraError::CorruptFormat(
                        "LZ4 truncated literal length".into(),
                    ));
                }
                let extra = src[ip] as usize;
                ip += 1;
                lit_len += extra;
                if extra != 255 {
                    break;
                }
            }
        }
        if ip + lit_len > src.len() {
            return Err(VectraError::CorruptFormat(
                "LZ4 literal slice out of bounds".into(),
            ));
        }
        if out.len() + lit_len > uncompressed_len {
            return Err(VectraError::CorruptFormat(
                "LZ4 output overflow during literals".into(),
            ));
        }
        out.extend_from_slice(&src[ip..ip + lit_len]);
        ip += lit_len;

        if ip >= src.len() {
            break;
        }

        if ip + 2 > src.len() {
            return Err(VectraError::CorruptFormat(
                "LZ4 truncated match offset".into(),
            ));
        }
        let offset = u16::from_le_bytes([src[ip], src[ip + 1]]) as usize;
        ip += 2;
        if offset == 0 || offset > out.len() {
            return Err(VectraError::CorruptFormat(format!(
                "LZ4 invalid match offset {offset} at out len {}",
                out.len()
            )));
        }

        let mut match_len = ((token & 0x0F) as usize) + MIN_MATCH;
        if (token & 0x0F) == 15 {
            loop {
                if ip >= src.len() {
                    return Err(VectraError::CorruptFormat(
                        "LZ4 truncated match length".into(),
                    ));
                }
                let extra = src[ip] as usize;
                ip += 1;
                match_len += extra;
                if extra != 255 {
                    break;
                }
            }
        }
        if out.len() + match_len > uncompressed_len {
            return Err(VectraError::CorruptFormat(
                "LZ4 output overflow during match copy".into(),
            ));
        }
        // Byte-by-byte overlapping copy handles RLE patterns (offset < match_len) accurately.
        for _ in 0..match_len {
            let b = out[out.len() - offset];
            out.push(b);
        }
    }

    if out.len() != uncompressed_len {
        return Err(VectraError::CorruptFormat(format!(
            "LZ4 decompressed {} bytes, expected {uncompressed_len}",
            out.len()
        )));
    }
    Ok(out)
}
