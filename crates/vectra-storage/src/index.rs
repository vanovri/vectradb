use serde::{Deserialize, Serialize};
use vectra_types::{FlatVector, ScalarValue};

/// Tri-state block pruning outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPruningResult {
    /// Proven that zero rows in this block can satisfy the predicate -> skip decompression!
    AlwaysFalse,
    /// Block might contain matching rows -> must decompress & filter.
    MaybeTrue,
    /// Proven that every row in this block satisfies the predicate.
    AlwaysTrue,
}

/// Min/Max + Null statistics maintained per RowGroup and per 2,048-row sub-block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneMap {
    pub min: Option<ScalarValue>,
    pub max: Option<ScalarValue>,
    pub null_count: u32,
    pub row_count: u32,
}

impl ZoneMap {
    pub fn empty() -> Self {
        Self {
            min: None,
            max: None,
            null_count: 0,
            row_count: 0,
        }
    }

    pub fn from_flat_vector(flat: &FlatVector) -> Self {
        let mut zm = Self::empty();
        zm.row_count = flat.len as u32;
        zm.null_count = flat.validity.count_nulls() as u32;
        for i in flat.validity.iter_valid_indices() {
            let v = flat.get_scalar(i);
            if v.is_null() {
                continue;
            }
            if matches!(v, ScalarValue::Float64(f) if f.is_nan()) {
                // Conservatively avoid tight float bounds if NaN is present.
                zm.min = None;
                zm.max = None;
                return zm;
            }
            zm.min = Some(match zm.min.take() {
                None => v.clone(),
                Some(cur) => {
                    if v.total_cmp(&cur).is_lt() {
                        v.clone()
                    } else {
                        cur
                    }
                }
            });
            zm.max = Some(match zm.max.take() {
                None => v,
                Some(cur) => {
                    if v.total_cmp(&cur).is_gt() {
                        v
                    } else {
                        cur
                    }
                }
            });
        }
        zm
    }

    pub fn merge(&mut self, other: &Self) {
        self.row_count += other.row_count;
        self.null_count += other.null_count;
        if let Some(omin) = &other.min {
            self.min = Some(match self.min.take() {
                None => omin.clone(),
                Some(cur) => {
                    if omin.total_cmp(&cur).is_lt() {
                        omin.clone()
                    } else {
                        cur
                    }
                }
            });
        }
        if let Some(omax) = &other.max {
            self.max = Some(match self.max.take() {
                None => omax.clone(),
                Some(cur) => {
                    if omax.total_cmp(&cur).is_gt() {
                        omax.clone()
                    } else {
                        cur
                    }
                }
            });
        }
    }

    pub fn has_nulls(&self) -> bool {
        self.null_count > 0
    }

    pub fn is_all_null(&self) -> bool {
        self.null_count == self.row_count
    }
}

/// Parquet-standard 256-bit Split-Block Bloom Filter salt constants (Putze et al.).
const SALT: [u32; 8] = [
    0x47b6_137b,
    0x4497_4d91,
    0x8824_ad5b,
    0xa2b7_289d,
    0x7054_95c7,
    0x2df1_424b,
    0x9efc_4947,
    0x5c6b_fb31,
];

/// Cache-friendly 256-bit Split-Block Bloom Filter (8 x `u32` lanes per block).
/// Sized at ~10 bits per key (< 1.5% false positive rate) for high-cardinality point lookups.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitBlockBloomFilter {
    blocks: Vec<[u32; 8]>,
}

impl SplitBlockBloomFilter {
    /// Creates a filter sized for `expected_keys` at ~10 bits/key (power-of-two block count).
    pub fn with_capacity(expected_keys: usize) -> Self {
        let bits = (expected_keys.max(1) * 10).max(256);
        let raw_blocks = bits.div_ceil(256);
        let num_blocks = raw_blocks.next_power_of_two().max(1);
        Self {
            blocks: vec![[0u32; 8]; num_blocks],
        }
    }

    #[inline]
    fn block_index(&self, hash: u64) -> usize {
        // Use upper 32 bits multiplied by num_blocks (Lemire fast range) or mask since power of 2.
        ((hash >> 32) as usize) & (self.blocks.len() - 1)
    }

