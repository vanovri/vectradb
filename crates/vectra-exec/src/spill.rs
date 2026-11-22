use crate::metrics::ExecutionMetrics;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use vectra_storage::{lz4_compress_block, lz4_decompress_block};
use vectra_types::{Result, VectraError};

const SPILL_MAGIC: [u8; 4] = *b"VSPL";
const PAGE_HEADER_LEN: usize = 16;

/// Manages temporary `.vspill` files, accounting for total spilled bytes and guaranteeing
/// RAII cleanup on completion, error, panic, or query cancellation.
#[derive(Debug)]
pub struct SpillManager {
    dir: PathBuf,
    next_id: AtomicU64,
    active_files: AtomicUsize,
    total_spilled_bytes: AtomicU64,
}

impl SpillManager {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let dir = dir.into();
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::fs::create_dir_all(&dir)?;
        }
        Ok(Arc::new(Self {
            dir,
            next_id: AtomicU64::new(1),
            active_files: AtomicUsize::new(0),
            total_spilled_bytes: AtomicU64::new(0),
        }))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn active_file_count(&self) -> usize {
        self.active_files.load(Ordering::Acquire)
    }

    pub fn total_spilled_bytes(&self) -> u64 {
        self.total_spilled_bytes.load(Ordering::Acquire)
    }

    pub fn create_writer(
        self: &Arc<Self>,
        plan_id: usize,
        metrics: Option<Arc<ExecutionMetrics>>,
    ) -> Result<SpillWriter> {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        self.active_files.fetch_add(1, Ordering::AcqRel);

        #[cfg(not(target_arch = "wasm32"))]
        {
            let pid = std::process::id();
            let path = self.dir.join(format!("vectra_{pid}_{plan_id}_{id}.vspill"));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .inspect_err(|_| {
                    self.active_files.fetch_sub(1, Ordering::AcqRel);
                })?;
            Ok(SpillWriter {
                manager: Arc::clone(self),
                plan_id,
                metrics,
                backend: SpillWriteBackend::File {
                    path,
                    writer: std::io::BufWriter::with_capacity(16 * 1024, file),
                },
                blocks_written: 0,
                bytes_written: 0,
                finished: false,
            })
        }

        #[cfg(target_arch = "wasm32")]
        {
            Ok(SpillWriter {
                manager: Arc::clone(self),
                plan_id,
                metrics,
                backend: SpillWriteBackend::Memory(Vec::new()),
                blocks_written: 0,
                bytes_written: 0,
                finished: false,
            })
        }
    }
}

#[derive(Debug)]
enum SpillWriteBackend {
    #[cfg(not(target_arch = "wasm32"))]
    File {
        path: PathBuf,
        writer: std::io::BufWriter<std::fs::File>,
    },
    #[cfg(target_arch = "wasm32")]
    Memory(Vec<u8>),
}

/// Streaming LZ4 + CRC32C spill writer. Drops and deletes its underlying `.vspill` file
/// if dropped before calling `finish()`.
#[derive(Debug)]
pub struct SpillWriter {
    manager: Arc<SpillManager>,
    plan_id: usize,
    metrics: Option<Arc<ExecutionMetrics>>,
    backend: SpillWriteBackend,
    blocks_written: usize,
    bytes_written: u64,
    finished: bool,
}

impl SpillWriter {
    /// Compresses `payload` with LZ4, computes CRC32C, and writes a framed spill block.
    pub fn write_block(&mut self, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return Ok(());
        }
        let compressed = lz4_compress_block(payload);
        let crc = crc32c::crc32c(&compressed);

        let mut header = [0u8; PAGE_HEADER_LEN];
        header[0..4].copy_from_slice(&SPILL_MAGIC);
        header[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[8..12].copy_from_slice(&(compressed.len() as u32).to_le_bytes());
        header[12..16].copy_from_slice(&crc.to_le_bytes());

        match &mut self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            SpillWriteBackend::File { writer, .. } => {
                use std::io::Write;
                writer.write_all(&header)?;
                writer.write_all(&compressed)?;
            }
            #[cfg(target_arch = "wasm32")]
            SpillWriteBackend::Memory(buf) => {
                buf.extend_from_slice(&header);
                buf.extend_from_slice(&compressed);
            }
        }

        let written = (PAGE_HEADER_LEN + compressed.len()) as u64;
        self.blocks_written += 1;
        self.bytes_written += written;
        self.manager
            .total_spilled_bytes
            .fetch_add(written, Ordering::Relaxed);
        if let Some(m) = &self.metrics {
            m.record_spill(self.plan_id, written);
        }
        Ok(())
    }

    pub fn blocks_written(&self) -> usize {
        self.blocks_written
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Flushes buffered data and converts into an RAII `SpillFile` handle ready for reading.
    pub fn finish(mut self) -> Result<SpillFile> {
        self.finished = true;
        let blocks = self.blocks_written;
        let bytes = self.bytes_written;
        let manager = Arc::clone(&self.manager);

        match &mut self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            SpillWriteBackend::File { path, writer } => {
                use std::io::Write;
                writer.flush()?;
                let owned_path = std::mem::take(path);
                Ok(SpillFile {
                    manager,
                    backend: SpillReadBackend::File(owned_path),
                    blocks,
                    bytes,
                })
            }
            #[cfg(target_arch = "wasm32")]
            SpillWriteBackend::Memory(buf) => {
                let data = std::mem::take(buf);
                Ok(SpillFile {
                    manager,
                    backend: SpillReadBackend::Memory(data),
                    blocks,
                    bytes,
                })
            }
        }
    }
}

