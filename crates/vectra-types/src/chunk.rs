use crate::error::Result;
use crate::memory::MemoryPool;
use crate::selection::SelectionVector;
use crate::types::{LogicalType, ScalarValue};
use crate::vector::{FlatVector, Vector};
use std::sync::Arc;

/// A batch of columnar `Vector`s sharing a logical `SelectionVector` and row `count`.
#[derive(Debug, Clone)]
pub struct DataChunk {
    pub columns: Vec<Vector>,
    pub sel: SelectionVector,
    pub count: usize,
}

impl DataChunk {
    pub fn new(columns: Vec<Vector>, count: usize) -> Self {
        for col in &columns {
            debug_assert!(
                col.len() >= count,
                "column len {} < chunk count {count}",
                col.len()
            );
        }
        Self {
            columns,
            sel: SelectionVector::identity(count),
            count,
        }
    }

    pub fn with_selection(columns: Vec<Vector>, sel: SelectionVector) -> Self {
        let count = sel.len();
        Self {
            columns,
            sel,
            count,
        }
    }

    pub fn empty(types: &[LogicalType], pool: &Arc<dyn MemoryPool>) -> Self {
        let columns = types
            .iter()
            .map(|&ty| Vector::Flat(FlatVector::empty(ty, pool)))
            .collect();
        Self::new(columns, 0)
    }

    #[inline]
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    pub fn column(&self, idx: usize) -> &Vector {
        &self.columns[idx]
    }

    /// Applies an additional selection filter (`child_sel` indexes into `0..self.count`)
    /// without copying any column buffers (late materialization!).
    pub fn filter(&self, child_sel: &SelectionVector) -> Self {
        let composed = self.sel.compose(child_sel);
        let count = composed.len();
        Self {
            columns: self.columns.clone(),
            sel: composed,
            count,
        }
    }

    /// Slices the logical view `[offset..offset + len)` without copying column data.
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let sliced_sel = self.sel.slice(offset, len);
        Self {
            columns: self.columns.clone(),
            sel: sliced_sel,
            count: len,
        }
    }

    /// Materializes every column through `self.sel` into flat, identity-indexed vectors.
    pub fn compact(&self, pool: &Arc<dyn MemoryPool>) -> Result<Self> {
        let mut flat_cols = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            flat_cols.push(Vector::Flat(col.flatten(&self.sel, self.count, pool)?));
        }
        Ok(Self::new(flat_cols, self.count))
    }

    /// Reads one logical row as `Vec<ScalarValue>` (for testing, CLI output, and reference eval).
    pub fn get_row(&self, logical_row: usize) -> Vec<ScalarValue> {
        assert!(logical_row < self.count);
        let phys = self.sel.get(logical_row);
        self.columns.iter().map(|c| c.get_scalar(phys)).collect()
    }

    pub fn logical_types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|c| c.logical_type()).collect()
    }

    pub fn memory_bytes(&self) -> usize {
        let cols: usize = self.columns.iter().map(|c| c.memory_bytes()).sum();
        let sel = self.sel.as_indices().map_or(0, |s| s.len() * 2);
        cols + sel
    }
}