    #[inline]
    pub fn insert_hash(&mut self, hash: u64) {
        let idx = self.block_index(hash);
        let key = hash as u32;
        let block = &mut self.blocks[idx];
        for lane in 0..8 {
            let bit = key.wrapping_mul(SALT[lane]) >> 27;
            block[lane] |= 1u32 << bit;
        }
    }

    #[inline]
    pub fn might_contain_hash(&self, hash: u64) -> bool {
        let idx = self.block_index(hash);
        let key = hash as u32;
        let block = &self.blocks[idx];
        for lane in 0..8 {
            let bit = key.wrapping_mul(SALT[lane]) >> 27;
            if (block[lane] & (1u32 << bit)) == 0 {
                return false;
            }
        }
        true
    }

    #[inline]
    pub fn insert_scalar(&mut self, val: &ScalarValue) {
        if !val.is_null() {
            self.insert_hash(val.canonical_hash());
        }
    }

    #[inline]
    pub fn might_contain_scalar(&self, val: &ScalarValue) -> bool {
        if val.is_null() {
            return true;
        }
        self.might_contain_hash(val.canonical_hash())
    }

    pub fn from_flat_vector(flat: &FlatVector) -> Self {
        let non_null = flat.validity.count_valid();
        let mut bf = Self::with_capacity(non_null);
        for i in flat.validity.iter_valid_indices() {
            let s = flat.get_scalar(i);
            bf.insert_scalar(&s);
        }
        bf
    }

    pub fn size_bytes(&self) -> usize {
        self.blocks.len() * 32
    }
}

/// Block-level pruning predicate pushable directly into columnar RowGroups and Parquet scans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PruningPredicate {
    Eq(usize, ScalarValue),
    Ne(usize, ScalarValue),
    Lt(usize, ScalarValue),
    Le(usize, ScalarValue),
    Gt(usize, ScalarValue),
    Ge(usize, ScalarValue),
    Between(usize, ScalarValue, ScalarValue),
    In(usize, Vec<ScalarValue>),
    IsNull(usize),
    IsNotNull(usize),
    And(Box<PruningPredicate>, Box<PruningPredicate>),
    Or(Box<PruningPredicate>, Box<PruningPredicate>),
}

