#![deny(unsafe_code)]

pub mod context;
pub mod engine;
pub mod executor;
pub mod metrics;
pub mod morsel;
pub mod ops;
pub mod spill;
pub mod stream;

pub use context::{ExecutionConfig, ExecutionContext};
pub use engine::{parse_byte_size, Database, QueryResult, Session};
pub use executor::{collect_chunks, execute_physical_plan};
pub use metrics::{
    format_bytes, format_explain_analyze, ExecutionMetrics, ExecutionMetricsSnapshot,
    OperatorMetrics, WallTimer,
};
pub use spill::{SpillFile, SpillManager, SpillReader, SpillWriter};
pub use stream::{chunk_from_scalar_rows, chunks_from_scalar_rows, ChunkStream};
