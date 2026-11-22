//! Pure-Rust, safe Google Snappy block compressor & decompressor for Apache Parquet `SNAPPY`.

use vectra_types::{Result, VectraError};

const HASH_BITS: usize = 12;
const HASH_SIZE: usize = 1 << HASH_BITS;

#[inline]
fn hash4(v: u32) -> usize {
    ((v.wrapping_mul(0x1e35_a7bd)) >> (32 - HASH_BITS)) as usize
}

#[inline]
fn read_u32(src: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([src[pos], src[pos + 1], src[pos + 2], src[pos + 3]])
}

fn write_varint(mut n: usize, out: &mut Vec<u8>) {
    while n >= 0x80 {
        out.push(((n & 0x7F) | 0x80) as u8);
        n >>= 7;
    }
    out.push(n as u8);
}

fn read_varint(src: &[u8], pos: &mut usize) -> Result<usize> {
    let mut val = 0usize;
    let mut shift = 0u32;
    loop {
        if *pos >= src.len() || shift >= 64 {
            return Err(VectraError::CorruptFormat("Snappy truncated varint".into()));
        }
        let b = src[*pos];
        *pos += 1;
        val |= ((b & 0x7F) as usize) << shift;
        if (b & 0x80) == 0 {
            return Ok(val);
        }
        shift += 7;
    }
}

fn emit_literal(lits: &[u8], out: &mut Vec<u8>) {
    if lits.is_empty() {
        return;
    }
    let n = lits.len() - 1;
    if n < 60 {
        out.push((n as u8) << 2);
    } else if n < (1 << 8) {
        out.push(60 << 2);
        out.push(n as u8);
    } else if n < (1 << 16) {
        out.push(61 << 2);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n < (1 << 24) {
        out.push(62 << 2);
        out.push((n & 0xFF) as u8);
        out.push(((n >> 8) & 0xFF) as u8);
        out.push(((n >> 16) & 0xFF) as u8);
    } else {
        out.push(63 << 2);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    }
    out.extend_from_slice(lits);
}

fn emit_copy(offset: usize, mut len: usize, out: &mut Vec<u8>) {
    while len > 0 {
        let take = len.min(64);
        if take <= 11 && offset < 2048 && take >= 4 {
            // Copy with 1-byte offset (tag 01)
            let tag = 0x01 | (((take - 4) as u8) << 2) | (((offset >> 8) as u8) << 5);
            out.push(tag);
            out.push((offset & 0xFF) as u8);
        } else if offset < 65_536 {
            // Copy with 2-byte offset (tag 10)
            let tag = 0x02 | (((take - 1) as u8) << 2);
            out.push(tag);
            out.extend_from_slice(&(offset as u16).to_le_bytes());
        } else {
            // Copy with 4-byte offset (tag 11)
            let tag = 0x03 | (((take - 1) as u8) << 2);
            out.push(tag);
            out.extend_from_slice(&(offset as u32).to_le_bytes());
        }
        len -= take;
    }
}

/// Compresses `src` into standard Snappy block format.
pub fn snappy_compress(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + 16);
    write_varint(src.len(), &mut out);
    if src.is_empty() {
        return out;
    }
    if src.len() < 8 {
        emit_literal(src, &mut out);
        return out;
    }

    let mut table = [usize::MAX; HASH_SIZE];
    let mut ip = 0usize;
    let mut anchor = 0usize;
    let limit = src.len() - 4;

    while ip <= limit {
        let h = hash4(read_u32(src, ip));
        let cand = table[h];
        table[h] = ip;

        if cand != usize::MAX
            && ip > cand
            && ip - cand < 65_536
            && read_u32(src, cand) == read_u32(src, ip)
        {
            emit_literal(&src[anchor..ip], &mut out);
            let offset = ip - cand;
            let mut mlen = 4usize;
            while ip + mlen < src.len() && src[cand + mlen] == src[ip + mlen] {
                mlen += 1;
            }
            emit_copy(offset, mlen, &mut out);
            ip += mlen;
            anchor = ip;
        } else {
            ip += 1;
        }
    }

    if anchor < src.len() {
        emit_literal(&src[anchor..], &mut out);
    }
    out
}

/// Decompresses a standard Snappy raw block.
pub fn snappy_decompress(src: &[u8]) -> Result<Vec<u8>> {
    let mut ip = 0usize;
    let uncompressed_len = read_varint(src, &mut ip)?;
    let mut out = Vec::with_capacity(uncompressed_len);

    while ip < src.len() {
        let tag = src[ip];
        ip += 1;
        match tag & 0x03 {
            0 => {
                // Literal
                let len_code = (tag >> 2) as usize;
                let lit_len = if len_code < 60 {
                    len_code + 1
                } else {
                    let extra_bytes = len_code - 59;
                    if ip + extra_bytes > src.len() {
                        return Err(VectraError::CorruptFormat(
                            "Snappy truncated literal length".into(),
                        ));
                    }
                    let mut n = 0usize;
                    for b in 0..extra_bytes {
                        n |= (src[ip + b] as usize) << (b * 8);
                    }
                    ip += extra_bytes;
                    n + 1
                };
                if ip + lit_len > src.len() || out.len() + lit_len > uncompressed_len {
                    return Err(VectraError::CorruptFormat(
                        "Snappy literal out of bounds".into(),
                    ));
                }
                out.extend_from_slice(&src[ip..ip + lit_len]);
                ip += lit_len;
            }
            1 => {
                // Copy with 1-byte offset
                if ip >= src.len() {
                    return Err(VectraError::CorruptFormat("Snappy truncated copy1".into()));
                }
                let len = (((tag >> 2) & 0x07) as usize) + 4;
                let offset = (((tag as usize) & 0xE0) << 3) | (src[ip] as usize);
                ip += 1;
                copy_from_history(&mut out, offset, len, uncompressed_len)?;
            }
            2 => {
                // Copy with 2-byte offset
                if ip + 2 > src.len() {
                    return Err(VectraError::CorruptFormat("Snappy truncated copy2".into()));
                }
                let len = ((tag >> 2) as usize) + 1;
                let offset = u16::from_le_bytes([src[ip], src[ip + 1]]) as usize;
                ip += 2;
                copy_from_history(&mut out, offset, len, uncompressed_len)?;
            }
            3 => {
                // Copy with 4-byte offset
                if ip + 4 > src.len() {
                    return Err(VectraError::CorruptFormat("Snappy truncated copy4".into()));
                }
                let len = ((tag >> 2) as usize) + 1;
                let offset =
                    u32::from_le_bytes([src[ip], src[ip + 1], src[ip + 2], src[ip + 3]]) as usize;
                ip += 4;
                copy_from_history(&mut out, offset, len, uncompressed_len)?;
            }
            _ => unreachable!(),
        }
    }

    if out.len() != uncompressed_len {
        return Err(VectraError::CorruptFormat(format!(
            "Snappy decompressed {} bytes, expected {uncompressed_len}",
            out.len()
        )));
    }
    Ok(out)
}

fn copy_from_history(out: &mut Vec<u8>, offset: usize, len: usize, limit: usize) -> Result<()> {
    if offset == 0 || offset > out.len() || out.len() + len > limit {
        return Err(VectraError::CorruptFormat(format!(
            "Snappy invalid copy offset={offset} len={len} out={}",
            out.len()
        )));
    }
    for _ in 0..len {
        let b = out[out.len() - offset];
        out.push(b);
    }
    Ok(())
}
