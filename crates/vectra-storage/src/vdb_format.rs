//! Single-file `.vdb` persistent database format with double-buffered ping-pong Superblocks
//! and hardware CRC32C validation on superblocks, catalog metadata, and every `ColumnPage`.

use crate::table::PersistedRowGroup;
use serde::{Deserialize, Serialize};
use vectra_types::{Result, Schema, VectraError};

pub const VDB_MAGIC: &[u8; 8] = b"VECTRADB";
pub const VDB_VERSION: u32 = 1;
pub const SUPERBLOCK_SIZE: usize = 64;
pub const HEADER_TOTAL_SIZE: usize = 16 + 2 * SUPERBLOCK_SIZE; // 144 bytes

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTable {
    pub name: String,
    pub schema: Schema,
    pub row_groups: Vec<PersistedRowGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedCatalog {
    pub tables: Vec<PersistedTable>,
}

#[derive(Debug, Clone, Copy)]
struct Superblock {
    generation: u64,
    payload_offset: u64,
    payload_len: u64,
    payload_crc32c: u32,
}

impl Superblock {
    fn encode(&self) -> [u8; SUPERBLOCK_SIZE] {
        let mut buf = [0u8; SUPERBLOCK_SIZE];
        buf[0..8].copy_from_slice(&self.generation.to_le_bytes());
        buf[8..16].copy_from_slice(&self.payload_offset.to_le_bytes());
        buf[16..24].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[24..28].copy_from_slice(&self.payload_crc32c.to_le_bytes());
        let sb_crc = crc32c::crc32c(&buf[0..60]);
        buf[60..64].copy_from_slice(&sb_crc.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < SUPERBLOCK_SIZE {
            return Err(VectraError::CorruptFormat("truncated superblock".into()));
        }
        let expected_crc = u32::from_le_bytes(buf[60..64].try_into().unwrap());
        let actual_crc = crc32c::crc32c(&buf[0..60]);
        if actual_crc != expected_crc {
            return Err(VectraError::ChecksumMismatch {
                expected: expected_crc,
                actual: actual_crc,
                context: "VDB Superblock".into(),
            });
        }
        Ok(Self {
            generation: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            payload_offset: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            payload_len: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            payload_crc32c: u32::from_le_bytes(buf[24..28].try_into().unwrap()),
        })
    }
}

/// Serializes `PersistedCatalog` into an in-memory `.vdb` byte buffer (also usable in WASM!).
pub fn serialize_vdb_bytes(catalog: &PersistedCatalog, generation: u64) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(catalog)
        .map_err(|e| VectraError::Internal(format!("catalog serialization failed: {e}")))?;
    let payload_crc = crc32c::crc32c(&payload);
    let sb = Superblock {
        generation: generation.max(1),
        payload_offset: HEADER_TOTAL_SIZE as u64,
        payload_len: payload.len() as u64,
        payload_crc32c: payload_crc,
    };

    let mut out = Vec::with_capacity(HEADER_TOTAL_SIZE + payload.len());
    out.extend_from_slice(VDB_MAGIC);
    out.extend_from_slice(&VDB_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    let sb_bytes = sb.encode();
    // Write both slots for fresh image; incremental file commits alternate slot 0 / slot 1.
    out.extend_from_slice(&sb_bytes);
    out.extend_from_slice(&sb_bytes);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Deserializes and validates a `.vdb` byte image, picking the highest valid superblock generation
/// and verifying both superblock and payload CRC32C checksums.
pub fn deserialize_vdb_bytes(bytes: &[u8]) -> Result<(PersistedCatalog, u64)> {
    if bytes.len() < HEADER_TOTAL_SIZE {
        return Err(VectraError::CorruptFormat("VDB file too small".into()));
    }
    if &bytes[0..8] != VDB_MAGIC {
        return Err(VectraError::CorruptFormat(
            "invalid VDB magic header".into(),
        ));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != VDB_VERSION {
        return Err(VectraError::CorruptFormat(format!(
            "unsupported VDB version {version}"
        )));
    }

    let sb0 = Superblock::decode(&bytes[16..16 + SUPERBLOCK_SIZE]);
    let sb1 = Superblock::decode(&bytes[16 + SUPERBLOCK_SIZE..HEADER_TOTAL_SIZE]);

    let chosen = match (sb0, sb1) {
        (Ok(a), Ok(b)) => {
            if a.generation >= b.generation {
                a
            } else {
                b
            }
        }
        (Ok(a), Err(_)) => a,
        (Err(_), Ok(b)) => b,
        (Err(e), Err(_)) => return Err(e),
    };

    let off = chosen.payload_offset as usize;
    let len = chosen.payload_len as usize;
    if off + len > bytes.len() {
        return Err(VectraError::CorruptFormat(
            "VDB payload extends past EOF".into(),
        ));
    }
    let payload = &bytes[off..off + len];
    let actual_crc = crc32c::crc32c(payload);
    if actual_crc != chosen.payload_crc32c {
        return Err(VectraError::ChecksumMismatch {
            expected: chosen.payload_crc32c,
            actual: actual_crc,
            context: "VDB Catalog Payload".into(),
        });
    }
    let catalog: PersistedCatalog = serde_json::from_slice(payload)
        .map_err(|e| VectraError::CorruptFormat(format!("invalid catalog metadata: {e}")))?;
    Ok((catalog, chosen.generation))
}

/// Atomic double-buffered ping-pong commit onto a filesystem `.vdb` file.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_vdb_file(path: &std::path::Path, catalog: &PersistedCatalog) -> Result<u64> {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut existing_gen = 0u64;
    if path.exists() {
        let mut f = OpenOptions::new().read(true).open(path)?;
        let mut hdr = [0u8; HEADER_TOTAL_SIZE];
        if f.read_exact(&mut hdr).is_ok() && &hdr[0..8] == VDB_MAGIC {
            if let Ok(a) = Superblock::decode(&hdr[16..16 + SUPERBLOCK_SIZE]) {
                existing_gen = existing_gen.max(a.generation);
            }
            if let Ok(b) = Superblock::decode(&hdr[16 + SUPERBLOCK_SIZE..HEADER_TOTAL_SIZE]) {
                existing_gen = existing_gen.max(b.generation);
            }
        }
    }

    let next_gen = existing_gen + 1;
    let payload = serde_json::to_vec(catalog)
        .map_err(|e| VectraError::Internal(format!("catalog serialize error: {e}")))?;
    let payload_crc = crc32c::crc32c(&payload);

    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;

    let file_len = file.metadata()?.len();
    if file_len < HEADER_TOTAL_SIZE as u64 {
        let mut init_hdr = [0u8; HEADER_TOTAL_SIZE];
        init_hdr[0..8].copy_from_slice(VDB_MAGIC);
        init_hdr[8..12].copy_from_slice(&VDB_VERSION.to_le_bytes());
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&init_hdr)?;
    }

    // Append new payload cleanly after current end-of-file so old generation remains intact until superblock flip!
    let payload_offset = file.seek(SeekFrom::End(0))?.max(HEADER_TOTAL_SIZE as u64);
    file.seek(SeekFrom::Start(payload_offset))?;
    file.write_all(&payload)?;
    file.sync_all()?;

    let sb = Superblock {
        generation: next_gen,
        payload_offset,
        payload_len: payload.len() as u64,
        payload_crc32c: payload_crc,
    };
    let slot_offset = if next_gen % 2 == 1 {
        16u64
    } else {
        (16 + SUPERBLOCK_SIZE) as u64
    };
    file.seek(SeekFrom::Start(slot_offset))?;
    file.write_all(&sb.encode())?;
    file.sync_all()?;
    Ok(next_gen)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_vdb_file(path: &std::path::Path) -> Result<(PersistedCatalog, u64)> {
    let bytes = std::fs::read(path)?;
    deserialize_vdb_bytes(&bytes)
}
