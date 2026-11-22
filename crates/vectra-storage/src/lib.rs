#![deny(unsafe_code)]

pub mod catalog;
pub mod codecs;
pub mod csv_loader;
pub mod index;
pub mod lz4;
pub mod mvcc;
pub mod parquet;
pub mod snappy;
pub mod table;
pub mod vdb_format;

pub use catalog::Catalog;
pub use codecs::{decode_page, encode_page, encode_page_auto, CodecId, ColumnPage};
pub use csv_loader::{infer_csv_schema, load_csv_chunks, parse_csv_record, CsvOptions};
pub use index::{BlockPruningResult, PruningPredicate, SplitBlockBloomFilter, ZoneMap};
pub use lz4::{lz4_compress_block, lz4_decompress_block};
pub use mvcc::{Snapshot, Timestamp, TransactionManager, TxnId, TxnState, BOOTSTRAP_TXN};
pub use parquet::{read_parquet_bytes, write_parquet_bytes, ParquetCompression, ParquetScanStats};
pub use snappy::{snappy_compress, snappy_decompress};
pub use table::{
    ColumnStorageSummary, PersistedRowGroup, RowGroup, ScanTelemetry, SubBlock, Table,
    TableStorageSummary, ROW_GROUP_SIZE, SUB_BLOCK_SIZE,
};
pub use vdb_format::{
    deserialize_vdb_bytes, serialize_vdb_bytes, PersistedCatalog, PersistedTable,
};
