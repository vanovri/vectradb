use crate::codecs::{decode_page, encode_page_auto, CodecId, ColumnPage};
use crate::index::{BlockPruningResult, PruningPredicate, SplitBlockBloomFilter, ZoneMap};
use crate::mvcc::{Snapshot, TransactionManager, TxnId, BOOTSTRAP_TXN};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use vectra_types::{
    DataChunk, FlatVector, MemoryPool, Result, ScalarValue, Schema, SelectionVector, Vector,
    VectraError, VECTOR_CAPACITY,
};

/// Maximum rows per RowGroup (65,536 = 32 sub-blocks of 2,048 rows).
pub const ROW_GROUP_SIZE: usize = 65_536;
/// Rows per compressed `ColumnPage` sub-block (matches `VECTOR_CAPACITY`).
pub const SUB_BLOCK_SIZE: usize = VECTOR_CAPACITY;

/// Telemetry collected during a columnar table scan.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct ScanTelemetry {
    pub row_groups_total: usize,
    pub row_groups_pruned: usize,
    pub sub_blocks_total: usize,
    pub sub_blocks_pruned: usize,
    pub pages_decompressed: usize,
    pub rows_scanned: usize,
}

impl ScanTelemetry {
    pub fn block_skip_ratio(&self) -> f64 {
        if self.sub_blocks_total == 0 {
            0.0
        } else {
            (self.sub_blocks_pruned as f64) / (self.sub_blocks_total as f64)
        }
    }
}

/// A 2,048-row compressed columnar sub-block with per-column pages and ZoneMaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubBlock {
    pub row_count: u32,
    pub pages: Vec<ColumnPage>,
    pub zone_maps: Vec<ZoneMap>,
}

/// Serializable snapshot of a committed `RowGroup` (used by `.vdb` persistence).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRowGroup {
    pub row_count: u32,
    pub sub_blocks: Vec<SubBlock>,
    pub zone_maps: Vec<ZoneMap>,
    pub blooms: Vec<Option<SplitBlockBloomFilter>>,
    pub deleted_rows: Vec<u32>,
}

/// In-memory & MVCC-versioned RowGroup storing up to 65,536 rows.
#[derive(Debug)]
pub struct RowGroup {
    pub id: u64,
    pub insert_txn: TxnId,
    pub row_count: u32,
    pub sub_blocks: Vec<SubBlock>,
    pub zone_maps: Vec<ZoneMap>,
    pub blooms: Vec<Option<SplitBlockBloomFilter>>,
    /// Maps local `row_offset` (0..row_count) -> `TxnId` that deleted it.
    pub deletions: RwLock<HashMap<u32, TxnId>>,
}

