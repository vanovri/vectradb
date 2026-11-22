use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use vectra_storage::{Catalog, Snapshot, Table, TransactionManager};
use vectra_types::{MemoryPool, Result, ScalarValue};

/// 14-bit precision HyperLogLog++ sketch (`m = 16,384` registers, ~0.81% standard error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyperLogLog {
    registers: Vec<u8>,
}

const HLL_P: u32 = 14;
const HLL_M: usize = 1 << HLL_P; // 16,384
const HLL_ALPHA_M: f64 = 0.7213 / (1.0 + 1.079 / (HLL_M as f64));

impl Default for HyperLogLog {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperLogLog {
    pub fn new() -> Self {
        Self {
            registers: vec![0u8; HLL_M],
        }
    }

    #[inline]
    pub fn insert_hash(&mut self, hash: u64) {
        let idx = (hash >> (64 - HLL_P)) as usize;
        let rem = (hash << HLL_P) | (1u64 << (HLL_P - 1));
        let rho = (rem.leading_zeros() + 1) as u8;
        if rho > self.registers[idx] {
            self.registers[idx] = rho;
        }
    }

    #[inline]
    pub fn insert_scalar(&mut self, val: &ScalarValue) {
        if !val.is_null() {
            self.insert_hash(val.canonical_hash());
        }
    }

    pub fn merge(&mut self, other: &Self) {
        for (a, &b) in self.registers.iter_mut().zip(other.registers.iter()) {
            if b > *a {
                *a = b;
            }
        }
    }