impl Drop for SpillWriter {
    fn drop(&mut self) {
        if !self.finished {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let SpillWriteBackend::File { path, .. } = &self.backend;
                let _ = std::fs::remove_file(path);
            }
            self.manager.active_files.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[derive(Debug)]
enum SpillReadBackend {
    #[cfg(not(target_arch = "wasm32"))]
    File(PathBuf),
    #[cfg(target_arch = "wasm32")]
    Memory(Vec<u8>),
}

/// RAII handle to a finalized spill file. Automatically deletes the temporary file on `Drop`!
#[derive(Debug)]
pub struct SpillFile {
    manager: Arc<SpillManager>,
    backend: SpillReadBackend,
    blocks: usize,
    bytes: u64,
}

impl SpillFile {
    pub fn block_count(&self) -> usize {
        self.blocks
    }

    pub fn byte_len(&self) -> u64 {
        self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.blocks == 0
    }

    /// Opens a streaming reader that keeps ownership of this `SpillFile` (auto-deleting when done!).
    pub fn into_reader(self) -> Result<SpillReader> {
        #[cfg(not(target_arch = "wasm32"))]
        let reader = match &self.backend {
            SpillReadBackend::File(path) => {
                let f = std::fs::File::open(path)?;
                SpillStreamReader::File(std::io::BufReader::with_capacity(16 * 1024, f))
            }
        };
        #[cfg(target_arch = "wasm32")]
        let reader = SpillStreamReader::Memory { offset: 0 };

        Ok(SpillReader {
            file: self,
            reader,
            blocks_read: 0,
        })
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let SpillReadBackend::File(path) = &self.backend;
            if !path.as_os_str().is_empty() {
                let _ = std::fs::remove_file(path);
            }
        }
        self.manager.active_files.fetch_sub(1, Ordering::AcqRel);
    }
}

enum SpillStreamReader {
    #[cfg(not(target_arch = "wasm32"))]
    File(std::io::BufReader<std::fs::File>),
    #[cfg(target_arch = "wasm32")]
    Memory { offset: usize },
}

/// Streaming block-by-block reader over a `SpillFile` verifying magic and CRC32C on every page.
pub struct SpillReader {
    file: SpillFile,
    reader: SpillStreamReader,
    blocks_read: usize,
}

impl SpillReader {
    /// Reads and decompresses the next framed block, returning `Ok(None)` at EOF.
    pub fn next_block(&mut self) -> Result<Option<Vec<u8>>> {
        if self.blocks_read >= self.file.blocks {
            return Ok(None);
        }

        let mut header = [0u8; PAGE_HEADER_LEN];
        let compressed = match &mut self.reader {
            #[cfg(not(target_arch = "wasm32"))]
            SpillStreamReader::File(br) => {
                use std::io::Read;
                br.read_exact(&mut header)?;
                let comp_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
                let mut comp = vec![0u8; comp_len];
                br.read_exact(&mut comp)?;
                comp
            }
            #[cfg(target_arch = "wasm32")]
            SpillStreamReader::Memory { offset } => {
                let SpillReadBackend::Memory(data) = &self.file.backend;
                if *offset + PAGE_HEADER_LEN > data.len() {
                    return Err(VectraError::CorruptFormat("truncated spill header".into()));
                }
                header.copy_from_slice(&data[*offset..*offset + PAGE_HEADER_LEN]);
                *offset += PAGE_HEADER_LEN;
                let comp_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
                if *offset + comp_len > data.len() {
                    return Err(VectraError::CorruptFormat("truncated spill payload".into()));
                }
                let comp = data[*offset..*offset + comp_len].to_vec();
                *offset += comp_len;
                comp
            }
        };

        if header[0..4] != SPILL_MAGIC {
            return Err(VectraError::CorruptFormat(
                "invalid VSPL spill page magic".into(),
            ));
        }
        let uncomp_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let expected_crc = u32::from_le_bytes(header[12..16].try_into().unwrap());
        let actual_crc = crc32c::crc32c(&compressed);
        if actual_crc != expected_crc {
            return Err(VectraError::ChecksumMismatch {
                expected: expected_crc,
                actual: actual_crc,
                context: "spill page".into(),
            });
        }

        let decompressed = lz4_decompress_block(&compressed, uncomp_len)?;
        self.blocks_read += 1;
        Ok(Some(decompressed))
    }
}
