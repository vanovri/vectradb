use crate::metrics::ExecutionMetrics;
use crate::spill::SpillManager;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vectra_storage::{Catalog, Snapshot};
use vectra_types::{BoundedMemoryPool, DataChunk, MemoryPool, Result, VectraError};

/// Configuration knobs for query execution (controlled via SQL `SET` or CLI `.memlimit`).
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    /// Hard memory ceiling enforced by `BoundedMemoryPool` (bytes).
    pub memory_limit_bytes: usize,
    /// Worker thread count for morsel-driven parallel operators.
    pub threads: usize,
    /// Directory where temporary `.vspill` files are stored.
    pub spill_dir: PathBuf,
    /// Safety guard for recursive CTE fixpoint iterations.
    pub max_recursive_iterations: usize,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 8))
            .unwrap_or(4);
        #[cfg(target_arch = "wasm32")]
        let threads = 1;

        #[cfg(not(target_arch = "wasm32"))]
        let spill_dir = std::env::temp_dir().join("vectradb_spill");
        #[cfg(target_arch = "wasm32")]
        let spill_dir = PathBuf::from("/tmp/vectradb_spill");

        Self {
            memory_limit_bytes: 256 * 1024 * 1024,
            threads,
            spill_dir,
            max_recursive_iterations: 1000,
        }
    }
}

/// Shared immutable/atomic runtime state threaded through every physical operator in a query.
#[derive(Clone)]
pub struct ExecutionContext {
    pub catalog: Arc<Catalog>,
    pub snapshot: Snapshot,
    pub pool: Arc<dyn MemoryPool>,
    pub spill: Arc<SpillManager>,
    pub metrics: Arc<ExecutionMetrics>,
    pub cancelled: Arc<AtomicBool>,
    pub config: ExecutionConfig,
    /// Active materialized CTE tables (populated by `RecursiveCte` fixpoint execution).
    pub cte_tables: Arc<HashMap<String, Vec<DataChunk>>>,
}

impl ExecutionContext {
    pub fn new(catalog: Arc<Catalog>, snapshot: Snapshot, config: ExecutionConfig) -> Result<Self> {
        let pool = BoundedMemoryPool::new(config.memory_limit_bytes).as_dyn();
        let spill = SpillManager::new(&config.spill_dir)?;
        Ok(Self {
            catalog,
            snapshot,
            pool,
            spill,
            metrics: Arc::new(ExecutionMetrics::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
            config,
            cte_tables: Arc::new(HashMap::new()),
        })
    }

    pub fn with_pool(
        catalog: Arc<Catalog>,
        snapshot: Snapshot,
        pool: Arc<dyn MemoryPool>,
        spill: Arc<SpillManager>,
        config: ExecutionConfig,
    ) -> Self {
        Self {
            catalog,
            snapshot,
            pool,
            spill,
            metrics: Arc::new(ExecutionMetrics::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
            config,
            cte_tables: Arc::new(HashMap::new()),
        }
    }

    pub fn with_cancel_flag(mut self, cancelled: Arc<AtomicBool>) -> Self {
        self.cancelled = cancelled;
        self
    }

    pub fn with_cte_binding(&self, name: String, chunks: Vec<DataChunk>) -> Self {
        let mut map = (*self.cte_tables).clone();
        map.insert(name.to_ascii_lowercase(), chunks);
        let mut next = self.clone();
        next.cte_tables = Arc::new(map);
        next
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(VectraError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Computes a safe working budget fraction of `pool.limit_bytes()` for a single
    /// memory-intensive operator (HashJoin build, HashAggregate table, ExternalSort run),
    /// ensuring room remains for streaming input/output chunks and spill buffers.
    pub fn operator_budget(&self, numerator: usize, denominator: usize) -> usize {
        let lim = self.pool.limit_bytes();
        if lim == usize::MAX {
            return usize::MAX / 2;
        }
        let target = (lim as u128 * numerator as u128 / denominator as u128) as usize;
        target.max(256 * 1024)
    }
}