impl RowGroup {
    pub fn from_chunk(
        id: u64,
        insert_txn: TxnId,
        chunk: &DataChunk,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<Self> {
        let compact = chunk.compact(pool)?;
        let total_rows = compact.len();
        let num_cols = compact.num_columns();
        let mut sub_blocks = Vec::new();
        let mut rg_zms: Vec<ZoneMap> = (0..num_cols).map(|_| ZoneMap::empty()).collect();

        // Build per-RowGroup bloom filters for hashable columns.
        let mut rg_blooms: Vec<Option<SplitBlockBloomFilter>> = Vec::with_capacity(num_cols);
        for col in &compact.columns {
            let Vector::Flat(flat) = col else {
                unreachable!("compact guarantees FlatVector");
            };
            if flat.logical_type == vectra_types::LogicalType::Boolean
                || flat.logical_type == vectra_types::LogicalType::Null
            {
                rg_blooms.push(None);
            } else {
                rg_blooms.push(Some(SplitBlockBloomFilter::from_flat_vector(flat)));
            }
        }

        let mut offset = 0usize;
        while offset < total_rows {
            let take = (total_rows - offset).min(SUB_BLOCK_SIZE);
            let mut pages = Vec::with_capacity(num_cols);
            let mut sb_zms = Vec::with_capacity(num_cols);

            for (c, col) in compact.columns.iter().enumerate() {
                let Vector::Flat(flat) = col else {
                    unreachable!();
                };
                let sub_flat = flat.slice(offset, take);
                let zm = ZoneMap::from_flat_vector(&sub_flat);
                rg_zms[c].merge(&zm);
                let page = encode_page_auto(&sub_flat)?;
                pages.push(page);
                sb_zms.push(zm);
            }

            sub_blocks.push(SubBlock {
                row_count: take as u32,
                pages,
                zone_maps: sb_zms,
            });
            offset += take;
        }

        Ok(Self {
            id,
            insert_txn,
            row_count: total_rows as u32,
            sub_blocks,
            zone_maps: rg_zms,
            blooms: rg_blooms,
            deletions: RwLock::new(HashMap::new()),
        })
    }

    pub fn from_persisted(id: u64, p: PersistedRowGroup) -> Self {
        let mut del = HashMap::with_capacity(p.deleted_rows.len());
        for r in p.deleted_rows {
            del.insert(r, BOOTSTRAP_TXN);
        }
        Self {
            id,
            insert_txn: BOOTSTRAP_TXN,
            row_count: p.row_count,
            sub_blocks: p.sub_blocks,
            zone_maps: p.zone_maps,
            blooms: p.blooms,
            deletions: RwLock::new(del),
        }
    }

    pub fn to_persisted(
        &self,
        txns: &TransactionManager,
        snap: Snapshot,
    ) -> Option<PersistedRowGroup> {
        if !txns.is_visible(self.insert_txn, snap) {
            return None;
        }
        let del = self.deletions.read();
        let mut deleted_rows = Vec::new();
        for (&row_off, &del_txn) in del.iter() {
            if txns.is_visible(del_txn, snap) {
                deleted_rows.push(row_off);
            }
        }
        deleted_rows.sort_unstable();
        Some(PersistedRowGroup {
            row_count: self.row_count,
            sub_blocks: self.sub_blocks.clone(),
            zone_maps: self.zone_maps.clone(),
            blooms: self.blooms.clone(),
            deleted_rows,
        })
    }
}

/// Column inspection telemetry (powers M10 WASM Studio Storage & ByteView X-Ray).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnStorageSummary {
    pub name: String,
    pub logical_type: String,
    pub dominant_codec: String,
    pub uncompressed_bytes: usize,
    pub compressed_bytes: usize,
    pub compression_ratio: f64,
    pub null_count: u64,
    pub min_value: Option<String>,
    pub max_value: Option<String>,
    pub bloom_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStorageSummary {
    pub table_name: String,
    pub total_rows: u64,
    pub row_groups: usize,
    pub sub_blocks: usize,
    pub columns: Vec<ColumnStorageSummary>,
}

/// A compressed, MVCC-versioned columnar table.
#[derive(Debug)]
pub struct Table {
    pub name: String,
    pub schema: Schema,
    row_groups: RwLock<Vec<Arc<RowGroup>>>,
    next_rg_id: std::sync::atomic::AtomicU64,
}

impl Table {
    pub fn new(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            schema,
            row_groups: RwLock::new(Vec::new()),
            next_rg_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    pub fn from_persisted(
        name: String,
        schema: Schema,
        persisted_rgs: Vec<PersistedRowGroup>,
    ) -> Self {
        let mut rgs = Vec::with_capacity(persisted_rgs.len());
        let mut next_id = 1u64;
        for p in persisted_rgs {
            rgs.push(Arc::new(RowGroup::from_persisted(next_id, p)));
            next_id += 1;
        }
        Self {
            name,
            schema,
            row_groups: RwLock::new(rgs),
            next_rg_id: std::sync::atomic::AtomicU64::new(next_id),
        }
    }

    pub fn to_persisted(
        &self,
        txns: &TransactionManager,
        snap: Snapshot,
    ) -> Vec<PersistedRowGroup> {
        let rgs = self.row_groups.read();
        rgs.iter()
            .filter_map(|rg| rg.to_persisted(txns, snap))
            .collect()
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Inserts a `DataChunk` into the table under `snapshot`, splitting into `ROW_GROUP_SIZE` groups.
    pub fn insert_chunk(
        &self,
        chunk: &DataChunk,
        snapshot: Snapshot,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<usize> {
        if chunk.num_columns() != self.schema.len() {
            return Err(VectraError::TypeMismatch {
                expected: format!("{} columns for table {}", self.schema.len(), self.name),
                actual: format!("{} columns", chunk.num_columns()),
            });
        }
        let n = chunk.len();
        if n == 0 {
            return Ok(0);
        }

        let mut created = Vec::new();
        let mut off = 0usize;
        while off < n {
            let take = (n - off).min(ROW_GROUP_SIZE);
            let sub = chunk.slice(off, take);
            let id = self
                .next_rg_id
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            created.push(Arc::new(RowGroup::from_chunk(
                id,
                snapshot.txn_id,
                &sub,
                pool,
            )?));
            off += take;
        }

        let mut rgs = self.row_groups.write();
        rgs.extend(created);
        Ok(n)
    }

    /// Returns a snapshot of current `Arc<RowGroup>`s (used by M06 Morsel-Driven Parallel Scan!).
    pub fn snapshot_row_groups(
        &self,
        txns: &TransactionManager,
        snap: Snapshot,
    ) -> Vec<Arc<RowGroup>> {
        let rgs = self.row_groups.read();
        rgs.iter()
            .filter(|rg| txns.is_visible(rg.insert_txn, snap))
            .cloned()
            .collect()
    }

    /// Scans the table with projection pruning, ZoneMap + Split-Block Bloom Filter pruning,
    /// and lock-free MVCC visibility filtering.
    pub fn scan(
        &self,
        projection: Option<&[usize]>,
        pruning: Option<&PruningPredicate>,
        txns: &TransactionManager,
        snap: Snapshot,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<(Vec<DataChunk>, ScanTelemetry)> {
        let proj_cols: Vec<usize> = match projection {
            Some(p) => p.to_vec(),
            None => (0..self.schema.len()).collect(),
        };
        let mut telemetry = ScanTelemetry::default();
        let mut chunks = Vec::new();
        let rgs = self.snapshot_row_groups(txns, snap);

        for rg in rgs {
            telemetry.row_groups_total += 1;
            if let Some(pred) = pruning {
                if pred.evaluate(&rg.zone_maps, Some(&rg.blooms)) == BlockPruningResult::AlwaysFalse
                {
                    telemetry.row_groups_pruned += 1;
                    telemetry.sub_blocks_total += rg.sub_blocks.len();
                    telemetry.sub_blocks_pruned += rg.sub_blocks.len();
                    continue;
                }
            }

            let del_guard = rg.deletions.read();
            let has_deletions = !del_guard.is_empty();
            let mut rg_row_base = 0u32;

            for sb in &rg.sub_blocks {
                let sb_rows = sb.row_count;
                let base = rg_row_base;
                rg_row_base += sb_rows;
                telemetry.sub_blocks_total += 1;

                if let Some(pred) = pruning {
                    if pred.evaluate(&sb.zone_maps, None) == BlockPruningResult::AlwaysFalse {
                        telemetry.sub_blocks_pruned += 1;
                        continue;
                    }
                }

                // Compute visible row selection for this sub-block under MVCC snapshot.
                let mut visible_sel: Option<Vec<u16>> = None;
                if has_deletions {
                    let mut keep = Vec::with_capacity(sb_rows as usize);
                    let mut deleted_any = false;
                    for local in 0..sb_rows {
                        let global_off = base + local;
                        let is_del = del_guard
                            .get(&global_off)
                            .is_some_and(|&dtxn| txns.is_visible(dtxn, snap));
                        if is_del {
                            deleted_any = true;
                        } else {
                            keep.push(local as u16);
                        }
                    }
                    if keep.is_empty() {
                        continue;
                    }
                    if deleted_any {
                        visible_sel = Some(keep);
                    }
                }

                let mut cols = Vec::with_capacity(proj_cols.len());
                for &col_idx in &proj_cols {
                    let flat = decode_page(&sb.pages[col_idx].raw_bytes, pool)?;
                    telemetry.pages_decompressed += 1;
                    cols.push(Vector::Flat(flat));
                }

                let chunk = match visible_sel {
                    None => DataChunk::new(cols, sb_rows as usize),
                    Some(sel) => DataChunk::with_selection(cols, SelectionVector::from_vec(sel)),
                };
                telemetry.rows_scanned += chunk.len();
                chunks.push(chunk);
            }
        }

        Ok((chunks, telemetry))
    }

    /// Scans a single `RowGroup` (used by M06 parallel morsel worker threads).
    pub fn scan_row_group(
        rg: &RowGroup,
        projection: &[usize],
        pruning: Option<&PruningPredicate>,
        txns: &TransactionManager,
        snap: Snapshot,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<(Vec<DataChunk>, ScanTelemetry)> {
        let mut telemetry = ScanTelemetry {
            row_groups_total: 1,
            ..Default::default()
        };
        if let Some(pred) = pruning {
            if pred.evaluate(&rg.zone_maps, Some(&rg.blooms)) == BlockPruningResult::AlwaysFalse {
                telemetry.row_groups_pruned = 1;
                telemetry.sub_blocks_total = rg.sub_blocks.len();
                telemetry.sub_blocks_pruned = rg.sub_blocks.len();
                return Ok((Vec::new(), telemetry));
            }
        }
        let mut chunks = Vec::new();
        let del_guard = rg.deletions.read();
        let has_deletions = !del_guard.is_empty();
        let mut rg_row_base = 0u32;

        for sb in &rg.sub_blocks {
            let sb_rows = sb.row_count;
            let base = rg_row_base;
            rg_row_base += sb_rows;
            telemetry.sub_blocks_total += 1;

            if let Some(pred) = pruning {
                if pred.evaluate(&sb.zone_maps, None) == BlockPruningResult::AlwaysFalse {
                    telemetry.sub_blocks_pruned += 1;
                    continue;
                }
            }

            let mut visible_sel: Option<Vec<u16>> = None;
            if has_deletions {
                let mut keep = Vec::with_capacity(sb_rows as usize);
                let mut deleted_any = false;
                for local in 0..sb_rows {
                    let is_del = del_guard
                        .get(&(base + local))
                        .is_some_and(|&dtxn| txns.is_visible(dtxn, snap));
                    if is_del {
                        deleted_any = true;
                    } else {
                        keep.push(local as u16);
                    }
                }
                if keep.is_empty() {
                    continue;
                }
                if deleted_any {
                    visible_sel = Some(keep);
                }
            }

            let mut cols = Vec::with_capacity(projection.len());
            for &col_idx in projection {
                let flat = decode_page(&sb.pages[col_idx].raw_bytes, pool)?;
                telemetry.pages_decompressed += 1;
                cols.push(Vector::Flat(flat));
            }
            let chunk = match visible_sel {
                None => DataChunk::new(cols, sb_rows as usize),
                Some(sel) => DataChunk::with_selection(cols, SelectionVector::from_vec(sel)),
            };
            telemetry.rows_scanned += chunk.len();
            chunks.push(chunk);
        }

        Ok((chunks, telemetry))
    }

    /// Deletes all visible rows matching `predicate` under MVCC Snapshot Isolation,
    /// raising `VectraError::TransactionConflict` on Write-Write conflicts!
    pub fn delete_where<F>(
        &self,
        mut predicate: F,
        txns: &TransactionManager,
        snap: Snapshot,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<usize>
    where
        F: FnMut(&[ScalarValue]) -> Result<bool>,
    {
        let rgs = self.snapshot_row_groups(txns, snap);
        let mut deleted_count = 0usize;

        for rg in rgs {
            let mut rg_row_base = 0u32;
            for sb in &rg.sub_blocks {
                let sb_rows = sb.row_count;
                let base = rg_row_base;
                rg_row_base += sb_rows;

                let mut cols: Vec<FlatVector> = Vec::with_capacity(self.schema.len());
                for c in 0..self.schema.len() {
                    cols.push(decode_page(&sb.pages[c].raw_bytes, pool)?);
                }

                let mut row_vals = Vec::with_capacity(self.schema.len());
                for local in 0..sb_rows {
                    let global_off = base + local;
                    // Check if already deleted before evaluating predicate.
                    let already_invisible = {
                        let del = rg.deletions.read();
                        del.get(&global_off)
                            .is_some_and(|&dtxn| txns.is_visible(dtxn, snap))
                    };
                    if already_invisible {
                        continue;
                    }

                    row_vals.clear();
                    for col in &cols {
                        row_vals.push(col.get_scalar(local as usize));
                    }
                    if predicate(&row_vals)? {
                        let mut del = rg.deletions.write();
                        if let Some(&existing) = del.get(&global_off) {
                            if !txns.check_write_conflict(existing, snap)? {
                                continue;
                            }
                        }
                        del.insert(global_off, snap.txn_id);
                        deleted_count += 1;
                    }
                }
            }
        }

        Ok(deleted_count)
    }

    /// Counts visible rows under `snapshot`.
    pub fn visible_row_count(&self, txns: &TransactionManager, snap: Snapshot) -> usize {
        let rgs = self.snapshot_row_groups(txns, snap);
        let mut total = 0usize;
        for rg in rgs {
            let del = rg.deletions.read();
            let vis_del = del
                .values()
                .filter(|&&dtxn| txns.is_visible(dtxn, snap))
                .count();
            total += (rg.row_count as usize).saturating_sub(vis_del);
        }
        total
    }

    /// Generates rich storage inspection metadata (codecs, compression ratios, ZoneMaps, blooms).
    pub fn inspect_storage(
        &self,
        txns: &TransactionManager,
        snap: Snapshot,
    ) -> TableStorageSummary {
        let rgs = self.snapshot_row_groups(txns, snap);
        let num_cols = self.schema.len();
        let mut total_rows = 0u64;
        let mut total_sbs = 0usize;

        let mut col_uncomp = vec![0usize; num_cols];
        let mut col_comp = vec![0usize; num_cols];
        let mut col_nulls = vec![0u64; num_cols];
        let mut col_bloom = vec![0usize; num_cols];
        let mut col_zms: Vec<ZoneMap> = (0..num_cols).map(|_| ZoneMap::empty()).collect();
        let mut codec_counts: Vec<HashMap<CodecId, usize>> =
            (0..num_cols).map(|_| HashMap::new()).collect();

        for rg in &rgs {
            total_rows += rg.row_count as u64;
            total_sbs += rg.sub_blocks.len();
            for c in 0..num_cols {
                col_zms[c].merge(&rg.zone_maps[c]);
                if let Some(bf) = &rg.blooms[c] {
                    col_bloom[c] += bf.size_bytes();
                }
            }
            for sb in &rg.sub_blocks {
                for (c, page) in sb.pages.iter().enumerate() {
                    col_uncomp[c] += page.uncompressed_len as usize;
                    col_comp[c] += page.raw_bytes.len();
                    col_nulls[c] += page.null_count as u64;
                    *codec_counts[c].entry(page.codec).or_insert(0) += 1;
                }
            }
        }

        let columns = self
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(c, f)| {
                let dominant = codec_counts[c]
                    .iter()
                    .max_by_key(|(_, &cnt)| cnt)
                    .map(|(&codec, _)| codec.name())
                    .unwrap_or("EMPTY")
                    .to_string();
                let ratio = if col_comp[c] == 0 {
                    1.0
                } else {
                    (col_uncomp[c].max(1) as f64) / (col_comp[c] as f64)
                };
                ColumnStorageSummary {
                    name: f.name.clone(),
                    logical_type: f.logical_type.to_string(),
                    dominant_codec: dominant,
                    uncompressed_bytes: col_uncomp[c],
                    compressed_bytes: col_comp[c],
                    compression_ratio: ratio,
                    null_count: col_nulls[c],
                    min_value: col_zms[c].min.as_ref().map(|v| v.to_display_string()),
                    max_value: col_zms[c].max.as_ref().map(|v| v.to_display_string()),
                    bloom_bytes: col_bloom[c],
                }
            })
            .collect();

        TableStorageSummary {
            table_name: self.name.clone(),
            total_rows,
            row_groups: rgs.len(),
            sub_blocks: total_sbs,
            columns,
        }
    }
}