impl PruningPredicate {
    /// Evaluates this predicate against per-column `ZoneMap`s and optional `SplitBlockBloomFilter`s.
    pub fn evaluate(
        &self,
        zone_maps: &[ZoneMap],
        blooms: Option<&[Option<SplitBlockBloomFilter>]>,
    ) -> BlockPruningResult {
        match self {
            Self::And(a, b) => {
                let ra = a.evaluate(zone_maps, blooms);
                if ra == BlockPruningResult::AlwaysFalse {
                    return BlockPruningResult::AlwaysFalse;
                }
                let rb = b.evaluate(zone_maps, blooms);
                match (ra, rb) {
                    (_, BlockPruningResult::AlwaysFalse) => BlockPruningResult::AlwaysFalse,
                    (BlockPruningResult::AlwaysTrue, BlockPruningResult::AlwaysTrue) => {
                        BlockPruningResult::AlwaysTrue
                    }
                    _ => BlockPruningResult::MaybeTrue,
                }
            }
            Self::Or(a, b) => {
                let ra = a.evaluate(zone_maps, blooms);
                if ra == BlockPruningResult::AlwaysTrue {
                    return BlockPruningResult::AlwaysTrue;
                }
                let rb = b.evaluate(zone_maps, blooms);
                match (ra, rb) {
                    (_, BlockPruningResult::AlwaysTrue) => BlockPruningResult::AlwaysTrue,
                    (BlockPruningResult::AlwaysFalse, BlockPruningResult::AlwaysFalse) => {
                        BlockPruningResult::AlwaysFalse
                    }
                    _ => BlockPruningResult::MaybeTrue,
                }
            }
            Self::IsNull(col) => {
                let zm = &zone_maps[*col];
                if zm.null_count == 0 {
                    BlockPruningResult::AlwaysFalse
                } else if zm.is_all_null() {
                    BlockPruningResult::AlwaysTrue
                } else {
                    BlockPruningResult::MaybeTrue
                }
            }
            Self::IsNotNull(col) => {
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    BlockPruningResult::AlwaysFalse
                } else if zm.null_count == 0 {
                    BlockPruningResult::AlwaysTrue
                } else {
                    BlockPruningResult::MaybeTrue
                }
            }
            Self::Eq(col, lit) => {
                if lit.is_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                if let (Some(min), Some(max)) = (&zm.min, &zm.max) {
                    if lit.total_cmp(min).is_lt() || lit.total_cmp(max).is_gt() {
                        return BlockPruningResult::AlwaysFalse;
                    }
                    if min == max && min == lit && zm.null_count == 0 {
                        return BlockPruningResult::AlwaysTrue;
                    }
                }
                if let Some(bf_slice) = blooms {
                    if let Some(Some(bf)) = bf_slice.get(*col) {
                        if !bf.might_contain_scalar(lit) {
                            return BlockPruningResult::AlwaysFalse;
                        }
                    }
                }
                BlockPruningResult::MaybeTrue
            }
            Self::Ne(col, lit) => {
                if lit.is_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                if let (Some(min), Some(max)) = (&zm.min, &zm.max) {
                    if min == max && min == lit {
                        return BlockPruningResult::AlwaysFalse;
                    }
                    if (lit.total_cmp(min).is_lt() || lit.total_cmp(max).is_gt())
                        && zm.null_count == 0
                    {
                        return BlockPruningResult::AlwaysTrue;
                    }
                }
                BlockPruningResult::MaybeTrue
            }
            Self::Lt(col, lit) => {
                if lit.is_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                if let (Some(min), Some(max)) = (&zm.min, &zm.max) {
                    if min.total_cmp(lit).is_ge() {
                        return BlockPruningResult::AlwaysFalse;
                    }
                    if max.total_cmp(lit).is_lt() && zm.null_count == 0 {
                        return BlockPruningResult::AlwaysTrue;
                    }
                }
                BlockPruningResult::MaybeTrue
            }
            Self::Le(col, lit) => {
                if lit.is_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                if let (Some(min), Some(max)) = (&zm.min, &zm.max) {
                    if min.total_cmp(lit).is_gt() {
                        return BlockPruningResult::AlwaysFalse;
                    }
                    if max.total_cmp(lit).is_le() && zm.null_count == 0 {
                        return BlockPruningResult::AlwaysTrue;
                    }
                }
                BlockPruningResult::MaybeTrue
            }
            Self::Gt(col, lit) => {
                if lit.is_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                if let (Some(min), Some(max)) = (&zm.min, &zm.max) {
                    if max.total_cmp(lit).is_le() {
                        return BlockPruningResult::AlwaysFalse;
                    }
                    if min.total_cmp(lit).is_gt() && zm.null_count == 0 {
                        return BlockPruningResult::AlwaysTrue;
                    }
                }
                BlockPruningResult::MaybeTrue
            }
            Self::Ge(col, lit) => {
                if lit.is_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                if let (Some(min), Some(max)) = (&zm.min, &zm.max) {
                    if max.total_cmp(lit).is_lt() {
                        return BlockPruningResult::AlwaysFalse;
                    }
                    if min.total_cmp(lit).is_ge() && zm.null_count == 0 {
                        return BlockPruningResult::AlwaysTrue;
                    }
                }
                BlockPruningResult::MaybeTrue
            }
            Self::Between(col, low, high) => {
                if low.is_null() || high.is_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                if let (Some(min), Some(max)) = (&zm.min, &zm.max) {
                    if max.total_cmp(low).is_lt() || min.total_cmp(high).is_gt() {
                        return BlockPruningResult::AlwaysFalse;
                    }
                    if min.total_cmp(low).is_ge()
                        && max.total_cmp(high).is_le()
                        && zm.null_count == 0
                    {
                        return BlockPruningResult::AlwaysTrue;
                    }
                }
                BlockPruningResult::MaybeTrue
            }
            Self::In(col, list) => {
                let non_nulls: Vec<&ScalarValue> = list.iter().filter(|v| !v.is_null()).collect();
                if non_nulls.is_empty() {
                    return BlockPruningResult::AlwaysFalse;
                }
                let zm = &zone_maps[*col];
                if zm.is_all_null() {
                    return BlockPruningResult::AlwaysFalse;
                }
                for lit in non_nulls {
                    let eq = Self::Eq(*col, lit.clone());
                    if eq.evaluate(zone_maps, blooms) != BlockPruningResult::AlwaysFalse {
                        return BlockPruningResult::MaybeTrue;
                    }
                }
                BlockPruningResult::AlwaysFalse
            }
        }
    }
}