    pub fn estimate(&self) -> f64 {
        let mut sum = 0.0f64;
        let mut zeros = 0usize;
        for &r in &self.registers {
            if r == 0 {
                zeros += 1;
            }
            sum += 2.0f64.powi(-(r as i32));
        }
        let m = HLL_M as f64;
        let raw = HLL_ALPHA_M * m * m / sum;
        // Small-range linear counting bias correction
        if raw <= 2.5 * m && zeros > 0 {
            m * (m / (zeros as f64)).ln()
        } else {
            raw
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramBucket {
    pub min: ScalarValue,
    pub max: ScalarValue,
    pub count: u64,
    pub ndv: f64,
}

/// 64-bucket Equi-Height Histogram for accurate range predicate selectivity estimation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EquiHeightHistogram {
    pub buckets: Vec<HistogramBucket>,
    pub total_non_null: u64,
}

impl EquiHeightHistogram {
    pub fn build(mut non_null_sample: Vec<ScalarValue>, max_buckets: usize) -> Self {
        let total = non_null_sample.len() as u64;
        if total == 0 || max_buckets == 0 {
            return Self::default();
        }
        non_null_sample.sort_by(|a, b| a.total_cmp(b));
        let num_buckets = max_buckets.min(non_null_sample.len());
        let mut buckets = Vec::with_capacity(num_buckets);
        let n = non_null_sample.len();

        for b in 0..num_buckets {
            let start = (b * n) / num_buckets;
            let end = ((b + 1) * n) / num_buckets;
            if start >= end {
                continue;
            }
            let slice = &non_null_sample[start..end];
            let mut distinct = 1u64;
            for w in slice.windows(2) {
                if w[0] != w[1] {
                    distinct += 1;
                }
            }
            buckets.push(HistogramBucket {
                min: slice[0].clone(),
                max: slice[slice.len() - 1].clone(),
                count: slice.len() as u64,
                ndv: distinct as f64,
            });
        }
        Self {
            buckets,
            total_non_null: total,
        }
    }

    /// Estimates fraction in `[0.0, 1.0]` of non-null values `< lit` (or `<= lit` when `inclusive`).
    pub fn selectivity_lt(&self, lit: &ScalarValue, inclusive: bool) -> Option<f64> {
        if self.buckets.is_empty() || self.total_non_null == 0 {
            return None;
        }
        let mut matching = 0.0f64;
        for b in &self.buckets {
            let cmp_min = lit.total_cmp(&b.min);
            let cmp_max = lit.total_cmp(&b.max);
            if cmp_max.is_gt() || (inclusive && cmp_max.is_eq()) {
                matching += b.count as f64;
            } else if cmp_min.is_lt() || (!inclusive && cmp_min.is_eq()) {
                break;
            } else {
                // Interpolate within bucket
                let frac = interpolate_fraction(&b.min, &b.max, lit).unwrap_or(0.5);
                matching += (b.count as f64) * frac;
                break;
            }
        }
        Some((matching / (self.total_non_null as f64)).clamp(0.0, 1.0))
    }

    /// Estimates fraction of non-null values inside `[low, high]`.
    pub fn selectivity_between(&self, low: &ScalarValue, high: &ScalarValue) -> Option<f64> {
        if low.total_cmp(high).is_gt() {
            return Some(0.0);
        }
        let up = self.selectivity_lt(high, true)?;
        let below = self.selectivity_lt(low, false)?;
        Some((up - below).clamp(0.0, 1.0))
    }
}

fn scalar_to_f64(v: &ScalarValue) -> Option<f64> {
    match v {
        ScalarValue::Int8(x) => Some(*x as f64),
        ScalarValue::Int16(x) => Some(*x as f64),
        ScalarValue::Int32(x) | ScalarValue::Date32(x) => Some(*x as f64),
        ScalarValue::Int64(x) | ScalarValue::TimestampMicros(x) => Some(*x as f64),
        ScalarValue::Float64(x) if x.is_finite() => Some(*x),
        ScalarValue::Decimal128 { value, scale, .. } => {
            Some((*value as f64) / 10f64.powi(*scale as i32))
        }
        _ => None,
    }
}

fn interpolate_fraction(min: &ScalarValue, max: &ScalarValue, val: &ScalarValue) -> Option<f64> {
    let a = scalar_to_f64(min)?;
    let b = scalar_to_f64(max)?;
    let x = scalar_to_f64(val)?;
    if (b - a).abs() < f64::EPSILON {
        Some(0.5)
    } else {
        Some(((x - a) / (b - a)).clamp(0.0, 1.0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnStats {
    pub col_name: String,
    pub ndv: f64,
    pub null_fraction: f64,
    pub min_value: Option<ScalarValue>,
    pub max_value: Option<ScalarValue>,
    pub avg_byte_width: f64,
    pub histogram: EquiHeightHistogram,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStats {
    pub table_name: String,
    pub row_count: u64,
    pub columns: Vec<ColumnStats>,
}

/// Thread-safe registry of analyzed table & column statistics.
#[derive(Debug, Default)]
pub struct StatsCatalog {
    tables: RwLock<HashMap<String, TableStats>>,
}

impl StatsCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, stats: TableStats) {
        self.tables
            .write()
            .insert(stats.table_name.to_ascii_lowercase(), stats);
    }

    pub fn get(&self, table_name: &str) -> Option<TableStats> {
        self.tables
            .read()
            .get(&table_name.to_ascii_lowercase())
            .cloned()
    }

    /// Runs `ANALYZE` on a single storage `Table`, computing HLL++ NDV, 64-bucket EquiHeightHistograms,
    /// null fractions, min/max bounds, and average byte widths.
    pub fn analyze_table(
        &self,
        table: &Table,
        txns: &TransactionManager,
        snap: Snapshot,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<TableStats> {
        let (chunks, _) = table.scan(None, None, txns, snap, pool)?;
        let num_cols = table.schema.len();
        let mut total_rows = 0u64;
        let mut hlls: Vec<HyperLogLog> = (0..num_cols).map(|_| HyperLogLog::new()).collect();
        let mut nulls = vec![0u64; num_cols];
        let mut mins: Vec<Option<ScalarValue>> = vec![None; num_cols];
        let mut maxs: Vec<Option<ScalarValue>> = vec![None; num_cols];
        let mut byte_sums = vec![0u64; num_cols];
        // Reservoir sample up to 8,192 values per column for fast 64-bucket histogram construction
        const MAX_SAMPLE: usize = 8_192;
        let mut samples: Vec<Vec<ScalarValue>> =
            (0..num_cols).map(|_| Vec::with_capacity(256)).collect();
        let mut rng = vectra_types::SplitMix64::new(0x57A7_5100_1234_5678);

        for chunk in &chunks {
            let n = chunk.len();
            total_rows += n as u64;
            for c in 0..num_cols {
                let col = chunk.column(c);
                let fixed_w = col.logical_type().fixed_byte_width();
                for r in 0..n {
                    let phys = chunk.sel.get(r);
                    let val = col.get_scalar(phys);
                    if val.is_null() {
                        nulls[c] += 1;
                        continue;
                    }
                    hlls[c].insert_scalar(&val);
                    byte_sums[c] += match &val {
                        ScalarValue::Utf8(s) => s.len() as u64,
                        ScalarValue::Binary(b) => b.len() as u64,
                        _ => fixed_w as u64,
                    };
                    if !matches!(val, ScalarValue::Float64(f) if f.is_nan()) {
                        mins[c] = Some(match mins[c].take() {
                            None => val.clone(),
                            Some(cur) => {
                                if val.total_cmp(&cur).is_lt() {
                                    val.clone()
                                } else {
                                    cur
                                }
                            }
                        });
                        maxs[c] = Some(match maxs[c].take() {
                            None => val.clone(),
                            Some(cur) => {
                                if val.total_cmp(&cur).is_gt() {
                                    val.clone()
                                } else {
                                    cur
                                }
                            }
                        });
                    }
                    if samples[c].len() < MAX_SAMPLE {
                        samples[c].push(val);
                    } else {
                        let seen = (total_rows as usize).max(1);
                        let j = rng.next_usize(seen);
                        if j < MAX_SAMPLE {
                            samples[c][j] = val;
                        }
                    }
                }
            }
        }

        let columns: Vec<ColumnStats> = table
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(c, f)| {
                let non_null = total_rows.saturating_sub(nulls[c]);
                let null_fraction = if total_rows == 0 {
                    0.0
                } else {
                    (nulls[c] as f64) / (total_rows as f64)
                };
                let avg_byte_width = if non_null == 0 {
                    f.logical_type.fixed_byte_width() as f64
                } else {
                    (byte_sums[c] as f64) / (non_null as f64)
                };
                let ndv = hlls[c]
                    .estimate()
                    .min(non_null as f64)
                    .max(if non_null > 0 { 1.0 } else { 0.0 });
                let histogram = EquiHeightHistogram::build(std::mem::take(&mut samples[c]), 64);
                ColumnStats {
                    col_name: f.name.clone(),
                    ndv,
                    null_fraction,
                    min_value: mins[c].take(),
                    max_value: maxs[c].take(),
                    avg_byte_width,
                    histogram,
                }
            })
            .collect();

        let ts = TableStats {
            table_name: table.name.clone(),
            row_count: total_rows,
            columns,
        };
        self.insert(ts.clone());
        Ok(ts)
    }

    pub fn analyze_catalog(
        &self,
        catalog: &Catalog,
        snap: Snapshot,
        pool: &Arc<dyn MemoryPool>,
    ) -> Result<()> {
        for name in catalog.list_tables() {
            let tbl = catalog.get_table(&name)?;
            let _ = self.analyze_table(&tbl, catalog.transaction_manager(), snap, pool)?;
        }
        Ok(())
    }
}
